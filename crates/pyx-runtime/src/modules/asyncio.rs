//! `asyncio`: eager, sequential compatibility layer.
//!
//! PyX has no coroutines, tasks or event loop: `async def` bodies execute
//! immediately when called and `await` is a no-op at the IR level. This module
//! therefore only provides the three documented entry points:
//!
//! * `asyncio.run(value)` returns `value` unchanged;
//! * `asyncio.sleep(seconds)` blocks the calling thread (parking it for the
//!   collector) and returns `None`;
//! * `asyncio.gather(iterable)` materialises the elements into a list in
//!   iteration order.
//!
//! Everything else resolves to `AttributeError` through the spec table, so no
//! unsupported API silently behaves like a coroutine. There is no real
//! concurrency: `gather` does not start tasks.

use std::time::Duration;

use crate::exc::{self, ATTRIBUTE_ERROR, OVERFLOW_ERROR, VALUE_ERROR};
use crate::heap;
use crate::iter;
use crate::value::{self, NONE};

/// Failure path for a member that exists in the spec table but not here.
fn missing_attribute(function: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'asyncio' has no attribute '{function}'"),
    )
}

/// `asyncio.run(value)`: eager mode has no coroutine object, so the argument is
/// already the fully evaluated result.
fn run(args: &[i64]) -> i64 {
    args[0]
}

/// `asyncio.sleep(seconds)`: blocking sleep; no suspension point.
fn sleep(args: &[i64]) -> i64 {
    match super::number(args[0]) {
        Ok(seconds) => {
            if seconds.is_nan() || seconds < 0.0 {
                exc::raise_with_message(VALUE_ERROR, "sleep length must be non-negative")
            } else if !seconds.is_finite() || seconds >= u64::MAX as f64 {
                exc::raise_with_message(OVERFLOW_ERROR, "sleep length is too large")
            } else {
                // Park for the collector while sleeping; no runtime lock is
                // held across the block.
                heap::blocking_enter();
                std::thread::sleep(Duration::from_secs_f64(seconds));
                heap::blocking_leave();
                NONE
            }
        }
        Err(err) => err,
    }
}

/// `asyncio.gather(iterable)`: materialises every element in iteration order.
/// The elements are collected sequentially; nothing runs concurrently.
fn gather(args: &[i64]) -> i64 {
    let iterator = iter::iter_new(args[0]);
    if exc::pending() {
        return NONE;
    }
    let result = value::list_new(Vec::new());
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(item);
            crate::heap::write_barrier_edge(result, item);
        }
    }
    result
}

/// Module-level function dispatch. Arity is validated against the shared spec
/// before this is called.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "run" => run(args),
        "sleep" => sleep(args),
        "gather" => gather(args),
        _ => missing_attribute(function),
    }
}
