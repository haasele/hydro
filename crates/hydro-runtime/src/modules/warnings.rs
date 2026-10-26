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
//! * `message`/`module` filters are matched as case-insensitive literal
//!   prefixes at the start of the text, which is the behaviour of the
//!   `re.I`-compiled `re.match` CPython uses for the common patterns; full
//!   regular expressions are not compiled here.
//! * `warnings.showwarning` is a native function, not a rebindable module
//!   attribute (native modules have no writable attributes in Hydro), so
//!   `catch_warnings(record=True)` style hooks are unavailable. Call
//!   `showwarning` directly to route a warning to a stream.
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

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR, WARNING};
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

struct State {
    /// Heap generation the state was built for; a runtime reset (which frees
    /// the heap and clears every collector root) invalidates the filters'
    /// cached class values.
    generation: u64,
    filters: Vec<Filter>,
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
    show(&text, category, &filename, lineno, NONE, NONE)
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

/// Materialises a `warnings` constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "filters" => filters(),
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
