//! Process primitives and the cross-process queue transport.
//!
//! The default start method is `fork(2)`: the child inherits the parent's
//! address space, so Hydro values (including heap objects and function wrapper
//! addresses) are usable across the fork without re-import. `fork` only
//! duplicates the calling thread; the runtime rebuilds its thread bookkeeping
//! in the child ([`crate::heap::after_fork_child`]).
//!
//! The `spawn` start method starts a fresh process by executing the current
//! executable again. The target function is named in a 0600 temporary payload
//! file (`HYDRO_SPAWN_PAYLOAD`) serialized in spawn mode; the child registers
//! the program's functions, resolves the target by name and runs only that
//! function (see [`crate::spawn`]). No module body runs in the child.
//!
//! Fork boundary: locks or queue mutexes held by threads that vanish in the
//! fork stay held in the child, and waiting threads never wake. Programs must
//! fork from a stable state.
//!
//! `multiprocessing.Queue` is a `socketpair(AF_UNIX, SOCK_STREAM)` whose file
//! descriptors are inherited by the child (across `fork` and across the
//! `exec` used by `spawn`). Frames are a 4-byte little-endian length followed
//! by the payload produced by [`crate::serialize`]. `maxsize` is accepted as
//! metadata only: a cross-process item count is not available.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::serialize::{self, PickleError};
use crate::value::{self, FunctionBody, MpQueueState, MpReadState, ProcessState, NO_EXIT_CODE};
use crate::{builtins, exc, heap, modules};

/// Upper bound for a single queue frame (guards against malformed lengths).
const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Start method selected for `multiprocessing.Process.start`.
///
/// `None` means "not resolved yet"; the first query reads
/// `HYDRO_START_METHOD` (a spawned child inherits `spawn`) and otherwise falls
/// back to `fork`.
static START_METHOD: Mutex<Option<String>> = Mutex::new(None);

/// Source file a JIT-compiled program was started from.
///
/// A spawned JIT child re-executes the CLI with `__spawn <source>`; an AOT
/// binary has no source path and re-executes itself without arguments.
static JIT_SOURCE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The active start method (`"fork"` or `"spawn"`).
pub(crate) fn get_start_method() -> String {
    let mut state = START_METHOD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(method) = state.as_ref() {
        return method.clone();
    }
    let method = match env::var("HYDRO_START_METHOD").as_deref() {
        Ok("spawn") => "spawn",
        _ => "fork",
    }
    .to_string();
    *state = Some(method.clone());
    method
}

/// Selects a start method; unknown names are rejected.
pub(crate) fn set_start_method(method: &str) -> Result<(), String> {
    match method {
        "fork" | "spawn" => {
            *START_METHOD
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(method.to_string());
            Ok(())
        }
        other => Err(format!(
            "start method '{other}' is not supported (use 'fork' or 'spawn')"
        )),
    }
}

/// Restores the environment-derived default at program entry.
pub(crate) fn reset_start_method() {
    *START_METHOD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// Records the canonical source path used to re-execute a JIT program.
pub(crate) fn set_jit_source(path: PathBuf) {
    *JIT_SOURCE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
}

fn jit_source() -> Option<PathBuf> {
    JIT_SOURCE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Failures of a `multiprocessing.Queue` operation.
#[derive(Debug)]
pub(crate) enum MpError {
    Full,
    Empty,
    Closed,
    Io(String),
    Pickle(PickleError),
}

// -------------------------------------------------------------------------
// fork / waitpid / exit
// -------------------------------------------------------------------------

/// `fork(2)`. On failure returns the OS error message.
pub(crate) fn fork() -> Result<i64, String> {
    // SAFETY: `fork` has no memory-safety preconditions. Threads other than
    // the caller do not survive, which the child-side reset handles.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(pid as i64)
}

/// Rebuilds runtime bookkeeping in a freshly forked child.
pub(crate) fn child_after_fork() {
    heap::after_fork_child();
    modules::after_fork_child();
    exc::init();
}

/// Flushes the standard streams before `_exit` (which skips destructors).
pub(crate) fn flush_stdio() {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

/// Terminates the calling process without running destructors or atexit
/// handlers.
pub(crate) fn exit_now(code: i32) -> ! {
    flush_stdio();
    // SAFETY: `_exit` terminates the process immediately; no Rust state is
    // observed afterwards.
    unsafe { libc::_exit(code) }
}

/// Executes the target of a `multiprocessing.Process` in the forked child and
/// terminates the child. Never returns.
fn run_child(process: &ProcessState) -> ! {
    child_after_fork();
    let target = process.target;
    let argv: Box<[i64]> = process.args.clone().into_boxed_slice();
    let argc = argv.len();
    let _ = builtins::call(target, argc, argv.as_ptr());
    if exc::pending() {
        let _ = exc::uncaught();
        exit_now(1);
    }
    exit_now(0)
}

/// Starts a `multiprocessing.Process` exactly once.
///
/// With the `fork` method the parent records the pid and the child resets the
/// runtime, runs the target and exits. With `spawn` the parent serializes the
/// target name and arguments, then executes a fresh copy of the program. In
/// both cases a failed start leaves the process unstarted so it can be
/// retried.
pub(crate) fn start_process(process: &ProcessState) -> Result<(), String> {
    if process.started.swap(true, Ordering::AcqRel) {
        return Err("cannot start a process twice".to_string());
    }
    let result = if get_start_method() == "spawn" {
        start_spawn(process)
    } else {
        start_fork(process)
    };
    if result.is_err() {
        process.started.store(false, Ordering::Release);
    }
    result
}

fn start_fork(process: &ProcessState) -> Result<(), String> {
    let pid = fork()?;
    if pid == 0 {
        run_child(process);
    }
    process.pid.store(pid, Ordering::Release);
    Ok(())
}

/// Starts a fresh process that executes only the target function.
fn start_spawn(process: &ProcessState) -> Result<(), String> {
    let Some(function) = value::as_function(process.target) else {
        return Err("process target is not callable".to_string());
    };
    match &function.body {
        FunctionBody::Wrapper { entry, captures } => {
            if *entry == 0 {
                return Err("process target has no code".to_string());
            }
            if !captures.is_empty() {
                return Err("cannot spawn a closure; use a named module-level function".to_string());
            }
        }
        FunctionBody::Native { .. } => {
            return Err(
                "cannot spawn a native function; use a named module-level function".to_string(),
            )
        }
        FunctionBody::Builtin(_) => {
            return Err(
                "cannot spawn a builtin function; use a named module-level function".to_string(),
            )
        }
        // Getter, partial, bound-method and identity callables have no stable
        // code entry.
        FunctionBody::AttributeGetter { .. }
        | FunctionBody::ItemGetter { .. }
        | FunctionBody::Partial { .. }
        | FunctionBody::BoundMethod { .. }
        | FunctionBody::Host { .. }
        | FunctionBody::Identity => {
            return Err(
                "cannot spawn a derived callable; use a named module-level function".to_string(),
            )
        }
    }
    if function.name == "<lambda>" || function.name.is_empty() {
        return Err(
            "cannot spawn a lambda function; use a named module-level function".to_string(),
        );
    }

    let name = function.name.clone();
    let pid = start_named_target(&name, process.args.clone())?;
    process.pid.store(pid, Ordering::Release);
    Ok(())
}

/// Starts a fresh process that executes the named registered function.
///
/// Used by `Process.start` and by the pool runtime, which launches its native
/// worker entry through the same payload/exec pipeline. Returns the child pid;
/// the caller reaps it with [`wait_for_child`].
pub(crate) fn start_named_target(name: &str, args: Vec<i64>) -> Result<i64, String> {
    let name_value = value::str_alloc(name.to_string());
    let args_value = value::tuple_new(args);
    let payload = value::tuple_new(vec![name_value, args_value]);
    let bytes = serialize::encode(payload, true).map_err(|error| error.dumps_message())?;
    let path = write_payload(&bytes)?;
    let result = launch_payload(&path);
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result
}

/// Writes the payload to a fresh, owner-only temporary file.
fn write_payload(bytes: &[u8]) -> Result<PathBuf, String> {
    static NEXT_PAYLOAD: AtomicU64 = AtomicU64::new(0);
    let directory = env::temp_dir();
    for _ in 0..64 {
        let sequence = NEXT_PAYLOAD.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "hydro-spawn-{}-{sequence}.payload",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes) {
                    let _ = fs::remove_file(&path);
                    return Err(format!(
                        "cannot write spawn payload '{}': {error}",
                        path.display()
                    ));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "cannot create spawn payload '{}': {error}",
                    path.display()
                ))
            }
        }
    }
    Err("cannot create a unique spawn payload file".to_string())
}

/// Re-executes the program so `hydro_spawn_child` can run the payload target.
/// Returns the child pid; the caller reaps it.
fn launch_payload(path: &Path) -> Result<i64, String> {
    let executable = env::current_exe()
        .map_err(|error| format!("cannot locate the current executable: {error}"))?;
    let mut command = Command::new(executable);
    if let Some(source) = jit_source() {
        command.arg("__spawn").arg(source);
    }
    command
        .env("HYDRO_SPAWN_PAYLOAD", path)
        .env("HYDRO_START_METHOD", "spawn")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // Keep prior output ahead of anything the child writes.
    flush_stdio();
    let child = command
        .spawn()
        .map_err(|error| format!("cannot start spawned process: {error}"))?;
    // Dropping `child` neither waits nor reaps; `join` waits for the pid.
    Ok(i64::from(child.id()))
}

/// Waits for `pid` and returns the raw wait status. Retries `EINTR`.
fn waitpid_raw(pid: i64, options: i32) -> Result<(i64, i64), String> {
    let Ok(pid) = i32::try_from(pid) else {
        return Err("invalid pid".to_string());
    };
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `status` is a valid out-pointer for the duration of the
        // call; `pid`/`options` are plain integers.
        let result = unsafe { libc::waitpid(pid, &mut status, options) };
        if result >= 0 {
            return Ok((i64::from(result), i64::from(status)));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error.to_string());
    }
}

/// Blocks in `waitpid(pid, 0)` while parked for the collector.
pub(crate) fn wait_for_process(process: &ProcessState) {
    let pid = process.pid.load(Ordering::Acquire);
    if pid <= 0 {
        return;
    }
    heap::blocking_enter();
    let result = waitpid_raw(pid, 0);
    heap::blocking_leave();
    match result {
        Ok((reaped, status)) => {
            // A successful wait reaps this exact pid (or reports ECHILD,
            // which surfaces as an error).
            if reaped == pid {
                store_exit(process, status);
            }
        }
        Err(_) => {
            // The child was reaped elsewhere (another waitpid): leave the
            // exit code unknown rather than inventing one.
            process.reaped.store(true, Ordering::Release);
        }
    }
}

/// Blocks until `pid` exits, while parked for the collector.
///
/// `ECHILD` means the child was already reaped (for example by a liveness
/// probe); that is success for a join.
pub(crate) fn wait_for_child(pid: i64) -> Result<(), String> {
    let Ok(raw_pid) = i32::try_from(pid) else {
        return Err("invalid pid".to_string());
    };
    if raw_pid <= 0 {
        return Err("invalid pid".to_string());
    }
    heap::blocking_enter();
    let mut status: libc::c_int = 0;
    let result = loop {
        // SAFETY: `status` is a valid out-pointer for the call.
        let waited = unsafe { libc::waitpid(raw_pid, &mut status, 0) };
        if waited >= 0 {
            break Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => break Ok(()),
            _ => break Err(error.to_string()),
        }
    };
    heap::blocking_leave();
    result
}

/// Grace period an orphaned child gets to exit after `SIGTERM` before it is
/// killed outright.
const ORPHAN_GRACE: Duration = Duration::from_millis(200);

/// Upper bound for reaping a child after `SIGKILL`.
///
/// `SIGKILL` cannot be ignored, so this only elapses for a child stuck in
/// uninterruptible sleep; the cleanup then gives up instead of parking a
/// payload `Drop` indefinitely.
const ORPHAN_KILL_GRACE: Duration = Duration::from_secs(1);

/// Poll interval while waiting for an orphaned child to be reaped.
const ORPHAN_POLL: Duration = Duration::from_millis(5);

/// Outcome of the non-blocking probe used by the orphan cleanup.
enum OrphanProbe {
    /// Exited and was reaped by the probe; holds the raw wait status.
    Reaped(i64),
    /// Still running and still a child of this process.
    Running,
    /// Not this process's child, or already reaped elsewhere. Nothing to do:
    /// in particular a forked child must not signal or wait for the parent's
    /// children (its inherited copies of that state).
    NotOurs,
}

/// Outcome of waiting for an orphaned child to exit.
enum OrphanWait {
    /// Exited and was reaped; holds the raw wait status.
    Reaped(i64),
    /// Reaped elsewhere (or not our child any more).
    Gone,
    /// Still running when the grace period elapsed.
    Timeout,
}

/// `waitpid(pid, WNOHANG)` with `EINTR` retried.
fn probe_orphan(raw_pid: i32) -> OrphanProbe {
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a valid out-pointer for the call.
        let waited = unsafe { libc::waitpid(raw_pid, &mut status, libc::WNOHANG) };
        if waited == raw_pid {
            return OrphanProbe::Reaped(i64::from(status));
        }
        if waited == 0 {
            return OrphanProbe::Running;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            // `ECHILD` covers both "reaped by another waiter" and "inherited
            // state whose pid is not our child".
            _ => return OrphanProbe::NotOurs,
        }
    }
}

/// Waits at most `grace` for `raw_pid` to exit, reaping it through `WNOHANG`.
fn wait_orphan(raw_pid: i32, grace: Duration) -> OrphanWait {
    let deadline = Instant::now() + grace;
    loop {
        match probe_orphan(raw_pid) {
            OrphanProbe::Reaped(status) => return OrphanWait::Reaped(status),
            OrphanProbe::NotOurs => return OrphanWait::Gone,
            OrphanProbe::Running => {}
        }
        if Instant::now() >= deadline {
            return OrphanWait::Timeout;
        }
        std::thread::sleep(ORPHAN_POLL);
    }
}

/// Terminates and reaps an orphaned child without parking for the collector.
///
/// Called from payload `Drop` implementations, which run inside the collector's
/// sweep where [`heap::blocking_enter`] is not allowed. The sequence is
/// `SIGTERM`, a bounded non-blocking grace period, then `SIGKILL`; the child is
/// reaped as soon as it exits. Only a direct child of this process is touched,
/// so a forked child dropping its inherited copy leaves the parent's processes
/// alone.
pub(crate) fn reap_orphan_child(pid: i64) {
    let Ok(raw_pid) = i32::try_from(pid) else {
        return;
    };
    if raw_pid <= 0 {
        return;
    }
    match probe_orphan(raw_pid) {
        OrphanProbe::Reaped(_) | OrphanProbe::NotOurs => {}
        OrphanProbe::Running => {
            // SAFETY: `kill` takes plain integers and has no other
            // preconditions. `ESRCH` just means the child exited meanwhile.
            unsafe { libc::kill(raw_pid, libc::SIGTERM) };
            if let OrphanWait::Timeout = wait_orphan(raw_pid, ORPHAN_GRACE) {
                // SAFETY: as above; `SIGKILL` cannot be caught or ignored.
                unsafe { libc::kill(raw_pid, libc::SIGKILL) };
                let _ = wait_orphan(raw_pid, ORPHAN_KILL_GRACE);
            }
        }
    }
}

/// Reaps a started `Process` that became unreachable without `join`.
///
/// Best effort: the child is stopped (as [`terminate_process`] would) and
/// reaped, and a successful reap stores the exit code, so the object keeps the
/// `join`/`exitcode` semantics up to the moment it is dropped. A child that was
/// already reaped -- by `join`, a liveness probe or an earlier drop -- is never
/// waited for twice. Never parks for the collector.
pub(crate) fn reap_orphan_process(process: &ProcessState) {
    if !process.started.load(Ordering::Acquire) || process.reaped.load(Ordering::Acquire) {
        return;
    }
    let pid = process.pid.load(Ordering::Acquire);
    let Ok(raw_pid) = i32::try_from(pid) else {
        return;
    };
    if raw_pid <= 0 {
        return;
    }
    // Claim the single reap before touching the child: a concurrent waiter
    // (or a second drop path) that already set the flag must not be joined by
    // a second `waitpid`.
    if process
        .reaped
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    match probe_orphan(raw_pid) {
        OrphanProbe::Reaped(status) => {
            process
                .exitcode
                .store(exit_status(status), Ordering::Release);
        }
        OrphanProbe::NotOurs => {}
        OrphanProbe::Running => {
            // SAFETY: `kill` takes plain integers and has no preconditions.
            unsafe { libc::kill(raw_pid, libc::SIGTERM) };
            let status = match wait_orphan(raw_pid, ORPHAN_GRACE) {
                OrphanWait::Reaped(status) => Some(status),
                OrphanWait::Gone => None,
                OrphanWait::Timeout => {
                    // SAFETY: as above; `SIGKILL` is unignorable, so the
                    // bounded follow-up wait reaps the child promptly.
                    unsafe { libc::kill(raw_pid, libc::SIGKILL) };
                    match wait_orphan(raw_pid, ORPHAN_KILL_GRACE) {
                        OrphanWait::Reaped(status) => Some(status),
                        OrphanWait::Gone | OrphanWait::Timeout => None,
                    }
                }
            };
            if let Some(status) = status {
                process
                    .exitcode
                    .store(exit_status(status), Ordering::Release);
            }
        }
    }
}

/// Non-blocking liveness probe for a raw pid.
///
/// Reaps the child when it has exited; a later [`wait_for_child`] then treats
/// `ECHILD` as success.
pub(crate) fn child_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    matches!(waitpid_raw(pid, libc::WNOHANG), Ok((0, _)))
}

/// Sends `SIGTERM` to a raw child pid.
///
/// A child that is already gone (`ESRCH`) is not an error: the caller only
/// needs the process to be stopped. The caller reaps the child with
/// [`wait_for_child`].
pub(crate) fn terminate_child(pid: i64) -> Result<(), String> {
    if pid <= 0 {
        return Err("invalid pid".to_string());
    }
    let Ok(raw_pid) = i32::try_from(pid) else {
        return Err("invalid pid".to_string());
    };
    // SAFETY: `kill` takes plain integers and has no other preconditions.
    if unsafe { libc::kill(raw_pid, libc::SIGTERM) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error.to_string())
}

/// `Process.terminate()`: sends `SIGTERM` after the start and before the
/// child is reaped. Idempotent; once the child is reaped it is a no-op.
///
/// A later `join` still waits for (and reaps) the child, so `exitcode`
/// reports the signal as a negative code.
pub(crate) fn terminate_process(process: &ProcessState) -> Result<(), String> {
    if !process.started.load(Ordering::Acquire) {
        return Err("cannot terminate process before it is started".to_string());
    }
    if process.reaped.load(Ordering::Acquire) {
        return Ok(());
    }
    let pid = process.pid.load(Ordering::Acquire);
    if pid <= 0 {
        return Ok(());
    }
    terminate_child(pid)
}

fn store_exit(process: &ProcessState, status: i64) {
    process
        .exitcode
        .store(exit_status(status), Ordering::Release);
    process.reaped.store(true, Ordering::Release);
}

/// Decodes a wait status the way Python does: exit code, or negative signal.
pub(crate) fn exit_status(status: i64) -> i64 {
    let status = status & 0xffff;
    if status & 0x7f == 0 {
        (status >> 8) & 0xff
    } else {
        -(status & 0x7f)
    }
}

/// Polls a process with `WNOHANG`; returns whether it is still alive.
pub(crate) fn process_alive(process: &ProcessState) -> bool {
    if !process.started.load(Ordering::Acquire) || process.reaped.load(Ordering::Acquire) {
        return false;
    }
    let pid = process.pid.load(Ordering::Acquire);
    if pid <= 0 {
        return false;
    }
    match waitpid_raw(pid, libc::WNOHANG) {
        Ok((0, _)) => true,
        Ok((reaped, status)) => {
            if reaped == pid {
                store_exit(process, status);
            }
            false
        }
        Err(_) => false,
    }
}

/// The process exit code, or `None` while it runs / when it was not started.
pub(crate) fn process_exitcode(process: &ProcessState) -> Option<i64> {
    if !process.started.load(Ordering::Acquire) {
        return None;
    }
    if !process.reaped.load(Ordering::Acquire) {
        // Opportunistically reap a finished child, like CPython's
        // `Process.exitcode`.
        let _ = process_alive(process);
    }
    if !process.reaped.load(Ordering::Acquire) {
        return None;
    }
    let code = process.exitcode.load(Ordering::Acquire);
    (code != NO_EXIT_CODE).then_some(code)
}

/// `os.waitpid(pid, options)`: returns `(pid, raw_status)`.
pub(crate) fn os_waitpid(pid: i64, options: i64) -> Result<(i64, i64), String> {
    let Ok(options) = i32::try_from(options) else {
        return Err("options out of range".to_string());
    };
    if options == 0 {
        heap::blocking_enter();
        let result = waitpid_raw(pid, options);
        heap::blocking_leave();
        result
    } else {
        waitpid_raw(pid, options)
    }
}

// -------------------------------------------------------------------------
// multiprocessing.Queue
// -------------------------------------------------------------------------

/// Creates a socketpair-backed queue object.
pub(crate) fn new_queue(maxsize: usize) -> Result<i64, String> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `fds` is a valid array of two descriptors for the call.
    let result = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // `socketpair` above does not pass `SOCK_CLOEXEC`, but clear the flag
    // explicitly: the descriptors must survive the `exec` performed by the
    // spawn start method.
    clear_cloexec(fds[0]);
    clear_cloexec(fds[1]);
    Ok(value::mp_queue_new(fds[0], fds[1], maxsize))
}

/// Makes `fd` survive an `execve` performed by a spawned child.
fn clear_cloexec(fd: i32) {
    // SAFETY: `fd` is a valid descriptor owned by the queue; failure leaves
    // the flags unchanged.
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, 0);
    }
}

fn take_frame(state: &mut MpReadState) -> Result<Option<Vec<u8>>, MpError> {
    if state.buffer.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([
        state.buffer[0],
        state.buffer[1],
        state.buffer[2],
        state.buffer[3],
    ]) as usize;
    if len > MAX_FRAME {
        return Err(MpError::Io("queue frame is too large".to_string()));
    }
    let total = 4 + len;
    if state.buffer.len() < total {
        return Ok(None);
    }
    let frame = state.buffer[4..total].to_vec();
    state.buffer.drain(..total);
    Ok(Some(frame))
}

/// Receives once into `state.buffer`. `nonblock` uses `MSG_DONTWAIT`.
fn recv_once(fd: i32, state: &mut MpReadState, nonblock: bool) -> Result<(), MpError> {
    let mut scratch = [0u8; 8192];
    let flags = if nonblock { libc::MSG_DONTWAIT } else { 0 };
    loop {
        // SAFETY: `scratch` is a valid writable buffer of `scratch.len()`.
        let read = unsafe {
            libc::recv(
                fd,
                scratch.as_mut_ptr().cast::<libc::c_void>(),
                scratch.len(),
                flags,
            )
        };
        if read > 0 {
            state.buffer.extend_from_slice(&scratch[..read as usize]);
            return Ok(());
        }
        if read == 0 {
            return Err(MpError::Closed);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                return Err(MpError::Empty)
            }
            _ => return Err(MpError::Io(format!("queue read: {error}"))),
        }
    }
}

/// Waits until `fd` is ready. Returns `false` on timeout.
fn poll_until(fd: i32, events: i16, deadline: Instant) -> Result<bool, MpError> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        let remaining = deadline - now;
        let timeout_ms = (remaining.as_millis().min(i32::MAX as u128) as i32).max(1);
        let mut pollfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: a single valid `pollfd` is passed with count 1.
        let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if result > 0 {
            return Ok(true);
        }
        if result == 0 {
            return Ok(false);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(MpError::Io(format!("queue poll: {error}")));
    }
}

/// Reads one complete frame. `state` must be locked by the caller.
pub(crate) fn read_frame(
    fd: i32,
    state: &mut MpReadState,
    block: bool,
    timeout: Option<Duration>,
) -> Result<Vec<u8>, MpError> {
    if let Some(frame) = take_frame(state)? {
        return Ok(frame);
    }
    if !block {
        recv_once(fd, state, true)?;
        return take_frame(state)?.ok_or(MpError::Empty);
    }
    match timeout {
        None => loop {
            recv_once(fd, state, false)?;
            if let Some(frame) = take_frame(state)? {
                return Ok(frame);
            }
        },
        Some(limit) => {
            let deadline = Instant::now() + limit;
            loop {
                if !poll_until(fd, libc::POLLIN, deadline)? {
                    return Err(MpError::Empty);
                }
                match recv_once(fd, state, true) {
                    Ok(()) | Err(MpError::Empty) => {}
                    Err(error) => return Err(error),
                }
                if let Some(frame) = take_frame(state)? {
                    return Ok(frame);
                }
            }
        }
    }
}

/// Sends one chunk, retrying `EINTR`/partial writes. `nonblock` surfaces
/// `EAGAIN` as [`MpError::Full`].
fn send_chunk(fd: i32, data: &[u8], nonblock: bool) -> Result<usize, MpError> {
    let flags = send_flags(nonblock);
    loop {
        // SAFETY: `data` is a valid readable buffer of `data.len()` bytes.
        let written =
            unsafe { libc::send(fd, data.as_ptr().cast::<libc::c_void>(), data.len(), flags) };
        if written > 0 {
            return Ok(written as usize);
        }
        if written == 0 {
            return Err(MpError::Closed);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                return Err(MpError::Full)
            }
            _ => return Err(MpError::Io(format!("queue write: {error}"))),
        }
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn send_flags(nonblock: bool) -> i32 {
    let base = libc::MSG_NOSIGNAL;
    if nonblock {
        base | libc::MSG_DONTWAIT
    } else {
        base
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
fn send_flags(nonblock: bool) -> i32 {
    if nonblock {
        libc::MSG_DONTWAIT
    } else {
        0
    }
}

/// Writes one complete frame (header + payload). `state` must be locked by
/// the caller.
pub(crate) fn write_frame(
    fd: i32,
    payload: &[u8],
    block: bool,
    timeout: Option<Duration>,
) -> Result<(), MpError> {
    if payload.len() > MAX_FRAME {
        return Err(MpError::Io("queue frame is too large".to_string()));
    }
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(payload);

    let mut offset = 0usize;
    // A non-blocking write (or one with a timeout) probes the socket without
    // sleeping in `send`; the timeout case polls for writability instead.
    let send_nonblock = !matches!((block, timeout), (true, None));
    let deadline = if block {
        timeout.map(|limit| Instant::now() + limit)
    } else {
        None
    };
    loop {
        match send_chunk(fd, &frame[offset..], send_nonblock) {
            Ok(written) => {
                offset += written;
                if offset >= frame.len() {
                    return Ok(());
                }
            }
            Err(MpError::Full) => {
                let Some(deadline) = deadline else {
                    return Err(MpError::Full);
                };
                if !poll_until(fd, libc::POLLOUT, deadline)? {
                    return Err(MpError::Full);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// `multiprocessing.Queue.put`.
pub(crate) fn queue_put(
    queue: &MpQueueState,
    item: i64,
    block: bool,
    timeout: Option<Duration>,
) -> Result<(), MpError> {
    let payload = serialize::encode(item, false).map_err(MpError::Pickle)?;
    // Park even for a non-blocking send: the write mutex may be held by a
    // peer blocked in `send`, and the collector must never wait on a thread
    // that is waiting for a parked thread.
    heap::blocking_enter();
    let result = {
        let _write = queue
            .write
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_frame(queue.write_fd, &payload, block, timeout)
    };
    heap::blocking_leave();
    result
}

/// `multiprocessing.Queue.get`.
pub(crate) fn queue_get(
    queue: &MpQueueState,
    block: bool,
    timeout: Option<Duration>,
) -> Result<i64, MpError> {
    // The read mutex is held across a blocking `recv`; a second reader must
    // be parked while it waits for it, or a stop-the-world collection could
    // wait forever on a thread that is itself waiting for a parked thread.
    heap::blocking_enter();
    let frame = {
        let mut state = queue
            .read
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        read_frame(queue.read_fd, &mut state, block, timeout)
    };
    heap::blocking_leave();
    let bytes = frame?;
    serialize::decode(&bytes, false).map_err(MpError::Pickle)
}
