#![allow(dead_code, clippy::question_mark, clippy::nonminimal_bool)]
//! `warnings` / `_warnings`: warning categories, filters, registries and the
//! standard display helpers.
//!
//! The registry is the CPython one: every warning category is a native
//! exception class from [`crate::exc`] ([`crate::exc::WARNING`] and its
//! subclasses), filters are `(action, message, category, module, lineno)`
//! entries evaluated in order with the first match winning, and `default` /
//! `module` / `once` warnings are recorded so they are shown only once per
//! location/module/process (per caller-supplied registry for the
//! `warn_explicit` form).
//!
//! Implemented here:
//!
//! * `warn`, `warn_explicit`, `filterwarnings`, `simplefilter`,
//!   `resetwarnings`, `showwarning`, `formatwarning`, plus the `filters`
//!   snapshot list;
//! * `catch_warnings(...)` as a real context manager: entering snapshots the
//!   filters, `record=True` installs an `always` filter and returns a list
//!   that receives one `WarningMessage`-shaped object per emitted warning,
//!   and exiting restores the snapshot (nested managers are independent);
//! * the private `_deprecated` helper used by `annotationlib` (and the rest
//!   of the 3.14 standard library): it validates its required arguments,
//!   formats the removal message and emits a `DeprecationWarning`;
//! * the public `warnings.deprecated` decorator class (PEP 702): construction
//!   validates `message`/`category`/`stacklevel`, and decorating records
//!   `__deprecated__` on the target. Hydro native functions have no
//!   per-decoration call slot, so call-time warning emission is the documented
//!   limitation instead of being simulated at the wrong time (see below);
//! * the CPython default filters (`__main__` deprecations shown, the usual
//!   `DeprecationWarning` / `PendingDeprecationWarning` / `ImportWarning` /
//!   `ResourceWarning` categories ignored);
//! * duplicate-filter elimination and the `append` flag, exactly like
//!   `warnings._add_filter`;
//! * the `"all"` spelling accepted as an alias of `"always"`.
//!
//! Documented limits (reported instead of silently ignored):
//!
//! * There is no Python frame stack accessible to native modules, so
//!   `warn(stacklevel=...)` cannot select a caller location: the location is
//!   always reported as `<unknown>:0`. `warn_explicit` takes the location
//!   explicitly and behaves like CPython.
//! * `warn(..., skip_file_prefixes=(...))` is accepted and validated exactly
//!   like CPython 3.14 (a tuple of `str`; anything else raises `TypeError`),
//!   but the prefixes filter candidate caller frames, and Hydro has none to
//!   filter. Only the `stacklevel`-based attribution remains, and that
//!   attribution already reports `<unknown>:0`; no filename is fabricated
//!   from a prefix.
//! * `message`/`module` filters are matched as case-insensitive literal
//!   prefixes at the start of the text, which is the behaviour of the
//!   `re.I`-compiled `re.match` CPython uses for the common patterns; full
//!   regular expressions are not compiled here.
//! * `warnings.showwarning` is a native function, not a rebindable module
//!   attribute (native modules have no writable attributes in Hydro).
//!   `catch_warnings(record=True)` does not rebind it either: the recording
//!   list is consulted directly by the emission path, which is observationally
//!   equivalent for callers that use the list returned by `__enter__`.
//! * `catch_warnings(module=...)` accepts only the real `warnings` module
//!   (or `None`); substituting another module object needs a Python-level
//!   filter stack Hydro does not have and reports `NotImplementedError`.
//! * `warnings.deprecated` (`_py_warnings.deprecated`) stores and validates
//!   its `category`/`stacklevel` fields and implements the PEP 702 identity
//!   decoration (`__deprecated__`, `category=None`). Wrapping a callable or a
//!   class so a warning fires on each call/instantiation/subclass creation is
//!   not possible for native function objects (they have no per-decoration
//!   call slot); no warning is fabricated at decoration time instead.
//!
//! The filter stack and both once-registries live behind one mutex, so
//! concurrent warnings from `_thread` workers are serialized and see one
//! consistent registry. Filters store the warning *tag*, not the class value:
//! a filter may outlive the thread that installed it, and a raw class value
//! kept only in Rust-side state could be collected with that thread's roots.
//! `filters` therefore reports the built-in warning class for the tag (a
//! user-defined subclass is projected onto the built-in ancestor it derives
//! from); matching itself still uses the full class hierarchy of the warning
//! being emitted.

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_ir::ParamKind;

use crate::container;
use crate::exc::{
    self, ATTRIBUTE_ERROR, DEPRECATION_WARNING, NOT_IMPLEMENTED_ERROR, PENDING_DEPRECATION_WARNING,
    RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR, WARNING,
};
use crate::heap;
use crate::methods;
use crate::ops;
use crate::value::{self, NONE};

/// The recognized filter actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Default,
    Error,
    Ignore,
    Always,
    Module,
    Once,
}

impl Action {
    fn parse(text: &str) -> Option<Action> {
        Some(match text {
            "default" => Action::Default,
            "error" => Action::Error,
            "ignore" => Action::Ignore,
            "always" | "all" => Action::Always,
            "module" => Action::Module,
            "once" => Action::Once,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Action::Default => "default",
            Action::Error => "error",
            Action::Ignore => "ignore",
            Action::Always => "always",
            Action::Module => "module",
            Action::Once => "once",
        }
    }
}

/// One installed filter. `message`/`module` are lower-case literal prefixes
/// (empty matches everything); `category_tag` is the warning tag of the
/// category, so subclasses match their ancestors. Only the tag is stored: a
/// filter may outlive the thread that installed it, and a raw class value
/// kept in Rust-side state could be collected with that thread's roots.
struct Filter {
    action: Action,
    message: String,
    category_tag: u32,
    module: String,
    lineno: i64,
}

impl Filter {
    fn same_as(&self, other: &Filter) -> bool {
        self.action == other.action
            && self.message == other.message
            && self.category_tag == other.category_tag
            && self.module == other.module
            && self.lineno == other.lineno
    }
}

/// Key of the per-call registry CPython writes into `__warningregistry__`.
type RegistryKey = (String, u32, i64);

/// One active `catch_warnings(record=True)` block. The list is pinned with a
/// persistent root so warnings emitted from another thread cannot lose it.
#[derive(Clone, Copy)]
struct RecordTarget {
    list: i64,
    root: usize,
}

struct State {
    /// Heap generation the state was built for; a runtime reset (which frees
    /// the heap and clears every collector root) invalidates the filters'
    /// cached class values.
    generation: u64,
    filters: Vec<Filter>,
    /// Innermost recording target; nested `record=True` blocks push, exit
    /// pops. An outer target stays visible to a nested `record=False` block,
    /// matching CPython's shared `_showwarnmsg_impl` hook.
    record_stack: Vec<RecordTarget>,
    /// Bumped by every filter mutation; external registries compare it.
    version: u64,
    /// `default` and `module` records for `warn` (no external registry).
    registry: HashSet<RegistryKey>,
    /// `once` records for `warn` (no external registry).
    once: HashSet<(String, u32)>,
}

impl State {
    fn new(generation: u64) -> State {
        let mut state = State {
            generation,
            filters: Vec::new(),
            record_stack: Vec::new(),
            version: 0,
            registry: HashSet::new(),
            once: HashSet::new(),
        };
        // `_py_warnings._setup_defaults` in CPython 3.14.
        state.filters.push(Filter {
            action: Action::Default,
            message: String::new(),
            category_tag: exc::DEPRECATION_WARNING,
            module: "__main__".to_string(),
            lineno: 0,
        });
        for tag in [
            exc::DEPRECATION_WARNING,
            exc::PENDING_DEPRECATION_WARNING,
            exc::IMPORT_WARNING,
            exc::RESOURCE_WARNING,
        ] {
            state.filters.push(Filter {
                action: Action::Ignore,
                message: String::new(),
                category_tag: tag,
                module: String::new(),
                lineno: 0,
            });
        }
        state.version = 1;
        state
    }

    fn mutated(&mut self) {
        self.version = self.version.wrapping_add(1);
        self.registry.clear();
        self.once.clear();
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::new(0)));

/// Locks the warning state, rebuilding it after a runtime reset.
fn state() -> MutexGuard<'static, State> {
    let mut guard = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = heap::generation();
    if guard.generation != generation {
        *guard = State::new(generation);
    }
    guard
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn attribute_error(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'warnings' has no attribute '{name}'"),
    )
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

/// The warning tag of `category` (a class, possibly a user subclass of a
/// built-in warning), or `None` when `category` is not a class at all.
fn category_tag(category: i64) -> Option<u32> {
    let mut current = category;
    for _ in 0..1024 {
        if let Some(tag) = exc::exception_tag_of_class(current) {
            return Some(tag);
        }
        let state = value::as_class(current)?;
        if state.base == NONE {
            return None;
        }
        current = state.base;
    }
    None
}

/// Validates a `category` argument and returns the cached native class for its
/// tag. `NONE` selects `default_tag` (CPython treats `None` as the default).
fn warning_category(category: i64, default_tag: u32) -> Option<u32> {
    if category == NONE {
        return Some(default_tag);
    }
    let tag = category_tag(category)?;
    if !exc::tag_is_subclass(tag, WARNING) {
        return None;
    }
    Some(tag)
}

/// Copy of a heap string as an owned Rust `String`.
fn string_arg(value: i64) -> Option<String> {
    value::as_str(value).map(str::to_string)
}

/// Whether `text` starts with the filter's case-insensitive literal prefix.
fn prefix_matches(pattern: &str, text: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    let mut pattern_chars = pattern.chars().flat_map(char::to_lowercase);
    let mut text_chars = text.chars().flat_map(char::to_lowercase);
    loop {
        match pattern_chars.next() {
            None => return true,
            Some(expected) => match text_chars.next() {
                Some(actual) if actual == expected => {}
                _ => return false,
            },
        }
    }
}

/// Selects the action for one warning; `None` means no filter matched and the
/// CPython default action applies.
fn select_action(
    filters: &[Filter],
    text: &str,
    tag: u32,
    module: &str,
    lineno: i64,
) -> Option<Action> {
    filters
        .iter()
        .find(|filter| {
            prefix_matches(&filter.message, text)
                && exc::tag_is_subclass(tag, filter.category_tag)
                && prefix_matches(&filter.module, module)
                && (filter.lineno == 0 || filter.lineno == lineno)
        })
        .map(|filter| filter.action)
}

fn int_arg(value: i64) -> Option<i64> {
    value::as_integer(value)
}

fn bool_arg(value: i64) -> Option<bool> {
    match value {
        NONE => Some(false),
        _ if value == value::TRUE => Some(true),
        _ if value == value::FALSE => Some(false),
        _ => None,
    }
}

/// `warnings.warn(message, category=UserWarning, stacklevel=1)`.
fn warn(args: &[i64]) -> i64 {
    let message = args.first().copied().unwrap_or(NONE);
    let category_arg = args.get(1).copied().unwrap_or(NONE);
    let stacklevel = args.get(2).copied().unwrap_or(NONE);
    if stacklevel != NONE && int_arg(stacklevel).is_none() {
        return type_error("stacklevel must be an integer");
    }
    // `None` and an omitted category both select `UserWarning`.
    emit(
        message,
        category_arg,
        "<unknown>".to_string(),
        0,
        String::new(),
        NONE,
        NONE,
        exc::USER_WARNING,
    )
}

/// Validates the CPython 3.14 `skip_file_prefixes` keyword.
///
/// CPython checks with `PyTuple_Check` (so tuple subclasses pass) and then
/// requires every element to be a `str`; both failures are `TypeError`, using
/// the messages of the C implementation
/// (`argument 'skip_file_prefixes' must be tuple, not list` and
/// `Found non-str 'int' in skip_file_prefixes.`).
///
/// The prefixes themselves select caller frames to skip when `stacklevel` is
/// resolved. Hydro has no frame stack in native module code (see the module
/// docs), so there is nothing to filter: the tuple is validated and then only
/// `stacklevel`-based attribution applies. The filename is never derived from
/// a prefix.
fn validate_skip_file_prefixes(value: i64) -> Result<(), i64> {
    let Some(prefixes) = value::as_tuple(value) else {
        return Err(type_error(&format!(
            "argument 'skip_file_prefixes' must be tuple, not {}",
            value::type_name(value)
        )));
    };
    for prefix in prefixes {
        if value::as_str(*prefix).is_none() {
            return Err(type_error(&format!(
                "Found non-str '{}' in skip_file_prefixes.",
                value::type_name(*prefix)
            )));
        }
    }
    Ok(())
}

/// `warnings.warn(message, category=None, stacklevel=1, *,
/// skip_file_prefixes=())` called with keyword arguments.
///
/// `message`, `category` and `stacklevel` are declared in the spec and are
/// already moved into their positional slots by the central keyword binder, so
/// the only name left here is the keyword-only `skip_file_prefixes`. Unknown
/// names are rejected instead of being dropped.
fn warn_kw(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut skip_seen = false;
    // Validate the tuple before emitting, like CPython's argument parser.
    for (name, value) in keywords {
        match name.as_str() {
            "skip_file_prefixes" => {
                if skip_seen {
                    return type_error(
                        "warn() got multiple values for keyword argument 'skip_file_prefixes'",
                    );
                }
                skip_seen = true;
                if let Err(error) = validate_skip_file_prefixes(*value) {
                    return error;
                }
            }
            _ => {
                return type_error(&format!(
                    "warn() got an unexpected keyword argument '{name}'"
                ));
            }
        }
    }
    warn(args)
}

/// Shared body of `warn` and `warn_explicit`.
#[allow(clippy::too_many_arguments)]
fn emit(
    message: i64,
    category_arg: i64,
    filename: String,
    lineno: i64,
    module: String,
    registry: i64,
    module_globals: i64,
    default_tag: u32,
) -> i64 {
    if module_globals != NONE && value::as_dict(module_globals).is_none() {
        return type_error("module_globals must be a dict or None");
    }
    // `warn` has no frame access, so it passes no module; `warn_explicit`
    // derives one from the caller's globals or the filename, like CPython.
    let mut module = module;
    if module.is_empty() {
        if let Some(globals) = value::as_dict(module_globals) {
            let key = value::str_alloc("__name__".to_string());
            if let Some(position) = container::dict_find(globals, key) {
                if let Some(name) = value::as_str(globals[position].1) {
                    module = name.to_string();
                }
            }
        }
        if module.is_empty() {
            module = module_from_filename(&filename);
        }
    }
    // A `Warning` instance supplies both the text and the category; an
    // explicit `category` argument is ignored in that form, like CPython.
    let (text, category_tag_value, category, explicit_category) =
        if let Some(data) = value::exception_data(message) {
            if exc::tag_is_subclass(data.tag, WARNING) {
                let category = exc::exception_class(data.tag);
                (data.msg.clone(), data.tag, category, false)
            } else {
                return type_error(&format!(
                    "expected a Warning instance or str, not {}",
                    value::type_name(message)
                ));
            }
        } else {
            let Some(text) = string_arg(message) else {
                return type_error(&format!(
                    "warn() argument 1 must be a string or Warning instance, not {}",
                    value::type_name(message)
                ));
            };
            let Some(tag) = warning_category(category_arg, default_tag) else {
                return type_error("category must be a Warning subclass");
            };
            (text, tag, exc::exception_class(tag), true)
        };
    // Explicit categories on `warn_explicit` may be user classes; keep the
    // argument's own class for display when it is a real Warning subclass.
    let category =
        if explicit_category && category_arg != NONE && value::as_class(category_arg).is_some() {
            category_arg
        } else {
            category
        };
    raw_emit(
        text,
        category,
        category_tag_value,
        filename,
        lineno,
        module,
        registry,
    )
}

/// Applies filters and either shows, ignores or raises the warning.
fn raw_emit(
    text: String,
    category: i64,
    tag: u32,
    filename: String,
    lineno: i64,
    module: String,
    registry: i64,
) -> i64 {
    // Read the action and the filter version under the lock; the registry
    // update and the display happen afterwards, so no dict/graph lock is
    // taken while the warning mutex is held.
    let (action, version) = {
        let state = state();
        let action =
            select_action(&state.filters, &text, tag, &module, lineno).unwrap_or(Action::Default);
        (action, state.version)
    };
    if action == Action::Ignore {
        return NONE;
    }
    if action == Action::Error {
        return exc::raise_with_message(tag, &text);
    }
    if registry != NONE {
        if value::as_dict(registry).is_none() {
            return type_error("registry must be a dict or None");
        }
        let version = ops::int_result(version as i64);
        let version_key = value::str_alloc("version".to_string());
        if external_registry_enter(
            registry,
            version_key,
            version,
            &text,
            category,
            lineno,
            action,
        ) {
            return NONE;
        }
    } else {
        let mut state = state();
        if record_internal(&mut state, &text, tag, lineno, action) {
            return NONE;
        }
    }
    let target = state().record_stack.last().copied();
    match target {
        Some(target) => record_warning(&target, &text, category, tag, &filename, lineno),
        None => show(&text, category, &filename, lineno, NONE, NONE),
    }
}

/// Appends one `WarningMessage`-shaped object to a recording list.
///
/// The message attribute is the `Warning` instance (`category(text)`), the
/// same object CPython passes to `showwarning`; `category` keeps the class the
/// caller supplied. Everything remains inspectable as attributes, which is
/// what `numpy.testing` and `warnings.catch_warnings(record=True)` consumers
/// read.
fn record_warning(
    target: &RecordTarget,
    text: &str,
    category: i64,
    tag: u32,
    filename: &str,
    lineno: i64,
) -> i64 {
    // The warning instance and the wrapper class are both fresh allocations;
    // root the parts while the class and instance attributes are created.
    let message = exc::exception_value(tag, text, vec![text.to_string()]);
    let warning = value::instance_new(warning_message_class());
    let category_name = value::class_display_name(category).unwrap_or("Warning");
    let fields = [
        ("message".to_string(), message),
        ("category".to_string(), category),
        (
            "filename".to_string(),
            value::str_alloc(filename.to_string()),
        ),
        ("lineno".to_string(), ops::int_result(lineno)),
        ("file".to_string(), NONE),
        ("line".to_string(), NONE),
        ("source".to_string(), NONE),
        (
            "_category_name".to_string(),
            value::str_alloc(category_name.to_string()),
        ),
    ];
    let roots = heap::root_values(&[message, warning, category, fields[2].1, fields[3].1]);
    for (name, item) in fields {
        value::instance_setattr(warning, name, item);
    }
    container::list_append(target.list, warning);
    heap::release_roots(roots);
    NONE
}

/// The `WarningMessage` wrapper class recorded in a list.
///
/// A fresh empty class is enough: every field is a per-instance attribute and
/// consumers only read them.
fn warning_message_class() -> i64 {
    value::class_new("WarningMessage".to_string(), NONE, Vec::new())
}

/// Records the warning in the caller-supplied registry dict (CPython's
/// `__warningregistry__`). Returns `true` when the warning was already
/// recorded and must not be shown again.
fn external_registry_enter(
    registry: i64,
    version_key: i64,
    version: i64,
    text: &str,
    category: i64,
    lineno: i64,
    action: Action,
) -> bool {
    // Version the registry like CPython: a filter mutation clears it.
    let stored_version = value::as_dict_data(registry)
        .and_then(|dict| {
            dict.find(version_key)
                .map(|position| dict.entries()[position].1)
        })
        .and_then(int_arg);
    if stored_version != Some(version) {
        if let Some(mut pairs) = value::dict_pairs_mut(registry) {
            pairs.clear();
        }
        container::dict_set(registry, version_key, version);
    }
    let text_value = value::str_alloc(text.to_string());
    let key = value::tuple_new(vec![text_value, category, ops::int_result(lineno)]);
    if value::as_dict_data(registry).is_some_and(|dict| dict.find(key).is_some()) {
        return true;
    }
    // Insert the key before building any alternative key: the dict then owns
    // (and therefore roots) `text_value` across the later allocations.
    container::dict_set(registry, key, ops::int_result(1));
    if lineno == 0 {
        // The line-number-specific key already is the "seen anywhere" key.
        return false;
    }
    if matches!(action, Action::Once | Action::Module) {
        let alt_key = value::tuple_new(vec![text_value, category, ops::int_result(0)]);
        if value::as_dict_data(registry).is_some_and(|dict| dict.find(alt_key).is_some()) {
            return true;
        }
        container::dict_set(registry, alt_key, ops::int_result(1));
    }
    false
}

/// Records the warning in the internal registry used when `warn` runs without
/// a caller-supplied registry dict. Returns `true` when the warning must not
/// be shown again.
fn record_internal(state: &mut State, text: &str, tag: u32, lineno: i64, action: Action) -> bool {
    if action == Action::Once {
        let once_key = (text.to_string(), tag);
        if !state.once.insert(once_key) {
            return true;
        }
        state.registry.insert((text.to_string(), tag, lineno));
        return false;
    }
    if action == Action::Module {
        let key = (text.to_string(), tag, lineno);
        if state.registry.contains(&key) {
            return true;
        }
        state.registry.insert(key);
        if lineno != 0 && state.registry.contains(&(text.to_string(), tag, 0)) {
            return true;
        }
        state.registry.insert((text.to_string(), tag, 0));
        return false;
    }
    if action == Action::Default {
        return !state.registry.insert((text.to_string(), tag, lineno));
    }
    false
}

/// `warnings.filterwarnings(...)` / `warnings.simplefilter(...)`.
fn install_filter(
    action_text: &str,
    message: String,
    category_arg: i64,
    module: String,
    lineno: i64,
    append: bool,
) -> i64 {
    let Some(action) = Action::parse(action_text) else {
        return value_error(&format!("invalid action: {action_text:?}"));
    };
    let Some(tag) = warning_category(category_arg, WARNING) else {
        return type_error("category must be a Warning subclass");
    };
    if lineno < 0 {
        return value_error("lineno must be an int >= 0");
    }
    let filter = Filter {
        action,
        message: message.to_lowercase(),
        category_tag: tag,
        module: module.to_lowercase(),
        lineno,
    };
    let mut state = state();
    if append {
        // CPython leaves an identical existing filter where it is.
        if !state
            .filters
            .iter()
            .any(|existing| existing.same_as(&filter))
        {
            state.filters.push(filter);
        }
    } else {
        // A duplicate is removed so the new entry takes its place at the
        // front, exactly like `warnings._add_filter`.
        state.filters.retain(|existing| !existing.same_as(&filter));
        state.filters.insert(0, filter);
    }
    state.mutated();
    NONE
}

fn filterwarnings(args: &[i64]) -> i64 {
    let Some(action) = args.first().and_then(|value| string_arg(*value)) else {
        return type_error("action must be a string");
    };
    let message = match args.get(1) {
        None | Some(&NONE) => String::new(),
        Some(&value) => match string_arg(value) {
            Some(message) => message,
            None => return type_error("message must be a string"),
        },
    };
    let category = args.get(2).copied().unwrap_or(NONE);
    let module = match args.get(3) {
        None | Some(&NONE) => String::new(),
        Some(&value) => match string_arg(value) {
            Some(module) => module,
            None => return type_error("module must be a string"),
        },
    };
    let lineno = match args.get(4) {
        None | Some(&NONE) => 0,
        Some(&value) => match int_arg(value) {
            Some(lineno) => lineno,
            None => return type_error("lineno must be an int"),
        },
    };
    let append = match args.get(5) {
        None => false,
        Some(&value) => match bool_arg(value) {
            Some(append) => append,
            None => return type_error("append must be a bool"),
        },
    };
    install_filter(&action, message, category, module, lineno, append)
}

fn simplefilter(args: &[i64]) -> i64 {
    let Some(action) = args.first().and_then(|value| string_arg(*value)) else {
        return type_error("action must be a string");
    };
    let category = args.get(1).copied().unwrap_or(NONE);
    let lineno = match args.get(2) {
        None | Some(&NONE) => 0,
        Some(&value) => match int_arg(value) {
            Some(lineno) => lineno,
            None => return type_error("lineno must be an int"),
        },
    };
    let append = match args.get(3) {
        None => false,
        Some(&value) => match bool_arg(value) {
            Some(append) => append,
            None => return type_error("append must be a bool"),
        },
    };
    install_filter(
        &action,
        String::new(),
        category,
        String::new(),
        lineno,
        append,
    )
}

fn resetwarnings() -> i64 {
    let mut state = state();
    state.filters.clear();
    state.mutated();
    NONE
}

/// `warnings.catch_warnings(...)`: builds the native context manager.
///
/// The arguments are parsed by name because the central keyword binder only
/// fills leading slots: `catch_warnings(action="ignore")` would otherwise be
/// mistaken for `record`.
fn catch_warnings(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut record = false;
    let mut module = NONE;
    let mut action = NONE;
    let mut append = false;
    if args.len() > 4 {
        return type_error("catch_warnings() takes at most 4 arguments");
    }
    for (index, &value) in args.iter().enumerate() {
        let result = match index {
            0 => assign_bool(&mut record, value, "record"),
            1 => {
                module = value;
                Ok(())
            }
            2 => {
                action = value;
                Ok(())
            }
            _ => assign_bool(&mut append, value, "append"),
        };
        if let Err(error) = result {
            return error;
        }
    }
    for (name, value) in keywords {
        let value = *value;
        let result = match name.as_str() {
            "record" => assign_bool(&mut record, value, "record"),
            "module" => {
                module = value;
                Ok(())
            }
            "action" => {
                action = value;
                Ok(())
            }
            "append" => assign_bool(&mut append, value, "append"),
            other => Err(type_error(&format!(
                "catch_warnings() got an unexpected keyword argument '{other}'"
            ))),
        };
        if let Err(error) = result {
            return error;
        }
    }
    if module != NONE {
        match value::as_module(module) {
            Some(state) if matches!(state.name(), "warnings" | "_warnings") => {}
            Some(_) => {
                return exc::raise_with_message(
                    NOT_IMPLEMENTED_ERROR,
                    "catch_warnings(module=...) only supports the warnings module in Hydro",
                )
            }
            None => return type_error("catch_warnings() module must be a module or None"),
        }
    }
    if action != NONE {
        let Some(text) = string_arg(action) else {
            return type_error("catch_warnings() action must be a string or None");
        };
        if Action::parse(&text).is_none() {
            return value_error(&format!("invalid action: {text:?}"));
        }
    }
    let class = catch_warnings_class();
    let instance = value::instance_new(class);
    let roots = heap::root_values(&[instance, action]);
    value::instance_setattr(instance, "_record".to_string(), bool_value(record));
    value::instance_setattr(instance, "_action".to_string(), action);
    value::instance_setattr(instance, "_append".to_string(), bool_value(append));
    value::instance_setattr(instance, "_entered".to_string(), value::FALSE);
    value::instance_setattr(instance, "_saved_filters".to_string(), NONE);
    value::instance_setattr(instance, "_record_list".to_string(), NONE);
    value::instance_setattr(instance, "_record_root".to_string(), NONE);
    heap::release_roots(roots);
    instance
}

/// True/False for a Python bool argument, `TypeError` otherwise.
fn bool_value(value: bool) -> i64 {
    if value {
        value::TRUE
    } else {
        value::FALSE
    }
}

fn assign_bool(slot: &mut bool, value: i64, name: &str) -> Result<(), i64> {
    match bool_arg(value) {
        Some(flag) => {
            *slot = flag;
            Ok(())
        }
        None => Err(type_error(&format!("{name} must be a bool"))),
    }
}

/// The native `catch_warnings` class. A fresh class per constructed manager is
/// unnecessary but harmless: the manager never compares class identity.
fn catch_warnings_class() -> i64 {
    let methods = vec![
        (
            "__enter__".to_string(),
            value::native_function_new("warnings", "_catch_warnings_enter", 1),
        ),
        (
            "__exit__".to_string(),
            value::native_function_new("warnings", "_catch_warnings_exit", 4),
        ),
    ];
    value::class_new("catch_warnings".to_string(), NONE, methods)
}

/// `catch_warnings.__enter__`.
fn catch_warnings_enter(instance: i64) -> i64 {
    let entered = value::instance_attr(instance, "_entered").unwrap_or(value::FALSE);
    if entered == value::TRUE {
        return exc::raise_with_message(RUNTIME_ERROR, "Cannot enter catch_warnings twice");
    }
    let record = value::instance_attr(instance, "_record").unwrap_or(value::FALSE) == value::TRUE;
    let action = value::instance_attr(instance, "_action").unwrap_or(NONE);
    let append = value::instance_attr(instance, "_append").unwrap_or(value::FALSE) == value::TRUE;
    let snapshot = filters();
    let roots = heap::root_values(&[instance, snapshot]);
    value::instance_setattr(instance, "_entered".to_string(), value::TRUE);
    value::instance_setattr(instance, "_saved_filters".to_string(), snapshot);
    heap::release_roots(roots);
    if let Some(text) = string_arg(action) {
        let result = install_filter(&text, String::new(), NONE, String::new(), 0, append);
        if exc::pending() {
            return result;
        }
    }
    if !record {
        return NONE;
    }
    let list = value::list_new(Vec::new());
    let root = heap::add_persistent_root(list);
    let root_value = if root == usize::MAX {
        NONE
    } else {
        ops::int_result(root as i64)
    };
    let roots = heap::root_values(&[instance, list, root_value]);
    value::instance_setattr(instance, "_record_list".to_string(), list);
    value::instance_setattr(instance, "_record_root".to_string(), root_value);
    state().record_stack.push(RecordTarget { list, root });
    heap::release_roots(roots);
    // CPython installs `simplefilter("always")` for recording; it keeps every
    // warning visible even when the surrounding filters ignored it.
    let result = install_filter("always", String::new(), NONE, String::new(), 0, false);
    if exc::pending() {
        return result;
    }
    list
}

/// `catch_warnings.__exit__(self, exc_type, exc_value, tb)`.
fn catch_warnings_exit(instance: i64) -> i64 {
    let entered = value::instance_attr(instance, "_entered").unwrap_or(value::FALSE);
    if entered != value::TRUE {
        return exc::raise_with_message(
            RUNTIME_ERROR,
            "Cannot exit catch_warnings without entering first",
        );
    }
    let saved = value::instance_attr(instance, "_saved_filters").unwrap_or(NONE);
    restore_filters(saved);
    let record = value::instance_attr(instance, "_record").unwrap_or(value::FALSE) == value::TRUE;
    if record {
        let root = value::instance_attr(instance, "_record_root").unwrap_or(NONE);
        if let Some(root) = int_arg(root) {
            if let Ok(root) = usize::try_from(root) {
                heap::remove_persistent_root(root);
            }
        }
        let _ = state().record_stack.pop();
        value::instance_setattr(instance, "_record_list".to_string(), NONE);
        value::instance_setattr(instance, "_record_root".to_string(), NONE);
    }
    value::instance_setattr(instance, "_entered".to_string(), value::FALSE);
    value::instance_setattr(instance, "_saved_filters".to_string(), NONE);
    value::FALSE
}

/// Replaces the live filter stack with the snapshot produced by [`filters`].
fn restore_filters(snapshot: i64) {
    let Some(items) = value::as_list(snapshot) else {
        return;
    };
    // Copy the entries out before taking the state lock: reading a tuple and
    // materialising strings allocates, and the lock is never held across an
    // allocation that may collect.
    let mut restored: Vec<(Action, String, u32, String, i64)> = Vec::with_capacity(items.len());
    for item in items {
        let Some(fields) = value::as_tuple(*item) else {
            continue;
        };
        if fields.len() != 5 {
            continue;
        }
        let Some(action) = string_arg(fields[0]).and_then(|text| Action::parse(&text)) else {
            continue;
        };
        let message = string_arg(fields[1]).unwrap_or_default();
        let Some(tag) = category_tag(fields[2]) else {
            continue;
        };
        let module = string_arg(fields[3]).unwrap_or_default();
        let lineno = int_arg(fields[4]).unwrap_or(0);
        restored.push((action, message, tag, module, lineno));
    }
    let mut state = state();
    state.filters.clear();
    state.filters.extend(restored.into_iter().map(
        |(action, message, category_tag, module, lineno)| Filter {
            action,
            message: message.to_lowercase(),
            category_tag,
            module: module.to_lowercase(),
            lineno,
        },
    ));
    state.mutated();
}

/// `warnings._deprecated(...)`: the private CPython helper.
///
/// Two spellings are accepted because the standard library uses both across
/// versions:
///
/// * 3.14: `_deprecated(name, message=..., *, remove=(major, minor))`;
/// * the newer helper shape: `_deprecated(message, *, name, since,
///   alternative=None, pending=False, stacklevel=2)`.
///
/// Both validate the required arguments before emitting; nothing is silently
/// defaulted into a misleading deprecation.
fn deprecated(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut name_keyword: Option<i64> = None;
    let mut since: Option<i64> = None;
    let mut alternative = NONE;
    let mut pending = false;
    let mut remove: Option<i64> = None;
    let mut version: Option<i64> = None;
    let mut stacklevel = 2i64;
    for (keyword, value) in keywords {
        let value = *value;
        match keyword.as_str() {
            "name" => name_keyword = Some(value),
            "since" => since = Some(value),
            "alternative" => alternative = value,
            "pending" => match bool_arg(value) {
                Some(flag) => pending = flag,
                None => return type_error("pending must be a bool"),
            },
            "remove" => remove = Some(value),
            "_version" => version = Some(value),
            "stacklevel" => match int_arg(value) {
                Some(level) => stacklevel = level,
                None => return type_error("stacklevel must be an int"),
            },
            other => {
                return type_error(&format!(
                    "_deprecated() got an unexpected keyword argument '{other}'"
                ))
            }
        }
    }
    if let Some(remove) = remove {
        return deprecated_removal(args, remove, version, stacklevel);
    }
    let Some(message_value) = args.first().copied() else {
        return type_error("_deprecated() missing required argument 'message'");
    };
    let Some(message) = string_arg(message_value) else {
        return type_error("_deprecated() message must be a string");
    };
    let Some(name_value) = name_keyword.or_else(|| args.get(1).copied()) else {
        return type_error("_deprecated() missing required keyword argument 'name'");
    };
    let Some(name) = string_arg(name_value) else {
        return type_error("_deprecated() name must be a string");
    };
    let Some(since_value) = since else {
        return type_error("_deprecated() missing required keyword argument 'since'");
    };
    let since_text = version_text(since_value)
        .ok_or_else(|| type_error("_deprecated() since must be a (major, minor) tuple"));
    let since_text = match since_text {
        Ok(text) => text,
        Err(error) => return error,
    };
    let alternative_text = match alternative {
        NONE => String::new(),
        value => match string_arg(value) {
            Some(text) => text,
            None => return type_error("_deprecated() alternative must be a string or None"),
        },
    };
    let text = format_deprecation(
        &message,
        &[
            ("name", name.as_str()),
            ("since", since_text.as_str()),
            ("alternative", alternative_text.as_str()),
        ],
    );
    emit_private_warning(
        &text,
        if pending {
            PENDING_DEPRECATION_WARNING
        } else {
            DEPRECATION_WARNING
        },
    )
}

/// 3.14 `_deprecated(name, message, *, remove=...)` semantics.
fn deprecated_removal(args: &[i64], remove: i64, version: Option<i64>, _stacklevel: i64) -> i64 {
    let Some(name) = args.first().copied().and_then(string_arg) else {
        return type_error("_deprecated() missing required argument 'name'");
    };
    let message = match args.get(1).copied() {
        None | Some(NONE) => {
            "{name!r} is deprecated and slated for removal in Python {remove}".to_string()
        }
        Some(value) => match string_arg(value) {
            Some(text) => text,
            None => return type_error("_deprecated() message must be a string"),
        },
    };
    deprecated_removal_message(&name, &message, remove, version)
}

fn deprecated_removal_message(name: &str, message: &str, remove: i64, version: Option<i64>) -> i64 {
    let Some((remove_major, remove_minor)) = version_pair(remove) else {
        return type_error("_deprecated() remove must be a (major, minor) tuple");
    };
    let current = match version {
        Some(value) => match version_triple(value) {
            Some(triple) => triple,
            None => return type_error("_deprecated() _version must be a 3-item version tuple"),
        },
        None => {
            let (major, minor, micro) = hydro_ir::PYTHON_VERSION;
            (major as i64, minor as i64, micro as i64)
        }
    };
    let removal_text = format!("{remove_major}.{remove_minor}");
    if (current.0, current.1) >= (remove_major, remove_minor) {
        return exc::raise_with_message(
            RUNTIME_ERROR,
            &format!("'{name}' was slated for removal after Python {removal_text} alpha"),
        );
    }
    let text = format_deprecation(
        message,
        &[
            ("name", name),
            ("remove", removal_text.as_str()),
            ("since", ""),
            ("alternative", ""),
        ],
    );
    emit_private_warning(&text, DEPRECATION_WARNING)
}

/// Formats one of the CPython deprecation messages with named placeholders.
fn format_deprecation(message: &str, values: &[(&str, &str)]) -> String {
    let mut text = message.to_string();
    for (name, value) in values {
        // `!r` is the common CPython spelling; format it with Python's repr
        // of the string and then substitute the plain placeholder.
        let repr = format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"));
        text = text.replace(&format!("{{{name}!r}}"), &repr);
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// Emits a warning from a private helper, outside the public filters API.
///
/// Filters still apply (a `catch_warnings(record=True)` block records it, and
/// the default `DeprecationWarning` filter ignores it outside `__main__`);
/// only the parameter validation differs.
fn emit_private_warning(message: &str, tag: u32) -> i64 {
    let text = value::str_alloc(message.to_string());
    emit(
        text,
        exc::exception_class(tag),
        "<unknown>".to_string(),
        0,
        String::new(),
        NONE,
        NONE,
        tag,
    )
}

/// Parses a two-element version tuple (or list).
fn version_pair(value: i64) -> Option<(i64, i64)> {
    let items = version_items(value)?;
    if items.len() != 2 {
        return None;
    }
    Some((items[0], items[1]))
}

/// Parses a version tuple with at least three integer items.
fn version_triple(value: i64) -> Option<(i64, i64, i64)> {
    let items = version_items(value)?;
    if items.len() < 3 {
        return None;
    }
    Some((items[0], items[1], items[2]))
}

fn version_items(value: i64) -> Option<Vec<i64>> {
    let items: Vec<i64> = if let Some(tuple) = value::as_tuple(value) {
        tuple.to_vec()
    } else if let Some(list) = value::as_list(value) {
        list.to_vec()
    } else {
        return None;
    };
    let mut numbers = Vec::with_capacity(items.len());
    for item in items {
        numbers.push(int_arg(item)?);
    }
    Some(numbers)
}

/// Renders a `since` value as `major.minor` for message formatting.
fn version_text(value: i64) -> Option<String> {
    if let Some((major, minor)) = version_pair(value) {
        return Some(format!("{major}.{minor}"));
    }
    string_arg(value)
}

/// Internal entry point used by `typing._deprecation_warning_for_no_type_params_passed`.
pub(crate) fn warn_category(message: &str, tag: u32) -> i64 {
    emit_private_warning(message, tag)
}

/// `warnings.formatwarning(message, category, filename, lineno, line=None)`.
fn format_warning(text: &str, category: i64, filename: &str, lineno: i64, line: i64) -> i64 {
    let name = value::class_display_name(category).unwrap_or("Warning");
    let mut rendered = String::with_capacity(text.len() + filename.len() + name.len() + 32);
    rendered.push_str(filename);
    rendered.push(':');
    rendered.push_str(&lineno.to_string());
    rendered.push_str(": ");
    rendered.push_str(name);
    rendered.push_str(": ");
    rendered.push_str(text);
    rendered.push('\n');
    if line != NONE {
        let source = value::to_string(line);
        rendered.push_str("  ");
        rendered.push_str(source.trim());
        rendered.push('\n');
    }
    value::str_alloc(rendered)
}

/// `warnings.showwarning(message, category, filename, lineno, file=None,
/// line=None)`.
fn show(text: &str, category: i64, filename: &str, lineno: i64, file: i64, line: i64) -> i64 {
    let rendered = format_warning(text, category, filename, lineno, line);
    if exc::pending() {
        return NONE;
    }
    if file == NONE {
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        let _ = stderr.write_all(value::as_str(rendered).unwrap_or("").as_bytes());
        let _ = stderr.flush();
        return NONE;
    }
    // A caller-supplied stream receives the same text through its `write`.
    // The rendered string is rooted while the call allocates its arguments.
    let root = heap::root_values(&[rendered]);
    let args = [rendered];
    methods::call(file, b"write".as_ptr(), 5, 1, args.as_ptr());
    heap::release_roots(root);
    NONE
}

/// `warnings.warn_explicit(...)`.
fn warn_explicit(args: &[i64]) -> i64 {
    let message = args.first().copied().unwrap_or(NONE);
    let category = args.get(1).copied().unwrap_or(NONE);
    let filename = match args.get(2) {
        None | Some(&NONE) => "<unknown>".to_string(),
        Some(&value) => match string_arg(value) {
            Some(filename) => filename,
            None => return type_error("filename must be a string"),
        },
    };
    let lineno = match args.get(3) {
        None | Some(&NONE) => 0,
        Some(&value) => match int_arg(value) {
            Some(lineno) => lineno,
            None => return type_error("lineno must be an int"),
        },
    };
    let module = match args.get(4) {
        None | Some(&NONE) => module_from_filename(&filename),
        Some(&value) => match string_arg(value) {
            Some(module) => module,
            None => return type_error("module must be a string"),
        },
    };
    let registry = args.get(5).copied().unwrap_or(NONE);
    if registry != NONE && value::as_dict(registry).is_none() {
        return type_error("registry must be a dict or None");
    }
    let module_globals = args.get(6).copied().unwrap_or(NONE);
    if module_globals != NONE && value::as_dict(module_globals).is_none() {
        return type_error("module_globals must be a dict or None");
    }
    emit(
        message,
        category,
        filename,
        lineno,
        module,
        registry,
        module_globals,
        exc::USER_WARNING,
    )
}

/// CPython's default module name for `warn_explicit`: the filename without a
/// trailing `.py`.
fn module_from_filename(filename: &str) -> String {
    let cut = filename.len().saturating_sub(3);
    if filename.is_char_boundary(cut) && filename[cut..].eq_ignore_ascii_case(".py") {
        return filename[..cut].to_string();
    }
    filename.to_string()
}

/// Reads a required string argument, raising `TypeError` on failure.
fn required_string(args: &[i64], index: usize, name: &str) -> Result<String, i64> {
    match args.get(index).copied() {
        Some(value) => {
            string_arg(value).ok_or_else(|| type_error(&format!("{name} must be a string")))
        }
        None => Err(type_error(&format!("{name} must be a string"))),
    }
}

/// Reads a message argument for `formatwarning`/`showwarning`.
fn display_message(args: &[i64]) -> Result<String, i64> {
    match args.first().copied() {
        Some(value) => match string_arg(value) {
            Some(text) => Ok(text),
            None => Err(type_error("message must be a string")),
        },
        None => Err(type_error("message must be a string")),
    }
}

/// Reads and validates a warning category argument for the display helpers.
fn display_category(args: &[i64]) -> Result<i64, i64> {
    match args.get(1).copied() {
        None | Some(NONE) => Ok(exc::exception_class(WARNING)),
        Some(value) => {
            if warning_category(value, WARNING).is_none() {
                return Err(type_error("category must be a Warning subclass"));
            }
            Ok(value)
        }
    }
}

/// Reads a required integer argument.
fn required_int(args: &[i64], index: usize, name: &str) -> Result<i64, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(0),
        Some(value) => int_arg(value).ok_or_else(|| type_error(&format!("{name} must be an int"))),
    }
}

/// Reads the optional `line` argument (a source string or `None`).
fn optional_line(args: &[i64], index: usize) -> Result<i64, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(NONE),
        Some(value) => {
            if value::as_str(value).is_some() {
                Ok(value)
            } else {
                Err(type_error("line must be a string"))
            }
        }
    }
}

/// Dispatches a native `warnings`/`_warnings` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "warn" => warn(args),
        "warn_explicit" => warn_explicit(args),
        "filterwarnings" => filterwarnings(args),
        "simplefilter" => simplefilter(args),
        "resetwarnings" => resetwarnings(),
        "catch_warnings" => catch_warnings(args, &[]),
        "_deprecated" => deprecated(args, &[]),
        "_catch_warnings_enter" => match args.first().copied() {
            Some(instance) => catch_warnings_enter(instance),
            None => type_error("__enter__() missing self"),
        },
        "_catch_warnings_exit" => match args.first().copied() {
            Some(instance) => catch_warnings_exit(instance),
            None => type_error("__exit__() missing self"),
        },
        "formatwarning" => {
            let text = match display_message(args) {
                Ok(text) => text,
                Err(error) => return error,
            };
            let category = match display_category(args) {
                Ok(category) => category,
                Err(error) => return error,
            };
            let filename = match required_string(args, 2, "filename") {
                Ok(filename) => filename,
                Err(error) => return error,
            };
            let lineno = match required_int(args, 3, "lineno") {
                Ok(lineno) => lineno,
                Err(error) => return error,
            };
            let line = match optional_line(args, 4) {
                Ok(line) => line,
                Err(error) => return error,
            };
            format_warning(&text, category, &filename, lineno, line)
        }
        "showwarning" => {
            let text = match display_message(args) {
                Ok(text) => text,
                Err(error) => return error,
            };
            let category = match display_category(args) {
                Ok(category) => category,
                Err(error) => return error,
            };
            let filename = match required_string(args, 2, "filename") {
                Ok(filename) => filename,
                Err(error) => return error,
            };
            let lineno = match required_int(args, 3, "lineno") {
                Ok(lineno) => lineno,
                Err(error) => return error,
            };
            let file = args.get(4).copied().unwrap_or(NONE);
            let line = match optional_line(args, 5) {
                Ok(line) => line,
                Err(error) => return error,
            };
            show(&text, category, &filename, lineno, file, line)
        }
        _ => attribute_error(function),
    }
}

/// Keyword-aware dispatch for `warn`, `catch_warnings` and `_deprecated`.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        "warn" => warn_kw(args, keywords),
        "catch_warnings" => catch_warnings(args, keywords),
        "_deprecated" => deprecated(args, keywords),
        _ => function_attribute_error(function),
    }
}

fn function_attribute_error(function: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'warnings' has no attribute '{function}'"),
    )
}

/// `warnings.deprecated`: the PEP 702 decorator class introduced in CPython
/// 3.14 (`_py_warnings.deprecated`).
///
/// It records the message plus the keyword-only `category`/`stacklevel`
/// metadata and returns its argument, setting `__deprecated__`. CPython wraps
/// callables and classes so the warning is emitted on call/instantiation;
/// Hydro's native function/class values have no per-decoration call slot, so
/// call-time emission is a documented limitation. The category/stacklevel
/// values are still validated and stored, which is what pydantic and
/// typing_extensions introspect (`warnings.deprecated | ...`, `__deprecated__`,
/// `category=None`).
pub(crate) fn deprecated_class() -> i64 {
    let key = "warnings.deprecated";
    if let Some(cached) = crate::core::globals::registry_get(key) {
        return cached;
    }
    let category_default = exc::exception_class(DEPRECATION_WARNING);
    let init = native_method(
        "_deprecated_init",
        deprecated_init_entry,
        vec![
            value::ParamMeta {
                name: "self".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            value::ParamMeta {
                name: "message".to_string(),
                kind: ParamKind::PosOnly,
                default: None,
            },
            value::ParamMeta {
                name: "category".to_string(),
                kind: ParamKind::KwOnly,
                default: Some(category_default),
            },
            value::ParamMeta {
                name: "stacklevel".to_string(),
                kind: ParamKind::KwOnly,
                default: Some(ops::int_result(1)),
            },
        ],
    );
    let call = native_method(
        "_deprecated_call",
        deprecated_call_entry,
        vec![
            value::ParamMeta {
                name: "self".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            value::ParamMeta {
                name: "arg".to_string(),
                kind: ParamKind::PosOnly,
                default: None,
            },
        ],
    );
    let roots = heap::root_values(&[init, call, category_default]);
    let class = value::class_new(
        "deprecated".to_string(),
        NONE,
        vec![
            ("__init__".to_string(), init),
            ("__call__".to_string(), call),
        ],
    );
    let class_roots = heap::root_values(&[class]);
    crate::core::globals::registry_set(key, class);
    heap::release_roots(class_roots);
    heap::release_roots(roots);
    class
}

/// A native method with full parameter metadata so keyword arguments bind and
/// the trampoline receives every slot.
fn native_method(
    name: &'static str,
    entry: extern "C" fn(*const i64) -> i64,
    signature: Vec<value::ParamMeta>,
) -> i64 {
    let function = value::native_function_new("warnings", name, usize::MAX);
    value::function_set_signature(function, signature);
    value::function_set_full_entry(function, entry as *const () as usize);
    function
}

extern "C" fn deprecated_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `(self, message, category, stacklevel)`.
    deprecated_init(
        unsafe { *argv },
        unsafe { *argv.add(1) },
        unsafe { *argv.add(2) },
        unsafe { *argv.add(3) },
    )
}

extern "C" fn deprecated_call_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `(self, arg)`.
    deprecated_call(unsafe { *argv }, unsafe { *argv.add(1) })
}

/// `deprecated(message, /, *, category=DeprecationWarning, stacklevel=1)`.
fn deprecated_init(instance: i64, message: i64, category: i64, stacklevel: i64) -> i64 {
    if value::as_str(message).is_none() {
        return type_error(&format!(
            "Expected an object of type str for 'message', not '{}'",
            value::type_name(message)
        ));
    }
    if category != NONE && value::as_class(category).is_none() {
        return type_error("category must be a Warning subclass or None");
    }
    if value::as_integer(stacklevel).is_none() {
        return type_error("stacklevel must be an int");
    }
    value::instance_setattr(instance, "message".to_string(), message);
    value::instance_setattr(instance, "category".to_string(), category);
    value::instance_setattr(instance, "stacklevel".to_string(), stacklevel);
    NONE
}

/// `decorator(arg, /)`: record `__deprecated__` and return the argument.
fn deprecated_call(instance: i64, arg: i64) -> i64 {
    let Some(message) = value::instance_attr(instance, "message") else {
        return type_error("deprecated decorator is not initialised");
    };
    let _ = methods::setattr(
        arg,
        b"__deprecated__".as_ptr(),
        "__deprecated__".len(),
        message,
    );
    if exc::pending() {
        // C-level callables have no writable `__dict__`; CPython skips the
        // attribute silently in that case.
        exc::clear();
    }
    arg
}

/// Materialises a `warnings` constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "filters" => filters(),
        "deprecated" => deprecated_class(),
        _ => attribute_error(name),
    }
}

/// Snapshot of the installed filters as CPython's list of 5-tuples:
/// `(action, message, category, module, lineno)`. Empty message/module
/// patterns are `None`.
pub(crate) fn filters() -> i64 {
    let state = state();
    // The list and every tuple are heap values; root each one until the
    // result list owns the whole snapshot.
    let start = heap::root_values(&[]);
    let mut entries = Vec::with_capacity(state.filters.len());
    for filter in &state.filters {
        let action = value::str_alloc(filter.action.as_str().to_string());
        heap::add_thread_root(action);
        let message = if filter.message.is_empty() {
            NONE
        } else {
            let message = value::str_alloc(filter.message.clone());
            heap::add_thread_root(message);
            message
        };
        let module = if filter.module.is_empty() {
            NONE
        } else {
            let module = value::str_alloc(filter.module.clone());
            heap::add_thread_root(module);
            module
        };
        let category = exc::exception_class(filter.category_tag);
        let merged = value::tuple_new(vec![
            action,
            message,
            category,
            module,
            ops::int_result(filter.lineno),
        ]);
        heap::add_thread_root(merged);
        entries.push(merged);
    }
    let result = value::list_new(entries);
    heap::release_roots(start);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_class, as_integer, as_str};

    #[test]
    fn deprecated_is_the_cached_warnings_class() {
        exc::init();
        let class = deprecated_class();
        assert_eq!(class, constant("deprecated"));
        assert_eq!(
            as_class(class).map(|state| state.name.as_str()),
            Some("deprecated")
        );
        assert!(value::class_lookup(class, "__init__").is_some());
        assert!(value::class_lookup(class, "__call__").is_some());
        assert!(!exc::pending());
    }

    #[test]
    fn deprecated_records_metadata_and_sets_the_marker() {
        exc::init();
        let message = value::str_alloc("use `new` instead".to_string());
        let category = exc::exception_class(DEPRECATION_WARNING);
        let instance = value::instance_new(deprecated_class());
        assert_eq!(
            deprecated_init(instance, message, category, ops::int_result(1)),
            NONE
        );
        assert_eq!(value::instance_attr(instance, "message"), Some(message));
        assert_eq!(value::instance_attr(instance, "category"), Some(category));
        assert_eq!(
            value::instance_attr(instance, "stacklevel").and_then(as_integer),
            Some(1)
        );

        let target = value::identity_function_new("target");
        assert_eq!(deprecated_call(instance, target), target);
        assert_eq!(
            value::function_attribute(target, "__deprecated__").and_then(as_str),
            Some("use `new` instead")
        );
        assert!(!exc::pending());

        // `category=None` keeps the metadata but is otherwise the identity
        // decorator (the same path; only the recorded category differs).
        let quiet = value::instance_new(deprecated_class());
        assert_eq!(
            deprecated_init(quiet, message, NONE, ops::int_result(1)),
            NONE
        );
        assert_eq!(value::instance_attr(quiet, "category"), Some(NONE));
        assert!(!exc::pending());
    }

    #[test]
    fn deprecated_validates_message_category_and_stacklevel() {
        exc::init();
        let instance = value::instance_new(deprecated_class());
        let _ = deprecated_init(instance, NONE, NONE, ops::int_result(1));
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(value::to_string(exc::pending_value()).contains("Expected an object of type str"));
        exc::clear();

        exc::init();
        let _ = deprecated_init(
            instance,
            value::str_alloc("message".to_string()),
            NONE,
            value::str_alloc("one".to_string()),
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(value::to_string(exc::pending_value()).contains("stacklevel must be an int"));
        exc::clear();

        exc::init();
        let _ = deprecated_init(
            instance,
            value::str_alloc("message".to_string()),
            ops::int_result(1),
            ops::int_result(1),
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(value::to_string(exc::pending_value()).contains("category must be"));
        exc::clear();
    }
}
