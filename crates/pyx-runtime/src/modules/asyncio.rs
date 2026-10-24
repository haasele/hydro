//! `asyncio`: cooperative event loop over stackful coroutine fibers.
//!
//! * `asyncio.run(coro)` drives the root coroutine: ready tasks run until one
//!   suspends in `asyncio.sleep`, in which case the loop sleeps until the
//!   earliest timer and resumes it.
//! * `asyncio.sleep(secs)` inside a fiber suspends only that fiber with an
//!   internal sleep request; the loop keeps running other tasks. Outside a
//!   fiber (and outside `asyncio.run`) it keeps the historical blocking
//!   behaviour.
//! * `asyncio.gather(iterable)` starts every coroutine in the iterable as a
//!   concurrent task, suspends the caller until all finish and returns the
//!   results in input order. The first failing task aborts the gather and its
//!   exception propagates.
//!
//! Everything else resolves to `AttributeError` through the spec table. There
//! is one single-threaded loop with no GIL: tasks interleave only at explicit
//! suspension points.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::exc::{self, ATTRIBUTE_ERROR, OVERFLOW_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::fiber::{self, FiberKind, WaitRequest};
use crate::iter;
use crate::value::{self, NONE};

/// Failure path for a member that exists in the spec table but not here.
fn missing_attribute(function: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'asyncio' has no attribute '{function}'"),
    )
}

/// Cooperative scheduler state.
///
/// Every value scheduled on the driver is also appended to `roots`, a heap
/// list that keeps it reachable for the conservative collector while it sits
/// in a Rust `Vec` (whose buffer the collector does not scan).
struct Driver {
    roots: i64,
    ready: VecDeque<i64>,
    timers: Vec<(Instant, i64)>,
    joins: Vec<(i64, Vec<i64>)>,
}

impl Driver {
    fn new() -> Driver {
        Driver {
            roots: value::list_new(Vec::new()),
            ready: VecDeque::new(),
            timers: Vec::new(),
            joins: Vec::new(),
        }
    }

    fn root(&mut self, task: i64) {
        if let Some(mut items) = value::list_items_mut(self.roots) {
            items.push(task);
        }
        crate::heap::write_barrier_edge(self.roots, task);
    }

    fn push_ready(&mut self, task: i64) {
        self.root(task);
        self.ready.push_back(task);
    }

    fn push_timer(&mut self, when: Instant, task: i64) {
        self.root(task);
        self.timers.push((when, task));
    }

    /// Runs scheduled work until `finished` reports completion or no work is
    /// left. Returns whether `finished` was reached.
    fn run_until(&mut self, finished: &dyn Fn(&Driver) -> bool) -> bool {
        loop {
            if finished(self) {
                return true;
            }
            if let Some(task) = self.ready.pop_front() {
                self.step(task);
                continue;
            }
            if self.timers.is_empty() {
                return finished(self);
            }
            self.wait_for_timer();
        }
    }

    /// Advances one task by one suspension interval.
    fn step(&mut self, task: i64) {
        if fiber::is_done(task) {
            self.complete(task);
            return;
        }
        match fiber::resume_value(task) {
            Some(true) => self.complete(task),
            Some(false) => match fiber::take_request(task) {
                Some(WaitRequest::Sleep(duration)) => {
                    let when = Instant::now() + duration;
                    self.push_timer(when, task);
                }
                Some(WaitRequest::Join(tasks)) => {
                    let mut pending = Vec::new();
                    for dependency in tasks {
                        if fiber::is_done(dependency) {
                            continue;
                        }
                        self.push_ready(dependency);
                        pending.push(dependency);
                    }
                    if pending.is_empty() {
                        self.push_ready(task);
                    } else {
                        self.joins.push((task, pending));
                    }
                }
                // A bare suspension (a gathered generator yielding): treat it
                // as runnable again so iteration makes progress.
                None => self.push_ready(task),
            },
            None => {}
        }
    }

    /// Records a finished task and wakes every join that was waiting for it.
    fn complete(&mut self, task: i64) {
        let failed = fiber::has_error(task);
        let mut index = 0;
        while index < self.joins.len() {
            let wake = {
                let (_, dependencies) = &self.joins[index];
                if !dependencies.contains(&task) {
                    false
                } else if failed {
                    // Abort the gather as soon as one of its tasks fails.
                    true
                } else {
                    dependencies.iter().all(|&item| fiber::is_done(item))
                }
            };
            if wake {
                let (caller, _) = self.joins.swap_remove(index);
                self.push_ready(caller);
            } else {
                index += 1;
            }
        }
    }

    /// Sleeps until the earliest timer, parking the thread for the collector,
    /// then moves every due timer back to the ready queue.
    fn wait_for_timer(&mut self) {
        let Some(&(when, _)) = self.timers.iter().min_by_key(|(when, _)| *when) else {
            return;
        };
        let now = Instant::now();
        if when > now {
            fiber::blocking_sleep(when - now);
        }
        let now = Instant::now();
        let mut due = Vec::new();
        self.timers.retain(|&(when, task)| {
            if when <= now {
                due.push(task);
                false
            } else {
                true
            }
        });
        for task in due {
            self.ready.push_back(task);
        }
    }
}

/// Drives `tasks` to completion (or until one fails) at the top level.
///
/// Used by `asyncio.gather` outside a fiber and by `pyx_await` when a
/// coroutine propagates a join request.
pub(crate) fn drive_join_tasks(tasks: &[i64]) {
    let mut driver = Driver::new();
    let mut queued = false;
    for &task in tasks {
        if fiber::is_fiber(task) && !fiber::is_done(task) {
            driver.push_ready(task);
            queued = true;
        }
    }
    if !queued {
        return;
    }
    let _ = driver.run_until(&|_| {
        tasks
            .iter()
            .all(|&task| !fiber::is_fiber(task) || fiber::is_done(task))
            || tasks.iter().any(|&task| fiber::has_error(task))
    });
}

/// Runs scheduled work until `finished`; shared by `asyncio.run`.
fn drive(root: i64) -> bool {
    let mut driver = Driver::new();
    driver.push_ready(root);
    driver.run_until(&|_| fiber::is_done(root))
}

/// `asyncio.run(value)`: drives a coroutine to completion and returns its
/// result. Non-fiber values are returned unchanged (historical eager mode).
fn run(args: &[i64]) -> i64 {
    let value = args[0];
    if !fiber::is_fiber(value) {
        return value;
    }
    if fiber::kind(value) != Some(FiberKind::Coroutine) {
        return exc::raise_with_message(TYPE_ERROR, "asyncio.run() requires a coroutine");
    }
    if !drive(value) {
        return exc::raise_with_message(RUNTIME_ERROR, "event loop has no runnable tasks");
    }
    fiber::finish_value(value)
}

/// `asyncio.sleep(seconds)`: suspends the running fiber, or blocks the thread
/// when called outside a fiber.
fn sleep(args: &[i64]) -> i64 {
    match super::number(args[0]) {
        Ok(seconds) => {
            if seconds.is_nan() || seconds < 0.0 {
                exc::raise_with_message(VALUE_ERROR, "sleep length must be non-negative")
            } else if !seconds.is_finite() || seconds >= u64::MAX as f64 {
                exc::raise_with_message(OVERFLOW_ERROR, "sleep length is too large")
            } else {
                let duration = Duration::from_secs_f64(seconds);
                if fiber::current_value().is_some() {
                    fiber::suspend_current(Some(WaitRequest::Sleep(duration)))
                } else {
                    // Park for the collector while sleeping; no runtime lock
                    // is held across the block.
                    fiber::blocking_sleep(duration);
                    NONE
                }
            }
        }
        Err(err) => err,
    }
}

/// Materialises an iterable into a fresh list. The list itself keeps the
/// gathered coroutine objects reachable while they wait in Rust-side queues.
fn materialize(iterable: i64) -> i64 {
    let iterator = iter::iter_new(iterable);
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
        }
        crate::heap::write_barrier_edge(result, item);
    }
    result
}

/// `asyncio.gather(*awaitables)`: runs every coroutine concurrently.
///
/// The CPython spelling takes the awaitables as separate arguments; the
/// historical PyX spelling with one list/tuple argument is still accepted.
fn gather(args: &[i64]) -> i64 {
    let items = if args.len() == 1 && !fiber::is_fiber(args[0]) {
        materialize(args[0])
    } else {
        value::list_new(args.to_vec())
    };
    if exc::pending() {
        return NONE;
    }
    let tasks: Vec<i64> = value::as_list(items)
        .map(|values| {
            values
                .iter()
                .copied()
                .filter(|&item| fiber::is_fiber(item) && !fiber::is_done(item))
                .collect()
        })
        .unwrap_or_default();
    if !tasks.is_empty() {
        if fiber::current_value().is_some() {
            // The driver starts the tasks and resumes this fiber once they
            // are done (or one failed).
            fiber::suspend_current(Some(WaitRequest::Join(tasks)));
        } else {
            drive_join_tasks(&tasks);
        }
    }
    collect_results(items)
}

/// Collects gathered results in input order, propagating the first failure in
/// completion order.
fn collect_results(items: i64) -> i64 {
    let Some(values) = value::as_list(items) else {
        return NONE;
    };
    let count = values.len();
    let mut first_error: Option<i64> = None;
    for &item in values {
        if fiber::is_fiber(item) && fiber::has_error(item) {
            first_error = match first_error {
                Some(previous)
                    if fiber::finish_sequence(previous) <= fiber::finish_sequence(item) =>
                {
                    Some(previous)
                }
                _ => Some(item),
            };
        }
    }
    if let Some(task) = first_error {
        return fiber::raise_error(task);
    }
    // Build the result list incrementally. Reading `items` on every step
    // keeps the source list (and therefore every result value) reachable
    // across the allocations below.
    let result = value::list_new(Vec::new());
    let mut index = 0;
    while index < count {
        let Some(values) = value::as_list(items) else {
            return NONE;
        };
        let item = values[index];
        let result_value = if fiber::is_fiber(item) {
            if !fiber::is_done(item) {
                return exc::raise_with_message(RUNTIME_ERROR, "gather() task did not complete");
            }
            fiber::result_value(item)
        } else {
            item
        };
        if let Some(mut out) = value::list_items_mut(result) {
            out.push(result_value);
        }
        crate::heap::write_barrier_edge(result, result_value);
        index += 1;
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
