//! `asyncio`: cooperative event loop over stackful coroutine fibers.
//!
//! * `asyncio.run(coro)` drives the root coroutine on a persistent,
//!   thread-local loop: ready tasks run until one suspends in
//!   `asyncio.sleep`, in which case the loop sleeps until the earliest timer
//!   and resumes it. The loop state (queues and the heap list that roots every
//!   scheduled task) survives between `run` calls; `reset()` drops it during a
//!   runtime reset.
//! * `asyncio.create_task(coro)` schedules a coroutine on the running loop and
//!   returns it; a task is a coroutine fiber with `cancel`, `done`,
//!   `cancelled`, `result` and `exception` methods.
//! * `asyncio.current_task()` returns the task whose body is running.
//! * `asyncio.TaskGroup()` is a native class: `async with TaskGroup() as tg`
//!   waits for every `tg.create_task(...)` at exit, propagates the first
//!   failure and cancels the remaining tasks (including when the body raises).
//! * `asyncio.sleep(secs)` suspends only the calling fiber with an internal
//!   sleep request; the loop keeps running other tasks. Outside a fiber (and
//!   outside `asyncio.run`) it keeps the historical blocking behaviour.
//! * `asyncio.gather(*awaitables)` starts every coroutine in the iterable as a
//!   concurrent task, suspends the caller until all finish and returns the
//!   results in input order. The first failing task aborts the gather and its
//!   exception propagates.
//! * `asyncio.wait_for(awaitable, timeout)` schedules the awaitable and polls
//!   it until it finishes or the deadline passes (then cancels it and raises
//!   `TimeoutError`).
//! * `asyncio.timeout(delay)` is a native async context manager whose
//!   `__aexit__` raises `TimeoutError` once the deadline has passed.
//! * `asyncio.Runner()` wraps `run`/`close` and the synchronous context
//!   manager protocol over the thread's loop.
//! * `asyncio.staggered.staggered_race(coro_fns, delay)` runs the candidate
//!   coroutine functions in order and returns `(winner, index, exceptions)`.
//!
//! Known deviations: failures inside a `TaskGroup` propagate as the first
//! exception rather than as an `ExceptionGroup`; `Timeout` reports expiry when
//! the body returns instead of cancelling every suspension; and
//! `staggered_race` starts its candidates sequentially (the driver has no
//! select-style wait) while keeping the result contract.
//!
//! Everything else resolves to `AttributeError` through the spec table. There
//! is one single-threaded loop with no GIL: tasks interleave only at explicit
//! suspension points.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, CANCELLED_ERROR, OVERFLOW_ERROR, RUNTIME_ERROR, TIMEOUT_ERROR, TYPE_ERROR,
    VALUE_ERROR,
};
use crate::fiber::{self, FiberKind, WaitRequest};
use crate::iter;
use crate::value::{self, FALSE, NONE, TRUE};
use hydro_ir::MethodKind;

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
/// list that keeps it reachable for the collector while it sits in a Rust
/// `Vec` (whose buffer the collector does not scan). While the loop is alive
/// that list is registered as a persistent heap root, so a suspended task is
/// never collected between driver steps.
struct Driver {
    roots: i64,
    ready: VecDeque<i64>,
    timers: Vec<(Instant, i64)>,
    joins: Vec<(i64, Vec<i64>)>,
}

impl Driver {
    fn new() -> Driver {
        Driver::with_roots(value::list_new(Vec::new()))
    }

    fn with_roots(roots: i64) -> Driver {
        Driver {
            roots,
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
        if fiber::is_scheduled(task) {
            return;
        }
        fiber::set_scheduled(task, true);
        self.root(task);
        self.ready.push_back(task);
    }

    fn push_timer(&mut self, when: Instant, task: i64) {
        if fiber::is_scheduled(task) {
            return;
        }
        fiber::set_scheduled(task, true);
        self.root(task);
        self.timers.push((when, task));
    }

    /// Forces `task` onto the ready queue, removing a stale timer/ready entry
    /// first. Used by `Task.cancel()` to interrupt a sleep.
    fn wake(&mut self, task: i64) {
        self.timers.retain(|&(_, queued)| queued != task);
        self.ready.retain(|&queued| queued != task);
        self.joins.retain(|(caller, _)| *caller != task);
        fiber::set_scheduled(task, false);
        self.push_ready(task);
    }

    /// Pops the next runnable task, parking until the earliest timer when the
    /// ready queue is empty. `None` means the loop has no work left.
    fn next_task(&mut self) -> Option<i64> {
        loop {
            while let Some(task) = self.ready.pop_front() {
                fiber::set_scheduled(task, false);
                if fiber::is_done(task) {
                    // A cancelled-before-start task never runs; wake joins.
                    self.complete(task);
                    continue;
                }
                return Some(task);
            }
            // Timers of finished tasks must not keep the loop sleeping, and
            // a finished join waiter can never consume a wakeup.
            self.timers.retain(|&(_, task)| !fiber::is_done(task));
            self.joins.retain(|(caller, _)| !fiber::is_done(*caller));
            if self.timers.is_empty() {
                return None;
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
        fiber::resume_value(task);
        self.settle(task);
    }

    /// Schedules work after one resume: a sleep becomes a timer, a join waits
    /// for its dependencies, a bare suspension is immediately runnable.
    fn settle(&mut self, task: i64) {
        if fiber::is_done(task) {
            self.complete(task);
            return;
        }
        match fiber::take_request(task) {
            Some(WaitRequest::Sleep(duration)) => {
                let when = Instant::now() + duration;
                self.push_timer(when, task);
            }
            Some(WaitRequest::Join(tasks)) => {
                // The request was moved out of the fiber, so root the
                // dependencies while scheduling them (which may collect).
                let token = crate::heap::root_values(&tasks);
                let mut pending = Vec::new();
                for &dependency in &tasks {
                    if fiber::is_done(dependency) {
                        continue;
                    }
                    self.push_ready(dependency);
                    pending.push(dependency);
                }
                // A dependency that already failed wakes the caller
                // immediately; it may cancel or re-join the rest.
                let failed = pending.iter().any(|&item| fiber::has_error(item));
                if pending.is_empty() || failed {
                    self.push_ready(task);
                } else {
                    // Managed by the loop while it waits: a join dependency
                    // must not be treated as an unscheduled coroutine.
                    fiber::set_scheduled(task, true);
                    self.root(task);
                    self.joins.push((task, pending));
                }
                crate::heap::release_roots(token);
            }
            // A bare suspension (a gathered generator yielding): treat it as
            // runnable again so iteration makes progress.
            None => self.push_ready(task),
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
                fiber::set_scheduled(caller, false);
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

    /// Runs this (non-persistent) driver until `finished` or no work is left.
    fn run_local(&mut self, finished: &dyn Fn() -> bool) {
        loop {
            if finished() {
                return;
            }
            let Some(task) = self.next_task() else {
                return;
            };
            self.step(task);
        }
    }
}

// -------------------------------------------------------------------------
// Persistent loop
// -------------------------------------------------------------------------

/// Thread-local loop state shared by every `asyncio.run` on this thread.
struct LoopState {
    driver: Driver,
    /// Task whose body is running (`NONE` outside a step).
    current: i64,
    running: bool,
    /// Persistent heap-root slot holding `driver.roots`.
    root_slot: usize,
}

impl LoopState {
    fn new() -> LoopState {
        let driver = Driver::new();
        let root_slot = crate::heap::add_persistent_root(driver.roots);
        LoopState {
            driver,
            current: NONE,
            running: false,
            root_slot,
        }
    }

    /// Drops queued work but keeps the loop (and its persistent-root slot)
    /// alive for the next `run`.
    fn reset_work(&mut self) {
        let roots = value::list_new(Vec::new());
        crate::heap::set_persistent_root(self.root_slot, roots);
        self.driver = Driver::with_roots(roots);
        self.current = NONE;
    }
}

thread_local! {
    static LOOP: RefCell<Option<Box<LoopState>>> = const { RefCell::new(None) };
}

/// Ensures the thread-local loop exists. The driver's roots list becomes a
/// persistent heap root at creation.
fn ensure_loop() {
    LOOP.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Box::new(LoopState::new()));
        }
    });
}

fn with_loop<R>(f: impl FnOnce(&mut LoopState) -> R) -> Option<R> {
    LOOP.with(|cell| cell.borrow_mut().as_mut().map(|state| f(state)))
}

fn loop_running() -> bool {
    with_loop(|state| state.running).unwrap_or(false)
}

/// Drops the loop state. Called by `gc::heap::reset` before the heap is
/// freed; the persistent root itself is cleared by the heap reset.
pub(crate) fn reset() {
    LOOP.with(|cell| *cell.borrow_mut() = None);
}

/// Wakes a task whose cancellation was requested while it sat in a timer or
/// join. Outside a running loop this is a no-op; the cancellation stays
/// queued in the fiber and is delivered on its next resume.
pub(crate) fn wake_task(task: i64) {
    LOOP.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            if state.running {
                state.driver.wake(task);
            }
        }
    });
}

/// Resumes one task with the loop's current-task field pointing at it.
fn step_task(task: i64) {
    with_loop(|state| state.current = task);
    fiber::resume_value(task);
    with_loop(|state| {
        if state.current == task {
            state.current = NONE;
        }
        state.driver.settle(task);
    });
}

// -------------------------------------------------------------------------
// asyncio.run
// -------------------------------------------------------------------------

/// Drives the loop until `root` finishes. Returns whether it finished.
fn drive_root(root: i64) -> bool {
    loop {
        if fiber::is_done(root) {
            return true;
        }
        let Some(task) = with_loop(|state| state.driver.next_task()).flatten() else {
            return fiber::is_done(root);
        };
        step_task(task);
    }
}

/// Every fiber scheduled on this loop that has not finished yet.
fn unfinished_tasks() -> Vec<i64> {
    with_loop(|state| {
        value::as_list(state.driver.roots)
            .map(|items| {
                items
                    .iter()
                    .copied()
                    .filter(|&task| fiber::is_fiber(task) && !fiber::is_done(task))
                    .collect()
            })
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

/// Cancels every task still scheduled when `run` returns, then drives the
/// loop until they have all observed the cancellation. This mirrors CPython's
/// `asyncio.run` shutdown.
fn shutdown_leftovers(root: i64) {
    let leftovers: Vec<i64> = unfinished_tasks()
        .into_iter()
        .filter(|&task| task != root)
        .collect();
    if leftovers.is_empty() {
        return;
    }
    let token = crate::heap::root_values(&leftovers);
    for &task in &leftovers {
        fiber::cancel_value(task);
    }
    loop {
        let pending: Vec<i64> = unfinished_tasks()
            .into_iter()
            .filter(|&task| task != root)
            .collect();
        if pending.is_empty() {
            break;
        }
        let Some(task) = with_loop(|state| state.driver.next_task()).flatten() else {
            break;
        };
        step_task(task);
    }
    crate::heap::release_roots(token);
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
    ensure_loop();
    if loop_running() {
        return exc::raise_with_message(
            RUNTIME_ERROR,
            "asyncio.run() cannot be called from a running event loop",
        );
    }
    with_loop(|state| {
        state.running = true;
        state.current = NONE;
        state.driver.push_ready(value);
    });
    let finished = drive_root(value);
    if !finished {
        with_loop(|state| {
            state.running = false;
            state.current = NONE;
            state.reset_work();
        });
        return exc::raise_with_message(RUNTIME_ERROR, "event loop has no runnable tasks");
    }
    // The root finished; cancel whatever it left behind before tearing down.
    shutdown_leftovers(value);
    with_loop(|state| {
        state.running = false;
        state.current = NONE;
        state.reset_work();
    });
    // The driver queues were reset, so root the finished task while
    // `finish_value` possibly allocates the pending exception copy.
    let token = crate::heap::root_values(&[value]);
    let result = fiber::finish_value(value);
    crate::heap::release_roots(token);
    result
}

// -------------------------------------------------------------------------
// Tasks
// -------------------------------------------------------------------------

/// `asyncio.create_task(coro)`: schedules a coroutine on the running loop.
fn create_task(args: &[i64]) -> i64 {
    let coro = args[0];
    if fiber::kind(coro) != Some(FiberKind::Coroutine) {
        return exc::raise_with_message(TYPE_ERROR, "create_task() requires a coroutine");
    }
    if !loop_running() {
        return exc::raise_with_message(RUNTIME_ERROR, "no running event loop");
    }
    let token = crate::heap::root_values(&[coro]);
    fiber::mark_owned(coro);
    with_loop(|state| state.driver.push_ready(coro));
    crate::heap::release_roots(token);
    coro
}

/// `asyncio.current_task()`: the task whose body is running, or `None`.
fn current_task() -> i64 {
    with_loop(|state| if state.running { state.current } else { NONE }).unwrap_or(NONE)
}

// -------------------------------------------------------------------------
// TaskGroup
// -------------------------------------------------------------------------

/// Marker attribute identifying the native `TaskGroup` class.
const TASK_GROUP_MARKER: &str = "__asyncio_task_group__";
const TASK_GROUP_TASKS: &str = "_tasks";

/// `asyncio.TaskGroup()`: the native class's zero-argument constructor.
/// Returning an instance (rather than the class) keeps both
/// `asyncio.TaskGroup()` and `from asyncio import TaskGroup` immediately
/// usable as an async context manager.
fn task_group_new() -> i64 {
    let class = task_group_class();
    let token = crate::heap::root_values(&[class]);
    let instance = value::instance_new(class);
    crate::heap::release_roots(token);
    instance
}

/// `asyncio.TaskGroup`: a native class carrying only the marker attribute.
/// State lives in instance attributes (`_tasks`), which the collector traces.
pub(crate) fn task_group_class() -> i64 {
    value::class_new_full(
        "TaskGroup".to_string(),
        NONE,
        Vec::new(),
        vec![(TASK_GROUP_MARKER.to_string(), TRUE)],
    )
}

/// Whether `obj` is an instance of the native `TaskGroup` class.
pub(crate) fn is_task_group(obj: i64) -> bool {
    let Some(state) = value::as_instance(obj) else {
        return false;
    };
    matches!(
        value::class_lookup(state.class, TASK_GROUP_MARKER),
        Some(value::ClassEntry::Attr(marker)) if marker == TRUE
    )
}

fn attribute_error(obj: i64, method: &str) -> i64 {
    let _ = obj;
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("'TaskGroup' object has no attribute '{method}'"),
    )
}

/// Positional arity check for the native TaskGroup methods.
fn check_arity(recv: &str, method: &str, argc: usize, min: usize, max: usize) -> Option<i64> {
    if argc >= min && argc <= max {
        return None;
    }
    let expected = if min == max {
        format!("{min}")
    } else {
        format!("{min} to {max}")
    };
    Some(exc::raise_with_message(
        TYPE_ERROR,
        &format!("{recv}.{method}() takes {expected} arguments ({argc} given)"),
    ))
}

/// Lazily creates the `_tasks` list on `obj`.
fn ensure_tasks(obj: i64) -> i64 {
    if let Some(tasks) = value::instance_attr(obj, TASK_GROUP_TASKS) {
        return tasks;
    }
    let token = crate::heap::root_values(&[obj]);
    let tasks = value::list_new(Vec::new());
    // `instance_setattr` allocates the attribute name; keep the fresh list
    // reachable across that allocation.
    let list_token = crate::heap::root_values(&[tasks]);
    value::instance_setattr(obj, TASK_GROUP_TASKS.to_string(), tasks);
    crate::heap::release_roots(list_token);
    crate::heap::release_roots(token);
    tasks
}

fn tasks_of(obj: i64) -> Vec<i64> {
    value::instance_attr(obj, TASK_GROUP_TASKS)
        .and_then(value::as_list)
        .map(|tasks| tasks.to_vec())
        .unwrap_or_default()
}

fn append_task(obj: i64, task: i64) {
    let tasks = ensure_tasks(obj);
    if let Some(mut items) = value::list_items_mut(tasks) {
        items.push(task);
    }
    crate::heap::write_barrier_edge(tasks, task);
}

/// Suspends the calling fiber until every task in `tasks` is done. Reissues
/// the join after a failure wakes the caller early, so the group always
/// observes all of its tasks before reporting.
fn join_tasks(tasks: &[i64]) -> i64 {
    loop {
        let pending: Vec<i64> = tasks
            .iter()
            .copied()
            .filter(|&task| !fiber::is_done(task))
            .collect();
        if pending.is_empty() {
            return NONE;
        }
        let token = crate::heap::root_values(&pending);
        if fiber::current_value().is_some() {
            fiber::suspend_current(Some(WaitRequest::Join(pending)));
        } else {
            drive_join_tasks(&pending);
        }
        crate::heap::release_roots(token);
        if exc::pending() {
            return NONE;
        }
    }
}

/// First task that failed, in completion order.
fn first_failed(tasks: &[i64]) -> Option<i64> {
    tasks
        .iter()
        .copied()
        .filter(|&task| fiber::is_fiber(task) && fiber::has_error(task))
        .min_by_key(|&task| fiber::finish_sequence(task))
}

fn task_group_enter(obj: i64, args: &[i64]) -> i64 {
    if let Some(err) = check_arity("TaskGroup", "__aenter__", args.len(), 0, 0) {
        return err;
    }
    ensure_tasks(obj);
    obj
}

/// Waits until all `tasks` are done or one fails. Used before a `TaskGroup`
/// decides whether to cancel its remaining tasks.
fn join_until_failure(tasks: &[i64]) -> i64 {
    loop {
        if tasks.iter().any(|&task| fiber::has_error(task)) {
            return NONE;
        }
        let pending: Vec<i64> = tasks
            .iter()
            .copied()
            .filter(|&task| !fiber::is_done(task))
            .collect();
        if pending.is_empty() {
            return NONE;
        }
        let token = crate::heap::root_values(&pending);
        if fiber::current_value().is_some() {
            fiber::suspend_current(Some(WaitRequest::Join(pending)));
        } else {
            drive_join_tasks(&pending);
        }
        crate::heap::release_roots(token);
        if exc::pending() {
            return NONE;
        }
    }
}

fn task_group_exit(obj: i64, args: &[i64]) -> i64 {
    if let Some(err) = check_arity("TaskGroup", "__aexit__", args.len(), 3, 3) {
        return err;
    }
    let body_error = args[1];
    let tasks = tasks_of(obj);
    let mut rooted = tasks.clone();
    rooted.push(obj);
    rooted.push(body_error);
    let token = crate::heap::root_values(&rooted);

    if body_error != NONE {
        // The body failed: cancel every remaining task, then wait for them so
        // no work outlives the `async with` block.
        for &task in &tasks {
            if !fiber::is_done(task) {
                fiber::cancel_value(task);
            }
        }
    } else {
        // Wait for the group; the first failing task aborts the wait so the
        // remaining siblings can be cancelled, like CPython.
        join_until_failure(&tasks);
        if exc::pending() {
            crate::heap::release_roots(token);
            return NONE;
        }
    }
    if first_failed(&tasks).is_some() {
        for &task in &tasks {
            if !fiber::is_done(task) {
                fiber::cancel_value(task);
            }
        }
    }
    join_tasks(&tasks);

    let mut result = FALSE;
    if !exc::pending() {
        if body_error != NONE {
            // Do not suppress the body exception.
            result = FALSE;
        } else if let Some(failed) = first_failed(&tasks) {
            let result = fiber::raise_error(failed);
            crate::heap::release_roots(token);
            return result;
        }
    }
    crate::heap::release_roots(token);
    result
}

fn task_group_create(obj: i64, args: &[i64]) -> i64 {
    if let Some(err) = check_arity("TaskGroup", "create_task", args.len(), 1, 1) {
        return err;
    }
    let coro = args[0];
    if fiber::kind(coro) != Some(FiberKind::Coroutine) {
        return exc::raise_with_message(TYPE_ERROR, "create_task() requires a coroutine");
    }
    if !loop_running() {
        return exc::raise_with_message(RUNTIME_ERROR, "no running event loop");
    }
    let token = crate::heap::root_values(&[obj, coro]);
    append_task(obj, coro);
    fiber::mark_owned(coro);
    with_loop(|state| state.driver.push_ready(coro));
    crate::heap::release_roots(token);
    coro
}

/// Dispatch for the native `TaskGroup` class methods.
pub(crate) fn task_group_method(obj: i64, method: &str, argc: usize, argv: *const i64) -> i64 {
    if argc > 0 && (argv.is_null() || argc > isize::MAX as usize / std::mem::size_of::<i64>()) {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("TaskGroup.{method}() received {argc} argument(s) without a buffer"),
        );
    }
    let args = if argc == 0 {
        Vec::new()
    } else {
        // SAFETY: ABI contract: `argv..argv+argc` is readable; checked above.
        unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
    };
    match method {
        "__aenter__" => task_group_enter(obj, &args),
        "__aexit__" => task_group_exit(obj, &args),
        "create_task" => task_group_create(obj, &args),
        _ => attribute_error(obj, method),
    }
}

// -------------------------------------------------------------------------
// Sleep and gather
// -------------------------------------------------------------------------

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

/// Drives `tasks` to completion (or until one fails) outside a fiber.
///
/// Used by `asyncio.gather` outside a fiber and by `hydro_await` when a
/// coroutine propagates a join request.
pub(crate) fn drive_join_tasks(tasks: &[i64]) {
    let mut driver = Driver::new();
    // The local driver has no persistent slot; keep its task-root list (and
    // therefore every scheduled task) alive across the resumes below.
    let driver_token = crate::heap::root_values(&[driver.roots]);
    let mut queued = false;
    for &task in tasks {
        if fiber::is_fiber(task) && !fiber::is_done(task) {
            driver.push_ready(task);
            queued = true;
        }
    }
    if !queued {
        crate::heap::release_roots(driver_token);
        return;
    }
    driver.run_local(&|| {
        tasks
            .iter()
            .all(|&task| !fiber::is_fiber(task) || fiber::is_done(task))
            || tasks.iter().any(|&task| fiber::has_error(task))
    });
    crate::heap::release_roots(driver_token);
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
/// historical Hydro spelling with one list/tuple argument is still accepted.
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
        let token = crate::heap::root_values(&tasks);
        if fiber::current_value().is_some() {
            // The driver starts the tasks and resumes this fiber once they
            // are done (or one failed).
            fiber::suspend_current(Some(WaitRequest::Join(tasks)));
        } else {
            drive_join_tasks(&tasks);
        }
        crate::heap::release_roots(token);
        if exc::pending() {
            return NONE;
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

// -------------------------------------------------------------------------
// wait_for, Timeout, Runner and asyncio.staggered
// -------------------------------------------------------------------------

/// Monotonic origin for the integer deadlines stored on native timeout
/// objects (instance attributes can only hold tagged values).
thread_local! {
    static MONOTONIC_EPOCH: Instant = Instant::now();
}

fn monotonic_nanos() -> i64 {
    MONOTONIC_EPOCH.with(|epoch| epoch.elapsed().as_nanos() as i64)
}

/// Converts a timeout in seconds to a monotonic deadline. A negative value
/// yields an already-expired deadline, like CPython.
fn deadline_from_seconds(seconds: f64) -> Option<i64> {
    if seconds.is_nan() {
        return None;
    }
    let offset = (seconds * 1_000_000_000.0) as i64;
    Some(monotonic_nanos().saturating_add(offset))
}

fn native_method(
    name: &str,
    function: &'static str,
    entry: extern "C" fn(*const i64) -> i64,
    signature: Vec<value::ParamMeta>,
) -> (String, i64, MethodKind) {
    let function_value = value::native_function_new("asyncio", function, usize::MAX);
    value::function_set_signature(function_value, signature);
    value::function_set_full_entry(function_value, entry as *const () as usize);
    (name.to_string(), function_value, MethodKind::Normal)
}

fn self_parameter() -> value::ParamMeta {
    value::ParamMeta {
        name: "self".to_string(),
        kind: hydro_ir::ParamKind::Positional,
        default: None,
    }
}

fn positional_parameter(name: &str) -> value::ParamMeta {
    value::ParamMeta {
        name: name.to_string(),
        kind: hydro_ir::ParamKind::Positional,
        default: None,
    }
}

/// `asyncio.wait_for(awaitable, timeout)`: waits at most `timeout` seconds and
/// cancels the task when the deadline passes.
///
/// The task is scheduled on the running loop; while waiting, the calling fiber
/// polls the task so the loop keeps running its other work. Timeouts shorter
/// than the polling quantum are honoured at 1 ms granularity.
fn wait_for(args: &[i64]) -> i64 {
    let awaitable = args[0];
    let timeout_value = args.get(1).copied().unwrap_or(NONE);
    if !fiber::is_fiber(awaitable) {
        // Hydro has no other awaitables: like `await`, return the value.
        return awaitable;
    }
    let deadline = if timeout_value == NONE {
        None
    } else {
        match super::number(timeout_value) {
            Ok(seconds) => match deadline_from_seconds(seconds) {
                Some(deadline) => Some(deadline),
                None => {
                    return exc::raise_with_message(VALUE_ERROR, "invalid timeout")
                }
            },
            Err(err) => return err,
        }
    };
    if loop_running() && !fiber::is_done(awaitable) {
        let token = crate::heap::root_values(&[awaitable]);
        fiber::mark_owned(awaitable);
        with_loop(|state| state.driver.push_ready(awaitable));
        crate::heap::release_roots(token);
    }
    let Some(deadline) = deadline else {
        return fiber::await_value(awaitable);
    };
    if fiber::current_value().is_none() {
        // No fiber to poll from: drive the task directly. The deadline still
        // applies before the first resume.
        if monotonic_nanos() >= deadline && !fiber::is_done(awaitable) {
            fiber::cancel_value(awaitable);
            return exc::raise_with_message(TIMEOUT_ERROR, "");
        }
        drive_join_tasks(&[awaitable]);
        return finish_finished_task(awaitable);
    }
    loop {
        if fiber::is_done(awaitable) {
            return finish_finished_task(awaitable);
        }
        let now = monotonic_nanos();
        if now >= deadline {
            fiber::cancel_value(awaitable);
            return exc::raise_with_message(TIMEOUT_ERROR, "");
        }
        let remaining = Duration::from_nanos((deadline - now).max(1) as u64)
            .min(Duration::from_millis(1));
        fiber::suspend_current(Some(WaitRequest::Sleep(remaining)));
        if exc::pending() {
            return NONE;
        }
    }
}

/// Reads a task that has finished: re-raises its failure, otherwise returns
/// its result.
fn finish_finished_task(task: i64) -> i64 {
    if fiber::has_error(task) {
        if fiber::is_cancelled(task) {
            return exc::raise_with_message(CANCELLED_ERROR, "");
        }
        return fiber::raise_error(task);
    }
    fiber::result_value(task)
}

const TIMEOUT_WHEN: &str = "_when";

/// The cached native `asyncio.Timeout` class.
fn timeout_class() -> i64 {
    let key = "asyncio.Timeout";
    if let Some(cached) = globals::registry_get(key) {
        return cached;
    }
    let methods = vec![
        native_method(
            "__aenter__",
            "_asyncio_timeout_enter",
            timeout_enter_entry,
            vec![self_parameter()],
        ),
        native_method(
            "__aexit__",
            "_asyncio_timeout_exit",
            timeout_exit_entry,
            vec![
                self_parameter(),
                positional_parameter("type"),
                positional_parameter("value"),
                positional_parameter("traceback"),
            ],
        ),
        native_method(
            "expired",
            "_asyncio_timeout_expired",
            timeout_expired_entry,
            vec![self_parameter()],
        ),
        native_method(
            "reschedule",
            "_asyncio_timeout_reschedule",
            timeout_reschedule_entry,
            vec![self_parameter(), positional_parameter("when")],
        ),
    ];
    let values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let roots = crate::heap::root_values(&values);
    let class = value::class_new_full("Timeout".to_string(), NONE, methods, Vec::new());
    let class_roots = crate::heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(key, class);
    crate::heap::release_roots(class_roots);
    crate::heap::release_roots(roots);
    class
}

/// `asyncio.timeout(delay)`: the async context manager timer.
fn timeout(args: &[i64]) -> i64 {
    let delay = args.first().copied().unwrap_or(NONE);
    let when = if delay == NONE {
        NONE
    } else {
        match super::number(delay) {
            Ok(seconds) => match deadline_from_seconds(seconds) {
                Some(deadline) => ops_int(deadline),
                None => return exc::raise_with_message(VALUE_ERROR, "invalid timeout"),
            },
            Err(err) => return err,
        }
    };
    let class = timeout_class();
    let roots = crate::heap::root_values(&[class, when]);
    let instance = value::instance_new(class);
    let inner = crate::heap::root_values(&[instance]);
    value::instance_setattr(instance, TIMEOUT_WHEN.to_string(), when);
    crate::heap::release_roots(inner);
    crate::heap::release_roots(roots);
    instance
}

fn ops_int(n: i64) -> i64 {
    crate::ops::int_result(n)
}

extern "C" fn timeout_enter_entry(argv: *const i64) -> i64 {
    let instance = unsafe { *argv };
    if timeout_expired(instance) {
        return exc::raise_with_message(TIMEOUT_ERROR, "");
    }
    instance
}

extern "C" fn timeout_exit_entry(argv: *const i64) -> i64 {
    // SAFETY: the binder passes `[self, type, value, traceback]`.
    let (instance, exception_type) = unsafe { (*argv, *argv.add(1)) };
    if timeout_expired(instance) {
        if exception_type != NONE {
            // The body already failed; its exception wins.
            return value::FALSE;
        }
        return exc::raise_with_message(TIMEOUT_ERROR, "");
    }
    value::FALSE
}

fn timeout_expired(instance: i64) -> bool {
    match value::instance_attr(instance, TIMEOUT_WHEN).and_then(value::as_integer) {
        Some(deadline) => monotonic_nanos() >= deadline,
        None => false,
    }
}

extern "C" fn timeout_expired_entry(argv: *const i64) -> i64 {
    let instance = unsafe { *argv };
    value::bool_value(timeout_expired(instance))
}

extern "C" fn timeout_reschedule_entry(argv: *const i64) -> i64 {
    // SAFETY: the binder passes `[self, when]`.
    let (instance, when) = unsafe { (*argv, *argv.add(1)) };
    let when = if when == NONE {
        NONE
    } else {
        match super::number(when) {
            Ok(seconds) => match deadline_from_seconds(seconds) {
                Some(deadline) => ops_int(deadline),
                None => return exc::raise_with_message(VALUE_ERROR, "invalid timeout"),
            },
            Err(err) => return err,
        }
    };
    let roots = crate::heap::root_values(&[instance, when]);
    value::instance_setattr(instance, TIMEOUT_WHEN.to_string(), when);
    crate::heap::release_roots(roots);
    NONE
}

/// The cached native `asyncio.Runner` class.
fn runner_class() -> i64 {
    let key = "asyncio.Runner";
    if let Some(cached) = globals::registry_get(key) {
        return cached;
    }
    let methods = vec![
        native_method(
            "run",
            "_asyncio_runner_run",
            runner_run_entry,
            vec![self_parameter(), positional_parameter("coro")],
        ),
        native_method(
            "close",
            "_asyncio_runner_close",
            runner_close_entry,
            vec![self_parameter()],
        ),
        native_method(
            "__enter__",
            "_asyncio_runner_enter",
            runner_enter_entry,
            vec![self_parameter()],
        ),
        native_method(
            "__exit__",
            "_asyncio_runner_exit",
            runner_exit_entry,
            vec![
                self_parameter(),
                positional_parameter("type"),
                positional_parameter("value"),
                positional_parameter("traceback"),
            ],
        ),
    ];
    let values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let roots = crate::heap::root_values(&values);
    let class = value::class_new_full("Runner".to_string(), NONE, methods, Vec::new());
    let class_roots = crate::heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(key, class);
    crate::heap::release_roots(class_roots);
    crate::heap::release_roots(roots);
    class
}

/// `asyncio.Runner()`: a native runner over the thread's event loop.
fn runner_new() -> i64 {
    let class = runner_class();
    let roots = crate::heap::root_values(&[class]);
    let instance = value::instance_new(class);
    crate::heap::release_roots(roots);
    instance
}

extern "C" fn runner_run_entry(argv: *const i64) -> i64 {
    // SAFETY: the binder passes `[self, coro]`.
    let coro = unsafe { *argv.add(1) };
    run(&[coro])
}

extern "C" fn runner_close_entry(_argv: *const i64) -> i64 {
    NONE
}

extern "C" fn runner_enter_entry(argv: *const i64) -> i64 {
    unsafe { *argv }
}

extern "C" fn runner_exit_entry(_argv: *const i64) -> i64 {
    value::FALSE
}

/// `asyncio.staggered.staggered_race(coro_fns, delay)`.
///
/// Starts each coroutine function in turn, returning `(winner_result,
/// winner_index, exceptions)` for the first one that returns normally.
///
/// Documented deviation: the candidates run one at a time, so a later
/// candidate only starts after the previous one fails (and after `delay`).
/// Hydro's driver has no select-style wait, so CPython's concurrent starts
/// with staggered timers are not reproduced; the result contract is.
fn staggered_race(args: &[i64]) -> i64 {
    let functions = materialize(args[0]);
    if exc::pending() {
        return NONE;
    }
    let delay = args.get(1).copied().unwrap_or(NONE);
    let delay_seconds = if delay == NONE {
        None
    } else {
        match super::number(delay) {
            Ok(seconds) if seconds.is_finite() && seconds >= 0.0 => Some(seconds),
            Ok(_) => {
                return exc::raise_with_message(
                    VALUE_ERROR,
                    "delay must be a non-negative number",
                )
            }
            Err(err) => return err,
        }
    };
    let candidates: Vec<i64> = value::as_list(functions)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let token = crate::heap::root_values(&candidates);
    let exceptions = value::list_new(Vec::new());
    let exception_roots = crate::heap::root_values(&[exceptions]);
    let mut winner: Option<(i64, i64)> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        if index > 0 {
            if let Some(seconds) = delay_seconds {
                if seconds > 0.0 {
                    fiber::blocking_sleep(Duration::from_secs_f64(seconds));
                }
            }
        }
        let coro = crate::builtins::call(*candidate, 0, std::ptr::null());
        if exc::pending() {
            let raised = exc::pending_value();
            exc::clear();
            push_exception(exceptions, raised);
            continue;
        }
        let result = fiber::await_value(coro);
        if exc::pending() {
            let raised = exc::pending_value();
            exc::clear();
            push_exception(exceptions, raised);
            continue;
        }
        winner = Some((index as i64, result));
        break;
    }
    let (winner_result, winner_index) = match winner {
        Some((index, result)) => (result, ops_int(index)),
        None => (NONE, NONE),
    };
    let winner_roots = crate::heap::root_values(&[winner_result, winner_index, exceptions]);
    let outcome = value::tuple_new(vec![winner_result, winner_index, exceptions]);
    crate::heap::release_roots(winner_roots);
    crate::heap::release_roots(exception_roots);
    crate::heap::release_roots(token);
    outcome
}

fn push_exception(exceptions: i64, raised: i64) {
    let roots = crate::heap::root_values(&[exceptions, raised]);
    if let Some(mut items) = value::list_items_mut(exceptions) {
        items.push(raised);
    }
    crate::heap::write_barrier_edge(exceptions, raised);
    crate::heap::release_roots(roots);
}

/// Materialises a constant of the `asyncio.staggered` module: its `contextlib`
/// and `asyncio` imports are real module values.
pub(crate) fn staggered_constant(name: &str) -> Option<i64> {
    match name {
        "contextlib" => Some(super::module_value_by_name("contextlib")),
        "asyncio" => Some(super::module_value_by_name("asyncio")),
        _ => None,
    }
}

/// Module-level function dispatch. Arity is validated against the shared spec
/// before this is called.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "run" => run(args),
        "sleep" => sleep(args),
        "gather" => gather(args),
        "create_task" => create_task(args),
        "current_task" => current_task(),
        "TaskGroup" => task_group_new(),
        "wait_for" => wait_for(args),
        "timeout" => timeout(args),
        "Runner" => runner_new(),
        "staggered_race" => staggered_race(args),
        _ => missing_attribute(function),
    }
}

/// Materialises a constant of the nested `asyncio.coroutines` module.
///
/// `_is_coroutine` is an identity sentinel: decorated coroutine functions
/// store it in their `_is_coroutine` attribute and
/// `asyncio.iscoroutinefunction` (and the compatibility shims around
/// `inspect.iscoroutinefunction`) compare with `is`. One cached plain object
/// gives that comparison stable identity across reads.
pub(crate) fn coroutines_constant(name: &str) -> Option<i64> {
    if name != "_is_coroutine" {
        return None;
    }
    let key = "asyncio.coroutines._is_coroutine";
    if let Some(cached) = globals::registry_get(key) {
        return Some(cached);
    }
    let sentinel = value::instance_new(value::object_class());
    globals::registry_set(key, sentinel);
    Some(sentinel)
}
