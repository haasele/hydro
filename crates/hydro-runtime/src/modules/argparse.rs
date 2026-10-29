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
use crate::exc::{self, ATTRIBUTE_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::methods::KeywordArgs;
use crate::value::{self, NONE};

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
    /// CPython 3.14's `color=` constructor keyword. Help output has no ANSI
    /// styling in Hydro, so the flag is stored for introspection only.
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
// Module functions and constants
// -------------------------------------------------------------------------

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "ArgumentParser" => new_parser(args),
        "Namespace" => namespace_call(args, &[]),
        "FileType" => file_type(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'argparse' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware module call: only `Namespace(**kwargs)` takes keywords.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
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
    value::namespace_new(
        keywords
            .iter()
            .map(|(name, item)| (name.clone(), *item))
            .collect(),
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
        "RawTextHelpFormatter"
        | "RawDescriptionHelpFormatter"
        | "ArgumentDefaultsHelpFormatter"
        | "MetavarTypeHelpFormatter"
        | "HelpFormatter"
        | "Action"
        | "BooleanOptionalAction" => value::class_new(name.to_string(), NONE, Vec::new()),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'argparse' has no attribute '{name}'"),
        ),
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
    let arg = |index: usize| args.get(index).copied().unwrap_or(NONE);
    let prog = match opt_str(arg(0), "prog must be str or None") {
        Ok(prog) => prog,
        Err(err) => return err,
    };
    let usage = match opt_str(arg(1), "usage must be str or None") {
        Ok(usage) => usage,
        Err(err) => return err,
    };
    let description = match opt_str(arg(2), "description must be str or None") {
        Ok(description) => description,
        Err(err) => return err,
    };
    let epilog = match opt_str(arg(3), "epilog must be str or None") {
        Ok(epilog) => epilog,
        Err(err) => return err,
    };
    let mut prefix_chars = match opt_str(arg(6), "prefix_chars must be str or None") {
        Ok(Some(text)) => text,
        Ok(None) => "-".to_string(),
        Err(err) => return err,
    };
    if prefix_chars.is_empty() {
        return value_error("prefix_chars must not be empty");
    }
    let fromfile_prefix_chars = match opt_str(arg(7), "fromfile_prefix_chars must be str or None") {
        Ok(chars) => chars,
        Err(err) => return err,
    };
    let conflict_handler = match opt_str(arg(9), "conflict_handler must be str or None") {
        Ok(Some(handler)) => handler,
        Ok(None) => "error".to_string(),
        Err(err) => return err,
    };
    if conflict_handler != "error" && conflict_handler != "resolve" {
        return value_error(&format!("unknown conflict_handler '{conflict_handler}'"));
    }

    // The help action's SUPPRESS default is the only tagged value created
    // here; keep it alive until the parser object owns it.
    let roots = value::list_new(Vec::new());
    let suppress = text_value(SUPPRESS);
    root_value(roots, suppress);

    let mut state = ArgParserState::new(prog.clone(), description);
    state.usage = usage;
    state.epilog = epilog;
    state.formatter_class = arg(5);
    state.prefix_chars = std::mem::take(&mut prefix_chars);
    state.fromfile_prefix_chars = fromfile_prefix_chars;
    state.argument_default = arg(8);
    state.conflict_handler = conflict_handler;
    state.add_help = opt_bool(arg(10), true);
    state.allow_abbrev = opt_bool(arg(11), true);
    state.exit_on_error = opt_bool(arg(12), true);
    state.color = opt_bool(arg(13), true);
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
            return type_error("parents must be a list or tuple of ArgumentParser objects");
        };
        for parent in entries {
            let Some(parent_state) = value::as_arg_parser(parent) else {
                return type_error("parents entries must be ArgumentParser objects");
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

    value::arg_parser_new(state)
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
    if value::as_arg_parser(obj).is_none() {
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
    if value::as_arg_parser(obj).is_none() {
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
    if let Some(mut state) = value::arg_parser_mut(parser) {
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
    let Some(state) = value::as_arg_parser(parser) else {
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
        let Some(mut parser_state) = value::arg_parser_mut(parser) else {
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
        crate::heap::write_barrier_edge(parser, spec.type_fn);
    }
    if spec.default != NONE {
        crate::heap::write_barrier_edge(parser, spec.default);
    }
    if spec.const_value != NONE {
        crate::heap::write_barrier_edge(parser, spec.const_value);
    }
    if let Some(items) = choices {
        for item in items {
            crate::heap::write_barrier_edge(parser, item);
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
        let Some(mut state) = value::arg_parser_mut(parser) else {
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
    if value::as_arg_parser(parser).is_none() {
        return attribute_error("add_subparsers");
    }
    if value::as_arg_parser(parser).is_some_and(|state| state.subparsers.is_some()) {
        return type_error("cannot have multiple subparser arguments");
    }
    let Some(mut state) = value::arg_parser_mut(parser) else {
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
    let mut slots = vec![NONE; 14];
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
            "color" => slots[13] = *argument,
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
    let Some(parent) = value::as_arg_parser(parser) else {
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
        let Some(mut state) = value::arg_parser_mut(parser) else {
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
    crate::heap::write_barrier_edge(parser, child);
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
    if let Some(mut state) = value::arg_parser_mut(parser) {
        for (name, item) in &updates {
            upsert(&mut state.defaults, name, *item);
        }
    }
    for (_, item) in updates {
        if item != NONE {
            crate::heap::write_barrier_edge(parser, item);
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
    let Some(state) = value::as_arg_parser(parser) else {
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
struct View {
    prog: String,
    usage: Option<String>,
    description: Option<String>,
    epilog: Option<String>,
    formatter_name: Option<String>,
    prefix_chars: String,
    fromfile_prefix_chars: Option<String>,
    allow_abbrev: bool,
    exit_on_error: bool,
    options: Vec<ArgSpec>,
    groups: Vec<GroupState>,
    subparsers: Option<SubparsersState>,
    defaults: Vec<(String, i64)>,
    test_mode: bool,
}

impl View {
    fn allocator_formatter(&self) -> bool {
        matches!(
            self.formatter_name.as_deref(),
            Some("ArgumentDefaultsHelpFormatter")
        )
    }

    fn metavar_formatter(&self) -> bool {
        matches!(
            self.formatter_name.as_deref(),
            Some("MetavarTypeHelpFormatter")
        )
    }

    fn raw_description(&self) -> bool {
        matches!(
            self.formatter_name.as_deref(),
            Some("RawTextHelpFormatter") | Some("RawDescriptionHelpFormatter")
        )
    }

    fn raw_text(&self) -> bool {
        matches!(self.formatter_name.as_deref(), Some("RawTextHelpFormatter"))
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
    let state = value::as_arg_parser(parser)?;
    Some(View {
        prog: state.prog.clone().unwrap_or_else(default_prog),
        usage: state.usage.clone(),
        description: state.description.clone(),
        epilog: state.epilog.clone(),
        formatter_name: value::class_display_name(state.formatter_class).map(str::to_string),
        prefix_chars: state.prefix_chars.clone(),
        fromfile_prefix_chars: state.fromfile_prefix_chars.clone(),
        allow_abbrev: state.allow_abbrev,
        exit_on_error: state.exit_on_error,
        options: state.options.clone(),
        groups: state.groups.clone(),
        subparsers: state.subparsers.clone(),
        defaults: state.defaults.clone(),
        test_mode: state.test_mode,
    })
}

/// Reports a parse error: prints the usage block and raises
/// `SystemExit(2)` (or the legacy exception in test mode).
fn fail(_parser: i64, view: &View, tag: u32, message: &str) -> i64 {
    if view.test_mode {
        return exc::raise_with_message(tag, message);
    }
    if !view.exit_on_error {
        return exc::raise_with_message(VALUE_ERROR, message);
    }
    print_usage_to_stderr(view);
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "{}: error: {}", view.prog, message);
    let _ = stderr.flush();
    exc::raise_exit(tagged_int(2))
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
    exc::raise_exit(status)
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
        return value::namespace_new(pairs.to_vec());
    }
    if value::as_namespace(namespace).is_none() {
        return type_error(&format!(
            "namespace must be a Namespace, not {}",
            value::type_name(namespace)
        ));
    }
    if let Some(mut state) = value::namespace_pairs_mut(namespace) {
        for (name, item) in pairs {
            upsert(&mut state.pairs, name, *item);
        }
    }
    for (_, item) in pairs {
        crate::heap::write_barrier_edge(namespace, *item);
    }
    namespace
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

fn optional_usage(view: &View, spec: &ArgSpec) -> String {
    let metavar = metavar_for(view, spec);
    let mut body = if spec.action.takes_no_value() {
        spec.flags.first().cloned().unwrap_or_default()
    } else {
        let suffix = value_usage(spec, &metavar);
        match spec.flags.first() {
            Some(flag) if suffix.is_empty() => flag.clone(),
            Some(flag) => format!("{flag} {suffix}"),
            None => suffix,
        }
    };
    if spec.action == ActionKind::BooleanOptional {
        let mut alternatives = Vec::new();
        for flag in spec.flags.iter().chain(spec.negative_flags.iter()) {
            alternatives.push(flag.clone());
        }
        body = alternatives.join(" | ");
    }
    if spec.required {
        body
    } else {
        format!("[{body}]")
    }
}

fn format_usage_text(view: &View) -> String {
    if let Some(usage) = &view.usage {
        let usage = usage.replace("%(prog)s", &view.prog);
        return format!("usage: {usage}\n");
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
                    let metavar = metavar_for(view, member_spec);
                    let mut body = if member_spec.action.takes_no_value() {
                        member_spec.flags.first().cloned().unwrap_or_default()
                    } else {
                        let suffix = value_usage(member_spec, &metavar);
                        format!(
                            "{} {suffix}",
                            member_spec.flags.first().cloned().unwrap_or_default()
                        )
                    };
                    if member_spec.action == ActionKind::BooleanOptional {
                        body = member_spec
                            .flags
                            .iter()
                            .chain(member_spec.negative_flags.iter())
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" | ");
                    }
                    alternatives.push(body);
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
        parts.push(optional_usage(view, spec));
    }
    for spec in view.options.iter().filter(|spec| spec.positional) {
        let metavar = positional_metavar(view, spec);
        let text = match spec.nargs {
            Nargs::Single => metavar,
            Nargs::Optional => format!("[{metavar}]"),
            Nargs::ZeroOrMore => format!("[{metavar} ...]"),
            Nargs::OneOrMore => format!("{metavar} [{metavar} ...]"),
            Nargs::Exact(count) => vec![metavar; count].join(" "),
        };
        parts.push(text);
    }
    if let Some(subparsers) = &view.subparsers {
        parts.push(subparser_metavar(subparsers));
    }
    let mut text = format!("usage: {}", view.prog);
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
    spec.flags
        .iter()
        .map(|flag| {
            if suffix.is_empty() {
                flag.clone()
            } else {
                format!("{flag} {suffix}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
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
    help: String,
    indent: usize,
}

fn format_help_text(view: &View) -> String {
    let mut sections: Vec<(String, Vec<HelpEntry>)> = Vec::new();

    // Positional arguments (including the subparsers action).
    let mut positional_entries = Vec::new();
    for spec in view.options.iter().filter(|spec| spec.positional) {
        if spec.help == SUPPRESS {
            continue;
        }
        positional_entries.push(HelpEntry {
            invoked: help_invocation(view, spec),
            help: help_text_for(view, spec),
            indent: 2,
        });
    }
    if let Some(subparsers) = &view.subparsers {
        positional_entries.push(HelpEntry {
            invoked: subparser_metavar(subparsers),
            help: String::new(),
            indent: 2,
        });
        for (name, _, help) in &subparsers.parsers {
            // CPython lists only choices that declare a help text.
            if help.is_empty() || help == SUPPRESS {
                continue;
            }
            positional_entries.push(HelpEntry {
                invoked: name.clone(),
                help: help.clone(),
                indent: 4,
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
        default_entries.push(HelpEntry {
            invoked: help_invocation(view, spec),
            help: help_text_for(view, spec),
            indent: 2,
        });
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
            entries.push(HelpEntry {
                invoked: help_invocation(view, spec),
                help: help_text_for(view, spec),
                indent: 2,
            });
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
    let help_position = (max_invocation + 2).min(24);

    let mut text = format_usage_text(view);
    if let Some(description) = &view.description {
        text.push('\n');
        if view.raw_text() || view.raw_description() {
            text.push_str(description);
        } else {
            text.push_str(&wrap_text(description, 80, 0));
        }
        text.push('\n');
    }
    for (title, entries) in sections {
        text.push('\n');
        text.push_str(&title);
        text.push_str(":\n");
        for entry in entries {
            let header = format!("{}{}", " ".repeat(entry.indent), entry.invoked);
            if entry.help.is_empty() {
                text.push_str(&header);
                text.push('\n');
                continue;
            }
            if header.chars().count() + 2 <= help_position {
                text.push_str(&header);
                let padding = help_position.saturating_sub(header.chars().count());
                text.push_str(&" ".repeat(padding.max(2)));
            } else {
                text.push_str(&header);
                text.push('\n');
                text.push_str(&" ".repeat(help_position));
            }
            let width = 80usize.saturating_sub(help_position).max(11);
            let wrapped = wrap_text(&entry.help, width, 0);
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
            text.push_str(&wrap_text(epilog, 80, 0));
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
    let state = value::as_arg_parser(obj)?;
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
        let state = value::as_arg_parser(parser).expect("parser");
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
        let state = value::as_arg_parser(parser).expect("parser");
        assert!(state.usage.is_none());
        assert!(state.prog.is_some());
    }

    #[test]
    fn argument_parser_accepts_every_documented_keyword() {
        exc::init();
        let mut slots = vec![NONE; 14];
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
        slots[13] = value::FALSE; // color
        let parser = new_parser(&slots);
        assert!(!exc::pending());
        let state = value::as_arg_parser(parser).expect("parser");
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
