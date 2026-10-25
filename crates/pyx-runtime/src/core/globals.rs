//! Module-level globals.
//!
//! PyX compiles each module body straight-line and keeps no Python module
//! object for user modules, so `global`/module-scope names need a side store
//! that survives across functions. The store is thread-local, keyed by
//! `module + "\0" + name`, mirroring CPython's per-module dictionaries.
//!
//! Values are owned by Rust memory the collector cannot scan, so every stored
//! value is also installed as a thread root. Conservative retention is enough
//! for this slice: replaced values stay rooted until the runtime resets.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::exc::{self, NAME_ERROR, RUNTIME_ERROR};
use crate::heap;
use crate::value::NONE;

thread_local! {
    /// Module globals of the current thread.
    static GLOBALS: RefCell<HashMap<String, i64>> = RefCell::new(HashMap::new());
}

/// Builds the store key. The NUL separator cannot occur in a Python
/// identifier, so different `(module, name)` pairs never collide.
fn key(module: &str, name: &str) -> String {
    let mut key = String::with_capacity(module.len() + 1 + name.len());
    key.push_str(module);
    key.push('\0');
    key.push_str(name);
    key
}

/// `module.name = value`: inserts or replaces the global.
pub(crate) fn set(module: &str, name: &str, value: i64) {
    GLOBALS.with(|globals| {
        globals.borrow_mut().insert(key(module, name), value);
    });
    // The map lives in Rust memory the collector cannot trace; keep the value
    // alive for as long as the entry (and, conservatively, beyond it).
    heap::add_thread_root(value);
}

/// Reads `module.name` without raising; used by runtime import fallbacks.
pub(crate) fn try_get(module: &str, name: &str) -> Option<i64> {
    GLOBALS.with(|globals| globals.borrow().get(&key(module, name)).copied())
}

/// Reads `module.name`. A missing entry raises `RuntimeError`, matching the
/// runtime's use of `RuntimeError` for internal name-resolution failures.
pub(crate) fn get(module: &str, name: &str) -> i64 {
    let found = GLOBALS.with(|globals| globals.borrow().get(&key(module, name)).copied());
    match found {
        Some(value) => value,
        None => exc::raise_with_message(
            RUNTIME_ERROR,
            &format!("name '{name}' is not defined in module '{module}'"),
        ),
    }
}

/// Forgets every stored global (a fresh program run).
pub(crate) fn reset() {
    GLOBALS.with(|globals| globals.borrow_mut().clear());
}

/// Removes `module.name`, reporting whether the binding existed.
pub(crate) fn remove(module: &str, name: &str) -> bool {
    GLOBALS.with(|globals| globals.borrow_mut().remove(&key(module, name)).is_some())
}

/// `del module.name`: removes the binding and returns the ABI dummy value.
/// A missing binding raises `NameError`, like CPython's `del` of an undefined
/// module name.
pub(crate) fn delete_value(module: &str, name: &str) -> i64 {
    if remove(module, name) {
        NONE
    } else {
        exc::raise_with_message(NAME_ERROR, &format!("name '{name}' is not defined"))
    }
}

/// Stores `value` and returns the ABI dummy value (`None`).
pub(crate) fn set_value(module: &str, name: &str, value: i64) -> i64 {
    set(module, name, value);
    NONE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{self, encode_int};

    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn pending_message() -> String {
        value::to_string(exc::pending_value())
    }

    #[test]
    fn set_and_get_roundtrip() {
        reset();
        exc::init();
        assert_eq!(set_value("app", "counter", int(7)), NONE);
        assert_eq!(get("app", "counter"), int(7));
        assert!(!exc::pending());

        // Replacing keeps the latest value.
        set("app", "counter", int(8));
        assert_eq!(get("app", "counter"), int(8));
    }

    #[test]
    fn missing_global_raises() {
        reset();
        exc::init();
        assert_eq!(get("app", "missing"), NONE);
        assert_eq!(exc::pending_tag(), RUNTIME_ERROR as i64);
        assert_eq!(
            pending_message(),
            "name 'missing' is not defined in module 'app'"
        );
    }

    #[test]
    fn modules_are_separate_namespaces() {
        reset();
        exc::init();
        set("first", "value", int(1));
        set("second", "value", int(2));
        assert_eq!(get("first", "value"), int(1));
        assert_eq!(get("second", "value"), int(2));
        // A name only resolves in its own module.
        assert_eq!(get("third", "value"), NONE);
        assert_eq!(exc::pending_tag(), RUNTIME_ERROR as i64);

        exc::init();
        reset();
        assert_eq!(get("first", "value"), NONE);
        assert_eq!(exc::pending_tag(), RUNTIME_ERROR as i64);
    }
}
