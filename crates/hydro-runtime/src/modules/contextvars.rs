//! `_contextvars` / `contextvars`: context variables, contexts and tokens.
//!
//! This is the native implementation of Python's `_contextvars` extension
//! module. `ContextVar`, `Context` and `Token` are real native classes; the
//! module-level `ContextVar(name, default=...)` constructor is declared as a
//! function because Hydro's native class path only binds parameters declared
//! on module functions (see `hydro_ir::stdlib::CONTEXTVARS`), and returns an
//! instance of that class.
//!
//! ## Storage and collector safety
//!
//! * A `Context` instance owns one `_data` dict mapping `ContextVar` object
//!   (identity keys) to values. The dict is an ordinary instance attribute, so
//!   the precise/conservative collector traces every stored value.
//! * The thread's current context is kept in a thread-local record and is
//!   registered as a *persistent* heap root, so values reachable only through
//!   the active context survive collection. The record carries the heap
//!   generation: after `hydro_runtime_init` the old root slot is gone and a
//!   fresh empty context is created instead of touching a stale slot.
//! * Class values are cached per thread and invalidated by the heap generation,
//!   the same scheme `_weakref` uses. Every class, sentinel and method value is
//!   registered as a thread root before the next allocation.
//!
//! ## Threads and re-entrancy
//!
//! Each OS thread has its own current context, matching CPython (a new thread
//! starts in an empty context unless its bootstrap calls `Context.run`). A
//! module-level mutex serialises every read-modify-write on a `Context`: the
//! instance attribute table has its own lock, but entering a context and
//! updating `_data` need check-then-act atomicity. `Context.run` holds that
//! mutex only around the enter/leave bookkeeping, never while user code runs.
//!
//! Documented limits: `iter(context)` is not implemented (there is no native
//! instance iterator in this runtime); `keys()`, `values()` and `items()`
//! return snapshots as lists. Fibers share their thread's current context:
//! `Context.run` restores the caller's context when its callback returns, so a
//! context around a suspending coroutine is not automatically re-entered per
//! task step.

use std::cell::RefCell;
use std::sync::{Mutex, MutexGuard};

use hydro_ir::MethodKind;

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR, LOOKUP_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::ops;
use crate::value::{self, FALSE, NONE, TRUE};

/// Instance attributes of a `ContextVar`.
const VAR_NAME: &str = "name";
const VAR_DEFAULT: &str = "_default";
const VAR_HAS_DEFAULT: &str = "_has_default";

/// Instance attributes of a `Context`.
const CTX_DATA: &str = "_data";
const CTX_ENTERED: &str = "_entered";

/// Instance attributes of a `Token`.
const TOKEN_VAR: &str = "var";
const TOKEN_OLD_VALUE: &str = "old_value";
const TOKEN_HAS_OLD: &str = "_has_old";
const TOKEN_USED: &str = "_used";

/// Serialises check-then-act sequences on contexts. It is never held while
/// user code runs (`Context.run` releases it before invoking the callback).
static CONTEXT_LOCK: Mutex<()> = Mutex::new(());

fn context_lock() -> MutexGuard<'static, ()> {
    CONTEXT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Per-thread cache of native class values, invalidated by the heap
/// generation (a runtime reset frees every heap object).
struct ClassCache {
    generation: u64,
    var_class: i64,
    context_class: i64,
    token_class: i64,
    missing: i64,
}

const NO_CLASS: i64 = NONE;

thread_local! {
    static CLASS_CACHE: RefCell<ClassCache> = const {
        RefCell::new(ClassCache {
            generation: u64::MAX,
            var_class: NO_CLASS,
            context_class: NO_CLASS,
            token_class: NO_CLASS,
            missing: NO_CLASS,
        })
    };
}

/// The active context of this thread.
#[derive(Clone, Copy)]
struct CurrentContext {
    /// Heap generation that owns `slot` and `context`.
    generation: u64,
    /// Persistent-root slot keeping `context` alive; `usize::MAX` outside the
    /// runtime (there is no collector then).
    slot: usize,
    context: i64,
}

thread_local! {
    static CURRENT: RefCell<Option<CurrentContext>> = const { RefCell::new(None) };
}

fn method(name: &str, function: &'static str, arity: usize) -> (String, i64, MethodKind) {
    (
        name.to_string(),
        value::native_function_new("_contextvars", function, arity),
        MethodKind::Normal,
    )
}

/// Builds a native method value and roots it before the next allocation.
fn rooted_method(name: &str, function: &'static str, arity: usize) -> (String, i64, MethodKind) {
    let entry = method(name, function, arity);
    heap::add_thread_root(entry.1);
    entry
}

fn build_classes(generation: u64) -> ClassCache {
    let var_class = value::class_new_full(
        "ContextVar".to_string(),
        NONE,
        vec![
            rooted_method("get", "_contextvars_var_get", 1),
            rooted_method("set", "_contextvars_var_set", 2),
            rooted_method("reset", "_contextvars_var_reset", 2),
            rooted_method("__repr__", "_contextvars_var_repr", 1),
        ],
        Vec::new(),
    );
    heap::add_thread_root(var_class);

    let context_class = value::class_new_full(
        "Context".to_string(),
        NONE,
        vec![
            rooted_method("__init__", "_contextvars_ctx_init", usize::MAX),
            rooted_method("run", "_contextvars_ctx_run", usize::MAX),
            rooted_method("copy", "_contextvars_ctx_copy", 1),
            rooted_method("get", "_contextvars_ctx_get", 3),
            rooted_method("__getitem__", "_contextvars_ctx_getitem", 2),
            rooted_method("__setitem__", "_contextvars_ctx_setitem", 3),
            rooted_method("__delitem__", "_contextvars_ctx_delitem", 2),
            rooted_method("__contains__", "_contextvars_ctx_contains", 2),
            rooted_method("__len__", "_contextvars_ctx_len", 1),
            rooted_method("keys", "_contextvars_ctx_keys", 1),
            rooted_method("values", "_contextvars_ctx_values", 1),
            rooted_method("items", "_contextvars_ctx_items", 1),
            rooted_method("__repr__", "_contextvars_ctx_repr", 1),
        ],
        Vec::new(),
    );
    heap::add_thread_root(context_class);

    let missing_class = value::class_new_full(
        "MISSING".to_string(),
        NONE,
        vec![rooted_method("__repr__", "_contextvars_missing_repr", 1)],
        Vec::new(),
    );
    heap::add_thread_root(missing_class);
    let missing = value::instance_new(missing_class);
    heap::add_thread_root(missing);

    let token_class = value::class_new_full(
        "Token".to_string(),
        NONE,
        vec![
            rooted_method("__init__", "_contextvars_token_init", usize::MAX),
            rooted_method("__repr__", "_contextvars_token_repr", 1),
        ],
        vec![("MISSING".to_string(), missing)],
    );
    heap::add_thread_root(token_class);

    ClassCache {
        generation,
        var_class,
        context_class,
        token_class,
        missing,
    }
}

/// Returns a cached class value, rebuilding the cache when the heap changed.
fn cached_class(select: fn(&ClassCache) -> i64) -> i64 {
    let generation = heap::generation();
    let cached = CLASS_CACHE.with(|cell| {
        let cache = cell.borrow();
        if cache.generation == generation {
            select(&cache)
        } else {
            NO_CLASS
        }
    });
    if cached != NO_CLASS {
        return cached;
    }
    let classes = build_classes(generation);
    let selected = select(&classes);
    CLASS_CACHE.with(|cell| *cell.borrow_mut() = classes);
    selected
}

fn var_class() -> i64 {
    cached_class(|cache| cache.var_class)
}

fn context_class() -> i64 {
    cached_class(|cache| cache.context_class)
}

fn token_class() -> i64 {
    cached_class(|cache| cache.token_class)
}

fn missing_sentinel() -> i64 {
    cached_class(|cache| cache.missing)
}

fn is_instance_of(value: i64, class_name: &str) -> bool {
    value::instance_class_name(value) == Some(class_name)
}

/// Materialises a module constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "Context" => context_class(),
        "Token" => token_class(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'contextvars' has no attribute '{name}'"),
        ),
    }
}

/// Dispatches a native `_contextvars`/`contextvars` call (public constructors,
/// `copy_context` and the native method bodies installed on the classes).
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "ContextVar" => new_var(args),
        "copy_context" => copy_context(),
        "_contextvars_var_get" => var_get(args),
        "_contextvars_var_set" => var_set(args),
        "_contextvars_var_reset" => var_reset(args),
        "_contextvars_var_repr" => var_repr(args),
        "_contextvars_token_init" => exc::raise_with_message(
            TYPE_ERROR,
            "Token objects are created by ContextVar.set(); they cannot be instantiated directly",
        ),
        "_contextvars_token_repr" => token_repr(args),
        "_contextvars_ctx_init" => ctx_init(args),
        "_contextvars_ctx_run" => ctx_run(args),
        "_contextvars_ctx_copy" => ctx_copy(args),
        "_contextvars_ctx_get" => ctx_get(args),
        "_contextvars_ctx_getitem" => ctx_getitem(args),
        "_contextvars_ctx_setitem" => ctx_setitem(args),
        "_contextvars_ctx_delitem" => ctx_delitem(args),
        "_contextvars_ctx_contains" => ctx_contains(args),
        "_contextvars_ctx_len" => ctx_len(args),
        "_contextvars_ctx_keys" => ctx_keys(args),
        "_contextvars_ctx_values" => ctx_values(args),
        "_contextvars_ctx_items" => ctx_items(args),
        "_contextvars_ctx_repr" => ctx_repr(args),
        "_contextvars_missing_repr" => value::str_alloc("MISSING".to_string()),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'contextvars' has no attribute '{function}'"),
        ),
    }
}

/// Allocates an empty `Context` with its own data dict.
fn fresh_context() -> i64 {
    let data = value::dict_new(Vec::new());
    let roots = heap::root_values(&[data]);
    let context = value::instance_new(context_class());
    value::instance_setattr(context, CTX_DATA.to_string(), data);
    value::instance_setattr(context, CTX_ENTERED.to_string(), FALSE);
    heap::release_roots(roots);
    context
}

/// The thread's current context, creating an empty one on first use.
fn current_state() -> CurrentContext {
    let generation = heap::generation();
    let cached = CURRENT.with(|cell| *cell.borrow());
    if let Some(state) = cached {
        if state.generation == generation {
            return state;
        }
    }
    let context = fresh_context();
    let slot = heap::add_persistent_root(context);
    let state = CurrentContext {
        generation,
        slot,
        context,
    };
    CURRENT.with(|cell| *cell.borrow_mut() = Some(state));
    state
}

fn current_context() -> i64 {
    current_state().context
}

/// Makes `context` current for this thread and returns the previous state.
fn enter_current(context: i64) -> CurrentContext {
    let previous = current_state();
    if previous.slot != usize::MAX {
        heap::set_persistent_root(previous.slot, context);
    }
    CURRENT.with(|cell| {
        *cell.borrow_mut() = Some(CurrentContext {
            generation: previous.generation,
            slot: previous.slot,
            context,
        })
    });
    previous
}

/// Restores the state [`enter_current`] replaced.
fn leave_current(previous: CurrentContext) {
    if previous.generation != heap::generation() {
        // A runtime reset invalidated the slot; a later read creates a fresh
        // empty context for this thread.
        CURRENT.with(|cell| *cell.borrow_mut() = None);
        return;
    }
    if previous.slot != usize::MAX {
        heap::set_persistent_root(previous.slot, previous.context);
    }
    CURRENT.with(|cell| *cell.borrow_mut() = Some(previous));
}

/// Returns the `Context` data dict, creating it for a bare class instance.
fn context_data(context: i64) -> Option<i64> {
    if !is_instance_of(context, "Context") {
        return None;
    }
    if let Some(data) = value::instance_attr(context, CTX_DATA) {
        if value::as_dict(data).is_some() {
            return Some(data);
        }
    }
    let data = value::dict_new(Vec::new());
    value::instance_setattr(context, CTX_DATA.to_string(), data);
    Some(data)
}

fn data_lookup(data: i64, var: i64) -> Option<i64> {
    value::as_dict(data)?
        .iter()
        .find(|(key, _)| container::equals(*key, var))
        .map(|(_, value)| *value)
}

fn data_store(data: i64, var: i64, value: i64) {
    container::dict_set(data, var, value);
}

fn data_remove(data: i64, var: i64) {
    if data_lookup(data, var).is_some() {
        container::delete_subscript(data, var);
    }
}

fn context_error() -> i64 {
    exc::raise_with_message(TYPE_ERROR, "argument must be a Context object")
}

fn var_error() -> i64 {
    exc::raise_with_message(TYPE_ERROR, "argument must be a ContextVar object")
}

/// `ContextVar(name, default=...)`: allocates the native variable object.
///
/// A one-argument call has no default and `get()` raises `LookupError` when the
/// variable is unset. The frontend pads keyword calls with `None`, so an
/// explicit `default=None` is distinguished from an omitted one by the
/// argument count.
fn new_var(args: &[i64]) -> i64 {
    let name = args.first().copied().unwrap_or(NONE);
    if value::as_str(name).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "context variable name must be a str");
    }
    let var = value::instance_new(var_class());
    value::instance_setattr(var, VAR_NAME.to_string(), name);
    if args.len() >= 2 {
        value::instance_setattr(var, VAR_DEFAULT.to_string(), args[1]);
        value::instance_setattr(var, VAR_HAS_DEFAULT.to_string(), TRUE);
    } else {
        value::instance_setattr(var, VAR_DEFAULT.to_string(), NONE);
        value::instance_setattr(var, VAR_HAS_DEFAULT.to_string(), FALSE);
    }
    var
}

/// `ContextVar.get()`: the value in the current context or the default.
fn var_get(args: &[i64]) -> i64 {
    let var = args.first().copied().unwrap_or(NONE);
    if !is_instance_of(var, "ContextVar") {
        return var_error();
    }
    let context = current_context();
    let Some(data) = context_data(context) else {
        return context_error();
    };
    if let Some(found) = data_lookup(data, var) {
        return found;
    }
    if value::instance_attr(var, VAR_HAS_DEFAULT) == Some(TRUE) {
        return value::instance_attr(var, VAR_DEFAULT).unwrap_or(NONE);
    }
    let name = value::instance_attr(var, VAR_NAME).unwrap_or(NONE);
    exc::raise_with_message(
        LOOKUP_ERROR,
        &format!(
            "<ContextVar name={} at 0x{:x}>",
            value::repr(name),
            var as usize
        ),
    )
}

/// `ContextVar.set(value)`: stores the value in the current context and
/// returns a token that restores the previous binding.
fn var_set(args: &[i64]) -> i64 {
    let (var, new_value) = match (args.first().copied(), args.get(1).copied()) {
        (Some(var), Some(value)) => (var, value),
        _ => return exc::raise_with_message(TYPE_ERROR, "set() requires a value"),
    };
    if !is_instance_of(var, "ContextVar") {
        return var_error();
    }
    let context = current_context();
    let result = {
        let _guard = context_lock();
        let Some(data) = context_data(context) else {
            return context_error();
        };
        let previous = data_lookup(data, var);
        data_store(data, var, new_value);
        if exc::pending() {
            return NONE;
        }
        previous
    };
    let has_old = result.is_some();
    let token = value::instance_new(token_class());
    value::instance_setattr(token, TOKEN_VAR.to_string(), var);
    value::instance_setattr(
        token,
        TOKEN_OLD_VALUE.to_string(),
        result.unwrap_or_else(missing_sentinel),
    );
    value::instance_setattr(token, TOKEN_HAS_OLD.to_string(), value::bool_value(has_old));
    value::instance_setattr(token, TOKEN_USED.to_string(), FALSE);
    token
}

/// `ContextVar.reset(token)`: restores the binding captured by `set`.
fn var_reset(args: &[i64]) -> i64 {
    let (var, token) = match (args.first().copied(), args.get(1).copied()) {
        (Some(var), Some(token)) => (var, token),
        _ => return exc::raise_with_message(TYPE_ERROR, "reset() requires a token"),
    };
    if !is_instance_of(var, "ContextVar") {
        return var_error();
    }
    if !is_instance_of(token, "Token") {
        return exc::raise_with_message(TYPE_ERROR, "reset() argument must be a Token");
    }
    if value::instance_attr(token, TOKEN_USED) == Some(TRUE) {
        return exc::raise_with_message(RUNTIME_ERROR, "Token has already been used once");
    }
    if value::instance_attr(token, TOKEN_VAR) != Some(var) {
        return exc::raise_with_message(VALUE_ERROR, "Token was created by a different ContextVar");
    }
    let context = current_context();
    let _guard = context_lock();
    let Some(data) = context_data(context) else {
        return context_error();
    };
    value::instance_setattr(token, TOKEN_USED.to_string(), TRUE);
    if value::instance_attr(token, TOKEN_HAS_OLD) == Some(TRUE) {
        let old = value::instance_attr(token, TOKEN_OLD_VALUE).unwrap_or(NONE);
        data_store(data, var, old);
    } else {
        data_remove(data, var);
    }
    NONE
}

fn var_repr(args: &[i64]) -> i64 {
    let var = args.first().copied().unwrap_or(NONE);
    let name = value::instance_attr(var, VAR_NAME).unwrap_or(NONE);
    value::str_alloc(format!(
        "<ContextVar name={} at 0x{:x}>",
        value::repr(name),
        var as usize
    ))
}

fn token_repr(args: &[i64]) -> i64 {
    let token = args.first().copied().unwrap_or(NONE);
    let var = value::instance_attr(token, TOKEN_VAR).unwrap_or(NONE);
    let var_name = value::instance_attr(var, VAR_NAME).unwrap_or(NONE);
    value::str_alloc(format!(
        "<Token var=<ContextVar name={} at 0x{:x}> at 0x{:x}>",
        value::repr(var_name),
        var as usize,
        token as usize
    ))
}

/// `Context()` accepts no arguments (the data dict is created lazily).
fn ctx_init(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return exc::raise_with_message(TYPE_ERROR, "Context() takes no arguments");
    }
    NONE
}

/// `Context.run(callable, *args)`: run `callable` with this context active.
fn ctx_run(args: &[i64]) -> i64 {
    let context = args.first().copied().unwrap_or(NONE);
    if context_data(context).is_none() {
        return context_error();
    }
    let Some(callable) = args.get(1).copied() else {
        return exc::raise_with_message(TYPE_ERROR, "run() requires a callable");
    };
    {
        let _guard = context_lock();
        if value::instance_attr(context, CTX_ENTERED) == Some(TRUE) {
            return exc::raise_with_message(
                RUNTIME_ERROR,
                "cannot enter context: Context is already entered",
            );
        }
        value::instance_setattr(context, CTX_ENTERED.to_string(), TRUE);
    }
    let previous = enter_current(context);
    let result = crate::builtins::call(callable, args.len() - 2, args[2..].as_ptr());
    leave_current(previous);
    {
        let _guard = context_lock();
        value::instance_setattr(context, CTX_ENTERED.to_string(), FALSE);
    }
    result
}

fn snapshot_pairs(context: i64) -> Option<Vec<(i64, i64)>> {
    let data = context_data(context)?;
    Some(value::as_dict(data)?.to_vec())
}

/// `copy_context()`: a new context holding the current bindings.
fn copy_context() -> i64 {
    let context = current_context();
    let pairs = {
        let _guard = context_lock();
        match snapshot_pairs(context) {
            Some(pairs) => pairs,
            None => return context_error(),
        }
    };
    context_from_pairs(pairs)
}

fn context_from_pairs(pairs: Vec<(i64, i64)>) -> i64 {
    let data = value::dict_new(pairs);
    let roots = heap::root_values(&[data]);
    let context = value::instance_new(context_class());
    value::instance_setattr(context, CTX_DATA.to_string(), data);
    value::instance_setattr(context, CTX_ENTERED.to_string(), FALSE);
    heap::release_roots(roots);
    context
}

fn ctx_copy(args: &[i64]) -> i64 {
    let context = args.first().copied().unwrap_or(NONE);
    let pairs = {
        let _guard = context_lock();
        match snapshot_pairs(context) {
            Some(pairs) => pairs,
            None => return context_error(),
        }
    };
    context_from_pairs(pairs)
}

fn ctx_get(args: &[i64]) -> i64 {
    let (context, var) = match (args.first().copied(), args.get(1).copied()) {
        (Some(context), Some(var)) => (context, var),
        _ => return exc::raise_with_message(TYPE_ERROR, "get() requires a ContextVar"),
    };
    let _guard = context_lock();
    let Some(data) = context_data(context) else {
        return context_error();
    };
    if let Some(found) = data_lookup(data, var) {
        return found;
    }
    if let Some(default) = args.get(2) {
        return *default;
    }
    let name = value::instance_attr(var, VAR_NAME).unwrap_or(NONE);
    exc::raise_with_message(
        LOOKUP_ERROR,
        &format!(
            "<ContextVar name={} at 0x{:x}>",
            value::repr(name),
            var as usize
        ),
    )
}

fn ctx_getitem(args: &[i64]) -> i64 {
    let (context, var) = match (args.first().copied(), args.get(1).copied()) {
        (Some(context), Some(var)) => (context, var),
        _ => return exc::raise_with_message(TYPE_ERROR, "__getitem__ requires a ContextVar"),
    };
    let _guard = context_lock();
    let Some(data) = context_data(context) else {
        return context_error();
    };
    match data_lookup(data, var) {
        Some(found) => found,
        None => exc::raise_with_message(
            exc::KEY_ERROR,
            &value::repr(value::instance_attr(var, VAR_NAME).unwrap_or(NONE)),
        ),
    }
}

fn ctx_setitem(args: &[i64]) -> i64 {
    let (context, var, new_value) = match (
        args.first().copied(),
        args.get(1).copied(),
        args.get(2).copied(),
    ) {
        (Some(context), Some(var), Some(value)) => (context, var, value),
        _ => {
            return exc::raise_with_message(
                TYPE_ERROR,
                "__setitem__ requires a ContextVar and a value",
            )
        }
    };
    let _guard = context_lock();
    let Some(data) = context_data(context) else {
        return context_error();
    };
    data_store(data, var, new_value);
    NONE
}

fn ctx_delitem(args: &[i64]) -> i64 {
    let (context, var) = match (args.first().copied(), args.get(1).copied()) {
        (Some(context), Some(var)) => (context, var),
        _ => return exc::raise_with_message(TYPE_ERROR, "__delitem__ requires a ContextVar"),
    };
    let _guard = context_lock();
    let Some(data) = context_data(context) else {
        return context_error();
    };
    if data_lookup(data, var).is_none() {
        return exc::raise_with_message(
            exc::KEY_ERROR,
            &value::repr(value::instance_attr(var, VAR_NAME).unwrap_or(NONE)),
        );
    }
    data_remove(data, var);
    NONE
}

fn ctx_contains(args: &[i64]) -> i64 {
    let (context, var) = match (args.first().copied(), args.get(1).copied()) {
        (Some(context), Some(var)) => (context, var),
        _ => return value::bool_value(false),
    };
    let _guard = context_lock();
    let found = context_data(context)
        .and_then(|data| data_lookup(data, var))
        .is_some();
    value::bool_value(found)
}

fn ctx_len(args: &[i64]) -> i64 {
    let context = args.first().copied().unwrap_or(NONE);
    let _guard = context_lock();
    let Some(data) = context_data(context) else {
        return context_error();
    };
    ops::int_result(value::as_dict(data).map_or(0, |pairs| pairs.len()) as i64)
}

fn ctx_keys(args: &[i64]) -> i64 {
    let context = args.first().copied().unwrap_or(NONE);
    let _guard = context_lock();
    let Some(pairs) = snapshot_pairs(context) else {
        return context_error();
    };
    value::list_new(pairs.into_iter().map(|(key, _)| key).collect())
}

fn ctx_values(args: &[i64]) -> i64 {
    let context = args.first().copied().unwrap_or(NONE);
    let _guard = context_lock();
    let Some(pairs) = snapshot_pairs(context) else {
        return context_error();
    };
    value::list_new(pairs.into_iter().map(|(_, value)| value).collect())
}

fn ctx_items(args: &[i64]) -> i64 {
    let context = args.first().copied().unwrap_or(NONE);
    let _guard = context_lock();
    let Some(pairs) = snapshot_pairs(context) else {
        return context_error();
    };
    value::list_new(
        pairs
            .into_iter()
            .map(|(key, value)| value::tuple_new(vec![key, value]))
            .collect(),
    )
}

fn ctx_repr(args: &[i64]) -> i64 {
    let context = args.first().copied().unwrap_or(NONE);
    value::str_alloc(format!("<Context at 0x{:x}>", context as usize))
}
