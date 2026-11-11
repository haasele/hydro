//! `argparse`: argument parsing with CPython-compatible exit behaviour.
//!
//! Supported surface: `ArgumentParser` with the documented constructor
//! parameters, `add_argument` with the standard actions (`store`,
//! `store_true`, `store_false`, `store_const`, `append`, `append_const`,
//! `count`, `help`, `version`, `boolean_optional`), `nargs` (`None`, `?`,
//! `*`, `+` and an exact count), `type`, `choices`, `default`, `required`,
//! `const`, `metavar` and `dest`; argument groups and mutually exclusive
//! groups; subparsers; `set_defaults`; `parse_args`/`parse_known_args`;
//! `error`/`exit`/`print_usage`/`print_help`/`format_usage`/`format_help`;
//! `Namespace`, `HelpFormatter`, `RawTextHelpFormatter`,
//! `RawDescriptionHelpFormatter`, `ArgumentDefaultsHelpFormatter`,
//! `MetavarTypeHelpFormatter`, `SUPPRESS`, `FileType`, `Action` and
//! `BooleanOptionalAction`.
//!
//! Parse errors print the usage block to stderr followed by
//! `prog: error: ...` and raise `SystemExit(2)`; `-h`/`--help` print the help
//! and raise `SystemExit(0)`, as does `--version`. Both exceptions are
//! ordinary catchable `SystemExit` values.
//!
//! An explicit `parse_args([...])` list behaves exactly like CPython; the
//! legacy non-exiting mode used by the runtime unit tests is available only
//! through the internal `_hydro_test_mode` method.

use std::io::Write;

use crate::builtins;
use crate::container;
use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::methods::KeywordArgs;
use crate::value::{self, ParamMeta, NONE};
use hydro_ir::{MethodKind, ParamKind};

/// `argparse.SUPPRESS`: the sentinel string CPython uses to hide a default or
/// a help entry.
pub(crate) const SUPPRESS: &str = "==SUPPRESS==";

/// Internal attribute marking a namespace as an argparse handle (a group or
/// the subparsers container).
const HANDLE: &str = "__argparse_handle__";
/// Handle attribute: the owning parser value.
const HANDLE_PARSER: &str = "__argparse_parser__";
/// Handle attribute: the group index for `add_argument_group` handles.
const HANDLE_GROUP: &str = "__argparse_group__";
/// Internal attribute marking a namespace as a `FileType` conversion helper.
const FILE_TYPE: &str = "__argparse_file_type__";

/// Argument consumption pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nargs {
    Single,
    Optional,
    ZeroOrMore,
    OneOrMore,
    Exact(usize),
}

/// What an argument does with its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionKind {
    Store,
    StoreTrue,
    StoreFalse,
    StoreConst,
    Append,
    AppendConst,
    Count,
    Help,
    Version,
    BooleanOptional,
}

impl ActionKind {
    /// Actions that consume no command-line value.
    fn takes_no_value(self) -> bool {
        matches!(
            self,
            ActionKind::StoreTrue
                | ActionKind::StoreFalse
                | ActionKind::StoreConst
                | ActionKind::AppendConst
                | ActionKind::Count
                | ActionKind::Help
                | ActionKind::Version
                | ActionKind::BooleanOptional
        )
    }
}

/// Definition of one `add_argument` call.
#[derive(Clone)]
pub(crate) struct ArgSpec {
    /// Source spellings: `["-f", "--flag"]` or `["input"]`.
    pub(crate) flags: Vec<String>,
    /// Negative spellings of a `BooleanOptionalAction` option.
    pub(crate) negative_flags: Vec<String>,
    pub(crate) dest: String,
    pub(crate) positional: bool,
    pub(crate) action: ActionKind,
    /// Conversion function value, or `None`.
    pub(crate) type_fn: i64,
    pub(crate) default: i64,
    /// Value used by `store_const`/`append_const` and by an option with
    /// `nargs="?"` that is present without a value.
    pub(crate) const_value: i64,
    pub(crate) required: bool,
    pub(crate) help: String,
    pub(crate) nargs: Nargs,
    /// Allowed converted values, in declaration order.
    pub(crate) choices: Option<Vec<i64>>,
    pub(crate) metavar: Option<String>,
    /// Group this argument belongs to (display or mutually exclusive group).
    pub(crate) group: usize,
    /// Version text of a `version` action.
    pub(crate) version: String,
}

/// One argument group. Index 0 is the unnamed default group.
#[derive(Clone, Default)]
#[allow(dead_code)] // description is retained for full help rendering.
pub(crate) struct GroupState {
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    /// Mutually exclusive group: at most one member may be supplied.
    pub(crate) mutex: bool,
    /// Display group containing a mutually exclusive group's arguments.
    pub(crate) parent: usize,
    pub(crate) required: bool,
    /// Indices into `ArgParserState::options`.
    pub(crate) members: Vec<usize>,
}

/// Subparser container attached to a parser.
#[derive(Clone)]
#[allow(dead_code)] // title/description/metavar are retained for future help rendering.
pub(crate) struct SubparsersState {
    pub(crate) dest: Option<String>,
    pub(crate) required: bool,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) metavar: Option<String>,
    /// `(name, child parser, help text)` in declaration order.
    pub(crate) parsers: Vec<(String, i64, String)>,
}

/// State of an `argparse.ArgumentParser`.
pub(crate) struct ArgParserState {
    pub(crate) prog: Option<String>,
    pub(crate) usage: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) epilog: Option<String>,
    pub(crate) formatter_class: i64,
    pub(crate) prefix_chars: String,
    pub(crate) fromfile_prefix_chars: Option<String>,
    pub(crate) argument_default: i64,
    pub(crate) conflict_handler: String,
    pub(crate) add_help: bool,
    pub(crate) allow_abbrev: bool,
    pub(crate) exit_on_error: bool,
    /// CPython 3.14's `suggest_on_error=` constructor keyword. Hydro's
    /// messages do not add "did you mean" hints yet, so the flag is stored for
    /// introspection and repr.
    pub(crate) suggest_on_error: bool,
    /// CPython 3.14's `color=` constructor keyword. When true (and
    /// `_colorize.can_colorize()` allows it) usage and help output carry the
    /// same ANSI styling as CPython's formatter.
    pub(crate) color: bool,
    pub(crate) options: Vec<ArgSpec>,
    pub(crate) groups: Vec<GroupState>,
    pub(crate) subparsers: Option<SubparsersState>,
    pub(crate) defaults: Vec<(String, i64)>,
    /// Internal: the legacy non-exiting behaviour used by runtime tests.
    pub(crate) test_mode: bool,
}

impl ArgParserState {
    pub(crate) fn new(prog: Option<String>, description: Option<String>) -> ArgParserState {
        ArgParserState {
            prog,
            usage: None,
            description,
            epilog: None,
            formatter_class: NONE,
            prefix_chars: "-".to_string(),
            fromfile_prefix_chars: None,
            argument_default: NONE,
            conflict_handler: "error".to_string(),
            add_help: true,
            allow_abbrev: true,
            exit_on_error: true,
            suggest_on_error: false,
            color: true,
            options: Vec::new(),
            groups: vec![GroupState::default()],
            subparsers: None,
            defaults: Vec::new(),
            test_mode: false,
        }
    }
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn attribute_error(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("'ArgumentParser' object has no attribute '{name}'"),
    )
}

fn tagged_int(n: i64) -> i64 {
    value::encode_int(n).unwrap_or(NONE)
}

fn text_value(text: &str) -> i64 {
    value::str_alloc(text.to_string())
}

/// Keeps a freshly allocated value alive while the remaining arguments are
/// converted (allocations may collect).
fn root_value(roots: i64, value: i64) {
    if let Some(mut items) = value::list_items_mut(roots) {
        items.push(value);
        crate::heap::write_barrier_edge(roots, value);
    }
}

// -------------------------------------------------------------------------
// Native classes and parser-instance plumbing
// -------------------------------------------------------------------------

/// Hidden instance attribute holding the native `ArgParserState` payload.
///
/// `ArgumentParser` is a real class so that `class P(...)` subclassing and
/// `type(parser)` work. The public Python attributes (`prog`, `usage`, ...)
/// live in the instance `__dict__`; the parsed definition stays native and is
/// linked through this NUL-prefixed name, which no Python attribute can spell.
const PARSER_STATE_ATTR: &str = "\u{0}hydro_arg_parser_state";

/// Registry keys for the cached class values.
const PARSER_CLASS_KEY: &str = "argparse.ArgumentParser.class";
const NAMESPACE_CLASS_KEY: &str = "argparse.Namespace.class";

fn self_param() -> ParamMeta {
    ParamMeta {
        name: "self".to_string(),
        kind: ParamKind::Positional,
        default: None,
    }
}

fn method_signature() -> Vec<ParamMeta> {
    vec![
        self_param(),
        ParamMeta {
            name: "args".to_string(),
            kind: ParamKind::VarArg,
            default: None,
        },
        ParamMeta {
            name: "kwargs".to_string(),
            kind: ParamKind::KwArg,
            default: None,
        },
    ]
}

/// Resolves the native parser payload behind either a bare native parser value
/// (the internal representation) or an `ArgumentParser` class instance.
fn parser_state_value(obj: i64) -> Option<i64> {
    if value::as_arg_parser(obj).is_some() {
        return Some(obj);
    }
    let inner = value::instance_attr(obj, PARSER_STATE_ATTR)?;
    value::as_arg_parser(inner).map(|_| inner)
}

fn parser_ref(obj: i64) -> Option<&'static ArgParserState> {
    parser_state_value(obj).and_then(value::as_arg_parser)
}

fn parser_mut(obj: i64) -> Option<value::ContainerGuard<ArgParserState>> {
    parser_state_value(obj).and_then(value::arg_parser_mut)
}

/// Builds a native function with the `(self, *args, **kwargs)` signature used
/// by every `ArgumentParser` method. The generic binder then hands the native
/// entry point the receiver, a list of positional arguments and a keyword
/// dict, matching this module's own argument parsing.
/// The qualified name of a parser method. Native function values require a
/// `&'static str`, so the closed set of method spellings is a match rather
/// than an allocation.
fn parser_member_name(display: &str) -> &'static str {
    match display {
        "add_argument" => "ArgumentParser.add_argument",
        "add_argument_group" => "ArgumentParser.add_argument_group",
        "add_mutually_exclusive_group" => "ArgumentParser.add_mutually_exclusive_group",
        "add_subparsers" => "ArgumentParser.add_subparsers",
        "set_defaults" => "ArgumentParser.set_defaults",
        "get_default" => "ArgumentParser.get_default",
        "parse_args" => "ArgumentParser.parse_args",
        "parse_known_args" => "ArgumentParser.parse_known_args",
        "parse_intermixed_args" => "ArgumentParser.parse_intermixed_args",
        "parse_known_intermixed_args" => "ArgumentParser.parse_known_intermixed_args",
        "print_help" => "ArgumentParser.print_help",
        "print_usage" => "ArgumentParser.print_usage",
        "format_help" => "ArgumentParser.format_help",
        "format_usage" => "ArgumentParser.format_usage",
        "error" => "ArgumentParser.error",
        "exit" => "ArgumentParser.exit",
        "__repr__" => "ArgumentParser.__repr__",
        _ => "ArgumentParser.__init__",
    }
}

fn namespace_member_name(display: &str) -> &'static str {
    match display {
        "__repr__" => "Namespace.__repr__",
        "__eq__" => "Namespace.__eq__",
        "__ne__" => "Namespace.__ne__",
        "__contains__" => "Namespace.__contains__",
        _ => "Namespace.__init__",
    }
}

fn parser_native_method(display: &str, entry: usize) -> i64 {
    let function = value::native_function_new("argparse", parser_member_name(display), usize::MAX);
    value::function_set_signature(function, method_signature());
    value::function_set_full_entry(function, entry);
    function
}

fn parser_native_init(entry: usize) -> i64 {
    let function = value::native_function_new("argparse", "ArgumentParser.__init__", usize::MAX);
    value::function_set_signature(function, method_signature());
    value::function_set_full_entry(function, entry);
    function
}

fn parser_plain_method(display: &str, params: Vec<ParamMeta>, entry: usize) -> i64 {
    let function = value::native_function_new("argparse", parser_member_name(display), usize::MAX);
    value::function_set_signature(function, params);
    value::function_set_full_entry(function, entry);
    function
}

fn parser_entry(name: &str, argv: *const i64) -> i64 {
    // SAFETY: the method binder supplies `(self, varargs_list, kwargs_dict)`
    // for the `(self, *args, **kwargs)` signature above.
    let receiver = unsafe { *argv };
    let mut positionals = unsafe { value::as_list(*argv.add(1)) }
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let mut keywords = Vec::new();
    if let Some(pairs) = unsafe { value::as_dict(*argv.add(2)) } {
        for (key, item) in pairs {
            if let Some(key) = value::as_str(*key) {
                keywords.push((key.to_string(), *item));
            }
        }
    }
    method_kw(
        receiver,
        name,
        &mut positionals,
        &KeywordArgs { items: keywords },
    )
}

/// Creates the cached `argparse.ArgumentParser` class.
fn argument_parser_class() -> i64 {
    if let Some(class) = globals::registry_get(PARSER_CLASS_KEY) {
        return class;
    }
    let mut methods: Vec<(String, i64, MethodKind)> = Vec::new();
    let functions = [
        parser_native_init(parser_init_entry as *const () as usize),
        parser_plain_method(
            "__repr__",
            vec![self_param()],
            parser_repr_entry as *const () as usize,
        ),
    ];
    for (display, entry) in parser_method_entries() {
        methods.push((
            display.to_string(),
            parser_native_method(display, entry),
            MethodKind::Normal,
        ));
    }
    methods.push(("__repr__".to_string(), functions[1], MethodKind::Normal));
    // `__init__` is the class constructor slot.
    methods.push(("__init__".to_string(), functions[0], MethodKind::Normal));
    let method_values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let roots = crate::heap::root_values(&method_values);
    let class = value::class_new_full("ArgumentParser".to_string(), NONE, methods, Vec::new());
    let class_roots = crate::heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(PARSER_CLASS_KEY, class);
    crate::heap::release_roots(class_roots);
    crate::heap::release_roots(roots);
    class
}

/// `ArgumentParser` methods that accept arbitrary positional/keyword argument
/// lists and reuse the module's existing `method_kw` dispatch. Function
/// pointers are converted to entry-point integers at run time because
/// pointer-to-integer casts are not permitted in const evaluation.
fn parser_method_entries() -> Vec<(&'static str, usize)> {
    vec![
        (
            "add_argument",
            parser_add_argument_entry as *const () as usize,
        ),
        (
            "add_argument_group",
            parser_add_argument_group_entry as *const () as usize,
        ),
        (
            "add_mutually_exclusive_group",
            parser_add_mutually_exclusive_group_entry as *const () as usize,
        ),
        (
            "add_subparsers",
            parser_add_subparsers_entry as *const () as usize,
        ),
        (
            "set_defaults",
            parser_set_defaults_entry as *const () as usize,
        ),
        (
            "get_default",
            parser_get_default_entry as *const () as usize,
        ),
        ("parse_args", parser_parse_args_entry as *const () as usize),
        (
            "parse_known_args",
            parser_parse_known_args_entry as *const () as usize,
        ),
        (
            "parse_intermixed_args",
            parser_parse_intermixed_args_entry as *const () as usize,
        ),
        (
            "parse_known_intermixed_args",
            parser_parse_known_intermixed_args_entry as *const () as usize,
        ),
        ("print_help", parser_print_help_entry as *const () as usize),
        (
            "print_usage",
            parser_print_usage_entry as *const () as usize,
        ),
        (
            "format_help",
            parser_format_help_entry as *const () as usize,
        ),
        (
            "format_usage",
            parser_format_usage_entry as *const () as usize,
        ),
        ("error", parser_error_entry as *const () as usize),
        ("exit", parser_exit_entry as *const () as usize),
        (
            "_hydro_test_mode",
            parser_test_mode_entry as *const () as usize,
        ),
    ]
}

macro_rules! parser_entries {
    ($(($entry:ident, $name:literal)),* $(,)?) => {
        $(
            extern "C" fn $entry(argv: *const i64) -> i64 {
                parser_entry($name, argv)
            }
        )*
    };
}

parser_entries! {
    (parser_add_argument_entry, "add_argument"),
    (parser_add_argument_group_entry, "add_argument_group"),
    (
        parser_add_mutually_exclusive_group_entry,
        "add_mutually_exclusive_group"
    ),
    (parser_add_subparsers_entry, "add_subparsers"),
    (parser_set_defaults_entry, "set_defaults"),
    (parser_get_default_entry, "get_default"),
    (parser_parse_args_entry, "parse_args"),
    (parser_parse_known_args_entry, "parse_known_args"),
    (parser_parse_intermixed_args_entry, "parse_intermixed_args"),
    (
        parser_parse_known_intermixed_args_entry,
        "parse_known_intermixed_args"
    ),
    (parser_print_help_entry, "print_help"),
    (parser_print_usage_entry, "print_usage"),
    (parser_format_help_entry, "format_help"),
    (parser_format_usage_entry, "format_usage"),
    (parser_error_entry, "error"),
    (parser_exit_entry, "exit"),
    (parser_test_mode_entry, "_hydro_test_mode"),
}

extern "C" fn parser_init_entry(argv: *const i64) -> i64 {
    // SAFETY: `__init__` uses the same binder shape as the other methods.
    let instance = unsafe { *argv };
    let positionals = unsafe { value::as_list(*argv.add(1)) }
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let mut keywords = Vec::new();
    if let Some(pairs) = unsafe { value::as_dict(*argv.add(2)) } {
        for (key, item) in pairs {
            if let Some(key) = value::as_str(*key) {
                keywords.push((key.to_string(), *item));
            }
        }
    }
    init_parser_instance(instance, &positionals, &keywords)
}

extern "C" fn parser_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: the signature is `(self)`.
    parser_repr(unsafe { *argv })
}

/// Creates a fresh `ArgumentParser` instance from the full positional
/// constructor slot layout, used by module `call` and internal callers.
fn new_parser_instance(args: &[i64]) -> i64 {
    new_parser(args)
}

/// Binds `ArgumentParser.__init__`'s declared parameters and installs the
/// native parser payload plus CPython's public instance attributes.
fn init_parser_instance(instance: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    const PARAMS: [&str; 15] = [
        "prog",
        "usage",
        "description",
        "epilog",
        "parents",
        "formatter_class",
        "prefix_chars",
        "fromfile_prefix_chars",
        "argument_default",
        "conflict_handler",
        "add_help",
        "allow_abbrev",
        "exit_on_error",
        "suggest_on_error",
        "color",
    ];
    /// CPython's `suggest_on_error` and `color` are keyword-only.
    const POSITIONAL_LIMIT: usize = 13;
    if args.len() > POSITIONAL_LIMIT {
        return type_error(&format!(
            "ArgumentParser.__init__() takes from 1 to {} positional arguments but {} were given",
            POSITIONAL_LIMIT + 1,
            args.len() + 1
        ));
    }
    let mut slots: [Option<i64>; PARAMS.len()] = [None; PARAMS.len()];
    for (index, argument) in args.iter().enumerate() {
        slots[index] = Some(*argument);
    }
    for (name, argument) in keywords {
        let Some(position) = PARAMS.iter().position(|param| *param == name.as_str()) else {
            return type_error(&format!(
                "ArgumentParser.__init__() got an unexpected keyword argument '{name}'"
            ));
        };
        if slots[position].is_some() {
            return type_error(&format!(
                "ArgumentParser.__init__() got multiple values for argument '{name}'"
            ));
        }
        slots[position] = Some(*argument);
    }
    let values: Vec<i64> = slots.iter().map(|slot| slot.unwrap_or(NONE)).collect();
    let state = match build_parser_state(&values) {
        Ok(state) => state,
        Err(error) => return error,
    };
    install_parser_state(instance, state)
}

/// Links a freshly built native definition to its Python instance and
/// publishes the public attributes CPython stores in `__dict__`.
fn install_parser_state(instance: i64, state: ArgParserState) -> i64 {
    let state_value = value::arg_parser_new(state);
    let roots = crate::heap::root_values(&[instance, state_value]);
    value::instance_setattr(instance, PARSER_STATE_ATTR.to_string(), state_value);
    publish_parser_attrs(instance, state_value);
    crate::heap::release_roots(roots);
    NONE
}

/// Mirrors CPython's public `ArgumentParser.__dict__` fields so attribute
/// reads, `vars()` and `repr` observe the constructor arguments.
fn publish_parser_attrs(instance: i64, state_value: i64) {
    let Some(state) = value::as_arg_parser(state_value) else {
        return;
    };
    let roots = crate::heap::root_values(&[instance, state_value]);
    let set = |name: &str, item: i64| {
        value::instance_setattr(instance, name.to_string(), item);
    };
    set(
        "prog",
        state.prog.as_ref().map_or(NONE, |prog| text_value(prog)),
    );
    set(
        "usage",
        state.usage.as_ref().map_or(NONE, |usage| text_value(usage)),
    );
    set(
        "description",
        state
            .description
            .as_ref()
            .map_or(NONE, |description| text_value(description)),
    );
    set(
        "epilog",
        state
            .epilog
            .as_ref()
            .map_or(NONE, |epilog| text_value(epilog)),
    );
    set("formatter_class", state.formatter_class);
    set("prefix_chars", text_value(&state.prefix_chars));
    set(
        "fromfile_prefix_chars",
        state
            .fromfile_prefix_chars
            .as_ref()
            .map_or(NONE, |chars| text_value(chars)),
    );
    set("argument_default", state.argument_default);
    set("conflict_handler", text_value(&state.conflict_handler));
    set("add_help", value::bool_value(state.add_help));
    set("allow_abbrev", value::bool_value(state.allow_abbrev));
    set("exit_on_error", value::bool_value(state.exit_on_error));
    set(
        "suggest_on_error",
        value::bool_value(state.suggest_on_error),
    );
    set("color", value::bool_value(state.color));
    crate::heap::release_roots(roots);
}

/// `ArgumentParser.__repr__`, matching CPython's `_AttributeHolder` format
/// while omitting the internal native payload.
fn parser_repr(instance: i64) -> i64 {
    // CPython's `ArgumentParser._get_kwargs` reprs exactly these fields.
    const FIELDS: [&str; 6] = [
        "prog",
        "usage",
        "description",
        "formatter_class",
        "conflict_handler",
        "add_help",
    ];
    let mut description = String::from("ArgumentParser(");
    for (index, name) in FIELDS.iter().enumerate() {
        if index > 0 {
            description.push_str(", ");
        }
        description.push_str(name);
        description.push('=');
        let item = value::instance_attr(instance, name).unwrap_or(NONE);
        description.push_str(&value::repr(item));
    }
    description.push(')');
    text_value(&description)
}

// -------------------------------------------------------------------------
// Module functions and constants
// -------------------------------------------------------------------------

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "ArgumentParser" => new_parser_instance(args),
        "Namespace" => namespace_call(args, &[]),
        "FileType" => file_type(args),
        // `argparse._` is `gettext.gettext` (the identity translation).
        "_" => args.first().copied().unwrap_or(NONE),
        // CPython's action-name helper (`_get_action_name`).
        "_get_action_name" => args
            .first()
            .map_or(NONE, |&argument| get_action_name(argument)),
        "ArgumentError.__init__" => argument_error_init(args),
        "ArgumentError.__str__" => argument_error_str(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'argparse' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware module call: only `Namespace(**kwargs)` takes keywords.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        "ArgumentParser" => {
            let instance = value::instance_new(argument_parser_class());
            let roots = crate::heap::root_values(std::slice::from_ref(&instance));
            let result = init_parser_instance(instance, args, keywords);
            crate::heap::release_roots(roots);
            if exc::pending() {
                result
            } else {
                instance
            }
        }
        "Namespace" => namespace_call(args, keywords),
        _ => type_error(&format!(
            "argparse.{function}() does not accept keyword arguments"
        )),
    }
}

/// `Namespace(**kwargs)`.
fn namespace_call(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if !args.is_empty() {
        return type_error(&format!(
            "Namespace() takes no positional arguments ({} given)",
            args.len()
        ));
    }
    namespace_instance(keywords)
}

/// Creates the cached `argparse.Namespace` class.
fn namespace_class() -> i64 {
    if let Some(class) = globals::registry_get(NAMESPACE_CLASS_KEY) {
        return class;
    }
    let init = value::native_function_new("argparse", "Namespace.__init__", usize::MAX);
    value::function_set_signature(
        init,
        vec![
            self_param(),
            ParamMeta {
                name: "kwargs".to_string(),
                kind: ParamKind::KwArg,
                default: None,
            },
        ],
    );
    value::function_set_full_entry(init, namespace_init_entry as *const () as usize);
    let mut methods: Vec<(String, i64, MethodKind)> =
        vec![("__init__".to_string(), init, MethodKind::Normal)];
    for (display, params, entry) in [
        (
            "__repr__",
            vec![self_param()],
            namespace_repr_entry as *const () as usize,
        ),
        (
            "__eq__",
            vec![
                self_param(),
                ParamMeta {
                    name: "other".to_string(),
                    kind: ParamKind::Positional,
                    default: None,
                },
            ],
            namespace_eq_entry as *const () as usize,
        ),
        (
            "__ne__",
            vec![
                self_param(),
                ParamMeta {
                    name: "other".to_string(),
                    kind: ParamKind::Positional,
                    default: None,
                },
            ],
            namespace_ne_entry as *const () as usize,
        ),
        (
            "__contains__",
            vec![
                self_param(),
                ParamMeta {
                    name: "key".to_string(),
                    kind: ParamKind::Positional,
                    default: None,
                },
            ],
            namespace_contains_entry as *const () as usize,
        ),
    ] {
        let function =
            value::native_function_new("argparse", namespace_member_name(display), usize::MAX);
        value::function_set_signature(function, params);
        value::function_set_full_entry(function, entry);
        methods.push((display.to_string(), function, MethodKind::Normal));
    }
    if let Some(repr) = methods
        .iter()
        .find(|(name, _, _)| name == "__repr__")
        .map(|(_, function, _)| *function)
    {
        methods.push(("__str__".to_string(), repr, MethodKind::Normal));
    }
    let roots = crate::heap::root_values(
        &methods
            .iter()
            .map(|(_, function, _)| *function)
            .collect::<Vec<_>>(),
    );
    let class = value::class_new_full("Namespace".to_string(), NONE, methods, Vec::new());
    let class_roots = crate::heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(NAMESPACE_CLASS_KEY, class);
    crate::heap::release_roots(class_roots);
    crate::heap::release_roots(roots);
    class
}

/// Builds a `Namespace` instance holding `pairs` as attributes.
fn namespace_instance(pairs: &[(String, i64)]) -> i64 {
    let class = namespace_class();
    let instance = value::instance_new(class);
    let values: Vec<i64> = pairs.iter().map(|(_, item)| *item).collect();
    let roots = crate::heap::root_values(
        &values
            .iter()
            .copied()
            .chain(std::iter::once(instance))
            .collect::<Vec<_>>(),
    );
    for (name, item) in pairs {
        value::instance_setattr(instance, name.clone(), *item);
    }
    crate::heap::release_roots(roots);
    instance
}

extern "C" fn namespace_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the signature is `(self, **kwargs)`.
    let instance = unsafe { *argv };
    if let Some(pairs) = unsafe { value::as_dict(*argv.add(1)) } {
        let names: Vec<(String, i64)> = pairs
            .iter()
            .filter_map(|(key, item)| value::as_str(*key).map(|name| (name.to_string(), *item)))
            .collect();
        for (name, item) in names {
            value::instance_setattr(instance, name, item);
        }
    }
    NONE
}

fn namespace_attributes(instance: i64) -> Vec<(String, i64)> {
    value::as_instance(instance)
        .map(|state| state.attrs.snapshot())
        .unwrap_or_default()
}

fn namespace_is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|character| character.is_alphanumeric() || character == '_')
}

extern "C" fn namespace_repr_entry(argv: *const i64) -> i64 {
    let instance = unsafe { *argv };
    let mut rendered = String::from("Namespace(");
    let mut first = true;
    let mut star_entries: Vec<(String, i64)> = Vec::new();
    for (name, item) in namespace_attributes(instance) {
        if name.starts_with('\u{0}') {
            continue;
        }
        if namespace_is_identifier(&name) {
            if !first {
                rendered.push_str(", ");
            }
            first = false;
            rendered.push_str(&name);
            rendered.push('=');
            rendered.push_str(&value::repr(item));
        } else {
            star_entries.push((name, item));
        }
    }
    if !star_entries.is_empty() {
        if !first {
            rendered.push_str(", ");
        }
        rendered.push_str("**{");
        for (index, (name, item)) in star_entries.into_iter().enumerate() {
            if index > 0 {
                rendered.push_str(", ");
            }
            rendered.push_str(&value::repr(text_value(&name)));
            rendered.push_str(": ");
            rendered.push_str(&value::repr(item));
        }
        rendered.push('}');
    }
    rendered.push(')');
    text_value(&rendered)
}

fn namespace_equals(left: i64, right: i64) -> Option<bool> {
    let right_class = value::instance_class_value(right)?;
    if !value::is_subclass_of(right_class, namespace_class()) {
        return None;
    }
    let left_attrs = namespace_attributes(left);
    let right_attrs = namespace_attributes(right);
    let visible = |pairs: &[(String, i64)]| {
        pairs
            .iter()
            .filter(|(name, _)| !name.starts_with('\u{0}'))
            .count()
    };
    if visible(&left_attrs) != visible(&right_attrs) {
        return Some(false);
    }
    for (name, item) in &left_attrs {
        if name.starts_with('\u{0}') {
            continue;
        }
        let Some((_, other)) = right_attrs.iter().find(|(key, _)| key == name) else {
            return Some(false);
        };
        if !crate::container::equals(*item, *other) {
            return Some(false);
        }
    }
    Some(true)
}

extern "C" fn namespace_eq_entry(argv: *const i64) -> i64 {
    // SAFETY: the signature is `(self, other)`.
    let left = unsafe { *argv };
    let right = unsafe { *argv.add(1) };
    match namespace_equals(left, right) {
        Some(equal) => value::bool_value(equal),
        None => value::NOT_IMPLEMENTED,
    }
}

extern "C" fn namespace_ne_entry(argv: *const i64) -> i64 {
    let left = unsafe { *argv };
    let right = unsafe { *argv.add(1) };
    match namespace_equals(left, right) {
        Some(equal) => value::bool_value(!equal),
        None => value::NOT_IMPLEMENTED,
    }
}

extern "C" fn namespace_contains_entry(argv: *const i64) -> i64 {
    let instance = unsafe { *argv };
    let key = unsafe { *argv.add(1) };
    let Some(name) = value::as_str(key) else {
        return value::FALSE;
    };
    value::bool_value(
        namespace_attributes(instance)
            .iter()
            .any(|(attribute, _)| attribute.as_str() == name),
    )
}

/// `FileType(mode='r', bufsize=-1, encoding=None, errors=None)`.
///
/// The result is a small marker namespace; `parse_args` opens the file with
/// the recorded mode when the argument is converted.
fn file_type(args: &[i64]) -> i64 {
    let mode = match args.first().copied().unwrap_or(NONE) {
        NONE => "r".to_string(),
        value => match value::as_str(value) {
            Some(mode) => mode.to_string(),
            None => return type_error("FileType() mode must be str"),
        },
    };
    if let Some(bufsize) = args.get(1).copied() {
        if bufsize != NONE && value::as_integer(bufsize).is_none() {
            return type_error("FileType() bufsize must be an integer");
        }
    }
    for (slot, name) in [(2usize, "encoding"), (3usize, "errors")] {
        if let Some(argument) = args.get(slot).copied() {
            if argument != NONE && value::as_str(argument).is_none() {
                return type_error(&format!("FileType() {name} must be str or None"));
            }
        }
    }
    let mode_value = text_value(&mode);
    value::namespace_new(vec![(FILE_TYPE.to_string(), mode_value)])
}

/// Resolves an `argparse` constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "SUPPRESS" => text_value(SUPPRESS),
        // `argparse.REMAINDER` is the literal string `"..."`, not a sentinel
        // object.
        "REMAINDER" => text_value("..."),
        "ArgumentParser" => argument_parser_class(),
        "Namespace" => namespace_class(),
        "RawTextHelpFormatter"
        | "RawDescriptionHelpFormatter"
        | "ArgumentDefaultsHelpFormatter"
        | "MetavarTypeHelpFormatter"
        | "HelpFormatter"
        | "Action"
        | "BooleanOptionalAction" => cached_module_class(name),
        // CPython's `PARSER` nargs sentinel.
        "PARSER" => text_value("A..."),
        // `argparse._sys` is the `sys` module value (`mock.patch` rewrites
        // `argparse._sys.exit`).
        "_sys" => super::module_value_by_name("sys"),
        "ArgumentError" => argument_error_class(),
        "ArgumentTypeError" => argument_type_error_class(),
        "__all__" => argparse_all_names(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'argparse' has no attribute '{name}'"),
        ),
    }
}

/// Identity-stable classes for the help formatters and action base names.
/// CPython treats these as singletons (`parser.formatter_class ==
/// argparse.HelpFormatter`), and tests compare them with `is`.
fn cached_module_class(name: &str) -> i64 {
    let key = format!("argparse.{name}.class");
    if let Some(class) = globals::registry_get(&key) {
        return class;
    }
    if !is_help_formatter_name(name) {
        let class = value::class_new(name.to_string(), NONE, Vec::new());
        let roots = crate::heap::root_values(std::slice::from_ref(&class));
        globals::registry_set(&key, class);
        crate::heap::release_roots(roots);
        return class;
    }
    // A formatter is a real class: CPython constructs it with `prog` and reads
    // the layout parameters from the resulting instance. `HelpFormatter` owns
    // the constructor; the specialized formatters derive from it.
    if name == "HelpFormatter" {
        let init = help_formatter_init();
        let roots = crate::heap::root_values(std::slice::from_ref(&init));
        let class = value::class_new_full(
            name.to_string(),
            NONE,
            vec![("__init__".to_string(), init, MethodKind::Normal)],
            Vec::new(),
        );
        let class_roots = crate::heap::root_values(std::slice::from_ref(&class));
        globals::registry_set(&key, class);
        crate::heap::release_roots(class_roots);
        crate::heap::release_roots(roots);
        return class;
    }
    let base = cached_module_class("HelpFormatter");
    let class = value::class_new_full(name.to_string(), base, Vec::new(), Vec::new());
    let roots = crate::heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(&key, class);
    crate::heap::release_roots(roots);
    class
}

/// The public `argparse` help formatters.
fn is_help_formatter_name(name: &str) -> bool {
    matches!(
        name,
        "HelpFormatter"
            | "RawTextHelpFormatter"
            | "RawDescriptionHelpFormatter"
            | "ArgumentDefaultsHelpFormatter"
            | "MetavarTypeHelpFormatter"
    )
}

/// Default formatter width. CPython uses `shutil.get_terminal_size().columns
/// - 2`; the runtime keeps the historical 80-column layout so plain output is
///   byte-identical.
const DEFAULT_FORMATTER_WIDTH: i64 = 80;

/// `HelpFormatter.__init__(prog, indent_increment=2, max_help_position=24,
/// width=None, color=True)`.
///
/// The instance records the layout parameters the way CPython does, so a
/// Python subclass (or a `formatter_class` callable returning a formatter)
/// controls the layout of `format_help`.
fn help_formatter_init() -> i64 {
    let function = value::native_function_new("argparse", "HelpFormatter.__init__", usize::MAX);
    value::function_set_signature(
        function,
        vec![
            self_param(),
            ParamMeta {
                name: "prog".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            ParamMeta {
                name: "indent_increment".to_string(),
                kind: ParamKind::Positional,
                default: Some(tagged_int(2)),
            },
            ParamMeta {
                name: "max_help_position".to_string(),
                kind: ParamKind::Positional,
                default: Some(tagged_int(24)),
            },
            ParamMeta {
                name: "width".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
            ParamMeta {
                name: "color".to_string(),
                kind: ParamKind::Positional,
                default: Some(value::TRUE),
            },
        ],
    );
    value::function_set_full_entry(function, help_formatter_init_entry as *const () as usize);
    function
}

extern "C" fn help_formatter_init_entry(argv: *const i64) -> i64 {
    // SAFETY: signature slots are `(self, prog, indent_increment,
    // max_help_position, width, color)`.
    let instance = unsafe { *argv };
    let indent = unsafe { *argv.add(2) };
    let max_help = unsafe { *argv.add(3) };
    let width = unsafe { *argv.add(4) };
    let Some(indent_increment) = value::as_integer(indent) else {
        return type_error("HelpFormatter() indent_increment must be an integer");
    };
    let Some(max_help_position) = value::as_integer(max_help) else {
        return type_error("HelpFormatter() max_help_position must be an integer");
    };
    let width = if width == NONE {
        DEFAULT_FORMATTER_WIDTH
    } else {
        let Some(width) = value::as_integer(width) else {
            return type_error("HelpFormatter() width must be an integer or None");
        };
        width
    };
    let indent_increment = indent_increment.max(0);
    // CPython clamps `max_help_position` against the width and indent.
    let max_help_position = max_help_position.min(
        width
            .saturating_sub(20)
            .max(indent_increment.saturating_mul(2)),
    );
    let roots = crate::heap::root_values(std::slice::from_ref(&instance));
    value::instance_setattr(
        instance,
        "_indent_increment".to_string(),
        tagged_int(indent_increment),
    );
    value::instance_setattr(
        instance,
        "_max_help_position".to_string(),
        tagged_int(max_help_position),
    );
    value::instance_setattr(instance, "_width".to_string(), tagged_int(width));
    crate::heap::release_roots(roots);
    NONE
}

// -------------------------------------------------------------------------
// Module classes and private helpers
// -------------------------------------------------------------------------

/// `argparse.ArgumentError`: an `Exception` subclass carrying
/// `argument_name` and `message`.
pub(crate) fn argument_error_class() -> i64 {
    const KEY: &str = "argparse.ArgumentError";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    let init = value::native_function_new("argparse", "ArgumentError.__init__", 3);
    let str_fn = value::native_function_new("argparse", "ArgumentError.__str__", 1);
    let class = value::class_new_full(
        "ArgumentError".to_string(),
        exc::exception_class(exc::EXCEPTION),
        vec![
            ("__init__".to_string(), init, hydro_ir::MethodKind::Normal),
            ("__str__".to_string(), str_fn, hydro_ir::MethodKind::Normal),
        ],
        vec![
            ("argument_name".to_string(), NONE),
            ("message".to_string(), text_value("")),
        ],
    );
    let roots = crate::heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(KEY, class);
    crate::heap::release_roots(roots);
    class
}

/// `argparse.ArgumentTypeError`: the `Exception` a `type=` callable raises.
pub(crate) fn argument_type_error_class() -> i64 {
    const KEY: &str = "argparse.ArgumentTypeError";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    let class = value::class_new_full(
        "ArgumentTypeError".to_string(),
        exc::exception_class(exc::EXCEPTION),
        Vec::new(),
        Vec::new(),
    );
    let roots = crate::heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(KEY, class);
    crate::heap::release_roots(roots);
    class
}

/// `argparse.__all__`, in CPython's order.
fn argparse_all_names() -> i64 {
    const KEY: &str = "argparse.__all__";
    if let Some(list) = globals::registry_get(KEY) {
        return list;
    }
    let names = [
        "ArgumentParser",
        "ArgumentError",
        "ArgumentTypeError",
        "FileType",
        "HelpFormatter",
        "ArgumentDefaultsHelpFormatter",
        "RawDescriptionHelpFormatter",
        "RawTextHelpFormatter",
        "MetavarTypeHelpFormatter",
        "Namespace",
        "Action",
        "BooleanOptionalAction",
        "SUPPRESS",
    ];
    let values = names.iter().map(|name| text_value(name)).collect();
    let list = value::list_new(values);
    globals::registry_set(KEY, list);
    list
}

/// Reads an attribute, clearing an `AttributeError` so the caller can fall
/// through to the next candidate.
fn try_action_attr(argument: i64, name: &str) -> Option<i64> {
    let item = crate::core::methods::getattr(argument, name.as_ptr(), name.len());
    if exc::pending() {
        if exc::pending_tag() == i64::from(ATTRIBUTE_ERROR) {
            exc::clear();
        }
        return None;
    }
    Some(item)
}

/// `argparse._get_action_name(argument)`: the option strings joined with `/`,
/// else the metavar, else the dest.
pub(crate) fn get_action_name(argument: i64) -> i64 {
    if argument == NONE {
        return NONE;
    }
    if let Some(options) = try_action_attr(argument, "option_strings") {
        if let Some(items) = value::as_list(options) {
            let names: Vec<&str> = items
                .iter()
                .filter_map(|&item| value::as_str(item))
                .collect();
            if !names.is_empty() {
                return text_value(&names.join("/"));
            }
        }
    }
    if let Some(metavar) = try_action_attr(argument, "metavar") {
        if let Some(name) = value::as_str(metavar) {
            if name != SUPPRESS {
                return text_value(name);
            }
        }
    }
    if let Some(dest) = try_action_attr(argument, "dest") {
        if let Some(name) = value::as_str(dest) {
            if name != SUPPRESS {
                return text_value(name);
            }
        }
    }
    NONE
}

/// `ArgumentError(argument, message)`.
fn argument_error_init(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("ArgumentError.__init__() missing self");
    };
    let argument = args.get(1).copied().unwrap_or(NONE);
    let message = args.get(2).copied().unwrap_or_else(|| text_value(""));
    let name = get_action_name(argument);
    let roots = crate::heap::root_values(&[instance, name, message]);
    value::instance_setattr(instance, "argument_name".to_string(), name);
    value::instance_setattr(instance, "message".to_string(), message);
    crate::heap::release_roots(roots);
    NONE
}

/// `ArgumentError.__str__`: the stored message.
fn argument_error_str(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return text_value("");
    };
    match try_action_attr(instance, "message") {
        Some(message) if value::as_str(message).is_some() => message,
        _ => text_value(""),
    }
}

// -------------------------------------------------------------------------
// Constructor
// -------------------------------------------------------------------------

fn opt_str(argument: i64, message: &str) -> Result<Option<String>, i64> {
    if argument == NONE {
        return Ok(None);
    }
    match value::as_str(argument) {
        Some(text) => Ok(Some(text.to_string())),
        None => Err(type_error(message)),
    }
}

fn opt_bool(argument: i64, default: bool) -> bool {
    if argument == NONE {
        default
    } else {
        value::truthy(argument)
    }
}

/// `ArgumentParser(prog, usage, description, epilog, parents,
/// formatter_class, prefix_chars, fromfile_prefix_chars, argument_default,
/// conflict_handler, add_help, allow_abbrev, exit_on_error, color)`.
///
/// Every parameter is optional. In particular `usage` defaults to `None`, so
/// both `ArgumentParser()` and `ArgumentParser(usage="...")` are valid and an
/// omitted `usage` keeps the auto-generated usage line.
fn new_parser(args: &[i64]) -> i64 {
    // Internal construction already supplies the full 15-slot layout, so it
    // bypasses `__init__`'s Python-level positional limit.
    let instance = value::instance_new(argument_parser_class());
    let roots = crate::heap::root_values(std::slice::from_ref(&instance));
    let state = match build_parser_state(args) {
        Ok(state) => state,
        Err(error) => {
            crate::heap::release_roots(roots);
            return error;
        }
    };
    let result = install_parser_state(instance, state);
    crate::heap::release_roots(roots);
    if result == NONE && exc::pending() {
        return result;
    }
    instance
}

/// Builds the native definition from the 15 constructor parameter slots.
fn build_parser_state(args: &[i64]) -> Result<ArgParserState, i64> {
    let arg = |index: usize| args.get(index).copied().unwrap_or(NONE);
    let prog = opt_str(arg(0), "prog must be str or None")?;
    let usage = opt_str(arg(1), "usage must be str or None")?;
    let description = opt_str(arg(2), "description must be str or None")?;
    let epilog = opt_str(arg(3), "epilog must be str or None")?;
    let mut prefix_chars = match opt_str(arg(6), "prefix_chars must be str or None") {
        Ok(Some(text)) => text,
        Ok(None) => "-".to_string(),
        Err(err) => return Err(err),
    };
    if prefix_chars.is_empty() {
        return Err(value_error("prefix_chars must not be empty"));
    }
    let fromfile_prefix_chars = opt_str(arg(7), "fromfile_prefix_chars must be str or None")?;
    let conflict_handler = match opt_str(arg(9), "conflict_handler must be str or None") {
        Ok(Some(handler)) => handler,
        Ok(None) => "error".to_string(),
        Err(err) => return Err(err),
    };
    if conflict_handler != "error" && conflict_handler != "resolve" {
        return Err(value_error(&format!(
            "unknown conflict_handler '{conflict_handler}'"
        )));
    }

    // The help action's SUPPRESS default is the only tagged value created
    // here; keep it alive until the parser object owns it.
    let roots = value::list_new(Vec::new());
    let suppress = text_value(SUPPRESS);
    root_value(roots, suppress);

    let mut state = ArgParserState::new(prog.clone(), description);
    state.usage = usage;
    state.epilog = epilog;
    state.formatter_class = if arg(5) == NONE {
        cached_module_class("HelpFormatter")
    } else {
        arg(5)
    };
    state.prefix_chars = std::mem::take(&mut prefix_chars);
    state.fromfile_prefix_chars = fromfile_prefix_chars;
    state.argument_default = arg(8);
    state.conflict_handler = conflict_handler;
    state.add_help = opt_bool(arg(10), true);
    state.allow_abbrev = opt_bool(arg(11), true);
    state.exit_on_error = opt_bool(arg(12), true);
    state.suggest_on_error = opt_bool(arg(13), false);
    state.color = opt_bool(arg(14), true);
    if state.prog.is_none() {
        state.prog = Some(default_prog());
    }

    if state.add_help {
        add_help_action(&mut state, suppress);
    }

    // `parents`: inherit every non-help action and the parser defaults.
    let parents = arg(4);
    if parents != NONE {
        let entries: Vec<i64> = if let Some(items) = value::as_list(parents) {
            items.to_vec()
        } else if let Some(items) = value::as_tuple(parents) {
            items.to_vec()
        } else {
            return Err(type_error(
                "parents must be a list or tuple of ArgumentParser objects",
            ));
        };
        for parent in entries {
            let Some(parent_state) = parser_ref(parent) else {
                return Err(type_error("parents entries must be ArgumentParser objects"));
            };
            for spec in &parent_state.options {
                if spec.action == ActionKind::Help {
                    continue;
                }
                if state
                    .options
                    .iter()
                    .any(|existing| existing.flags.iter().any(|flag| spec.flags.contains(flag)))
                {
                    continue;
                }
                let mut spec = spec.clone();
                spec.group = 0;
                state.options.push(spec);
            }
            for (dest, default) in &parent_state.defaults {
                upsert(&mut state.defaults, dest, *default);
            }
        }
    }

    Ok(state)
}

/// The default `prog`: the basename of `sys.argv[0]`, like CPython.
fn default_prog() -> String {
    let argv = crate::argv();
    let Some(first) = argv.first() else {
        return "program".to_string();
    };
    let name = std::path::Path::new(first)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| first.clone());
    if name.is_empty() {
        "program".to_string()
    } else {
        name
    }
}

fn add_help_action(state: &mut ArgParserState, default: i64) {
    let prefix = state.prefix_chars.chars().next().unwrap_or('-');
    let short = format!("{prefix}h");
    let long = format!("{prefix}{prefix}help");
    state.options.push(ArgSpec {
        flags: vec![short, long],
        negative_flags: Vec::new(),
        dest: "help".to_string(),
        positional: false,
        action: ActionKind::Help,
        type_fn: NONE,
        default,
        const_value: NONE,
        required: false,
        help: "show this help message and exit".to_string(),
        nargs: Nargs::Single,
        choices: None,
        metavar: None,
        group: 0,
        version: String::new(),
    });
}

// -------------------------------------------------------------------------
// Method dispatch
// -------------------------------------------------------------------------

/// Whether `obj` is a group or subparsers handle returned by this module.
pub(crate) fn is_handle(obj: i64) -> bool {
    value::as_namespace(obj)
        .is_some_and(|pairs| pairs.iter().any(|(name, _)| name.as_str() == HANDLE))
}

fn handle_attr(obj: i64, name: &str) -> Option<i64> {
    value::as_namespace(obj)?
        .iter()
        .find(|(key, _)| key.as_str() == name)
        .map(|(_, value)| *value)
}

fn handle_parts(obj: i64) -> Option<(i64, usize, String)> {
    if !is_handle(obj) {
        return None;
    }
    let parser = handle_attr(obj, HANDLE_PARSER)?;
    let group = handle_attr(obj, HANDLE_GROUP)
        .and_then(value::as_integer)
        .unwrap_or(0) as usize;
    let kind = handle_attr(obj, HANDLE)
        .and_then(value::as_str)?
        .to_string();
    Some((parser, group, kind))
}

/// Method dispatch without keywords.
pub(crate) fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    if parser_ref(obj).is_none() {
        return attribute_error(name);
    }
    match name {
        "add_argument" => add_argument(obj, args, &[], 0),
        "add_argument_group" => add_argument_group(obj, args, &[], 0, false, false),
        "add_mutually_exclusive_group" => add_argument_group(obj, args, &[], 0, true, false),
        "add_subparsers" => add_subparsers(obj, args, &[]),
        "set_defaults" => set_defaults(obj, &[]),
        "get_default" => get_default(obj, args),
        "parse_args" => parse(obj, args, &[], false),
        "parse_known_args" => parse(obj, args, &[], true),
        // The intermixed spellings share the same scanner: this parser already
        // accepts optionals and positionals in any order.
        "parse_intermixed_args" => parse(obj, args, &[], false),
        "parse_known_intermixed_args" => parse(obj, args, &[], true),
        "print_help" => print_help_method(obj, args),
        "print_usage" => print_usage_method(obj, args),
        "format_help" => text_value(&format_help(obj)),
        "format_usage" => text_value(&format_usage(obj)),
        "error" => error_method(obj, args),
        "exit" => exit_method(obj, args, &[]),
        "_hydro_test_mode" => set_test_mode(obj),
        _ => attribute_error(name),
    }
}

/// Method dispatch with keywords.
pub(crate) fn method_kw(obj: i64, name: &str, args: &mut [i64], keywords: &KeywordArgs) -> i64 {
    if parser_ref(obj).is_none() {
        return attribute_error(name);
    }
    match name {
        "add_argument" => add_argument(obj, args, &keywords.items, 0),
        "add_argument_group" => add_argument_group(obj, args, &keywords.items, 0, false, false),
        "add_mutually_exclusive_group" => {
            add_argument_group(obj, args, &keywords.items, 0, true, false)
        }
        "add_subparsers" => add_subparsers(obj, args, &keywords.items),
        "set_defaults" => set_defaults(obj, &keywords.items),
        "get_default" => {
            if let Some((keyword, _)) = keywords.items.first() {
                return type_error(&format!(
                    "ArgumentParser.get_default() got an unexpected keyword argument '{keyword}'"
                ));
            }
            get_default(obj, args)
        }
        "parse_args" => parse(obj, args, &keywords.items, false),
        "parse_known_args" => parse(obj, args, &keywords.items, true),
        "parse_intermixed_args" => parse(obj, args, &keywords.items, false),
        "parse_known_intermixed_args" => parse(obj, args, &keywords.items, true),
        "print_help" => print_help_method(obj, args),
        "print_usage" => print_usage_method(obj, args),
        "format_help" | "format_usage" => {
            if let Some((keyword, _)) = keywords.items.first() {
                return type_error(&format!(
                    "ArgumentParser.{name}() got an unexpected keyword argument '{keyword}'"
                ));
            }
            method(obj, name, args)
        }
        "error" => error_method(obj, args),
        "exit" => exit_method(obj, args, &keywords.items),
        "_hydro_test_mode" => set_test_mode(obj),
        _ => {
            if let Some((keyword, _)) = keywords.items.first() {
                return type_error(&format!(
                    "ArgumentParser.{name}() got an unexpected keyword argument '{keyword}'"
                ));
            }
            method(obj, name, args)
        }
    }
}

/// Method dispatch for a group or subparsers handle.
pub(crate) fn handle_method(obj: i64, name: &str, args: &[i64]) -> i64 {
    let Some((parser, group, kind)) = handle_parts(obj) else {
        return attribute_error(name);
    };
    if kind == "subparsers" {
        return match name {
            "add_parser" => add_parser(parser, args, &[]),
            _ => attribute_error(name),
        };
    }
    match name {
        "add_argument" => add_argument(parser, args, &[], group),
        "add_mutually_exclusive_group" => add_argument_group(parser, args, &[], group, true, false),
        "set_defaults" => set_defaults(parser, &[]),
        "add_argument_group" => add_argument_group(parser, args, &[], group, false, false),
        _ => attribute_error(name),
    }
}

/// Keyword variant of [`handle_method`].
pub(crate) fn handle_method_kw(
    obj: i64,
    name: &str,
    args: &mut [i64],
    keywords: &KeywordArgs,
) -> i64 {
    let Some((parser, group, kind)) = handle_parts(obj) else {
        return attribute_error(name);
    };
    if kind == "subparsers" {
        return match name {
            "add_parser" => add_parser(parser, args, &keywords.items),
            _ => attribute_error(name),
        };
    }
    match name {
        "add_argument" => add_argument(parser, args, &keywords.items, group),
        "add_mutually_exclusive_group" => {
            add_argument_group(parser, args, &keywords.items, group, true, false)
        }
        "set_defaults" => set_defaults(parser, &keywords.items),
        "add_argument_group" => {
            add_argument_group(parser, args, &keywords.items, group, false, false)
        }
        _ => attribute_error(name),
    }
}

fn set_test_mode(parser: i64) -> i64 {
    if let Some(mut state) = parser_mut(parser) {
        state.test_mode = true;
    }
    NONE
}

// -------------------------------------------------------------------------
// add_argument and related definitions
// -------------------------------------------------------------------------

/// Normalises flags to a dest name: long option first, leading dashes
/// stripped, `-` replaced by `_`.
fn dest_for(flags: &[String], prefixes: &str) -> String {
    let source = flags
        .iter()
        .find(|flag| is_long_flag(flag, prefixes))
        .or_else(|| flags.first())
        .map(String::as_str)
        .unwrap_or("");
    source
        .trim_start_matches(|c: char| prefixes.contains(c))
        .replace('-', "_")
}

fn is_long_flag(flag: &str, prefixes: &str) -> bool {
    let mut chars = flag.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    prefixes.contains(first) && chars.next() == Some(first)
}

fn starts_with_prefix(text: &str, prefixes: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|first| prefixes.contains(first))
}

fn action_from_value(value: i64) -> Result<Option<ActionKind>, i64> {
    if value == NONE {
        return Ok(None);
    }
    let name = if let Some(text) = value::as_str(value) {
        text.to_string()
    } else if let Some(class) = value::class_display_name(value) {
        class.to_string()
    } else {
        return Err(type_error(
            "add_argument() action must be str or an Action class",
        ));
    };
    let action = match name.as_str() {
        "store" => ActionKind::Store,
        "store_true" => ActionKind::StoreTrue,
        "store_false" => ActionKind::StoreFalse,
        "store_const" => ActionKind::StoreConst,
        "append" => ActionKind::Append,
        "append_const" => ActionKind::AppendConst,
        "count" => ActionKind::Count,
        "help" => ActionKind::Help,
        "version" => ActionKind::Version,
        "BooleanOptionalAction" | "boolean_optional" => ActionKind::BooleanOptional,
        _ => return Err(value_error(&format!("unsupported action '{name}'"))),
    };
    Ok(Some(action))
}

fn nargs_from_value(value: i64) -> Result<Nargs, i64> {
    if value == NONE {
        return Ok(Nargs::Single);
    }
    if let Some(count) = value::as_integer(value) {
        if count < 0 {
            return Err(value_error("nargs must not be negative"));
        }
        return Ok(Nargs::Exact(count as usize));
    }
    let Some(text) = value::as_str(value) else {
        return Err(type_error("add_argument() nargs must be str, int or None"));
    };
    match text {
        "?" => Ok(Nargs::Optional),
        "*" => Ok(Nargs::ZeroOrMore),
        "+" => Ok(Nargs::OneOrMore),
        other => match other.parse::<usize>() {
            Ok(count) => Ok(Nargs::Exact(count)),
            Err(_) => Err(value_error(&format!("invalid nargs value '{other}'"))),
        },
    }
}

/// Whether `value` is the marker namespace returned by `FileType(...)`.
fn is_file_type_marker(value: i64) -> bool {
    value::as_namespace(value)
        .is_some_and(|pairs| pairs.iter().any(|(name, _)| name.as_str() == FILE_TYPE))
}

fn choices_from_value(value: i64) -> Result<Option<Vec<i64>>, i64> {
    if value == NONE {
        return Ok(None);
    }
    if let Some(items) = value::as_list(value) {
        return Ok(Some(items.to_vec()));
    }
    if let Some(items) = value::as_tuple(value) {
        return Ok(Some(items.to_vec()));
    }
    if let Some(items) = value::as_dict(value) {
        return Ok(Some(items.iter().map(|(key, _)| *key).collect()));
    }
    Err(type_error(
        "add_argument() choices must be a list, tuple or dict",
    ))
}

fn add_argument(parser: i64, args: &[i64], keywords: &[(String, i64)], group: usize) -> i64 {
    let Some(state) = parser_ref(parser) else {
        return attribute_error("add_argument");
    };
    let prefixes = state.prefix_chars.clone();
    let argument_default = state.argument_default;
    let conflict_handler = state.conflict_handler.clone();

    // Split positional names from optional flags.
    let mut names: Vec<String> = Vec::with_capacity(args.len());
    for &arg in args {
        let Some(name) = value::as_str(arg) else {
            return type_error(&format!(
                "add_argument() argument names must be str, not {}",
                value::type_name(arg)
            ));
        };
        if name.is_empty() {
            return value_error("argument names must not be empty");
        }
        names.push(name.to_string());
    }
    if names.is_empty() {
        return type_error("add_argument() requires at least one argument name");
    }

    let mut seen_keywords = std::collections::HashSet::new();
    let mut type_fn = NONE;
    let mut default: Option<i64> = None;
    let mut required: Option<bool> = None;
    let mut action_value: Option<ActionKind> = None;
    let mut help_text: Option<String> = None;
    let mut dest: Option<String> = None;
    let mut nargs: Option<Nargs> = None;
    let mut choices: Option<Vec<i64>> = None;
    let mut metavar: Option<String> = None;
    let mut const_value: Option<i64> = None;
    let mut version: Option<String> = None;
    for (name, argument) in keywords {
        if !seen_keywords.insert(name.as_str()) {
            return type_error(&format!(
                "add_argument() got multiple values for argument '{name}'"
            ));
        }
        match name.as_str() {
            "type" => {
                if *argument != NONE
                    && value::as_function(*argument).is_none()
                    && !is_file_type_marker(*argument)
                {
                    return type_error("add_argument() type must be callable or None");
                }
                type_fn = *argument;
            }
            "default" => default = Some(*argument),
            "required" => required = Some(value::truthy(*argument)),
            "action" => match action_from_value(*argument) {
                Ok(action) => action_value = action,
                Err(err) => return err,
            },
            "help" => match value::as_str(*argument) {
                Some(text) => help_text = Some(text.to_string()),
                None => return type_error("add_argument() help must be str"),
            },
            "dest" => match value::as_str(*argument) {
                Some(text) => dest = Some(text.to_string()),
                None => return type_error("add_argument() dest must be str"),
            },
            "nargs" => match nargs_from_value(*argument) {
                Ok(value) => nargs = Some(value),
                Err(err) => return err,
            },
            "choices" => match choices_from_value(*argument) {
                Ok(value) => choices = value,
                Err(err) => return err,
            },
            "metavar" => match value::as_str(*argument) {
                Some(text) => metavar = Some(text.to_string()),
                None => return type_error("add_argument() metavar must be str"),
            },
            "const" => const_value = Some(*argument),
            "version" => match value::as_str(*argument) {
                Some(text) => version = Some(text.to_string()),
                None => return type_error("add_argument() version must be str"),
            },
            _ => {
                return type_error(&format!(
                    "add_argument() got an unexpected keyword argument '{name}'"
                ));
            }
        }
    }

    let positional = !names.iter().any(|name| starts_with_prefix(name, &prefixes));
    if !positional
        && names
            .iter()
            .any(|name| !starts_with_prefix(name, &prefixes))
    {
        return type_error("add_argument() cannot mix positional and optional names");
    }
    if positional && names.len() > 1 {
        return type_error("add_argument() accepts one positional name");
    }
    if positional && required == Some(true) {
        return type_error("required is an invalid keyword argument for positional arguments");
    }

    let action = action_value.unwrap_or(ActionKind::Store);
    if positional && !matches!(action, ActionKind::Store | ActionKind::Append) {
        return type_error("only store/append actions are supported for positional arguments");
    }
    if action == ActionKind::Version && version.is_none() {
        version = Some("%(prog)s".to_string());
    }

    let derived_dest = match dest {
        Some(dest) => dest,
        None if positional => names[0].clone(),
        None => dest_for(&names, &prefixes),
    };
    if derived_dest.is_empty() {
        return value_error("could not derive a destination name");
    }

    let mut flags = names;
    let mut negative_flags: Vec<String> = Vec::new();
    if action == ActionKind::BooleanOptional && !positional {
        let prefix = flags
            .iter()
            .find(|flag| is_long_flag(flag.as_str(), &prefixes))
            .and_then(|flag| flag.chars().next())
            .unwrap_or('-');
        let mut positives = Vec::new();
        for flag in flags.drain(..) {
            if is_long_flag(&flag, &prefixes) {
                let body: String = flag.chars().skip_while(|c| *c == prefix).collect();
                negative_flags.push(format!("{prefix}{prefix}no-{body}"));
            }
            positives.push(flag);
        }
        flags = positives;
    }

    let nargs = nargs.unwrap_or(Nargs::Single);
    let default = match default {
        Some(default) => default,
        None => match action {
            ActionKind::StoreFalse => value::TRUE,
            ActionKind::StoreTrue => value::FALSE,
            ActionKind::Help | ActionKind::Version => text_value(SUPPRESS),
            _ => argument_default,
        },
    };
    let const_value = const_value.unwrap_or(NONE);
    let required = required.unwrap_or(false);

    let spec = ArgSpec {
        flags,
        negative_flags,
        dest: derived_dest,
        positional,
        action,
        type_fn,
        default,
        const_value,
        required,
        help: help_text.unwrap_or_default(),
        nargs,
        choices: choices.clone(),
        metavar,
        group,
        version: version.unwrap_or_default(),
    };

    // Duplicate handling and insertion. Errors are raised after the guard is
    // dropped: raising allocates, and the guard holds the collector lock.
    let mut conflict: Option<String> = None;
    let mut replaced = false;
    {
        let Some(mut parser_state) = parser_mut(parser) else {
            return attribute_error("add_argument");
        };
        if let Some(position) = parser_state.options.iter().position(|existing| {
            existing
                .flags
                .iter()
                .chain(existing.negative_flags.iter())
                .any(|flag| spec.flags.contains(flag) || spec.negative_flags.contains(flag))
        }) {
            if conflict_handler == "resolve" {
                let old_group = parser_state.options[position].group;
                parser_state.options[position] = spec.clone();
                replaced = true;
                if old_group < parser_state.groups.len() {
                    parser_state.groups[old_group]
                        .members
                        .retain(|index| *index != position);
                }
                if group < parser_state.groups.len() {
                    parser_state.groups[group].members.push(position);
                }
            } else {
                let flag = spec
                    .flags
                    .iter()
                    .chain(spec.negative_flags.iter())
                    .find(|flag| {
                        parser_state.options.iter().any(|existing| {
                            existing.flags.contains(*flag)
                                || existing.negative_flags.contains(*flag)
                        })
                    })
                    .map(|flag| (*flag).clone())
                    .unwrap_or_default();
                conflict = Some(flag);
            }
        }
        if conflict.is_none() && !replaced {
            let index = parser_state.options.len();
            if group < parser_state.groups.len() {
                parser_state.groups[group].members.push(index);
            }
            parser_state.options.push(spec.clone());
        }
    }
    if let Some(flag) = conflict {
        return type_error(&format!(
            "argument {flag}: conflicting option string: {flag}"
        ));
    }

    if spec.type_fn != NONE {
        crate::heap::write_barrier_edge(parser_state_value(parser).unwrap_or(parser), spec.type_fn);
    }
    if spec.default != NONE {
        crate::heap::write_barrier_edge(parser_state_value(parser).unwrap_or(parser), spec.default);
    }
    if spec.const_value != NONE {
        crate::heap::write_barrier_edge(
            parser_state_value(parser).unwrap_or(parser),
            spec.const_value,
        );
    }
    if let Some(items) = choices {
        for item in items {
            crate::heap::write_barrier_edge(parser_state_value(parser).unwrap_or(parser), item);
        }
    }
    NONE
}

/// `add_argument_group(title=None, description=None)` and
/// `add_mutually_exclusive_group(required=False)`.
fn add_argument_group(
    parser: i64,
    args: &[i64],
    keywords: &[(String, i64)],
    parent: usize,
    mutex: bool,
    _unused_required: bool,
) -> i64 {
    let mut title = None;
    let mut description = None;
    let mut required = false;
    if mutex {
        // `required` may be positional in CPython (deprecated) or a keyword.
        if let Some(argument) = args.first().copied() {
            required = value::truthy(argument);
        }
    } else {
        if let Some(argument) = args.first().copied() {
            match value::as_str(argument) {
                Some(text) => title = Some(text.to_string()),
                None => return type_error("add_argument_group() title must be str"),
            }
        }
        if let Some(argument) = args.get(1).copied() {
            match value::as_str(argument) {
                Some(text) => description = Some(text.to_string()),
                None => return type_error("add_argument_group() description must be str"),
            }
        }
    }
    for (name, argument) in keywords {
        match name.as_str() {
            "title" if !mutex => match value::as_str(*argument) {
                Some(text) => title = Some(text.to_string()),
                None => return type_error("add_argument_group() title must be str"),
            },
            "description" if !mutex => match value::as_str(*argument) {
                Some(text) => description = Some(text.to_string()),
                None => return type_error("add_argument_group() description must be str"),
            },
            "required" if mutex => required = value::truthy(*argument),
            _ => {
                return type_error(&format!(
                    "add_argument_group() got an unexpected keyword argument '{name}'"
                ));
            }
        }
    }
    if !mutex {
        required = false;
    }
    let group = GroupState {
        title,
        description,
        mutex,
        parent,
        required,
        members: Vec::new(),
    };
    let index = {
        let Some(mut state) = parser_mut(parser) else {
            return attribute_error("add_argument_group");
        };
        let index = state.groups.len();
        state.groups.push(group);
        index
    };
    handle_namespace(parser, index, "group")
}

fn handle_namespace(parser: i64, group: usize, kind: &str) -> i64 {
    let marker = text_value(kind);
    value::namespace_new(vec![
        (HANDLE.to_string(), marker),
        (HANDLE_PARSER.to_string(), parser),
        (HANDLE_GROUP.to_string(), tagged_int(group as i64)),
    ])
}

/// `add_subparsers(dest=None, required=False, title=None, description=None,
/// metavar=None)`.
fn add_subparsers(parser: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut dest = None;
    let mut required = false;
    let mut title = None;
    let mut description = None;
    let mut metavar = None;
    let mut positional_index = 0usize;
    for (name, argument) in keywords {
        match name.as_str() {
            "dest" => match value::as_str(*argument) {
                Some(text) => dest = Some(text.to_string()),
                None => return type_error("add_subparsers() dest must be str"),
            },
            "required" => required = value::truthy(*argument),
            "title" => match value::as_str(*argument) {
                Some(text) => title = Some(text.to_string()),
                None => return type_error("add_subparsers() title must be str"),
            },
            "description" => match value::as_str(*argument) {
                Some(text) => description = Some(text.to_string()),
                None => return type_error("add_subparsers() description must be str"),
            },
            "metavar" => match value::as_str(*argument) {
                Some(text) => metavar = Some(text.to_string()),
                None => return type_error("add_subparsers() metavar must be str"),
            },
            "help" | "prog" => {}
            _ => {
                return type_error(&format!(
                    "add_subparsers() got an unexpected keyword argument '{name}'"
                ));
            }
        }
    }
    while let Some(argument) = args.get(positional_index).copied() {
        positional_index += 1;
        match positional_index {
            1 => match value::as_str(argument) {
                Some(text) => dest = Some(text.to_string()),
                None => return type_error("add_subparsers() dest must be str"),
            },
            _ => return type_error("add_subparsers() takes at most one positional argument"),
        }
    }
    if parser_ref(parser).is_none() {
        return attribute_error("add_subparsers");
    }
    if parser_ref(parser).is_some_and(|state| state.subparsers.is_some()) {
        return type_error("cannot have multiple subparser arguments");
    }
    let Some(mut state) = parser_mut(parser) else {
        return attribute_error("add_subparsers");
    };
    state.subparsers = Some(SubparsersState {
        dest,
        required,
        title,
        description,
        metavar,
        parsers: Vec::new(),
    });
    drop(state);
    handle_namespace(parser, 0, "subparsers")
}

/// `subparsers.add_parser(name, **kwargs)`.
fn add_parser(parser: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(name_value) = args.first().copied() else {
        return type_error("add_parser() requires a name");
    };
    let Some(name) = value::as_str(name_value).map(str::to_string) else {
        return type_error("add_parser() name must be str");
    };
    let mut slots = vec![NONE; 15];
    let mut help_text = String::new();
    let mut explicit_prog = false;
    for (keyword, argument) in keywords {
        match keyword.as_str() {
            "prog" => {
                slots[0] = *argument;
                explicit_prog = true;
            }
            "usage" => slots[1] = *argument,
            "description" => slots[2] = *argument,
            "epilog" => slots[3] = *argument,
            "parents" => slots[4] = *argument,
            "formatter_class" => slots[5] = *argument,
            "prefix_chars" => slots[6] = *argument,
            "fromfile_prefix_chars" => slots[7] = *argument,
            "argument_default" => slots[8] = *argument,
            "conflict_handler" => slots[9] = *argument,
            "add_help" => slots[10] = *argument,
            "allow_abbrev" => slots[11] = *argument,
            "exit_on_error" => slots[12] = *argument,
            "suggest_on_error" => slots[13] = *argument,
            "color" => slots[14] = *argument,
            "help" => match value::as_str(*argument) {
                Some(text) => help_text = text.to_string(),
                None => return type_error("add_parser() help must be str"),
            },
            _ => {
                return type_error(&format!(
                    "add_parser() got an unexpected keyword argument '{keyword}'"
                ));
            }
        }
    }
    let Some(parent) = parser_ref(parser) else {
        return attribute_error("add_parser");
    };
    let parent_prog = parent.prog.clone().unwrap_or_else(default_prog);
    let parent_test_mode = parent.test_mode;
    let roots = value::list_new(Vec::new());
    if !explicit_prog {
        let prog = text_value(&format!("{parent_prog} {name}"));
        root_value(roots, prog);
        slots[0] = prog;
    }
    let child = new_parser(&slots);
    if exc::pending() {
        return NONE;
    }
    if parent_test_mode {
        set_test_mode(child);
    }
    let mut conflict = false;
    {
        let Some(mut state) = parser_mut(parser) else {
            return attribute_error("add_parser");
        };
        match state.subparsers.as_mut() {
            Some(subparsers) => {
                if subparsers
                    .parsers
                    .iter()
                    .any(|(existing, _, _)| existing == &name)
                {
                    conflict = true;
                } else {
                    subparsers.parsers.push((name.clone(), child, help_text));
                }
            }
            None => conflict = true,
        }
    }
    if conflict {
        return value_error(&format!("conflicting subparser: {name}"));
    }
    crate::heap::write_barrier_edge(parser_state_value(parser).unwrap_or(parser), child);
    child
}

fn upsert(pairs: &mut Vec<(String, i64)>, name: &str, item: i64) {
    if let Some(entry) = pairs.iter_mut().find(|(key, _)| key == name) {
        entry.1 = item;
    } else {
        pairs.push((name.to_string(), item));
    }
}

/// `set_defaults(**kwargs)`.
fn set_defaults(parser: i64, keywords: &[(String, i64)]) -> i64 {
    let mut updates = Vec::with_capacity(keywords.len());
    for (name, item) in keywords {
        updates.push((name.clone(), *item));
    }
    if let Some(mut state) = parser_mut(parser) {
        for (name, item) in &updates {
            upsert(&mut state.defaults, name, *item);
        }
    }
    for (_, item) in updates {
        if item != NONE {
            crate::heap::write_barrier_edge(parser_state_value(parser).unwrap_or(parser), item);
        }
    }
    NONE
}

/// `get_default(dest)`.
fn get_default(parser: i64, args: &[i64]) -> i64 {
    let Some(argument) = args.first().copied() else {
        return type_error("get_default() requires a dest");
    };
    let Some(dest) = value::as_str(argument) else {
        return type_error("get_default() dest must be str");
    };
    let Some(state) = parser_ref(parser) else {
        return attribute_error("get_default");
    };
    if let Some((_, item)) = state
        .defaults
        .iter()
        .find(|(name, _)| name.as_str() == dest)
    {
        return *item;
    }
    if let Some(spec) = state.options.iter().find(|spec| spec.dest == dest) {
        return spec.default;
    }
    NONE
}

// -------------------------------------------------------------------------
// Parsing
// -------------------------------------------------------------------------

/// Snapshot of the parser definition used by the scanner and formatter.
#[derive(Clone)]
struct View {
    prog: String,
    usage: Option<String>,
    description: Option<String>,
    epilog: Option<String>,
    formatter_class: i64,
    prefix_chars: String,
    fromfile_prefix_chars: Option<String>,
    allow_abbrev: bool,
    exit_on_error: bool,
    color: bool,
    options: Vec<ArgSpec>,
    groups: Vec<GroupState>,
    subparsers: Option<SubparsersState>,
    defaults: Vec<(String, i64)>,
    test_mode: bool,
}

impl View {
    /// Whether the active formatter derives from `name`.
    fn formatter_is(&self, name: &str) -> bool {
        value::is_subclass_of(self.formatter_class, cached_module_class(name))
    }

    fn allocator_formatter(&self) -> bool {
        self.formatter_is("ArgumentDefaultsHelpFormatter")
    }

    fn metavar_formatter(&self) -> bool {
        self.formatter_is("MetavarTypeHelpFormatter")
    }

    fn raw_description(&self) -> bool {
        self.formatter_is("RawTextHelpFormatter")
            || self.formatter_is("RawDescriptionHelpFormatter")
    }

    fn raw_text(&self) -> bool {
        self.formatter_is("RawTextHelpFormatter")
    }

    /// The display group holding `spec`'s help entry.
    fn display_group(&self, spec: &ArgSpec) -> usize {
        let group = self.groups.get(spec.group);
        match group {
            Some(group) if group.mutex => group.parent,
            _ => spec.group.min(self.groups.len().saturating_sub(1)),
        }
    }
}

fn view(parser: i64) -> Option<View> {
    let state = parser_ref(parser)?;
    Some(View {
        prog: state.prog.clone().unwrap_or_else(default_prog),
        usage: state.usage.clone(),
        description: state.description.clone(),
        epilog: state.epilog.clone(),
        formatter_class: state.formatter_class,
        prefix_chars: state.prefix_chars.clone(),
        fromfile_prefix_chars: state.fromfile_prefix_chars.clone(),
        allow_abbrev: state.allow_abbrev,
        exit_on_error: state.exit_on_error,
        color: state.color,
        options: state.options.clone(),
        groups: state.groups.clone(),
        subparsers: state.subparsers.clone(),
        defaults: state.defaults.clone(),
        test_mode: state.test_mode,
    })
}

/// CPython 3.14's default `_colorize.Argparse` ANSI palette.
///
/// Every field holds an ANSI SGR prefix. A palette with empty strings is the
/// plain-text formatter, so every rendering path can use `paint`
/// unconditionally and produce byte-identical plain output.
#[derive(Clone)]
struct Palette {
    usage: String,
    prog: String,
    prog_extra: String,
    heading: String,
    summary_long_option: String,
    summary_short_option: String,
    summary_label: String,
    summary_action: String,
    long_option: String,
    short_option: String,
    label: String,
    action: String,
    reset: String,
}

/// `_colorize.ANSIColors` spellings used by the default argparse theme.
const ANSI_BOLD_BLUE: &str = "\x1b[1;34m";
const ANSI_BOLD_MAGENTA: &str = "\x1b[1;35m";
const ANSI_BOLD_CYAN: &str = "\x1b[1;36m";
const ANSI_BOLD_GREEN: &str = "\x1b[1;32m";
const ANSI_BOLD_YELLOW: &str = "\x1b[1;33m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_MAGENTA: &str = "\x1b[35m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_RESET: &str = "\x1b[0m";

impl Palette {
    fn plain() -> Palette {
        Palette {
            usage: String::new(),
            prog: String::new(),
            prog_extra: String::new(),
            heading: String::new(),
            summary_long_option: String::new(),
            summary_short_option: String::new(),
            summary_label: String::new(),
            summary_action: String::new(),
            long_option: String::new(),
            short_option: String::new(),
            label: String::new(),
            action: String::new(),
            reset: String::new(),
        }
    }

    fn colored() -> Palette {
        Palette {
            usage: ANSI_BOLD_BLUE.to_string(),
            prog: ANSI_BOLD_MAGENTA.to_string(),
            prog_extra: ANSI_MAGENTA.to_string(),
            heading: ANSI_BOLD_BLUE.to_string(),
            summary_long_option: ANSI_CYAN.to_string(),
            summary_short_option: ANSI_GREEN.to_string(),
            summary_label: ANSI_YELLOW.to_string(),
            summary_action: ANSI_GREEN.to_string(),
            long_option: ANSI_BOLD_CYAN.to_string(),
            short_option: ANSI_BOLD_GREEN.to_string(),
            label: ANSI_BOLD_YELLOW.to_string(),
            action: ANSI_BOLD_GREEN.to_string(),
            reset: ANSI_RESET.to_string(),
        }
    }

    fn paint(&self, color: &str, text: &str) -> String {
        if color.is_empty() {
            text.to_string()
        } else {
            format!("{color}{text}{}", self.reset)
        }
    }
}

/// Imports `_colorize`, the stdlib module owning CPython's color policy.
fn colorize_module() -> Option<i64> {
    let name = text_value("_colorize");
    let module = crate::modules::importlib::call("import_module", std::slice::from_ref(&name));
    if exc::pending() {
        exc::clear();
        return None;
    }
    (module != NONE).then_some(module)
}

/// Reads an attribute without letting a missing member leak an exception.
fn colorize_attr(object: i64, name: &str) -> Option<i64> {
    let item = try_action_attr(object, name)?;
    (item != NONE).then_some(item)
}

/// Calls `_colorize.can_colorize()`; `None` when the hook is unavailable.
fn colorize_can_colorize(module: i64) -> Option<bool> {
    let function = colorize_attr(module, "can_colorize")?;
    let roots = crate::heap::root_values(&[function]);
    let result = crate::core::builtins::call(function, 0, std::ptr::null());
    let enabled = if exc::pending() {
        exc::clear();
        None
    } else {
        Some(value::truthy(result))
    };
    crate::heap::release_roots(roots);
    enabled
}

/// Reads the active `_colorize.Theme.argparse` section.
fn palette_from_colorize(module: i64) -> Option<Palette> {
    let theme = colorize_attr(module, "_theme")?;
    let roots = crate::heap::root_values(&[theme]);
    let section = colorize_attr(theme, "argparse");
    let palette = section.and_then(|section| {
        let roots = crate::heap::root_values(&[section]);
        let palette = palette_from_section(section);
        crate::heap::release_roots(roots);
        palette
    });
    crate::heap::release_roots(roots);
    palette
}

/// Reads every `Argparse` theme field, falling back to the defaults when the
/// section is not the expected mapping/dataclass shape.
fn palette_from_section(section: i64) -> Option<Palette> {
    fn field(section: i64, name: &str) -> Option<String> {
        colorize_attr(section, name)
            .and_then(value::as_str)
            .map(str::to_string)
    }
    Some(Palette {
        usage: field(section, "usage")?,
        prog: field(section, "prog")?,
        prog_extra: field(section, "prog_extra")?,
        heading: field(section, "heading")?,
        summary_long_option: field(section, "summary_long_option")?,
        summary_short_option: field(section, "summary_short_option")?,
        summary_label: field(section, "summary_label")?,
        summary_action: field(section, "summary_action")?,
        long_option: field(section, "long_option")?,
        short_option: field(section, "short_option")?,
        label: field(section, "label")?,
        action: field(section, "action")?,
        reset: field(section, "reset")?,
    })
}

/// CPython's `HelpFormatter._set_color`: `color=` plus the stdlib policy.
///
/// `can_colorize()` accounts for `PYTHON_COLORS`, `NO_COLOR`, `FORCE_COLOR`,
/// `TERM=dumb` and `isatty`; consulting the Python hook (rather than reading
/// those variables here) also honours `test.support.force_color` and other
/// callers that monkeypatch the policy.
fn active_palette(color: bool) -> Palette {
    if !color {
        return Palette::plain();
    }
    let Some(module) = colorize_module() else {
        return Palette::plain();
    };
    let roots = crate::heap::root_values(&[module]);
    let palette = if colorize_can_colorize(module) == Some(true) {
        palette_from_colorize(module).unwrap_or_else(Palette::colored)
    } else {
        Palette::plain()
    };
    crate::heap::release_roots(roots);
    palette
}

/// Reports a parse error: prints the usage block and raises
/// `SystemExit(2)` (or the legacy exception in test mode).
fn fail(_parser: i64, view: &View, tag: u32, message: &str) -> i64 {
    if view.test_mode {
        return exc::raise_with_message(tag, message);
    }
    if !view.exit_on_error {
        return raise_argument_error(NONE, message);
    }
    print_usage_to_stderr(view);
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "{}: error: {}", view.prog, message);
    let _ = stderr.flush();
    exc::raise_exit(tagged_int(2))
}

/// Raises `argparse.ArgumentError(argument, message)`; used for
/// `exit_on_error=False` and for conflicts detected while defining arguments.
fn raise_argument_error(argument: i64, message: &str) -> i64 {
    let class = argument_error_class();
    let text = text_value(message);
    let roots = crate::heap::root_values(&[class, text, argument]);
    let argv = [argument, text];
    let instance = crate::builtins::call(class, argv.len(), argv.as_ptr());
    crate::heap::release_roots(roots);
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

fn print_usage_to_stderr(view: &View) {
    let text = format_usage_text(view);
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
}

fn print_help_method(parser: i64, args: &[i64]) -> i64 {
    let file = args.first().copied().unwrap_or(NONE);
    let text = format_help(parser);
    write_text(file, &text)
}

/// Prints the help text to stdout (used by the automatic help action).
fn print_help(parser: i64) {
    let text = format_help(parser);
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

fn print_usage_method(parser: i64, args: &[i64]) -> i64 {
    let file = args.first().copied().unwrap_or(NONE);
    let text = format_usage(parser);
    write_text(file, &text)
}

fn write_text(file: i64, text: &str) -> i64 {
    if file != NONE {
        let value = text_value(text);
        return crate::methods::call(file, b"write".as_ptr(), "write".len(), 1, [value].as_ptr());
    }
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
    NONE
}

fn format_usage(parser: i64) -> String {
    match view(parser) {
        Some(view) => format_usage_text(&view),
        None => {
            attribute_error("format_usage");
            String::new()
        }
    }
}

fn format_help(parser: i64) -> String {
    match view(parser) {
        Some(view) => format_help_text(&view),
        None => {
            attribute_error("format_help");
            String::new()
        }
    }
}

fn error_method(parser: i64, args: &[i64]) -> i64 {
    let Some(argument) = args.first().copied() else {
        return type_error("error() requires a message");
    };
    let Some(view) = view(parser) else {
        return attribute_error("error");
    };
    let message = match value::as_str(argument) {
        Some(text) => text.to_string(),
        None => value::to_string(argument),
    };
    print_usage_to_stderr(&view);
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "{}: error: {}", view.prog, message);
    let _ = stderr.flush();
    exc::raise_exit(tagged_int(2))
}

fn exit_method(_parser: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut status = args.first().copied().unwrap_or(NONE);
    let mut message = args.get(1).copied().unwrap_or(NONE);
    for (name, argument) in keywords {
        match name.as_str() {
            "status" => status = *argument,
            "message" => message = *argument,
            _ => {
                return type_error(&format!(
                    "ArgumentParser.exit() got an unexpected keyword argument '{name}'"
                ));
            }
        }
    }
    if message != NONE {
        let text = match value::as_str(message) {
            Some(text) => text.to_string(),
            None => value::to_string(message),
        };
        if value::truthy(status) && status != NONE {
            let mut stderr = std::io::stderr();
            let _ = stderr.write_all(text.as_bytes());
            let _ = stderr.flush();
        } else {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.flush();
        }
    }
    if status == NONE {
        status = tagged_int(0);
    }
    // CPython's `exit` calls `_sys.exit`, which `unittest.mock` patches in the
    // test suite. Route through the module attribute so the patch takes effect
    // (a patched exit returns normally instead of raising `SystemExit`).
    let sys = super::module_value_by_name("sys");
    let exit = crate::core::methods::getattr(sys, b"exit".as_ptr(), 4);
    if exc::pending() {
        exc::clear();
        return exc::raise_exit(status);
    }
    let _ = crate::core::builtins::call(exit, 1, [status].as_ptr());
    if exc::pending() {
        return NONE;
    }
    NONE
}

/// `parse_args`/`parse_known_args`: binds `args` and `namespace`.
fn parse(parser: i64, args: &[i64], keywords: &[(String, i64)], known: bool) -> i64 {
    let Some(view) = view(parser) else {
        return attribute_error(if known {
            "parse_known_args"
        } else {
            "parse_args"
        });
    };
    let method = if known {
        "parse_known_args"
    } else {
        "parse_args"
    };
    if args.len() > 2 {
        return type_error(&format!(
            "ArgumentParser.{method}() takes at most 2 arguments ({} given)",
            args.len()
        ));
    }
    let mut argv_value = args.first().copied().unwrap_or(NONE);
    let mut namespace_value = args.get(1).copied().unwrap_or(NONE);
    let mut saw_args = !args.is_empty();
    let mut saw_namespace = args.len() > 1;
    for (name, argument) in keywords {
        match name.as_str() {
            "args" => {
                if saw_args {
                    return type_error(&format!(
                        "ArgumentParser.{method}() got multiple values for argument 'args'"
                    ));
                }
                argv_value = *argument;
                saw_args = true;
            }
            "namespace" => {
                if saw_namespace {
                    return type_error(&format!(
                        "ArgumentParser.{method}() got multiple values for argument 'namespace'"
                    ));
                }
                namespace_value = *argument;
                saw_namespace = true;
            }
            _ => {
                return type_error(&format!(
                    "ArgumentParser.{method}() got an unexpected keyword argument '{name}'"
                ));
            }
        }
    }

    // Parsed values and default-argv tokens stay reachable from this list so a
    // collection triggered by a later allocation cannot free them.
    let roots = value::list_new(Vec::new());
    let tokens = match argument_tokens(argv_value, roots) {
        Ok(tokens) => tokens,
        Err(err) => return err,
    };

    let (pairs, extras) = match parse_tokens(parser, &view, &tokens, roots) {
        Ok(result) => result,
        Err(err) => return err,
    };
    let extras_list = value::list_new(extras.clone());
    root_value(roots, extras_list);

    if !known && !extras.is_empty() {
        let rendered: Vec<String> = extras.iter().map(|item| value::to_string(*item)).collect();
        return fail(
            parser,
            &view,
            RUNTIME_ERROR,
            &format!("unrecognized arguments: {}", rendered.join(" ")),
        );
    }

    let namespace = merge_namespace(namespace_value, &pairs);
    if exc::pending() {
        return NONE;
    }
    if known {
        value::tuple_new(vec![namespace, extras_list])
    } else {
        namespace
    }
}

fn argument_tokens(argv: i64, roots: i64) -> Result<Vec<i64>, i64> {
    if argv == NONE {
        let mut tokens = Vec::new();
        for argument in crate::argv().into_iter().skip(1) {
            let token = value::str_alloc(argument);
            root_value(roots, token);
            tokens.push(token);
        }
        return Ok(tokens);
    }
    let items = if let Some(items) = value::as_list(argv) {
        items.to_vec()
    } else if let Some(items) = value::as_tuple(argv) {
        items.to_vec()
    } else {
        return Err(type_error(&format!(
            "parse_args() argument must be a list or tuple, not {}",
            value::type_name(argv)
        )));
    };
    for &token in &items {
        if value::as_str(token).is_none() {
            return Err(type_error(&format!(
                "parse_args() arguments must be str, not {}",
                value::type_name(token)
            )));
        }
    }
    Ok(items)
}

fn merge_namespace(namespace: i64, pairs: &[(String, i64)]) -> i64 {
    if namespace == NONE {
        return namespace_instance(pairs);
    }
    // CPython writes every parsed value with `setattr`, so any object with a
    // writable attribute namespace is accepted (`parse_args(namespace=NS())`).
    if let Some(mut state) = value::namespace_pairs_mut(namespace) {
        for (name, item) in pairs {
            upsert(&mut state.pairs, name, *item);
        }
        return namespace;
    }
    if value::as_instance(namespace).is_some() {
        let roots = crate::heap::root_values(std::slice::from_ref(&namespace));
        for (name, item) in pairs {
            value::instance_setattr(namespace, name.clone(), *item);
        }
        crate::heap::release_roots(roots);
        return namespace;
    }
    type_error(&format!(
        "namespace must be a Namespace, not {}",
        value::type_name(namespace)
    ))
}

struct Scan<'a> {
    view: &'a View,
    parser: i64,
    roots: i64,
    values: Vec<Option<i64>>,
    seen: Vec<bool>,
    group_hit: Vec<Option<usize>>,
    extras: Vec<i64>,
    help_requested: bool,
    subcommand: Option<String>,
    child_pairs: Vec<(String, i64)>,
}

impl<'a> Scan<'a> {
    fn fail(&self, tag: u32, message: &str) -> i64 {
        fail(self.parser, self.view, tag, message)
    }

    fn root(&self, value: i64) {
        if value != NONE {
            root_value(self.roots, value);
        }
    }

    /// Converts one raw argument according to the action's `type`.
    fn convert(&self, spec: &ArgSpec, raw: i64) -> Result<i64, i64> {
        if spec.type_fn == NONE {
            return Ok(raw);
        }
        // `FileType` markers open the file with the recorded mode.
        if let Some(pairs) = value::as_namespace(spec.type_fn) {
            if let Some((_, mode)) = pairs.iter().find(|(name, _)| name.as_str() == FILE_TYPE) {
                let mode = *mode;
                let opened = crate::modules::io_open(&[raw, mode]);
                if exc::pending() {
                    let detail = pending_message();
                    exc::clear();
                    return Err(self.fail(
                        VALUE_ERROR,
                        &format!("argument {}: {detail}", self.action_name(spec)),
                    ));
                }
                return Ok(opened);
            }
        }
        let argv = [raw];
        let converted = builtins::call(spec.type_fn, 1, argv.as_ptr());
        if exc::pending() {
            let detail = pending_message();
            exc::clear();
            return Err(self.fail(
                VALUE_ERROR,
                &format!("argument {}: {detail}", self.action_name(spec)),
            ));
        }
        Ok(converted)
    }

    fn action_name(&self, spec: &ArgSpec) -> String {
        if spec.positional {
            spec.metavar.clone().unwrap_or_else(|| spec.dest.clone())
        } else if !spec.flags.is_empty() {
            spec.flags.join("/")
        } else {
            spec.dest.clone()
        }
    }

    fn check_choices(&self, spec: &ArgSpec, candidate: i64) -> Result<(), i64> {
        let Some(choices) = &spec.choices else {
            return Ok(());
        };
        if choices
            .iter()
            .any(|choice| container::equals(*choice, candidate))
        {
            return Ok(());
        }
        let rendered: Vec<String> = choices.iter().map(|choice| value::repr(*choice)).collect();
        Err(self.fail(
            VALUE_ERROR,
            &format!(
                "argument {}: invalid choice: {} (choose from {})",
                self.action_name(spec),
                value::repr(candidate),
                rendered.join(", ")
            ),
        ))
    }

    /// Applies one occurrence of an optional argument.
    fn apply_option(
        &mut self,
        index: usize,
        matched_flag: Option<&str>,
        raw: &[i64],
    ) -> Result<(), i64> {
        let spec = self.view.options[index].clone();
        // Mutually exclusive check.
        if let Some(group) = self.view.groups.get(spec.group) {
            if group.mutex && spec.group < self.group_hit.len() {
                let name = self.action_name(&spec);
                if let Some(previous) = self.group_hit[spec.group] {
                    if previous != index {
                        let previous_name = self.action_name(&self.view.options[previous]);
                        return Err(self.fail(
                            RUNTIME_ERROR,
                            &format!("argument {name}: not allowed with argument {previous_name}"),
                        ));
                    }
                }
                self.group_hit[spec.group] = Some(index);
            }
        }
        self.seen[index] = true;

        match spec.action {
            ActionKind::StoreTrue => {
                self.values[index] = Some(value::TRUE);
                return Ok(());
            }
            ActionKind::StoreFalse => {
                self.values[index] = Some(value::FALSE);
                return Ok(());
            }
            ActionKind::BooleanOptional => {
                let negated = matched_flag.is_some_and(|flag| {
                    spec.negative_flags.iter().any(|negative| negative == flag)
                });
                self.values[index] = Some(if negated { value::FALSE } else { value::TRUE });
                return Ok(());
            }
            ActionKind::StoreConst => {
                self.values[index] = Some(spec.const_value);
                return Ok(());
            }
            ActionKind::Count => {
                let current = self.values[index].and_then(value::as_integer).unwrap_or(0);
                let next = current.saturating_add(1);
                let next = tagged_int(next);
                self.values[index] = Some(next);
                return Ok(());
            }
            ActionKind::Help => {
                print_help(self.parser);
                if self.view.test_mode {
                    self.help_requested = true;
                    return Ok(());
                }
                exc::raise_exit(tagged_int(0));
                return Err(NONE);
            }
            ActionKind::Version => {
                let text = spec.version.replace("%(prog)s", &self.view.prog);
                println!("{text}");
                exc::raise_exit(tagged_int(0));
                return Err(NONE);
            }
            ActionKind::Append | ActionKind::AppendConst => {
                let item = if spec.action == ActionKind::AppendConst {
                    spec.const_value
                } else {
                    match self.converted_value(&spec, raw)? {
                        Some(value) => value,
                        None => spec.const_value,
                    }
                };
                self.root(item);
                let current = self.values[index];
                if let Some(list) = current {
                    if value::as_list(list).is_some() {
                        if let Some(mut items) = value::list_items_mut(list) {
                            items.push(item);
                            crate::heap::write_barrier_edge(list, item);
                        }
                        self.values[index] = Some(list);
                        return Ok(());
                    }
                }
                let list = value::list_new(vec![item]);
                self.root(list);
                self.values[index] = Some(list);
                return Ok(());
            }
            ActionKind::Store => {}
        }

        let value = self.converted_value(&spec, raw)?;
        let value = match value {
            Some(value) => value,
            None => spec.const_value,
        };
        self.root(value);
        self.values[index] = Some(value);
        Ok(())
    }

    /// Converts the raw operand list of `spec`, returning `None` when `?`
    /// was supplied without a value (the caller uses `const`).
    fn converted_value(&self, spec: &ArgSpec, raw: &[i64]) -> Result<Option<i64>, i64> {
        match spec.nargs {
            Nargs::Single => {
                let Some(first) = raw.first().copied() else {
                    return Err(self.fail(
                        RUNTIME_ERROR,
                        &format!("argument {}: expected one argument", self.action_name(spec)),
                    ));
                };
                let converted = self.convert(spec, first)?;
                self.check_choices(spec, converted)?;
                Ok(Some(converted))
            }
            Nargs::Optional => match raw.first().copied() {
                None => Ok(None),
                Some(first) => {
                    let converted = self.convert(spec, first)?;
                    self.check_choices(spec, converted)?;
                    Ok(Some(converted))
                }
            },
            Nargs::Exact(count) => {
                if raw.len() != count {
                    return Err(self.fail(
                        RUNTIME_ERROR,
                        &format!(
                            "argument {}: expected {} argument(s)",
                            self.action_name(spec),
                            count
                        ),
                    ));
                }
                let mut converted = Vec::with_capacity(count);
                for &item in raw {
                    let value = self.convert(spec, item)?;
                    self.check_choices(spec, value)?;
                    self.root(value);
                    converted.push(value);
                }
                Ok(Some(value::list_new(converted)))
            }
            Nargs::ZeroOrMore | Nargs::OneOrMore => {
                let minimum = usize::from(spec.nargs == Nargs::OneOrMore);
                if raw.len() < minimum {
                    return Err(self.fail(
                        RUNTIME_ERROR,
                        &format!(
                            "argument {}: expected at least one argument",
                            self.action_name(spec)
                        ),
                    ));
                }
                let mut converted = Vec::with_capacity(raw.len());
                for &item in raw {
                    let value = self.convert(spec, item)?;
                    self.check_choices(spec, value)?;
                    self.root(value);
                    converted.push(value);
                }
                Ok(Some(value::list_new(converted)))
            }
        }
    }

    /// Applies one positional occurrence.
    fn apply_positional(&mut self, index: usize, raw: &[i64]) -> Result<(), i64> {
        let spec = self.view.options[index].clone();
        self.seen[index] = true;
        if matches!(
            spec.nargs,
            Nargs::ZeroOrMore | Nargs::OneOrMore | Nargs::Exact(_)
        ) {
            let mut converted = Vec::with_capacity(raw.len());
            for &item in raw {
                let value = self.convert(&spec, item)?;
                self.check_choices(&spec, value)?;
                self.root(value);
                converted.push(value);
            }
            let list = value::list_new(converted);
            self.root(list);
            self.values[index] = Some(list);
            return Ok(());
        }
        if raw.is_empty() {
            return Ok(());
        }
        let value = self.convert(&spec, raw[0])?;
        self.check_choices(&spec, value)?;
        self.root(value);
        self.values[index] = Some(value);
        Ok(())
    }
}

fn pending_message() -> String {
    let pending = exc::pending_value();
    let text = value::to_string(pending);
    if text.is_empty() {
        value::repr(pending)
    } else {
        text
    }
}

fn note_option_like(text: &str, view: &View) -> bool {
    if text == "--" {
        return false;
    }
    let Some(first) = text.chars().next() else {
        return false;
    };
    if !view.prefix_chars.contains(first) {
        return false;
    }
    if text.len() == 1 {
        return false;
    }
    if first == '-' && negative_number_allowed(view) && is_negative_number(text) {
        return false;
    }
    true
}

fn is_negative_number(text: &str) -> bool {
    text.len() > 1
        && text.starts_with('-')
        && text[1..].chars().next().is_some_and(|c| c.is_ascii_digit())
        && text.parse::<f64>().is_ok()
}

fn negative_number_allowed(view: &View) -> bool {
    !view.options.iter().any(|spec| {
        !spec.positional
            && spec.flags.iter().any(|flag| {
                flag.starts_with('-')
                    && flag[1..].chars().next().is_some_and(|c| c.is_ascii_digit())
            })
    })
}

/// Expands `@file`-style tokens when `fromfile_prefix_chars` was configured.
fn expand_fromfile(view: &View, tokens: &[i64], roots: i64) -> Result<Vec<i64>, i64> {
    let Some(chars) = &view.fromfile_prefix_chars else {
        return Ok(tokens.to_vec());
    };
    let mut out = Vec::with_capacity(tokens.len());
    for &token in tokens {
        let Some(text) = value::as_str(token) else {
            out.push(token);
            continue;
        };
        let Some(first) = text.chars().next() else {
            out.push(token);
            continue;
        };
        if !chars.contains(first) || text.len() == 1 {
            out.push(token);
            continue;
        }
        let path = &text[text
            .char_indices()
            .nth(1)
            .map(|(index, _)| index)
            .unwrap_or(1)..];
        match std::fs::read_to_string(path) {
            Ok(content) => {
                for line in content.lines() {
                    let item = value::str_alloc(line.to_string());
                    root_value(roots, item);
                    out.push(item);
                }
            }
            Err(error) => {
                return Err(fail(NONE, view, RUNTIME_ERROR, &format!("{path}: {error}")));
            }
        }
    }
    Ok(out)
}

#[allow(clippy::type_complexity)]
fn parse_tokens(
    parser: i64,
    view: &View,
    tokens: &[i64],
    roots: i64,
) -> Result<(Vec<(String, i64)>, Vec<i64>), i64> {
    // Re-root the caller's tokens so a collection during conversion keeps
    // them alive even when the caller's list is temporary.
    let expanded = expand_fromfile(view, tokens, roots)?;
    for &token in expanded.iter() {
        root_value(roots, token);
    }
    let tokens = expanded.as_slice();
    let mut scan = Scan {
        view,
        parser,
        roots,
        values: vec![None; view.options.len()],
        seen: vec![false; view.options.len()],
        group_hit: vec![None; view.groups.len()],
        extras: Vec::new(),
        help_requested: false,
        subcommand: None,
        child_pairs: Vec::new(),
    };

    let positionals: Vec<usize> = view
        .options
        .iter()
        .enumerate()
        .filter(|(_, spec)| spec.positional)
        .map(|(index, _)| index)
        .collect();
    let mut position = 0usize;
    let mut only_positional = false;
    let mut index = 0usize;

    while index < tokens.len() {
        let token = tokens[index];
        let Some(text) = value::as_str(token).map(str::to_string) else {
            return Err(type_error("parse_args() arguments must be str"));
        };

        if !only_positional && text == "--" {
            only_positional = true;
            index += 1;
            continue;
        }

        if !only_positional && note_option_like(&text, view) {
            match handle_option(&mut scan, tokens, index)? {
                OptionStep::Consumed(next) => {
                    index = next;
                    if scan.help_requested {
                        break;
                    }
                    continue;
                }
                OptionStep::Unknown => {
                    scan.extras.push(token);
                    index += 1;
                    continue;
                }
            }
        }

        // Positional token.
        if position >= positionals.len() {
            if let Some(subparsers) = view.subparsers.clone() {
                let Some((_, child, _)) = subparsers
                    .parsers
                    .iter()
                    .find(|(name, _, _)| name == &text)
                    .cloned()
                else {
                    let choices: Vec<String> = subparsers
                        .parsers
                        .iter()
                        .map(|(name, _, _)| format!("'{name}'"))
                        .collect();
                    let label = subparsers
                        .dest
                        .clone()
                        .or_else(|| subparsers.metavar.clone())
                        .unwrap_or_else(|| {
                            let names: Vec<String> = subparsers
                                .parsers
                                .iter()
                                .map(|(name, _, _)| name.clone())
                                .collect();
                            format!("{{{}}}", names.join(","))
                        });
                    return Err(scan.fail(
                        VALUE_ERROR,
                        &format!(
                            "argument {label}: invalid choice: '{text}' (choose from {})",
                            choices.join(", ")
                        ),
                    ));
                };
                let child_view = match view_of(child) {
                    Some(child_view) => child_view,
                    None => return Err(attribute_error("add_parser")),
                };
                let (pairs, extras) =
                    parse_child(child, &child_view, &tokens[index + 1..], scan.roots)?;
                for (_, item) in &pairs {
                    root_value(scan.roots, *item);
                }
                for &item in &extras {
                    root_value(scan.roots, item);
                }
                scan.child_pairs = pairs;
                scan.extras.extend(extras);
                scan.subcommand = Some(text);
                break;
            }
            scan.extras.push(token);
            index += 1;
            continue;
        }

        let spec_index = positionals[position];
        let spec = view.options[spec_index].clone();
        match spec.nargs {
            Nargs::Single | Nargs::Optional => {
                scan.apply_positional(spec_index, &[token])?;
                position += 1;
                index += 1;
            }
            Nargs::Exact(count) => {
                let mut raw = Vec::with_capacity(count);
                while raw.len() < count && index + raw.len() < tokens.len() {
                    let candidate = tokens[index + raw.len()];
                    let candidate_text = value::as_str(candidate).unwrap_or("");
                    if !only_positional && note_option_like(candidate_text, view) {
                        break;
                    }
                    raw.push(candidate);
                }
                if raw.len() != count {
                    return Err(scan.fail(
                        RUNTIME_ERROR,
                        &format!(
                            "the following arguments are required: {}",
                            spec.metavar.clone().unwrap_or_else(|| spec.dest.clone())
                        ),
                    ));
                }
                scan.apply_positional(spec_index, &raw)?;
                position += 1;
                index += count;
            }
            Nargs::ZeroOrMore | Nargs::OneOrMore => {
                let minimum = usize::from(spec.nargs == Nargs::OneOrMore);
                let reserve = minimum_required_after(view, &positionals, position + 1);
                let mut raw = Vec::new();
                while index + raw.len() < tokens.len() {
                    let candidate = tokens[index + raw.len()];
                    let candidate_text = value::as_str(candidate).unwrap_or("");
                    if !only_positional && note_option_like(candidate_text, view) {
                        break;
                    }
                    raw.push(candidate);
                }
                let take = if reserve == 0 {
                    raw.len()
                } else {
                    raw.len().saturating_sub(reserve)
                };
                let take = take.max(minimum);
                if raw.len() < take {
                    return Err(scan.fail(
                        RUNTIME_ERROR,
                        &format!(
                            "the following arguments are required: {}",
                            spec.metavar.clone().unwrap_or_else(|| spec.dest.clone())
                        ),
                    ));
                }
                let collected = raw[..take.min(raw.len())].to_vec();
                scan.apply_positional(spec_index, &collected)?;
                position += 1;
                index += collected.len();
            }
        }
    }

    // Required actions.
    if !scan.help_requested {
        let mut missing: Vec<String> = Vec::new();
        for (index, spec) in view.options.iter().enumerate() {
            if scan.seen[index] {
                continue;
            }
            let is_required = match spec.nargs {
                Nargs::Single => spec.positional || spec.required,
                Nargs::OneOrMore | Nargs::Exact(_) => spec.positional || spec.required,
                Nargs::Optional | Nargs::ZeroOrMore => spec.required,
            };
            if is_required {
                missing.push(scan.action_name(spec));
            }
        }
        if !missing.is_empty() {
            return Err(scan.fail(
                RUNTIME_ERROR,
                &format!(
                    "the following arguments are required: {}",
                    missing.join(", ")
                ),
            ));
        }
        for (group_index, group) in view.groups.iter().enumerate() {
            if !group.mutex || !group.required || scan.group_hit[group_index].is_some() {
                continue;
            }
            let names: Vec<String> = group
                .members
                .iter()
                .map(|index| scan.action_name(&view.options[*index]))
                .collect();
            return Err(scan.fail(
                RUNTIME_ERROR,
                &format!("one of the arguments {} is required", names.join(" ")),
            ));
        }
        if let Some(subparsers) = &view.subparsers {
            if subparsers.required && scan.subcommand.is_none() {
                let label = match &subparsers.dest {
                    Some(dest) => dest.clone(),
                    None => subparsers.metavar.clone().unwrap_or_else(|| {
                        let names: Vec<String> = subparsers
                            .parsers
                            .iter()
                            .map(|(name, _, _)| name.clone())
                            .collect();
                        format!("{{{}}}", names.join(","))
                    }),
                };
                return Err(scan.fail(
                    RUNTIME_ERROR,
                    &format!("the following arguments are required: {label}"),
                ));
            }
        }
    }

    // Build the result pairs: parsed values, defaults, then subparser values.
    let test_mode_help = scan.help_requested && view.test_mode;
    let mut pairs: Vec<(String, i64)> = Vec::new();
    if test_mode_help {
        pairs.push(("_help".to_string(), value::TRUE));
        return Ok((pairs, scan.extras));
    }
    for (index, spec) in view.options.iter().enumerate() {
        if spec.action == ActionKind::Help || spec.action == ActionKind::Version {
            continue;
        }
        let parsed = scan.values[index];
        let value = match parsed {
            Some(value) => Some(value),
            None => {
                if let Some((_, default)) = view
                    .defaults
                    .iter()
                    .find(|(name, _)| name.as_str() == spec.dest)
                {
                    Some(*default)
                } else if value::as_str(spec.default) == Some(SUPPRESS) {
                    None
                } else if spec.default != NONE {
                    Some(spec.default)
                } else if spec.nargs == Nargs::ZeroOrMore && spec.positional {
                    let empty = value::list_new(Vec::new());
                    root_value(scan.roots, empty);
                    Some(empty)
                } else {
                    Some(NONE)
                }
            }
        };
        if let Some(value) = value {
            upsert(&mut pairs, &spec.dest, value);
        }
    }
    if let Some(subparsers) = &view.subparsers {
        if let (Some(dest), Some(command)) = (&subparsers.dest, &scan.subcommand) {
            let command = text_value(command);
            root_value(scan.roots, command);
            upsert(&mut pairs, dest, command);
        }
    }
    for (name, value) in &view.defaults {
        if !pairs.iter().any(|(key, _)| key == name) {
            pairs.push((name.clone(), *value));
        }
    }
    for (name, value) in &scan.child_pairs {
        upsert(&mut pairs, name, *value);
    }
    Ok((pairs, scan.extras))
}

/// Parses a child parser over the remaining tokens.
#[allow(clippy::type_complexity)]
fn parse_child(
    parser: i64,
    view: &View,
    tokens: &[i64],
    roots: i64,
) -> Result<(Vec<(String, i64)>, Vec<i64>), i64> {
    for &token in tokens {
        root_value(roots, token);
    }
    let (pairs, extras) = parse_tokens(parser, view, tokens, roots)?;
    Ok((pairs, extras))
}

fn view_of(parser: i64) -> Option<View> {
    view(parser)
}

/// Minimum number of tokens the positional specs after `from` require.
fn minimum_required_after(view: &View, positionals: &[usize], from: usize) -> usize {
    let mut total = 0usize;
    for &index in &positionals[from.min(positionals.len())..] {
        total += match view.options[index].nargs {
            Nargs::Single | Nargs::Optional => 1,
            Nargs::OneOrMore => 1,
            Nargs::Exact(count) => count,
            Nargs::ZeroOrMore => 0,
        };
    }
    if view.subparsers.is_some() {
        total += 1;
    }
    total
}

enum OptionStep {
    Consumed(usize),
    Unknown,
}

/// Handles the option token at `index`, returning the next token index.
fn handle_option(scan: &mut Scan<'_>, tokens: &[i64], index: usize) -> Result<OptionStep, i64> {
    let view = scan.view;
    let text = value::as_str(tokens[index]).unwrap_or("");
    let (name, inline) = match text.split_once('=') {
        Some((name, inline)) => (name, Some(inline.to_string())),
        None => (text, None),
    };

    if let Some(spec_index) = lookup_option_checked(scan, name)? {
        let spec = view.options[spec_index].clone();
        if spec.action.takes_no_value() {
            if let Some(inline) = &inline {
                return Err(scan.fail(
                    VALUE_ERROR,
                    &format!(
                        "argument {}: ignored explicit argument '{inline}'",
                        scan.action_name(&spec)
                    ),
                ));
            }
            scan.apply_option(spec_index, Some(name), &[])?;
            return Ok(OptionStep::Consumed(index + 1));
        }
        if let Some(inline) = &inline {
            if !matches!(spec.nargs, Nargs::Single | Nargs::Optional) {
                return Err(scan.fail(
                    VALUE_ERROR,
                    &format!(
                        "argument {}: ignored explicit argument '{inline}'",
                        scan.action_name(&spec)
                    ),
                ));
            }
            let raw = text_value(inline);
            scan.root(raw);
            scan.apply_option(spec_index, Some(name), &[raw])?;
            return Ok(OptionStep::Consumed(index + 1));
        }
        let (raw, consumed) = collect_operands(scan, &spec, tokens, index + 1, Some(name))?;
        scan.apply_option(spec_index, Some(name), &raw)?;
        return Ok(OptionStep::Consumed(index + 1 + consumed));
    }

    // Short option cluster (`-abc`).
    if inline.is_none() {
        if let Some(step) = handle_cluster(scan, tokens, index)? {
            return Ok(step);
        }
    }
    Ok(OptionStep::Unknown)
}

/// Abbreviation-aware lookup that reports ambiguity through the scanner.
fn lookup_option_checked(scan: &Scan<'_>, name: &str) -> Result<Option<usize>, i64> {
    let view = scan.view;
    if let Some(index) = view.options.iter().position(|spec| {
        !spec.positional
            && (spec.flags.iter().any(|flag| flag == name)
                || spec.negative_flags.iter().any(|flag| flag == name))
    }) {
        return Ok(Some(index));
    }
    if !view.allow_abbrev || !is_long_flag(name, &view.prefix_chars) {
        return Ok(None);
    }
    let mut matches: Vec<usize> = Vec::new();
    for (index, spec) in view.options.iter().enumerate() {
        if spec.positional {
            continue;
        }
        for flag in spec.flags.iter().chain(spec.negative_flags.iter()) {
            if flag.starts_with(name) && flag.len() > name.len() {
                matches.push(index);
                break;
            }
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => {
            let mut rendered = Vec::new();
            for index in &matches {
                let spec = &view.options[*index];
                for flag in spec.flags.iter().chain(spec.negative_flags.iter()) {
                    if flag.starts_with(name) && flag.len() > name.len() {
                        rendered.push(flag.clone());
                    }
                }
            }
            Err(scan.fail(
                RUNTIME_ERROR,
                &format!(
                    "ambiguous option: {name} could match {}",
                    rendered.join(", ")
                ),
            ))
        }
    }
}

/// Handles a short-option cluster such as `-vv` or `-ovalue`.
fn handle_cluster(
    scan: &mut Scan<'_>,
    tokens: &[i64],
    index: usize,
) -> Result<Option<OptionStep>, i64> {
    let view = scan.view;
    let text = value::as_str(tokens[index]).unwrap_or("");
    let mut chars = text.chars();
    let Some(prefix) = chars.next() else {
        return Ok(None);
    };
    if !view.prefix_chars.contains(prefix) {
        return Ok(None);
    }
    let rest: Vec<char> = chars.collect();
    if rest.len() < 2 {
        return Ok(None);
    }
    // A cluster may only consist of declared single-character options.
    for (position, character) in rest.iter().enumerate() {
        let flag = format!("{prefix}{character}");
        let Some(spec_index) = view.options.iter().position(|spec| {
            !spec.positional && spec.flags.iter().any(|existing| existing == &flag)
        }) else {
            return Ok(None);
        };
        let spec = view.options[spec_index].clone();
        if spec.action.takes_no_value() {
            scan.apply_option(spec_index, Some(flag.as_str()), &[])?;
            continue;
        }
        let attached: String = rest[position + 1..].iter().collect();
        if !attached.is_empty() {
            let attached = attached.trim_start_matches('=').to_string();
            let raw = text_value(&attached);
            scan.root(raw);
            scan.apply_option(spec_index, Some(flag.as_str()), &[raw])?;
            return Ok(Some(OptionStep::Consumed(index + 1)));
        }
        let (raw, consumed) =
            collect_operands(scan, &spec, tokens, index + 1, Some(flag.as_str()))?;
        scan.apply_option(spec_index, Some(flag.as_str()), &raw)?;
        return Ok(Some(OptionStep::Consumed(index + 1 + consumed)));
    }
    Ok(Some(OptionStep::Consumed(index + 1)))
}

/// Collects the operand tokens of `spec` starting at `start`.
fn collect_operands(
    scan: &Scan<'_>,
    spec: &ArgSpec,
    tokens: &[i64],
    start: usize,
    flag: Option<&str>,
) -> Result<(Vec<i64>, usize), i64> {
    let view = scan.view;
    let label = flag.unwrap_or("argument");
    let mut raw = Vec::new();
    let mut offset = 0usize;
    let mut take_next = || -> Option<i64> {
        if start + offset >= tokens.len() {
            return None;
        }
        let candidate = tokens[start + offset];
        let text = value::as_str(candidate).unwrap_or("");
        if text == "--" || note_option_like(text, view) {
            return None;
        }
        offset += 1;
        Some(candidate)
    };
    match spec.nargs {
        Nargs::Single => match take_next() {
            Some(token) => raw.push(token),
            None => {
                return Err(scan.fail(
                    RUNTIME_ERROR,
                    &format!("argument {label}: expected one argument"),
                ));
            }
        },
        Nargs::Optional => {
            if let Some(token) = take_next() {
                raw.push(token);
            }
        }
        Nargs::Exact(count) => {
            for _ in 0..count {
                match take_next() {
                    Some(token) => raw.push(token),
                    None => {
                        return Err(scan.fail(
                            RUNTIME_ERROR,
                            &format!("argument {label}: expected {count} argument(s)"),
                        ));
                    }
                }
            }
        }
        Nargs::ZeroOrMore | Nargs::OneOrMore => {
            while let Some(token) = take_next() {
                raw.push(token);
            }
            if spec.nargs == Nargs::OneOrMore && raw.is_empty() {
                return Err(scan.fail(
                    RUNTIME_ERROR,
                    &format!("argument {label}: expected at least one argument"),
                ));
            }
        }
    }
    Ok((raw, offset))
}

// -------------------------------------------------------------------------
// Help and usage rendering
// -------------------------------------------------------------------------

fn metavar_for(view: &View, spec: &ArgSpec) -> String {
    if let Some(metavar) = &spec.metavar {
        return metavar.clone();
    }
    if let Some(choices) = &spec.choices {
        let rendered: Vec<String> = choices
            .iter()
            .map(|choice| value::to_string(*choice))
            .collect();
        return format!("{{{}}}", rendered.join(","));
    }
    if view.metavar_formatter() && spec.type_fn != NONE {
        if let Some(function) = value::as_function(spec.type_fn) {
            return function.name.clone();
        }
    }
    spec.dest.to_uppercase()
}

fn positional_metavar(view: &View, spec: &ArgSpec) -> String {
    if let Some(metavar) = &spec.metavar {
        return metavar.clone();
    }
    if let Some(choices) = &spec.choices {
        let rendered: Vec<String> = choices
            .iter()
            .map(|choice| value::to_string(*choice))
            .collect();
        return format!("{{{}}}", rendered.join(","));
    }
    if view.metavar_formatter() && spec.type_fn != NONE {
        if let Some(function) = value::as_function(spec.type_fn) {
            return function.name.clone();
        }
    }
    spec.dest.clone()
}

fn value_usage(spec: &ArgSpec, metavar: &str) -> String {
    match spec.nargs {
        Nargs::Single => metavar.to_string(),
        Nargs::Optional => format!("[{metavar}]"),
        Nargs::ZeroOrMore => format!("[{metavar} ...]"),
        Nargs::OneOrMore => format!("{metavar} [{metavar} ...]"),
        Nargs::Exact(count) => vec![metavar; count].join(" "),
    }
}

/// CPython's `_format_args` result for one positional argument.
fn positional_usage_text(view: &View, spec: &ArgSpec) -> String {
    let metavar = positional_metavar(view, spec);
    match spec.nargs {
        Nargs::Single => metavar,
        Nargs::Optional => format!("[{metavar}]"),
        Nargs::ZeroOrMore => format!("[{metavar} ...]"),
        Nargs::OneOrMore => format!("{metavar} [{metavar} ...]"),
        Nargs::Exact(count) => vec![metavar; count].join(" "),
    }
}

/// ANSI style for an option spelling: CPython treats any string longer than
/// two characters as a long option.
fn option_style<'a>(palette: &'a Palette, flag: &str) -> &'a str {
    if flag.chars().count() > 2 {
        &palette.summary_long_option
    } else {
        &palette.summary_short_option
    }
}

/// One `_get_actions_usage_parts` entry, without the outer brackets that group
/// contexts add. `in_group` suppresses an optional's own brackets and strips a
/// positional's `[...]`; `group_required` mirrors a required mutex group.
fn summary_usage_part(
    view: &View,
    spec: &ArgSpec,
    palette: &Palette,
    in_group: bool,
    group_required: bool,
) -> String {
    if spec.positional {
        let mut text = positional_usage_text(view, spec);
        if in_group && text.starts_with('[') && text.ends_with(']') {
            text = text[1..text.len() - 1].to_string();
        }
        return palette.paint(&palette.summary_action, &text);
    }
    let first = spec.flags.first().cloned().unwrap_or_default();
    let style = option_style(palette, &first);
    let body = if spec.action.takes_no_value() {
        if spec.action == ActionKind::BooleanOptional {
            let text = spec
                .flags
                .iter()
                .chain(spec.negative_flags.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ");
            palette.paint(style, &text)
        } else {
            palette.paint(style, &first)
        }
    } else {
        let metavar = metavar_for(view, spec);
        let suffix = value_usage(spec, &metavar);
        format!(
            "{style}{first} {}{suffix}{}",
            palette.summary_label, palette.reset
        )
    };
    if spec.required || group_required || in_group {
        body
    } else {
        format!("[{body}]")
    }
}

fn format_usage_text(view: &View) -> String {
    format_usage_text_with(view, &active_palette(view.color))
}

fn format_usage_text_with(view: &View, palette: &Palette) -> String {
    if let Some(usage) = &view.usage {
        let colored_prog = format!(
            "{}{}{}{}",
            palette.prog, view.prog, palette.reset, palette.prog_extra
        );
        let usage = usage.replace("%(prog)s", &colored_prog);
        return format!(
            "{}usage: {}{}{}{}\n",
            palette.usage, palette.reset, palette.prog_extra, usage, palette.reset
        );
    }
    let mut parts: Vec<String> = Vec::new();
    // Optionals first, in declaration order, with mutex groups collapsed.
    let mut rendered_groups: Vec<usize> = Vec::new();
    for spec in view.options.iter() {
        if spec.positional {
            continue;
        }
        let group_index = spec.group;
        if let Some(group) = view.groups.get(group_index) {
            if group.mutex {
                if rendered_groups.contains(&group_index) {
                    continue;
                }
                rendered_groups.push(group_index);
                let mut alternatives = Vec::new();
                for member in &group.members {
                    let member_spec = &view.options[*member];
                    alternatives.push(summary_usage_part(
                        view,
                        member_spec,
                        palette,
                        true,
                        group.required,
                    ));
                }
                let body = alternatives.join(" | ");
                parts.push(if group.required {
                    format!("({body})")
                } else {
                    format!("[{body}]")
                });
                continue;
            }
        }
        parts.push(summary_usage_part(view, spec, palette, false, false));
    }
    for spec in view.options.iter().filter(|spec| spec.positional) {
        // A positional already rendered as part of a mutex group is not
        // repeated later in the usage line.
        if view
            .groups
            .get(spec.group)
            .is_some_and(|group| group.mutex && rendered_groups.contains(&spec.group))
        {
            continue;
        }
        parts.push(summary_usage_part(view, spec, palette, false, false));
    }
    if let Some(subparsers) = &view.subparsers {
        parts.push(palette.paint(&palette.summary_action, &subparser_metavar(subparsers)));
    }
    let mut text = format!(
        "{}usage: {}{}{}{}",
        palette.usage, palette.reset, palette.prog, view.prog, palette.reset
    );
    for part in parts {
        text.push(' ');
        text.push_str(&part);
    }
    text.push('\n');
    text
}

fn subparser_metavar(subparsers: &SubparsersState) -> String {
    let label = subparsers.metavar.clone().unwrap_or_else(|| {
        let names: Vec<String> = subparsers
            .parsers
            .iter()
            .map(|(name, _, _)| name.clone())
            .collect();
        format!("{{{}}}", names.join(","))
    });
    format!("{label} ...")
}

fn help_invocation(view: &View, spec: &ArgSpec) -> String {
    if spec.positional {
        return positional_metavar(view, spec);
    }
    if spec.action == ActionKind::BooleanOptional {
        return spec
            .flags
            .iter()
            .chain(spec.negative_flags.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
    }
    if spec.action.takes_no_value() {
        return spec.flags.join(", ");
    }
    let metavar = metavar_for(view, spec);
    let suffix = value_usage(spec, &metavar);
    let options = spec.flags.join(", ");
    if suffix.is_empty() {
        options
    } else {
        format!("{options} {suffix}")
    }
}

/// ANSI style for an option spelling in the help listing: CPython uses the
/// bold variants there and the summary variants in usage lines.
fn help_option_style<'a>(palette: &'a Palette, flag: &str) -> &'a str {
    if flag.chars().count() > 2 {
        &palette.long_option
    } else {
        &palette.short_option
    }
}

fn help_invocation_colored(view: &View, spec: &ArgSpec, palette: &Palette) -> String {
    if spec.positional {
        return palette.paint(&palette.action, &positional_metavar(view, spec));
    }
    let paint_flag = |flag: &String| palette.paint(help_option_style(palette, flag), flag);
    if spec.action == ActionKind::BooleanOptional {
        return spec
            .flags
            .iter()
            .chain(spec.negative_flags.iter())
            .map(paint_flag)
            .collect::<Vec<_>>()
            .join(", ");
    }
    if spec.action.takes_no_value() {
        return spec
            .flags
            .iter()
            .map(paint_flag)
            .collect::<Vec<_>>()
            .join(", ");
    }
    let metavar = metavar_for(view, spec);
    let suffix = value_usage(spec, &metavar);
    let options = spec
        .flags
        .iter()
        .map(paint_flag)
        .collect::<Vec<_>>()
        .join(", ");
    if suffix.is_empty() {
        options
    } else {
        format!("{options} {}{suffix}{}", palette.label, palette.reset)
    }
}

fn help_text_for(view: &View, spec: &ArgSpec) -> String {
    if spec.help == SUPPRESS {
        return String::new();
    }
    let mut text = spec.help.clone();
    if view.allocator_formatter() && !text.is_empty() {
        if let Some(default) = effective_default(view, spec) {
            text.push_str(&format!(" (default: {})", value::to_string(default)));
        }
    }
    text
}

fn effective_default(view: &View, spec: &ArgSpec) -> Option<i64> {
    if let Some((_, default)) = view
        .defaults
        .iter()
        .find(|(name, _)| name.as_str() == spec.dest)
    {
        return Some(*default);
    }
    if value::as_str(spec.default) == Some(SUPPRESS) {
        return None;
    }
    Some(spec.default)
}

struct HelpEntry {
    invoked: String,
    invoked_colored: String,
    help: String,
    indent: usize,
}

impl HelpEntry {
    fn new(view: &View, spec: &ArgSpec, palette: &Palette, indent: usize) -> HelpEntry {
        HelpEntry {
            invoked: help_invocation(view, spec),
            invoked_colored: help_invocation_colored(view, spec, palette),
            help: help_text_for(view, spec),
            indent,
        }
    }
}

/// Layout parameters CPython's `HelpFormatter` reads from its instance.
#[derive(Clone, Copy)]
struct FormatterLayout {
    indent_increment: usize,
    max_help_position: usize,
    width: usize,
}

impl Default for FormatterLayout {
    fn default() -> Self {
        Self {
            indent_increment: 2,
            max_help_position: 24,
            width: DEFAULT_FORMATTER_WIDTH as usize,
        }
    }
}

/// Instantiates `formatter_class(prog)` and copies its layout parameters.
///
/// `view.formatter_class` is replaced with the instance's class, so behaviour
/// (`RawTextHelpFormatter`, `ArgumentDefaultsHelpFormatter`, ...) follows
/// subclasses and callables that return a formatter, exactly as CPython reads
/// the overridden methods from the constructed formatter.
fn resolve_formatter_layout(view: &mut View) -> FormatterLayout {
    let mut layout = FormatterLayout::default();
    if view.formatter_class == NONE {
        return layout;
    }
    let prog = text_value(&view.prog);
    let roots = crate::heap::root_values(std::slice::from_ref(&prog));
    let instance = crate::core::builtins::call(view.formatter_class, 1, [prog].as_ptr());
    let instance_roots = crate::heap::root_values(std::slice::from_ref(&instance));
    crate::heap::release_roots(roots);
    if exc::pending() {
        crate::heap::release_roots(instance_roots);
        return layout;
    }
    if let Some(state) = value::as_instance(instance) {
        view.formatter_class = state.class;
    }
    layout.indent_increment =
        formatter_layout_attr(instance, "_indent_increment", layout.indent_increment);
    if !exc::pending() {
        layout.max_help_position =
            formatter_layout_attr(instance, "_max_help_position", layout.max_help_position);
    }
    if !exc::pending() {
        layout.width = formatter_layout_attr(instance, "_width", layout.width);
    }
    crate::heap::release_roots(instance_roots);
    layout
}

/// Reads a non-negative integer layout attribute, falling back to `default`
/// when the formatter does not define it.
fn formatter_layout_attr(instance: i64, name: &str, default: usize) -> usize {
    let attribute = crate::core::methods::getattr(instance, name.as_ptr(), name.len());
    if exc::pending() {
        if exc::pending_tag() == i64::from(ATTRIBUTE_ERROR) {
            exc::clear();
        }
        return default;
    }
    match value::as_integer(attribute) {
        Some(number) => number.max(0) as usize,
        None => default,
    }
}

fn format_help_text(view: &View) -> String {
    let mut resolved = view.clone();
    let layout = resolve_formatter_layout(&mut resolved);
    if exc::pending() {
        return String::new();
    }
    let view = &resolved;
    let palette = active_palette(view.color);
    let indent = layout.indent_increment;
    let mut sections: Vec<(String, Vec<HelpEntry>)> = Vec::new();

    // Positional arguments (including the subparsers action).
    let mut positional_entries = Vec::new();
    for spec in view.options.iter().filter(|spec| spec.positional) {
        if spec.help == SUPPRESS {
            continue;
        }
        positional_entries.push(HelpEntry::new(view, spec, &palette, indent));
    }
    if let Some(subparsers) = &view.subparsers {
        positional_entries.push(HelpEntry {
            invoked: subparser_metavar(subparsers),
            invoked_colored: palette.paint(&palette.action, &subparser_metavar(subparsers)),
            help: String::new(),
            indent,
        });
        for (name, _, help) in &subparsers.parsers {
            // CPython lists only choices that declare a help text.
            if help.is_empty() || help == SUPPRESS {
                continue;
            }
            positional_entries.push(HelpEntry {
                invoked: name.clone(),
                invoked_colored: palette.paint(&palette.action, name),
                help: help.clone(),
                indent: indent.saturating_mul(2),
            });
        }
    }
    if !positional_entries.is_empty() {
        sections.push(("positional arguments".to_string(), positional_entries));
    }

    // Options by display group.
    // The default group first.
    let mut default_entries = Vec::new();
    for spec in view.options.iter().filter(|spec| !spec.positional) {
        if view.display_group(spec) != 0 {
            continue;
        }
        if spec.help == SUPPRESS {
            continue;
        }
        default_entries.push(HelpEntry::new(view, spec, &palette, indent));
    }
    if !default_entries.is_empty() {
        sections.push(("options".to_string(), default_entries));
    }
    for (index, group) in view.groups.iter().enumerate() {
        if index == 0 || group.mutex {
            continue;
        }
        let mut entries = Vec::new();
        for spec in view.options.iter() {
            if spec.positional || view.display_group(spec) != index {
                continue;
            }
            if spec.help == SUPPRESS {
                continue;
            }
            entries.push(HelpEntry::new(view, spec, &palette, indent));
        }
        if !entries.is_empty() {
            let title = group.title.clone().unwrap_or_else(|| "options".to_string());
            sections.push((title, entries));
        }
    }

    let max_invocation = sections
        .iter()
        .flat_map(|(_, entries)| entries.iter())
        .map(|entry| entry.indent + entry.invoked.chars().count())
        .max()
        .unwrap_or(0);
    let help_position = (max_invocation + 2).min(layout.max_help_position);

    let mut text = format_usage_text_with(view, &palette);
    if let Some(description) = &view.description {
        text.push('\n');
        if view.raw_text() || view.raw_description() {
            text.push_str(description);
        } else {
            text.push_str(&wrap_text(description, layout.width.max(11), 0));
        }
        text.push('\n');
    }
    for (title, entries) in sections {
        text.push('\n');
        text.push_str(&palette.heading);
        text.push_str(&title);
        text.push(':');
        text.push_str(&palette.reset);
        text.push('\n');
        for entry in entries {
            let header = format!("{}{}", " ".repeat(entry.indent), entry.invoked);
            let colored_header = format!("{}{}", " ".repeat(entry.indent), entry.invoked_colored);
            if entry.help.is_empty() {
                text.push_str(&colored_header);
                text.push('\n');
                continue;
            }
            if header.chars().count() + 2 <= help_position {
                text.push_str(&colored_header);
                let padding = help_position.saturating_sub(header.chars().count());
                text.push_str(&" ".repeat(padding.max(2)));
            } else {
                text.push_str(&colored_header);
                text.push('\n');
                text.push_str(&" ".repeat(help_position));
            }
            let width = layout.width.saturating_sub(help_position).max(11);
            let wrapped = if view.raw_text() {
                // `RawTextHelpFormatter._split_lines` returns the lines as
                // written instead of re-filling them.
                entry.help.lines().collect::<Vec<_>>().join("\n")
            } else {
                wrap_text(&entry.help, width, 0)
            };
            for (line_index, line) in wrapped.split('\n').enumerate() {
                if line_index > 0 {
                    text.push('\n');
                    text.push_str(&" ".repeat(help_position));
                }
                text.push_str(line);
            }
            text.push('\n');
        }
    }
    if let Some(epilog) = &view.epilog {
        text.push('\n');
        if view.raw_text() || view.raw_description() {
            text.push_str(epilog);
        } else {
            text.push_str(&wrap_text(epilog, layout.width.max(11), 0));
        }
        text.push('\n');
    }
    text
}

/// Greedy word wrapping used for descriptions and help text. Existing
/// newlines start a new paragraph line.
fn wrap_text(text: &str, width: usize, indent: usize) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut line = String::new();
    let mut first = true;
    for paragraph in text.split('\n') {
        if !first {
            if !line.is_empty() {
                out.push_str(&line);
            }
            out.push('\n');
            line.clear();
        }
        first = false;
        for word in paragraph.split_whitespace() {
            if line.is_empty() {
                line.push_str(&" ".repeat(indent));
                line.push_str(word);
            } else if line.chars().count() + 1 + word.chars().count() <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push_str(&line);
                out.push('\n');
                line.clear();
                line.push_str(&" ".repeat(indent));
                line.push_str(word);
            }
        }
    }
    if !line.is_empty() {
        out.push_str(&line);
    }
    out
}

// -------------------------------------------------------------------------
// Attribute access
// -------------------------------------------------------------------------

/// Data attributes of an `ArgumentParser`.
pub(crate) fn parser_attr(obj: i64, attr: &str) -> Option<i64> {
    let state = parser_ref(obj)?;
    match attr {
        "prog" => Some(match &state.prog {
            Some(prog) => text_value(prog),
            None => text_value(&default_prog()),
        }),
        "usage" => Some(match &state.usage {
            Some(usage) => text_value(usage),
            None => NONE,
        }),
        "description" => Some(match &state.description {
            Some(description) => text_value(description),
            None => NONE,
        }),
        "epilog" => Some(match &state.epilog {
            Some(epilog) => text_value(epilog),
            None => NONE,
        }),
        "formatter_class" => Some(state.formatter_class),
        "prefix_chars" => Some(text_value(&state.prefix_chars)),
        "fromfile_prefix_chars" => Some(match &state.fromfile_prefix_chars {
            Some(chars) => text_value(chars),
            None => NONE,
        }),
        "argument_default" => Some(state.argument_default),
        "conflict_handler" => Some(text_value(&state.conflict_handler)),
        "add_help" => Some(if state.add_help {
            value::TRUE
        } else {
            value::FALSE
        }),
        "allow_abbrev" => Some(if state.allow_abbrev {
            value::TRUE
        } else {
            value::FALSE
        }),
        "exit_on_error" => Some(if state.exit_on_error {
            value::TRUE
        } else {
            value::FALSE
        }),
        "suggest_on_error" => Some(if state.suggest_on_error {
            value::TRUE
        } else {
            value::FALSE
        }),
        "color" => Some(if state.color {
            value::TRUE
        } else {
            value::FALSE
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dest_prefers_the_long_flag() {
        assert_eq!(
            dest_for(&["-n".to_string(), "--num".to_string()], "-"),
            "num"
        );
        assert_eq!(dest_for(&["-x".to_string()], "-"), "x");
        assert_eq!(dest_for(&["--dry-run".to_string()], "-"), "dry_run");
    }

    #[test]
    fn negative_numbers_are_positional_unless_an_option_uses_them() {
        let mut state = ArgParserState::new(Some("demo".to_string()), None);
        state.options.push(ArgSpec {
            flags: vec!["--n".to_string()],
            negative_flags: Vec::new(),
            dest: "n".to_string(),
            positional: false,
            action: ActionKind::Store,
            type_fn: NONE,
            default: NONE,
            const_value: NONE,
            required: false,
            help: String::new(),
            nargs: Nargs::Single,
            choices: None,
            metavar: None,
            group: 0,
            version: String::new(),
        });
        let parser = value::arg_parser_new(state);
        let view = view(parser).expect("view");
        assert!(negative_number_allowed(&view));
        assert!(!note_option_like("-5", &view));
        assert!(note_option_like("--n", &view));
    }

    #[test]
    fn argument_parser_accepts_omitted_usage() {
        exc::init();
        // `usage` is optional and defaults to None; the usage line is derived
        // from `prog` and the `-h` action.
        let parser = new_parser(&[text_value("demo")]);
        assert!(!exc::pending());
        let state = parser_ref(parser).expect("parser");
        assert!(state.usage.is_none());
        assert_eq!(state.prog.as_deref(), Some("demo"));
        assert!(state.add_help);
        assert!(state.allow_abbrev);
        assert!(state.exit_on_error);
        assert!(state.color);
        assert_eq!(state.prefix_chars, "-");
        assert_eq!(state.conflict_handler, "error");
        assert_eq!(format_usage(parser), "usage: demo [-h]\n");

        // No arguments at all is equally valid.
        let parser = new_parser(&[]);
        assert!(!exc::pending());
        let state = parser_ref(parser).expect("parser");
        assert!(state.usage.is_none());
        assert!(state.prog.is_some());
    }

    #[test]
    fn argument_parser_accepts_every_documented_keyword() {
        exc::init();
        let mut slots = vec![NONE; 15];
        slots[0] = text_value("demo");
        slots[1] = text_value("%(prog)s [opts]");
        slots[2] = text_value("description");
        slots[3] = text_value("epilog");
        slots[5] = NONE; // formatter_class
        slots[6] = text_value("-+");
        slots[7] = text_value("@");
        slots[8] = NONE; // argument_default
        slots[9] = text_value("resolve");
        slots[10] = value::FALSE; // add_help
        slots[11] = value::FALSE; // allow_abbrev
        slots[12] = value::FALSE; // exit_on_error
        slots[13] = value::FALSE; // suggest_on_error
        slots[14] = value::FALSE; // color
        let parser = new_parser(&slots);
        assert!(!exc::pending());
        let state = parser_ref(parser).expect("parser");
        assert_eq!(state.usage.as_deref(), Some("%(prog)s [opts]"));
        assert_eq!(state.description.as_deref(), Some("description"));
        assert_eq!(state.epilog.as_deref(), Some("epilog"));
        assert_eq!(state.prefix_chars, "-+");
        assert_eq!(state.fromfile_prefix_chars.as_deref(), Some("@"));
        assert_eq!(state.conflict_handler, "resolve");
        assert!(!state.add_help);
        assert!(!state.allow_abbrev);
        assert!(!state.exit_on_error);
        assert!(!state.color);
        assert_eq!(format_usage(parser), "usage: demo [opts]\n");
        // With add_help disabled the help section is absent.
        assert!(!format_help(parser).contains("-h, --help"));
        // The parsed attributes mirror the constructor values.
        assert_eq!(
            value::as_str(parser_attr(parser, "usage").expect("usage")),
            Some("%(prog)s [opts]")
        );
    }
}
