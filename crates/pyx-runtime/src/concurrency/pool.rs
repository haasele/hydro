//! `multiprocessing.Pool`: a fixed set of worker processes with one private
//! task queue and one private result queue per worker.
//!
//! Tasks and results always travel in the `spawn` serialization mode, as a hex
//! string inside the queue frame:
//!
//! ```text
//! task   = [id, callable, args_list]
//! result = [id, ok, value_or_error_string]
//! ```
//!
//! Because the callable is encoded by name, the same task works for `fork`
//! (the registry was copied) and `spawn` (the child registers every generated
//! function before the worker runs). A callable must therefore be a named
//! module-level wrapper; lambdas and native functions are rejected when the
//! task is submitted.
//!
//! Scheduling keeps at most one task in flight per worker: before a worker
//! receives its next task, the previous result is read. This preserves
//! parallelism across workers while ensuring a worker can never block writing
//! a result queue whose reader is in turn blocked writing a full task queue.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::value::{self, NONE};
use crate::{builtins, exc, heap, iter, process, serialize};

/// Name of the native worker entry in the spawn registry.
pub(crate) const WORKER_NAME: &str = "__pyx_pool_worker";

/// Upper bound for a pool created from a user-supplied process count. Larger
/// requests are clamped instead of exhausting the process table.
const MAX_PROCESSES: usize = 1024;

/// How long a result read waits before probing whether its worker is alive.
const RESULT_POLL: Duration = Duration::from_millis(200);

/// Default worker count: the number of available CPUs, at least one.
pub(crate) fn default_processes() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .max(1)
}

// -------------------------------------------------------------------------
// Construction
// -------------------------------------------------------------------------

/// Creates the queue pairs, allocates the pool object and starts every worker.
pub(crate) fn new_pool(processes: usize) -> Result<i64, String> {
    let processes = processes.min(MAX_PROCESSES);
    if processes == 0 {
        return Err("Number of processes must be at least 1".to_string());
    }

    // The queue objects are only reachable from Rust vectors while they are
    // created; keep them alive through a temporary PyX list, which the
    // conservative collector scans as a stack root.
    let nursery = value::list_new(Vec::new());
    let mut task_queues = Vec::with_capacity(processes);
    let mut result_queues = Vec::with_capacity(processes);
    for _ in 0..processes {
        for queue_slot in 0..2 {
            let queue = process::new_queue(0)
                .map_err(|message| format!("cannot create queue: {message}"))?;
            push_nursery(nursery, queue);
            if queue_slot == 0 {
                task_queues.push(queue);
            } else {
                result_queues.push(queue);
            }
        }
    }

    let pool_value = value::pool_new(task_queues, result_queues);
    // The queue tags above live in Rust vectors; the nursery (a PyX list on
    // this stack frame) is what kept them alive during the allocation.
    std::hint::black_box(nursery);
    for index in 0..processes {
        let (task_queue, result_queue) = match queue_pair(pool_value, index) {
            Some(pair) => pair,
            None => {
                abort_start(pool_value);
                return Err("internal queue setup failed".to_string());
            }
        };
        match start_worker(task_queue, result_queue) {
            Ok(pid) => {
                store_pid(pool_value, pid.into());
            }
            Err(message) => {
                // Workers already started are blocked in their task queue;
                // release and reap them before reporting the failure.
                abort_start(pool_value);
                return Err(format!("cannot start worker: {message}"));
            }
        }
    }
    Ok(pool_value)
}

fn push_nursery(nursery: i64, item: i64) {
    if let Some(mut items) = value::list_items_mut(nursery) {
        items.push(item);
        heap::write_barrier_edge(nursery, item);
    }
}

/// Copies one worker's queue tags out of the pool object.
fn queue_pair(pool_value: i64, index: usize) -> Option<(i64, i64)> {
    let pool = value::as_pool(pool_value)?;
    Some((
        *pool.task_queues.get(index)?,
        *pool.result_queues.get(index)?,
    ))
}

fn store_pid(pool_value: i64, pid: i64) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    if let Some(pool) = value::as_pool(pool_value) {
        pool.pids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(pid);
    }
}

fn start_worker(task_queue: i64, result_queue: i64) -> Result<i32, String> {
    if process::get_start_method() == "spawn" {
        let pid = process::start_named_target(WORKER_NAME, vec![task_queue, result_queue])?;
        i32::try_from(pid).map_err(|_| "invalid pid".to_string())
    } else {
        let pid = process::fork()?;
        if pid == 0 {
            // Child: rebuild the runtime bookkeeping, run the worker loop and
            // leave without returning into the parent's code.
            process::child_after_fork();
            run_worker(task_queue, result_queue);
            process::exit_now(0);
        }
        i32::try_from(pid).map_err(|_| "invalid pid".to_string())
    }
}

/// Stops and reaps the workers that were started before a construction error.
fn abort_start(pool_value: i64) {
    let Some(pool) = value::as_pool(pool_value) else {
        return;
    };
    let task_queues = pool.task_queues.clone();
    pool.closed.store(true, Ordering::Release);
    for queue in task_queues {
        if let Some(state) = value::as_mp_queue(queue) {
            let _ = process::queue_put(state, NONE, true, None);
        }
    }
    let pids = value::as_pool(pool_value)
        .map(|pool| {
            pool.pids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        })
        .unwrap_or_default();
    for pid in pids {
        let _ = process::wait_for_child(i64::from(pid));
    }
    if let Some(pool) = value::as_pool(pool_value) {
        pool.joined.store(true, Ordering::Release);
    }
}

// -------------------------------------------------------------------------
// Public pool operations
// -------------------------------------------------------------------------

/// `Pool.map(function, iterable)`: materialises the iterable, runs one task
/// per element and returns the results in input order.
pub(crate) fn map(pool_value: i64, function: i64, iterable: i64) -> Result<i64, String> {
    ensure_open(pool_value)?;
    let items = materialize(iterable)?;
    let count = value::as_list(items)
        .map(|values| values.len())
        .unwrap_or(0);
    run_jobs(pool_value, function, count, |index| {
        let item = value::as_list(items)
            .and_then(|values| values.get(index).copied())
            .ok_or_else(|| "Pool.map(): iterable changed while collecting tasks".to_string())?;
        Ok(value::list_new(vec![item]))
    })
}

/// `Pool.apply(function, args)`: runs exactly one task and returns its value.
pub(crate) fn apply(pool_value: i64, function: i64, args: i64) -> Result<i64, String> {
    ensure_open(pool_value)?;
    let arguments = argument_list(args)?;
    let results = run_jobs(pool_value, function, 1, |_| Ok(arguments))?;
    value::as_list(results)
        .and_then(|values| values.first().copied())
        .ok_or_else(|| "Pool.apply(): worker returned no result".to_string())
}

/// Rejects work on a closed (or joined) pool.
fn ensure_open(pool_value: i64) -> Result<(), String> {
    let pool = value::as_pool(pool_value).ok_or_else(|| "invalid Pool object".to_string())?;
    if pool.closed.load(Ordering::Acquire) || pool.joined.load(Ordering::Acquire) {
        return Err("Pool is closed".to_string());
    }
    Ok(())
}

/// `Pool.close()`: tells every worker to exit after its pending tasks.
pub(crate) fn close(pool_value: i64) -> Result<(), String> {
    let Some(pool) = value::as_pool(pool_value) else {
        return Err("close() on a non-Pool object".to_string());
    };
    if pool.closed.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let workers = value::as_pool(pool_value)
        .map(|pool| pool.size())
        .unwrap_or(0);
    let mut first_error = None;
    for index in 0..workers {
        let Some(queue) = queue_tag(pool_value, true, index) else {
            if first_error.is_none() {
                first_error = Some("Pool task queue is invalid".to_string());
            }
            continue;
        };
        let Some(state) = value::as_mp_queue(queue) else {
            if first_error.is_none() {
                first_error = Some("Pool task queue is invalid".to_string());
            }
            continue;
        };
        if let Err(error) = process::queue_put(state, NONE, true, None) {
            if first_error.is_none() {
                first_error = Some(mp_message(&error));
            }
        }
    }
    // Keep the pool (and therefore every queue object) rooted while the
    // final queue writes above may park for the collector.
    std::hint::black_box(pool_value);
    match first_error {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

/// `Pool.join()`: waits for every worker to exit. Idempotent.
pub(crate) fn join(pool_value: i64) -> Result<(), String> {
    let Some(pool) = value::as_pool(pool_value) else {
        return Err("join() on a non-Pool object".to_string());
    };
    if !pool.closed.load(Ordering::Acquire) {
        return Err("Pool.join() cannot be called before close()".to_string());
    }
    if pool.joined.load(Ordering::Acquire) {
        return Ok(());
    }
    // Never hold the pid lock across a blocking wait.
    let pids = pool
        .pids
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let mut first_error = None;
    for pid in pids {
        if let Err(message) = process::wait_for_child(i64::from(pid)) {
            if first_error.is_none() {
                first_error = Some(message);
            }
        }
    }
    match first_error {
        Some(message) => Err(message),
        None => {
            if let Some(pool) = value::as_pool(pool_value) {
                pool.joined.store(true, Ordering::Release);
            }
            Ok(())
        }
    }
}

/// `Pool.terminate()`: kills every worker with `SIGTERM` and reaps it.
///
/// Workers blocked reading their task queue die from the signal, so no
/// sentinel is written and a terminated pool can never deadlock on a queue
/// write. Terminating after `close`, `join` or another `terminate` is a
/// no-op.
pub(crate) fn terminate(pool_value: i64) -> Result<(), String> {
    let Some(pool) = value::as_pool(pool_value) else {
        return Err("terminate() on a non-Pool object".to_string());
    };
    if pool.joined.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    pool.closed.store(true, Ordering::Release);
    // Never hold the pid lock across a blocking wait.
    let pids = pool
        .pids
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    // Keep the pool rooted while signalling/collecting may park.
    std::hint::black_box(pool_value);
    for pid in &pids {
        // A worker that already exited no longer accepts the signal; the
        // reap below is what reports failures.
        let _ = process::terminate_child(i64::from(*pid));
    }
    let mut first_error = None;
    for pid in pids {
        if let Err(message) = process::wait_for_child(i64::from(pid)) {
            if first_error.is_none() {
                first_error = Some(message);
            }
        }
    }
    match first_error {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

// -------------------------------------------------------------------------
// Task submission and result collection
// -------------------------------------------------------------------------

fn validate_callable(function: i64) -> Result<(), String> {
    let Some(func) = value::as_function(function) else {
        return Err(format!(
            "Pool task target must be callable, not {}",
            value::type_name(function)
        ));
    };
    match &func.body {
        value::FunctionBody::Wrapper { entry, captures } => {
            if *entry == 0 {
                return Err("Pool task target has no code".to_string());
            }
            if !captures.is_empty() {
                return Err(
                    "cannot run a closure in a Pool; use a named module-level function".to_string(),
                );
            }
        }
        value::FunctionBody::Native { .. } => {
            return Err(
                "cannot run a native function in a Pool; use a named module-level function"
                    .to_string(),
            );
        }
        value::FunctionBody::Builtin(_) => {
            return Err(
                "cannot run a builtin function in a Pool; use a named module-level function"
                    .to_string(),
            );
        }
        // Getter, partial, bound-method and identity callables have no stable
        // code entry.
        value::FunctionBody::AttributeGetter { .. }
        | value::FunctionBody::ItemGetter { .. }
        | value::FunctionBody::Partial { .. }
        | value::FunctionBody::BoundMethod { .. }
        | value::FunctionBody::Host { .. }
        | value::FunctionBody::Identity => {
            return Err(
                "cannot run a derived callable in a Pool; use a named module-level function"
                    .to_string(),
            );
        }
    }
    if func.name == "<lambda>" || func.name.is_empty() {
        return Err("cannot run a lambda in a Pool; use a named module-level function".to_string());
    }
    Ok(())
}

/// Materialises an iterable into a PyX list (a stack root for the elements).
fn materialize(iterable: i64) -> Result<i64, String> {
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        return Err(take_pending_message());
    }
    let out = value::list_new(Vec::new());
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return Err(take_pending_message());
        }
        if status == 0 {
            break;
        }
        if let Some(mut items) = value::list_items_mut(out) {
            items.push(item);
            heap::write_barrier_edge(out, item);
        }
    }
    Ok(out)
}

/// Validates the `apply` argument sequence into a fresh PyX list.
fn argument_list(args: i64) -> Result<i64, String> {
    if args == NONE {
        return Ok(value::list_new(Vec::new()));
    }
    if let Some(items) = value::as_list(args) {
        return Ok(value::list_new(items.to_vec()));
    }
    if let Some(items) = value::as_tuple(args) {
        return Ok(value::list_new(items.to_vec()));
    }
    Err(format!(
        "Pool.apply() 'args' must be a tuple or list, not {}",
        value::type_name(args)
    ))
}

/// Runs `count` tasks, distributing them round-robin, and returns a list with
/// the results in input order.
fn run_jobs<F>(pool_value: i64, function: i64, count: usize, args_for: F) -> Result<i64, String>
where
    F: Fn(usize) -> Result<i64, String>,
{
    validate_callable(function)?;
    let (processes, start, closed) = {
        let pool = value::as_pool(pool_value).ok_or_else(|| "invalid Pool object".to_string())?;
        (
            pool.size(),
            pool.next.fetch_add(count, Ordering::Relaxed),
            pool.closed.load(Ordering::Acquire) || pool.joined.load(Ordering::Acquire),
        )
    };
    if processes == 0 {
        return Err("Pool has no workers".to_string());
    }
    if closed {
        return Err("Pool is closed".to_string());
    }

    let results = value::list_new(vec![NONE; count]);
    if count == 0 {
        return Ok(results);
    }
    let start = start % processes;

    // At most one task per worker is outstanding; its id is stored here.
    let mut outstanding: Vec<Option<usize>> = vec![None; processes];
    let mut first_error: Option<String> = None;
    let mut abort = false;

    for id in 0..count {
        let worker = (start + id) % processes;
        if let Some(previous) = outstanding[worker].take() {
            match collect_one(pool_value, worker, previous, results) {
                Ok(None) => {}
                Ok(Some(message)) => note_error(&mut first_error, message),
                Err(message) => {
                    note_error(&mut first_error, message);
                    abort = true;
                }
            }
        }
        if abort {
            continue;
        }
        let arguments = match args_for(id) {
            Ok(arguments) => arguments,
            Err(message) => {
                note_error(&mut first_error, message);
                abort = true;
                continue;
            }
        };
        let encoded = match encode_task(id, function, arguments) {
            Ok(encoded) => encoded,
            Err(message) => {
                note_error(&mut first_error, message);
                abort = true;
                continue;
            }
        };
        let Some(task_queue) = queue_tag(pool_value, true, worker) else {
            note_error(&mut first_error, "Pool task queue is invalid".to_string());
            abort = true;
            continue;
        };
        let Some(state) = value::as_mp_queue(task_queue) else {
            note_error(&mut first_error, "Pool task queue is invalid".to_string());
            abort = true;
            continue;
        };
        // The blocking write can only wait for this worker to pick the frame
        // up: it has no outstanding result, so it is blocked in its task read.
        let hex = value::str_alloc(encoded);
        match process::queue_put(state, hex, true, None) {
            Ok(()) => outstanding[worker] = Some(id),
            Err(error) => {
                note_error(&mut first_error, mp_message(&error));
                abort = true;
            }
        }
    }

    // Drain the last task of every worker (and any task whose loop iteration
    // was skipped after an error) so no stale frame reaches a later call.
    for (worker, pending) in outstanding.iter_mut().enumerate() {
        if let Some(id) = pending.take() {
            match collect_one(pool_value, worker, id, results) {
                Ok(None) => {}
                Ok(Some(message)) => note_error(&mut first_error, message),
                Err(message) => note_error(&mut first_error, message),
            }
        }
    }

    match first_error {
        Some(message) => Err(message),
        None => Ok(results),
    }
}

fn note_error(slot: &mut Option<String>, message: String) {
    if slot.is_none() {
        *slot = Some(message);
    }
}

fn queue_tag(pool_value: i64, task: bool, worker: usize) -> Option<i64> {
    let pool = value::as_pool(pool_value)?;
    let queues = if task {
        &pool.task_queues
    } else {
        &pool.result_queues
    };
    queues.get(worker).copied()
}

/// Encodes one task as the hex transport string.
fn encode_task(id: usize, function: i64, arguments: i64) -> Result<String, String> {
    let id_value = i64::try_from(id)
        .ok()
        .and_then(value::encode_int)
        .ok_or_else(|| "Pool task id is out of range".to_string())?;
    let task = value::list_new(vec![id_value, function, arguments]);
    std::hint::black_box((function, arguments));
    let bytes = serialize::encode(task, true).map_err(|error| error.dumps_message())?;
    Ok(serialize::to_hex(&bytes))
}

/// Reads the result for `expected` from one worker and stores it at that
/// index. Returns `Ok(Some(message))` for a task-level failure and `Err` when
/// the transport itself failed (the worker is unusable).
fn collect_one(
    pool_value: i64,
    worker: usize,
    expected: usize,
    results: i64,
) -> Result<Option<String>, String> {
    let Some(result_queue) = queue_tag(pool_value, false, worker) else {
        return Err("Pool result queue is invalid".to_string());
    };
    let pid = worker_pid(pool_value, worker);
    let frame = read_result(result_queue, pid)?;
    let Some(text) = value::as_str(frame) else {
        return Ok(Some("Pool worker returned a non-string result".to_string()));
    };
    let text = text.to_string();
    match decode_result(&text) {
        Ok((id, ok, payload)) => {
            if id != expected as i64 {
                // A worker that cannot decode its task cannot know the id and
                // reports `None`; its message is still the useful diagnosis.
                if id == NONE && !ok {
                    store_result(results, expected, NONE);
                    return Ok(Some(result_message(payload)));
                }
                return Ok(Some(format!(
                    "Pool worker returned result {id} for task {expected}"
                )));
            }
            let stored = if ok { payload } else { NONE };
            store_result(results, expected, stored);
            if ok {
                Ok(None)
            } else {
                Ok(Some(result_message(payload)))
            }
        }
        Err(message) => {
            store_result(results, expected, NONE);
            Ok(Some(message))
        }
    }
}

fn worker_pid(pool_value: i64, worker: usize) -> i64 {
    value::as_pool(pool_value)
        .and_then(|pool| {
            pool.pids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(worker)
                .copied()
        })
        .map(i64::from)
        .unwrap_or(0)
}

fn store_result(results: i64, index: usize, value: i64) {
    if let Some(mut items) = value::list_items_mut(results) {
        if let Some(slot) = items.get_mut(index) {
            *slot = value;
            heap::write_barrier_edge(results, value);
        }
    }
}

/// Reads one result frame, detecting a worker that exited without producing it.
fn read_result(result_queue: i64, pid: i64) -> Result<i64, String> {
    if pid <= 0 {
        return Err("Pool worker is not running".to_string());
    }
    loop {
        let Some(state) = value::as_mp_queue(result_queue) else {
            return Err("Pool result queue is invalid".to_string());
        };
        match process::queue_get(state, true, Some(RESULT_POLL)) {
            Ok(value) => return Ok(value),
            Err(process::MpError::Empty) => {
                if pid > 0 && !process::child_alive(pid) {
                    // A result may have been written just before exit; give
                    // the final bytes a bounded chance to arrive.
                    let Some(state) = value::as_mp_queue(result_queue) else {
                        return Err("Pool result queue is invalid".to_string());
                    };
                    return match process::queue_get(state, true, Some(RESULT_POLL)) {
                        Ok(value) => Ok(value),
                        Err(process::MpError::Empty) | Err(process::MpError::Closed) => {
                            Err("Pool worker exited unexpectedly".to_string())
                        }
                        Err(error) => Err(mp_message(&error)),
                    };
                }
            }
            Err(process::MpError::Closed) => {
                return Err("Pool result queue is closed".to_string());
            }
            Err(error) => return Err(mp_message(&error)),
        }
    }
}

fn decode_result(hex: &str) -> Result<(i64, bool, i64), String> {
    let bytes =
        serialize::from_hex(hex).ok_or_else(|| "invalid Pool result encoding".to_string())?;
    let decoded = serialize::decode(&bytes, true).map_err(|error| error.loads_message())?;
    let items = value::as_list(decoded)
        .or_else(|| value::as_tuple(decoded))
        .ok_or_else(|| "Pool result must be a list".to_string())?;
    if items.len() != 3 {
        return Err("Pool result must be [id, ok, value]".to_string());
    }
    let id = value::as_integer(items[0]).unwrap_or(NONE);
    Ok((id, value::truthy(items[1]), items[2]))
}

fn result_message(payload: i64) -> String {
    match value::as_str(payload) {
        Some(message) => message.to_string(),
        None => value::to_string(payload),
    }
}

fn mp_message(error: &process::MpError) -> String {
    match error {
        process::MpError::Full => "queue.Full".to_string(),
        process::MpError::Empty => "queue.Empty".to_string(),
        process::MpError::Closed => "queue is closed".to_string(),
        process::MpError::Io(message) => message.clone(),
        process::MpError::Pickle(error) => error.dumps_message(),
    }
}

fn take_pending_message() -> String {
    let pending = exc::pending_value();
    let message = value::exception_data(pending)
        .map(|data| format!("{}: {}", data.name, data.msg))
        .unwrap_or_else(|| "Pool task failed".to_string());
    exc::clear();
    message
}

// -------------------------------------------------------------------------
// Worker
// -------------------------------------------------------------------------

/// Native spawn entry for a pool worker: `__pyx_pool_worker(tasks, results)`.
///
/// Registered by [`crate::spawn::reset`] so a spawned child can resolve the
/// name; the fork path calls [`run_worker`] directly. Never returns.
pub(crate) extern "C" fn pool_worker_entry(argv: *const i64) -> i64 {
    if argv.is_null() {
        process::exit_now(0);
    }
    // SAFETY: the spawn registry declares arity 2 and `spawn::run_payload`
    // checks the decoded argument count before calling the entry point.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    run_worker(args[0], args[1]);
    process::exit_now(0)
}

/// Task loop: block on the private task queue, run the callable and publish a
/// result. `None` is the shutdown sentinel.
fn run_worker(task_queue: i64, result_queue: i64) {
    loop {
        let Some(state) = value::as_mp_queue(task_queue) else {
            return;
        };
        let task = match process::queue_get(state, true, None) {
            Ok(task) => task,
            Err(process::MpError::Closed) => return,
            Err(error) => {
                let _ = send_error(result_queue, NONE, mp_message(&error));
                return;
            }
        };
        if task == NONE {
            return;
        }

        let Some(text) = value::as_str(task) else {
            if send_error(
                result_queue,
                NONE,
                "Pool task frame is not a string".to_string(),
            )
            .is_err()
            {
                return;
            }
            continue;
        };
        let text = text.to_string();
        match run_task(&text) {
            Ok((id, value)) => {
                if send_result(result_queue, id, true, value).is_err() {
                    return;
                }
            }
            Err((id, message)) => {
                if send_error(result_queue, id, message).is_err() {
                    return;
                }
            }
        }
    }
}

/// Decodes and executes one task. Errors carry the task id when it could be
/// recovered so the parent can attribute the failure.
fn run_task(hex: &str) -> Result<(i64, i64), (i64, String)> {
    let bytes =
        serialize::from_hex(hex).ok_or_else(|| (NONE, "invalid Pool task encoding".to_string()))?;
    let task = serialize::decode(&bytes, true).map_err(|error| (NONE, error.loads_message()))?;
    let id = task_id(task);
    let (callable, arguments) = decode_task(task).map_err(|message| (id, message))?;

    let args = if let Some(items) = value::as_list(arguments) {
        items.to_vec()
    } else if let Some(items) = value::as_tuple(arguments) {
        items.to_vec()
    } else {
        return Err((id, "Pool task arguments must be a list".to_string()));
    };
    let argc = args.len();
    let argv = if argc == 0 {
        std::ptr::null()
    } else {
        args.as_ptr()
    };

    // `task` owns the callable and the argument list. Keep both reachable on
    // the stack for the whole user call: the collector scans the native stack.
    let anchor = (task, callable, arguments);
    let result = builtins::call(callable, argc, argv);
    std::hint::black_box(anchor);

    if exc::pending() {
        Err((id, take_pending_message()))
    } else {
        Ok((id, result))
    }
}

fn task_id(task: i64) -> i64 {
    value::as_list(task)
        .or_else(|| value::as_tuple(task))
        .and_then(|items| items.first().copied())
        .filter(|item| value::as_integer(*item).is_some())
        .unwrap_or(NONE)
}

/// Validates `[id, callable, args]` and returns the callable and argument list.
fn decode_task(task: i64) -> Result<(i64, i64), String> {
    let items = value::as_list(task)
        .or_else(|| value::as_tuple(task))
        .ok_or_else(|| "Pool task must be a list".to_string())?;
    if items.len() != 3 {
        return Err("Pool task must be [id, callable, args]".to_string());
    }
    if value::as_integer(items[0]).is_none() {
        return Err("Pool task id must be an int".to_string());
    }
    if value::as_function(items[1]).is_none() {
        return Err("Pool task callable is not a function".to_string());
    }
    let arguments = items[2];
    if value::as_list(arguments).is_none() && value::as_tuple(arguments).is_none() {
        return Err("Pool task arguments must be a list".to_string());
    }
    Ok((items[1], arguments))
}

/// Encodes and publishes an error result. Non-string payloads cannot occur
/// here, but the fallback in [`send_result`] still guards against it.
fn send_error(result_queue: i64, id: i64, message: String) -> Result<(), String> {
    let payload = value::str_alloc(message);
    send_result(result_queue, id, false, payload)
}

/// Encodes and publishes one result frame. A value that cannot be encoded
/// (for example a function returned by the user) becomes an error result.
fn send_result(result_queue: i64, id: i64, ok: bool, payload: i64) -> Result<(), String> {
    let encoded = encode_result(id, ok, payload).or_else(|_| {
        let message = format!("cannot pickle '{}' object", value::type_name(payload));
        let fallback = value::str_alloc(message);
        encode_result(id, false, fallback)
    })?;
    let Some(state) = value::as_mp_queue(result_queue) else {
        return Err("Pool result queue is invalid".to_string());
    };
    let hex = value::str_alloc(encoded);
    process::queue_put(state, hex, true, None).map_err(|error| mp_message(&error))
}

fn encode_result(id: i64, ok: bool, payload: i64) -> Result<String, String> {
    let result = value::list_new(vec![id, value::bool_value(ok), payload]);
    let bytes = serialize::encode(result, true).map_err(|error| error.dumps_message())?;
    Ok(serialize::to_hex(&bytes))
}
