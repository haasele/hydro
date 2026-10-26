//! `atexit`: LIFO shutdown callbacks.
//!
//! `register` appends a callback (with its positional arguments) to a
//! process-wide stack, `unregister` removes every registration of a callback,
//! and `_run_exitfuncs` runs the stack from the top, clearing each entry
//! before calling it so a callback that registers another callback still sees
//! the CPython "run until empty" behaviour.
//!
//! Error isolation: an exception raised by a callback is reported on stderr
//! (`Exception ignored in atexit callback: ...`) and cleared; the remaining
//! callbacks still run, exactly like CPython.
//!
//! GC safety: each registration is stored as a real Python tuple
//! `(callback, arg0, ...)` and installed as a collector persistent root while
//! it is registered, so the callback and its arguments stay reachable even
//! when no Python frame mentions them. The stack is dropped when the runtime
//! heap generation changes (`hydro_runtime_init`), because the old roots and
//! tuples belong to the freed heap.
//!
//! Documented limit: persistent roots belong to the registering thread, so
//! `_run_exitfuncs` must run on that thread. Running it elsewhere raises
//! `RuntimeError` instead of dereferencing a callback whose root may already
//! be gone.

use std::io::Write as _;
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::builtins;
use crate::exc::{self, ATTRIBUTE_ERROR, RUNTIME_ERROR, TYPE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

struct Entry {
    /// Persistent-root slot that keeps `call` alive, or `usize::MAX` when the
    /// runtime is not active.
    slot: usize,
    /// Id of the thread whose root table owns `slot`.
    thread: i64,
    /// `(callback, arg0, ...)` tuple.
    call: i64,
}

struct State {
    generation: u64,
    entries: Vec<Entry>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        generation: 0,
        entries: Vec::new(),
    })
});

fn state() -> MutexGuard<'static, State> {
    let mut guard = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = heap::generation();
    if guard.generation != generation {
        // The heap reset already dropped the persistent roots and the tuples
        // they referenced; drop the Rust-side bookkeeping with them.
        guard.entries.clear();
        guard.generation = generation;
    }
    guard
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn attribute_error(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'atexit' has no attribute '{name}'"),
    )
}

/// Whether a value can be called by the runtime's ordinary call path.
fn is_callable(value: i64) -> bool {
    value::as_function(value).is_some() || value::as_class(value).is_some()
}

/// `atexit.register(func, *args)`: stores the call and returns `func`.
fn register(args: &[i64]) -> i64 {
    let Some(&function) = args.first() else {
        return type_error("register() requires a callable");
    };
    if !is_callable(function) {
        return type_error(&format!(
            "the first argument must be callable, not {}",
            value::type_name(function)
        ));
    }
    let call = value::tuple_new(args.to_vec());
    let slot = heap::add_persistent_root(call);
    let thread = heap::current_thread_id();
    state().entries.push(Entry { slot, thread, call });
    function
}

/// `atexit.unregister(func)`: removes every registration of `func`.
fn unregister(function: i64) -> i64 {
    let removed = {
        let mut state = state();
        let mut removed = Vec::new();
        state.entries.retain(|entry| {
            let keep = tuple_callback(entry.call) != Some(function);
            if !keep {
                removed.push((entry.slot, entry.thread));
            }
            keep
        });
        removed
    };
    for (slot, thread) in removed {
        release(slot, thread);
    }
    NONE
}

/// The callback stored at index 0 of a registration tuple.
fn tuple_callback(call: i64) -> Option<i64> {
    value::as_tuple(call).and_then(|items| items.first().copied())
}

/// Releases a registration's root. Persistent slots belong to the thread
/// that created them, so a foreign slot is left alone (its thread's
/// registration or the next runtime reset drops it).
fn release(slot: usize, thread: i64) {
    if slot != usize::MAX && heap::current_thread_id() == thread {
        heap::remove_persistent_root(slot);
    }
}

/// Runs a registration tuple: `callback(arg0, ...)`.
fn invoke(call: i64) {
    let Some(items) = value::as_tuple(call) else {
        return;
    };
    let Some((&function, rest)) = items.split_first() else {
        return;
    };
    // `call` is still installed as a persistent root, so `items`/`rest` stay
    // valid across the allocations the callback may make.
    builtins::call(function, rest.len(), rest.as_ptr());
}

fn report_pending() {
    let pending = exc::pending_value();
    let text = value::to_string(pending);
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(stderr, "Exception ignored in atexit callback: {text}");
    let _ = stderr.flush();
    exc::clear();
}

/// `atexit._run_exitfuncs()`: drains the stack in LIFO order.
fn run_exitfuncs() -> i64 {
    let current = heap::current_thread_id();
    loop {
        let entry = {
            let mut state = state();
            if state.entries.iter().any(|entry| entry.thread != current) {
                return exc::raise_with_message(
                    RUNTIME_ERROR,
                    "atexit callbacks must run on the thread that registered them",
                );
            }
            state.entries.pop()
        };
        let Some(entry) = entry else {
            return NONE;
        };
        invoke(entry.call);
        release(entry.slot, entry.thread);
        if exc::pending() {
            report_pending();
        }
    }
}

/// `atexit._clear()`: drops every registration without calling it.
fn clear() -> i64 {
    let entries = {
        let mut state = state();
        std::mem::take(&mut state.entries)
    };
    for entry in entries {
        release(entry.slot, entry.thread);
    }
    NONE
}

/// Dispatches a native `atexit` function. Arity is enforced by the spec.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "register" => register(args),
        "unregister" => unregister(args.first().copied().unwrap_or(NONE)),
        "_run_exitfuncs" => run_exitfuncs(),
        "_clear" => clear(),
        _ => attribute_error(function),
    }
}
