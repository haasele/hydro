//! `subprocess`: process spawning and pipe capture.
//!
//! The module covers [`run`], [`check_output`], `call`/`check_call` and a
//! native `Popen` object implementing CPython's stream-handle,
//! `communicate()` and context-manager protocol. `call` returns the child's
//! exit status and accepts `shell=True`, which routes the command through
//! `/bin/sh -c` like CPython on POSIX.
//!
//! `Popen` accepts CPython's `stdin`/`stdout`/`stderr`, `text`, `encoding`,
//! `errors`, `close_fds`, `start_new_session`, `cwd`, `env`, `shell`,
//! `executable` and `preexec_fn` parameters. Each stream selector accepts
//! `None`, `PIPE`, `DEVNULL`, `STDOUT` (for `stderr`) or a file descriptor /
//! object exposing `fileno()`. `stdin=PIPE` exposes a writable `p.stdin`,
//! `stdout=PIPE` a readable `p.stdout` and `stderr=PIPE` a readable
//! `p.stderr`; the objects are backed by the runtime's file values, so
//! `read`/`readline`/`write`/`flush`/`close`/`fileno` and `select` all work
//! on them. `stderr=STDOUT` shares the `stdout` pipe, so `p.stderr` stays
//! `None` and `p.stdout` sees the interleaved output.
//!
//! `encoding`/`errors` are validated and recorded; `text_mode` follows
//! CPython's `encoding or errors or text` truthiness. Pipe file objects are
//! binary or text according to `text_mode` (UTF-8). `universal_newlines` is
//! exposed as a read-only property aliasing `text_mode`.
//! `preexec_fn` cannot run a Python callable between `fork` and `exec`, so a
//! supplied hook raises `NotImplementedError` instead of being ignored.
//!
//! `communicate(input=None, timeout=None)` feeds `p.stdin`, drains both
//! output pipes concurrently, waits for the child and closes the handles,
//! returning `(stdout, stderr)`; unredirected streams answer `None` and text
//! mode applies universal-newline translation. A timeout raises the module's
//! `TimeoutExpired` class and keeps the buffered state so a later
//! `communicate()` call resumes and returns the complete output. `p.stdout`
//! (and `p.stderr`) may be read directly instead; data already read is not
//! replayed.
//!
//! `with Popen(...) as p:` returns the instance from `__enter__`; `__exit__`
//! closes the pipe handles and waits for the child.
//!
//! `wait`/`poll`/`send_signal`/`terminate`/`kill` are thread-safe: the child
//! entry is claimed by one waiter while concurrent callers poll the recorded
//! `returncode`, so every caller observes the same exit status.
//!
//! Spawned `Popen` processes live in a process-global
//! `Mutex<HashMap<u64, Child>>`; the instance carries the key in a private
//! `$id` attribute. `wait`/`poll` remove and reap the entry, so a second reap
//! is impossible; the recorded `returncode` answers later `poll`/`wait` calls.
//!
//! Blocking waits park the thread with [`heap::blocking_enter`] so a
//! concurrent collection does not wait for a thread sleeping in the kernel;
//! the helper threads copy raw bytes and never touch Hydro objects.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::core::{globals, methods};
use crate::exc::{
    self, ATTRIBUTE_ERROR, EXCEPTION, NOT_IMPLEMENTED_ERROR, OS_ERROR, OVERFLOW_ERROR,
    RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::value::{self, NONE};
use hydro_ir::MethodKind;

/// Private instance attribute holding the key into [`CHILDREN`].
const ID_ATTR: &str = "$id";

/// Private instance attribute set once `communicate()` starts, so a later
/// call with new input reports CPython's `ValueError` instead of re-feeding
/// the closed pipe.
const COMMUNICATION_STARTED_ATTR: &str = "$communication_started";

/// Cached `Popen` class.
const POPEN_CLASS_KEY: &str = "subprocess.Popen.class";

/// Cached `TimeoutExpired` exception class.
const TIMEOUT_EXPIRED_KEY: &str = "subprocess.TimeoutExpired.class";

/// `subprocess.PIPE`.
pub(crate) const PIPE: i64 = -1;
/// `subprocess.STDOUT`.
pub(crate) const STDOUT: i64 = -2;
/// `subprocess.DEVNULL`.
pub(crate) const DEVNULL: i64 = -3;

/// Live children of `Popen` objects, keyed by the instance's `$id`.
static CHILDREN: LazyLock<Mutex<HashMap<u64, Child>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Next key handed to a `Popen` instance. Starts at 1 so `0` is never a key.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn children() -> MutexGuard<'static, HashMap<u64, Child>> {
    // No user code runs while the map is locked, so poisoning (only possible
    // through a panic in this module) can be ignored.
    CHILDREN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One in-flight `communicate()` call.
///
/// The reader threads append raw bytes into the shared buffers; a timed-out
/// call leaves the structure here so a later `communicate()` resumes reading
/// instead of restarting (and replaying) the exchange.
struct Communication {
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    stdin_writer: Option<JoinHandle<()>>,
}

/// In-flight communications keyed by the owning instance's `$id`.
static COMMUNICATIONS: LazyLock<Mutex<HashMap<u64, Communication>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn communications() -> MutexGuard<'static, HashMap<u64, Communication>> {
    COMMUNICATIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// -------------------------------------------------------------------------
// Errors
// -------------------------------------------------------------------------

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn runtime_error(message: &str) -> i64 {
    exc::raise_with_message(RUNTIME_ERROR, message)
}

/// `OSError` carrying the platform errno, if any.
fn io_error(error: io::Error) -> i64 {
    match error.raw_os_error() {
        Some(code) => exc::raise_with_message(OS_ERROR, &format!("[Errno {code}] {error}")),
        None => exc::raise_with_message(OS_ERROR, &error.to_string()),
    }
}

// -------------------------------------------------------------------------
// Value helpers
// -------------------------------------------------------------------------

/// The argument at `index`, or `None` when the caller omitted it.
fn argument(args: &[i64], index: usize) -> i64 {
    args.get(index).copied().unwrap_or(NONE)
}

/// Copies a `str` or `bytes` value into a `String` (lossy for non-UTF-8).
fn text_of(argument: i64) -> Option<String> {
    if let Some(text) = value::as_str(argument) {
        return Some(text.to_string());
    }
    value::as_bytes(argument).map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

/// Encodes an `i64` as a Python integer, or `None` if out of range.
fn int_value(n: i64) -> i64 {
    value::encode_int(n).unwrap_or(NONE)
}

/// Converts the `args` parameter into the program and its arguments.
fn command_args(argument: i64) -> Result<Vec<String>, i64> {
    if let Some(text) = text_of(argument) {
        return Ok(vec![text]);
    }
    let Some(items) = value::as_list(argument).or_else(|| value::as_tuple(argument)) else {
        return Err(type_error(&format!(
            "subprocess: args must be a str or a sequence of str, not '{}'",
            value::type_name(argument)
        )));
    };
    let mut result = Vec::with_capacity(items.len());
    for &item in items {
        let Some(text) = text_of(item) else {
            return Err(type_error(&format!(
                "subprocess: args items must be str, not '{}'",
                value::type_name(item)
            )));
        };
        result.push(text);
    }
    Ok(result)
}

/// The Python-visible `args` attribute: a string stays a string, sequences
/// become a fresh list of strings (as CPython reports for `Popen.args`).
fn args_value(original: i64, args: &[String]) -> i64 {
    if value::as_str(original).is_some() || value::as_bytes(original).is_some() {
        return original;
    }
    value::list_new(
        args.iter()
            .map(|arg| value::str_alloc(arg.clone()))
            .collect(),
    )
}

/// An optional filesystem path (`None`, `str` or `bytes`).
fn path_option(argument: i64, name: &str) -> Result<Option<String>, i64> {
    if argument == NONE {
        return Ok(None);
    }
    if let Some(path) = value::as_path_like(argument) {
        return Ok(Some(path.to_string()));
    }
    match text_of(argument) {
        Some(text) => Ok(Some(text)),
        None => Err(type_error(&format!(
            "subprocess: {name} must be a str or bytes path, not '{}'",
            value::type_name(argument)
        ))),
    }
}

/// A float/int number of seconds, rejecting negative and non-finite values.
fn duration_option(argument: i64, name: &str) -> Result<Option<Duration>, i64> {
    if argument == NONE {
        return Ok(None);
    }
    let seconds = if let Some(value) = value::as_float(argument) {
        value
    } else if let Some(value) = value::as_integer(argument) {
        value as f64
    } else {
        return Err(type_error(&format!(
            "subprocess: {name} must be a number, not '{}'",
            value::type_name(argument)
        )));
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(value_error(&format!(
            "subprocess: {name} must be a non-negative number"
        )));
    }
    if seconds >= MAX_TIMEOUT_SECONDS {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "timeout is too large",
        ));
    }
    // The cap above keeps `from_secs_f64` (and a later `Instant + Duration`)
    // far from their limits even for hostile user input.
    match Duration::try_from_secs_f64(seconds) {
        Ok(duration) => Ok(Some(duration)),
        Err(_) => Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "timeout is too large",
        )),
    }
}

/// Upper bound for a parsed timeout (~31 years), matching the runtime's
/// thread/socket helpers so `Instant + Duration` cannot overflow.
const MAX_TIMEOUT_SECONDS: f64 = 1_000_000_000.0;

/// Optional `env` mapping. `None` entries are ignored (unset variables are
/// simply absent after `env_clear`).
fn env_option(argument: i64) -> Result<Option<Vec<(String, String)>>, i64> {
    if argument == NONE {
        return Ok(None);
    }
    let Some(data) = value::as_dict_data(argument) else {
        return Err(type_error(&format!(
            "subprocess: env must be a dict, not '{}'",
            value::type_name(argument)
        )));
    };
    let mut pairs = Vec::with_capacity(data.entries().len());
    for &(key, val) in data.entries() {
        let Some(name) = value::as_str(key) else {
            return Err(type_error(&format!(
                "subprocess: env keys must be str, not '{}'",
                value::type_name(key)
            )));
        };
        if val == NONE {
            continue;
        }
        let Some(text) = text_of(val) else {
            return Err(type_error(&format!(
                "subprocess: env values must be str, not '{}'",
                value::type_name(val)
            )));
        };
        pairs.push((name.to_string(), text));
    }
    Ok(Some(pairs))
}

/// `input=` payload (`str` or `bytes`).
fn input_option(argument: i64) -> Result<Option<Vec<u8>>, i64> {
    if argument == NONE {
        return Ok(None);
    }
    if let Some(text) = value::as_str(argument) {
        return Ok(Some(text.as_bytes().to_vec()));
    }
    if let Some(bytes) = value::as_bytes(argument) {
        return Ok(Some(bytes.to_vec()));
    }
    Err(type_error(&format!(
        "subprocess: input must be str or bytes, not '{}'",
        value::type_name(argument)
    )))
}

/// Decodes captured bytes according to `text=`.
fn output_value(bytes: Vec<u8>, text: bool) -> i64 {
    if text {
        value::str_alloc(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        value::bytes_new(bytes)
    }
}

// -------------------------------------------------------------------------
// Stream selectors
// -------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamMode {
    Inherit,
    Pipe,
    DevNull,
    Stdout,
    /// An already-open integer file descriptor supplied by the caller.
    Fd(i32),
}

fn stream_mode(argument: i64, name: &str) -> Result<StreamMode, i64> {
    if argument == NONE {
        return Ok(StreamMode::Inherit);
    }
    if let Some(number) = value::as_integer(argument) {
        return match number {
            PIPE => Ok(StreamMode::Pipe),
            STDOUT => Ok(StreamMode::Stdout),
            DEVNULL => Ok(StreamMode::DevNull),
            fd if (0..=i64::from(i32::MAX)).contains(&fd) => Ok(StreamMode::Fd(fd as i32)),
            _ => Err(value_error(&format!(
                "subprocess: {name} must be None, subprocess.PIPE, subprocess.STDOUT, \
                 subprocess.DEVNULL or a non-negative file descriptor, not '{}'",
                value::type_name(argument)
            ))),
        };
    }
    // CPython accepts any object exposing `fileno()`; the descriptor is
    // duplicated for the child, so the Python-visible object keeps ownership.
    if value::as_bigint(argument).is_some() {
        return Err(value_error(&format!(
            "subprocess: {name} is out of range for a file descriptor"
        )));
    }
    let method = methods::getattr(argument, b"fileno".as_ptr(), b"fileno".len());
    if exc::pending() {
        exc::clear();
        return Err(value_error(&format!(
            "subprocess: {name} must be None, subprocess.PIPE, subprocess.STDOUT, \
             subprocess.DEVNULL or a file descriptor, not '{}'",
            value::type_name(argument)
        )));
    }
    let result = crate::builtins::call(method, 0, std::ptr::null());
    if exc::pending() {
        return Err(NONE);
    }
    match value::as_integer(result) {
        Some(fd) if (0..=i64::from(i32::MAX)).contains(&fd) => Ok(StreamMode::Fd(fd as i32)),
        Some(_) => Err(value_error(&format!(
            "subprocess: {name}.fileno() is out of range for a file descriptor"
        ))),
        None => Err(type_error(&format!(
            "subprocess: {name}.fileno() must return an integer, not '{}'",
            value::type_name(result)
        ))),
    }
}

/// Duplicates a caller-owned descriptor for `Stdio`; the child setup closes
/// its copy, while the Python-visible descriptor stays owned by the caller.
fn fd_stdio(fd: i32) -> Result<Stdio, i64> {
    // SAFETY: `dup` takes an integer descriptor and returns a fresh one.
    let duplicated = unsafe { libc::dup(fd) };
    if duplicated < 0 {
        return Err(io_error(io::Error::last_os_error()));
    }
    // SAFETY: `duplicated` is a newly owned descriptor and is transferred to
    // `Stdio` exactly once.
    Ok(Stdio::from(unsafe { OwnedFd::from_raw_fd(duplicated) }))
}

fn output_mode(argument: i64, name: &str) -> Result<StreamMode, i64> {
    let mode = stream_mode(argument, name)?;
    if mode == StreamMode::Stdout {
        return Err(value_error(&format!(
            "subprocess: {name} must not be subprocess.STDOUT"
        )));
    }
    Ok(mode)
}

/// Creates a close-on-exec pipe; returns the read and write ends.
fn os_pipe() -> io::Result<(File, File)> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` points at two writable integers.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: both descriptors were just created by `pipe`.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    // SAFETY: the descriptors are fresh and owned by the returned `File`s.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

/// A `Stdio` that writes to the parent's stdout descriptor, used for
/// `stderr=STDOUT` when stdout is inherited.
fn duplicate_stdout() -> Result<Stdio, i64> {
    // SAFETY: `dup` only reads the descriptor number.
    let duplicated = unsafe { libc::dup(1) };
    if duplicated < 0 {
        return Err(io_error(io::Error::last_os_error()));
    }
    // SAFETY: `dup` returned a new descriptor owned by this call.
    Ok(Stdio::from(unsafe { OwnedFd::from_raw_fd(duplicated) }))
}

/// Wraps the parent's read end of a child pipe in a runtime file object.
fn pipe_reader_value(fd: OwnedFd, binary: bool) -> i64 {
    value::file_new_pipe(ChildStdout::from(fd), binary)
}

/// Wraps the parent's write end of a child pipe in a writable file object.
fn pipe_writer_value(pipe: ChildStdin, binary: bool) -> i64 {
    let file = File::from(OwnedFd::from(pipe));
    value::file_new_write("<stdin>".to_string(), 'w', file, binary)
}

/// The runtime file value stored in `popen.<name>`, if any.
fn stream_handle(instance: i64, name: &str) -> Option<i64> {
    value::instance_attr(instance, name).filter(|&handle| handle != NONE)
}

/// Closes `popen.<name>` when it is an open runtime file object. Closing an
/// already-closed file is a no-op, matching CPython.
fn close_stream_handle(instance: i64, name: &str) {
    if let Some(handle) = stream_handle(instance, name) {
        if let Some(file) = value::as_file(handle) {
            file.close();
        }
    }
}

// -------------------------------------------------------------------------
// Spawning and capture
// -------------------------------------------------------------------------

/// One fully parsed `run`/`check_output` invocation.
struct Plan {
    args: Vec<String>,
    stdin: StreamMode,
    input: Option<Vec<u8>>,
    stdout: StreamMode,
    stderr: StreamMode,
    cwd: Option<String>,
    timeout: Option<Duration>,
    env: Option<Vec<(String, String)>>,
}

struct Captured {
    status: ExitStatus,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
}

enum WaitError {
    Timeout,
    Io(io::Error),
}

fn build_command(
    args: &[String],
    cwd: &Option<String>,
    env: &Option<Vec<(String, String)>>,
    executable: Option<&str>,
) -> Result<Command, i64> {
    let Some((program, rest)) = args.split_first() else {
        return Err(value_error("subprocess: args must not be empty"));
    };
    // `executable=` replaces the program handed to `exec`, while `args[0]`
    // remains the child's `argv[0]` (CPython passes the sequence verbatim).
    let mut command = Command::new(executable.unwrap_or(program));
    #[cfg(unix)]
    if executable.is_some() {
        command.arg0(program);
    }
    command.args(rest);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some(pairs) = env {
        command.env_clear();
        for (name, val) in pairs {
            command.env(name, val);
        }
    }
    Ok(command)
}

/// Spawns the child with the requested descriptors, writes `input`, waits
/// (with an optional timeout) and drains every captured pipe.
///
/// All blocking work happens between `blocking_enter`/`blocking_leave`; the
/// reader threads copy bytes only, they never touch Hydro objects.
fn execute(plan: &Plan) -> Result<Captured, i64> {
    let mut command = build_command(&plan.args, &plan.cwd, &plan.env, None)?;

    match plan.stdout {
        StreamMode::Inherit => {}
        StreamMode::DevNull => {
            command.stdout(Stdio::null());
        }
        StreamMode::Pipe => {
            command.stdout(Stdio::piped());
        }
        StreamMode::Stdout => {
            return Err(value_error(
                "subprocess: stdout must not be subprocess.STDOUT",
            ));
        }
        StreamMode::Fd(fd) => {
            command.stdout(fd_stdio(fd)?);
        }
    }

    let mut merged_reader: Option<File> = None;
    match plan.stderr {
        StreamMode::Inherit => {}
        StreamMode::DevNull => {
            command.stderr(Stdio::null());
        }
        StreamMode::Pipe => {
            command.stderr(Stdio::piped());
        }
        StreamMode::Stdout => match plan.stdout {
            StreamMode::Pipe => {
                // Both child descriptors write into one pipe; the read end is
                // drained by a helper thread.
                let (reader, writer) = os_pipe().map_err(io_error)?;
                let second = writer.try_clone().map_err(io_error)?;
                command.stdout(Stdio::from(writer));
                command.stderr(Stdio::from(second));
                merged_reader = Some(reader);
            }
            StreamMode::DevNull => {
                command.stderr(Stdio::null());
            }
            StreamMode::Inherit | StreamMode::Stdout => {
                command.stderr(duplicate_stdout()?);
            }
            StreamMode::Fd(fd) => {
                command.stderr(fd_stdio(fd)?);
            }
        },
        StreamMode::Fd(fd) => {
            command.stderr(fd_stdio(fd)?);
        }
    }

    let stdin_mode = if plan.input.is_some() {
        // `run`/`check_output` only build a plan with `input` when stdin was
        // omitted (the caller rejects passing both).
        StreamMode::Pipe
    } else {
        plan.stdin
    };
    match stdin_mode {
        StreamMode::Inherit => {}
        StreamMode::DevNull => {
            command.stdin(Stdio::null());
        }
        StreamMode::Pipe => {
            command.stdin(Stdio::piped());
        }
        StreamMode::Stdout => {
            return Err(value_error(
                "subprocess: stdin must not be subprocess.STDOUT",
            ));
        }
        StreamMode::Fd(fd) => {
            command.stdin(fd_stdio(fd)?);
        }
    }

    let mut child = command.spawn().map_err(io_error)?;
    // `spawn` borrows the command and keeps the Stdio handles alive; dropping
    // it closes the parent's copies of pipe write ends so drain threads see
    // EOF after the child exits.
    drop(command);

    // Start the drains before touching stdin: a child that echoes its input
    // must not fill the output pipe while the parent still writes.
    let stdout_reader = child.stdout.take().and_then(spawn_drain);
    let stderr_reader = child.stderr.take().and_then(spawn_drain);
    let merged_thread = merged_reader.take().and_then(spawn_drain);

    // Feed stdin from a helper thread: a child that never reads its input but
    // keeps running must not trap the parent in `write_all` past a timeout.
    // Without `input`, taking (and immediately dropping) the pipe sends EOF.
    let stdin_writer = if stdin_mode == StreamMode::Pipe {
        child
            .stdin
            .take()
            .and_then(|pipe| match plan.input.clone() {
                Some(data) => spawn_writer(pipe, data),
                None => {
                    drop(pipe);
                    None
                }
            })
    } else {
        None
    };

    heap::blocking_enter();
    let waited = wait_child(&mut child, plan.timeout, true);
    let stdout = join_drain(stdout_reader);
    let stderr = join_drain(stderr_reader);
    let merged = join_drain(merged_thread);
    if let Some(writer) = stdin_writer {
        let _ = writer.join();
    }
    heap::blocking_leave();

    match waited {
        Ok(status) => Ok(Captured {
            status,
            stdout: merged.or(stdout),
            stderr,
        }),
        Err(WaitError::Timeout) => Err(runtime_error(&format!(
            "Command '{}' timed out after {} seconds",
            command_repr(&plan.args),
            plan.timeout
                .map(|timeout| timeout.as_secs_f64())
                .unwrap_or(0.0)
        ))),
        Err(WaitError::Io(error)) => Err(io_error(error)),
    }
}

/// Drains a pipe on a helper thread so a chatty child cannot fill the pipe
/// buffer (and block) while the parent waits for it to exit.
fn spawn_drain<R: Read + Send + 'static>(
    mut reader: R,
) -> Option<std::thread::JoinHandle<Vec<u8>>> {
    std::thread::Builder::new()
        .stack_size(32 * 1024)
        .spawn(move || {
            let mut buffer = Vec::new();
            let _ = reader.read_to_end(&mut buffer);
            buffer
        })
        .ok()
}

fn join_drain(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Option<Vec<u8>> {
    handle.map(|handle| handle.join().unwrap_or_default())
}

/// Writes `data` to the child's stdin on a helper thread; a write error such
/// as `BrokenPipe` is ignored exactly as CPython does for `input=`.
fn spawn_writer<W: Write + Send + 'static>(
    mut pipe: W,
    data: Vec<u8>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .stack_size(16 * 1024)
        .spawn(move || {
            let _ = pipe.write_all(&data);
        })
        .ok()
}

/// Waits for `child`, polling so a timeout can interrupt the wait.
///
/// With `kill_on_timeout` the child is terminated and reaped before
/// `WaitError::Timeout` is reported, leaving no zombie behind. Without it the
/// child keeps running and the caller is responsible for it (the `Popen.wait`
/// path puts it back into the registry).
fn wait_child(
    child: &mut Child,
    timeout: Option<Duration>,
    kill_on_timeout: bool,
) -> Result<ExitStatus, WaitError> {
    let Some(limit) = timeout else {
        return child.wait().map_err(WaitError::Io);
    };
    // A representable but enormous timeout must not overflow `Instant`.
    let Some(deadline) = Instant::now().checked_add(limit) else {
        return child.wait().map_err(WaitError::Io);
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => return Err(WaitError::Io(error)),
        }
        let now = Instant::now();
        if now >= deadline {
            if kill_on_timeout {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err(WaitError::Timeout);
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(2)));
    }
}

/// Converts an exit status to CPython's `returncode` (negative signal).
fn returncode(status: ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code as i64,
        None => -i64::from(status.signal().unwrap_or(0)),
    }
}

/// CPython-style `['program', 'arg']` rendering used in error messages.
fn command_repr(args: &[String]) -> String {
    let mut rendered = String::from("[");
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push('\'');
        for character in arg.chars() {
            if character == '\'' || character == '\\' {
                rendered.push('\\');
            }
            rendered.push(character);
        }
        rendered.push('\'');
    }
    rendered.push(']');
    rendered
}

fn flag(argument: i64) -> bool {
    value::truthy(argument)
}

// -------------------------------------------------------------------------
// run / check_output
// -------------------------------------------------------------------------

fn run(args: &[i64]) -> i64 {
    let args_argument = argument(args, 0);
    let argv = match command_args(args_argument) {
        Ok(argv) => argv,
        Err(error) => return error,
    };
    let stdin = match output_mode(argument(args, 1), "stdin") {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let input = match input_option(argument(args, 2)) {
        Ok(input) => input,
        Err(error) => return error,
    };
    if input.is_some() && stdin != StreamMode::Inherit {
        return value_error("subprocess: stdin and input arguments may not both be used");
    }
    let capture = flag(argument(args, 5));
    let stdout_argument = argument(args, 3);
    let stderr_argument = argument(args, 4);
    if capture && (stdout_argument != NONE || stderr_argument != NONE) {
        return value_error(
            "subprocess: stdout and stderr arguments may not be used with capture_output",
        );
    }
    let stdout = if capture {
        StreamMode::Pipe
    } else {
        match output_mode(stdout_argument, "stdout") {
            Ok(mode) => mode,
            Err(error) => return error,
        }
    };
    let stderr = if capture {
        StreamMode::Pipe
    } else {
        match stream_mode(stderr_argument, "stderr") {
            Ok(mode) => mode,
            Err(error) => return error,
        }
    };
    let text = flag(argument(args, 6));
    let cwd = match path_option(argument(args, 7), "cwd") {
        Ok(cwd) => cwd,
        Err(error) => return error,
    };
    let timeout = match duration_option(argument(args, 8), "timeout") {
        Ok(timeout) => timeout,
        Err(error) => return error,
    };
    let check = flag(argument(args, 9));
    let env = match env_option(argument(args, 10)) {
        Ok(env) => env,
        Err(error) => return error,
    };

    let plan = Plan {
        args: argv,
        stdin,
        input,
        stdout,
        stderr,
        cwd,
        timeout,
        env,
    };
    let captured = match execute(&plan) {
        Ok(captured) => captured,
        Err(error) => return error,
    };
    let code = returncode(captured.status);
    if check && code != 0 {
        return runtime_error(&format!(
            "Command '{}' returned non-zero exit status {}.",
            command_repr(&plan.args),
            code
        ));
    }

    let stdout_value = match captured.stdout {
        Some(bytes) => output_value(bytes, text),
        None => NONE,
    };
    let stderr_value = match captured.stderr {
        Some(bytes) => output_value(bytes, text),
        None => NONE,
    };
    value::namespace_new(vec![
        ("args".to_string(), args_value(args_argument, &plan.args)),
        ("returncode".to_string(), int_value(code)),
        ("stdout".to_string(), stdout_value),
        ("stderr".to_string(), stderr_value),
    ])
}

fn check_output(args: &[i64]) -> i64 {
    let args_argument = argument(args, 0);
    let argv = match command_args(args_argument) {
        Ok(argv) => argv,
        Err(error) => return error,
    };
    let stdin = match output_mode(argument(args, 1), "stdin") {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let input = match input_option(argument(args, 2)) {
        Ok(input) => input,
        Err(error) => return error,
    };
    if input.is_some() && stdin != StreamMode::Inherit {
        return value_error("subprocess: stdin and input arguments may not both be used");
    }
    let stderr = match stream_mode(argument(args, 3), "stderr") {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let cwd = match path_option(argument(args, 4), "cwd") {
        Ok(cwd) => cwd,
        Err(error) => return error,
    };
    let timeout = match duration_option(argument(args, 5), "timeout") {
        Ok(timeout) => timeout,
        Err(error) => return error,
    };
    let encoding = argument(args, 8);
    let errors = argument(args, 9);
    let text = flag(argument(args, 6)) || encoding != NONE || errors != NONE;
    let env = match env_option(argument(args, 7)) {
        Ok(env) => env,
        Err(error) => return error,
    };

    let plan = Plan {
        args: argv,
        stdin,
        input,
        stdout: StreamMode::Pipe,
        stderr,
        cwd,
        timeout,
        env,
    };
    let captured = match execute(&plan) {
        Ok(captured) => captured,
        Err(error) => return error,
    };
    let code = returncode(captured.status);
    if code != 0 {
        return runtime_error(&format!(
            "Command '{}' returned non-zero exit status {}.",
            command_repr(&plan.args),
            code
        ));
    }
    output_value(captured.stdout.unwrap_or_default(), text)
}

// -------------------------------------------------------------------------
// call
// -------------------------------------------------------------------------

/// `subprocess.call(args, *, stdin=None, stdout=None, stderr=None,
/// shell=False, timeout=None, cwd=None, env=None, text=False)`: run the
/// command and return its exit status (negative when a signal terminated it).
///
/// This is `Popen(...).wait()` with no object kept: the child runs to
/// completion, its streams are drained (and discarded) as `run` does, and the
/// plan's timeout kills the child and reports the runtime's timeout error.
/// `shell=True` prepends `/bin/sh -c`, exactly the POSIX argument vector
/// CPython builds.
fn call_command(args: &[i64]) -> i64 {
    let args_argument = argument(args, 0);
    let mut argv = match command_args(args_argument) {
        Ok(argv) => argv,
        Err(error) => return error,
    };
    if flag(argument(args, 4)) {
        let mut command = vec!["/bin/sh".to_string(), "-c".to_string()];
        command.append(&mut argv);
        argv = command;
    }
    let stdin = match output_mode(argument(args, 1), "stdin") {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let stdout = match output_mode(argument(args, 2), "stdout") {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let stderr = match stream_mode(argument(args, 3), "stderr") {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let timeout = match duration_option(argument(args, 5), "timeout") {
        Ok(timeout) => timeout,
        Err(error) => return error,
    };
    let cwd = match path_option(argument(args, 6), "cwd") {
        Ok(cwd) => cwd,
        Err(error) => return error,
    };
    let env = match env_option(argument(args, 7)) {
        Ok(env) => env,
        Err(error) => return error,
    };
    let plan = Plan {
        args: argv,
        stdin,
        input: None,
        stdout,
        stderr,
        cwd,
        timeout,
        env,
    };
    let captured = match execute(&plan) {
        Ok(captured) => captured,
        Err(error) => return error,
    };
    int_value(returncode(captured.status))
}

/// `subprocess.check_call(args, *, stdin=None, stdout=None, stderr=None,
/// shell=False, timeout=None, cwd=None, env=None, text=False)`: run the
/// command through [`call_command`] and return `0`, or raise
/// `CalledProcessError(returncode, cmd, output=None, stderr=None)` for a
/// non-zero exit status.
fn check_call(args: &[i64]) -> i64 {
    // Resolve the command text before running: the exception payload needs it,
    // and a malformed `args` must fail exactly like `call` does.
    let args_argument = argument(args, 0);
    let mut command = match command_args(args_argument) {
        Ok(argv) => argv,
        Err(error) => return error,
    };
    if flag(argument(args, 4)) {
        let mut shell_command = vec!["/bin/sh".to_string(), "-c".to_string()];
        shell_command.append(&mut command);
        command = shell_command;
    }
    let status = call_command(args);
    if exc::pending() {
        return status;
    }
    let Some(code) = value::as_integer(status) else {
        return status;
    };
    if code == 0 {
        return int_value(0);
    }
    raise_called_process_error(code, command)
}

/// `subprocess.CalledProcessError`: the exception class `check_call` raises.
///
/// A real `Exception` subclass so `except subprocess.CalledProcessError` and
/// `isinstance` see it; the module attribute is wired by the central spec.
pub(crate) fn called_process_error_class() -> i64 {
    const KEY: &str = "subprocess.CalledProcessError.class";
    if let Some(class) = crate::core::globals::registry_get(KEY) {
        return class;
    }
    let class = value::class_new(
        "CalledProcessError".to_string(),
        exc::exception_class(EXCEPTION),
        Vec::new(),
    );
    let roots = heap::root_values(std::slice::from_ref(&class));
    crate::core::globals::registry_set(KEY, class);
    heap::release_roots(roots);
    class
}

/// Builds and raises `CalledProcessError(code, command)`.
///
/// The `output` and `stderr` attributes are `None` because `check_call` does
/// not capture streams (matching CPython's `call`-based implementation).
fn raise_called_process_error(code: i64, command: Vec<String>) -> i64 {
    let message = format!(
        "Command '{}' returned non-zero exit status {}.",
        command_repr(&command),
        code
    );
    let plain = exc::exception_value(EXCEPTION, &message, vec![message.clone()]);
    let Some(data) = value::exception_clone(plain) else {
        return NONE;
    };
    let class = called_process_error_class();
    let instance = value::exc_instance_new(class, data);
    let returncode = int_value(code);
    let cmd = value::list_new(command.into_iter().map(value::str_alloc).collect());
    let roots = heap::root_values(&[instance, returncode, cmd]);
    value::instance_setattr(instance, "returncode".to_string(), returncode);
    value::instance_setattr(instance, "cmd".to_string(), cmd);
    value::instance_setattr(instance, "output".to_string(), NONE);
    value::instance_setattr(instance, "stderr".to_string(), NONE);
    let result = exc::raise_value(instance);
    heap::release_roots(roots);
    result
}

// -------------------------------------------------------------------------
// Popen
// -------------------------------------------------------------------------

/// A positional parameter slot for a native class method.
fn method_param(name: &str, default: Option<i64>) -> value::ParamMeta {
    value::ParamMeta {
        name: name.to_string(),
        kind: hydro_ir::ParamKind::Positional,
        default,
    }
}

/// Builds a native class method that binds arguments through the shared
/// method binder (`function_signature` + `function_full_entry`).
///
/// Methods with parameter metadata are reached through
/// `core::methods::call_bound_entry`, which fills the receiver slot and
/// applies defaults before calling `entry`; the central module spec (and its
/// `min_args..=max_args` validation) is not consulted. This is the same
/// mechanism `asyncio.Timeout` uses.
fn bounded_method(
    native: &'static str,
    entry: extern "C" fn(*const i64) -> i64,
    signature: Vec<value::ParamMeta>,
) -> i64 {
    let function = value::native_function_new("subprocess", native, usize::MAX);
    value::function_set_signature(function, signature);
    value::function_set_full_entry(function, entry as *const () as usize);
    function
}

/// `Popen.communicate(input=None, timeout=None)`.
extern "C" fn communicate_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `[self, input, timeout]`.
    let (instance, input, timeout) = unsafe { (*argv, *argv.add(1), *argv.add(2)) };
    popen_communicate(instance, input, timeout)
}

/// `Popen.wait(timeout=None)`.
extern "C" fn wait_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `[self, timeout]`.
    let (instance, timeout_argument) = unsafe { (*argv, *argv.add(1)) };
    let timeout = match duration_option(timeout_argument, "timeout") {
        Ok(timeout) => timeout,
        Err(error) => return error,
    };
    popen_wait(instance, timeout)
}

/// `Popen.send_signal(sig)`.
extern "C" fn send_signal_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `[self, sig]`.
    let (instance, signal) = unsafe { (*argv, *argv.add(1)) };
    popen_send_signal(instance, signal)
}

/// Builds (once) the native `Popen` class.
///
/// The class is cached: `type(p)` compares by identity across instances, like
/// CPython's class object. The spec-validated `_popen_*` entries keep their
/// module-dispatched implementations; `communicate` and `send_signal` carry
/// their own parameter metadata and are bound by the generic method binder.
fn popen_class() -> i64 {
    if let Some(class) = globals::registry_get(POPEN_CLASS_KEY) {
        return class;
    }
    let methods: Vec<(String, i64, MethodKind)> = vec![
        (
            "wait".to_string(),
            bounded_method(
                "_popen_wait",
                wait_entry,
                vec![
                    method_param("self", None),
                    method_param("timeout", Some(NONE)),
                ],
            ),
            MethodKind::Normal,
        ),
        (
            "poll".to_string(),
            value::native_function_new("subprocess", "_popen_poll", 1),
            MethodKind::Normal,
        ),
        (
            "terminate".to_string(),
            value::native_function_new("subprocess", "_popen_terminate", 1),
            MethodKind::Normal,
        ),
        (
            "kill".to_string(),
            value::native_function_new("subprocess", "_popen_kill", 1),
            MethodKind::Normal,
        ),
        (
            "__enter__".to_string(),
            value::native_function_new("subprocess", "_popen_enter", 1),
            MethodKind::Normal,
        ),
        (
            "__exit__".to_string(),
            value::native_function_new("subprocess", "_popen_exit", 4),
            MethodKind::Normal,
        ),
        (
            "communicate".to_string(),
            bounded_method(
                "_popen_communicate",
                communicate_entry,
                vec![
                    method_param("self", None),
                    method_param("input", Some(NONE)),
                    method_param("timeout", Some(NONE)),
                ],
            ),
            MethodKind::Normal,
        ),
        (
            "send_signal".to_string(),
            bounded_method(
                "_popen_send_signal",
                send_signal_entry,
                vec![method_param("self", None), method_param("sig", None)],
            ),
            MethodKind::Normal,
        ),
    ];
    let values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let roots = heap::root_values(&values);
    let class = value::class_new_full("Popen".to_string(), NONE, methods, Vec::new());
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(POPEN_CLASS_KEY, class);
    heap::release_roots(class_roots);
    heap::release_roots(roots);
    class
}

fn popen_new(args: &[i64]) -> i64 {
    let args_argument = argument(args, 0);
    let original_argv = match command_args(args_argument) {
        Ok(argv) => argv,
        Err(error) => return error,
    };
    let mut argv = original_argv.clone();
    // `shell=True` runs the command through `/bin/sh -c`, matching CPython's
    // POSIX argument vector. A list is joined in the same simple way the
    // module-level `call` helper does.
    if value::truthy(argument(args, 13)) {
        let command = argv.join(" ");
        argv = vec!["/bin/sh".to_string(), "-c".to_string(), command];
    }
    let cwd = match path_option(argument(args, 1), "cwd") {
        Ok(cwd) => cwd,
        Err(error) => return error,
    };
    let executable = match path_option(argument(args, 14), "executable") {
        Ok(executable) => executable,
        Err(error) => return error,
    };
    let stdout = match output_mode(argument(args, 2), "stdout") {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let stderr = match stream_mode(argument(args, 3), "stderr") {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let env = match env_option(argument(args, 5)) {
        Ok(env) => env,
        Err(error) => return error,
    };
    // Slot 8 is `stdin`; `output_mode` rejects `STDOUT`, which CPython also
    // refuses for the input side.
    let stdin = match output_mode(argument(args, 8), "stdin") {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    // Slots 9 and 10 are `encoding` and `errors`. CPython stores them
    // verbatim (after validating the type through the io machinery) and
    // derives `text_mode` from their truthiness.
    let encoding_argument = argument(args, 9);
    let errors_argument = argument(args, 10);
    if encoding_argument != NONE && value::as_str(encoding_argument).is_none() {
        return type_error(&format!(
            "subprocess: encoding must be str or None, not '{}'",
            value::type_name(encoding_argument)
        ));
    }
    if errors_argument != NONE && value::as_str(errors_argument).is_none() {
        return type_error(&format!(
            "subprocess: errors must be str or None, not '{}'",
            value::type_name(errors_argument)
        ));
    }
    // `text` is slot 4; CPython's `text_mode` is the truthiness of `encoding`,
    // `errors`, `text` or `universal_newlines` (`encoding=""` does not enable
    // it). `universal_newlines` is CPython's backwards-compatible alias; when
    // the central spec declares it, it arrives in slot 15 (after
    // `executable`), and a value that contradicts `text` is rejected exactly
    // like `SubprocessError('Cannot disambiguate ...')`.
    let universal_newlines = argument(args, 15);
    if universal_newlines != NONE
        && argument(args, 4) != NONE
        && value::truthy(universal_newlines) != value::truthy(argument(args, 4))
    {
        return exc::raise_with_message(
            exc::EXCEPTION,
            "Cannot disambiguate when both text and universal_newlines are supplied \
             but differ. Pass one or the other.",
        );
    }
    let text_mode = value::truthy(encoding_argument)
        || value::truthy(errors_argument)
        || value::truthy(argument(args, 4))
        || value::truthy(universal_newlines);
    // Slot 11 is `preexec_fn`. CPython calls it between `fork` and `exec`,
    // where no Python-level code (and therefore no Hydro object) may run:
    // rejecting the hook is the only honest option.
    if argument(args, 11) != NONE {
        return exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "subprocess: preexec_fn is not supported",
        );
    }
    // `close_fds=True` is CPython's default. Hydro's child inherits the
    // descriptor table as `std::process::Command` leaves it (standard streams
    // replaced, unrelated descriptors neither closed nor forced inheritable),
    // so no descriptor-set effect can be claimed here; the value is validated
    // and recorded on the instance instead of being dropped. It stays
    // observable as `popen.close_fds`.
    let close_fds_argument = argument(args, 6);
    let close_fds = if close_fds_argument == NONE {
        true
    } else {
        value::truthy(close_fds_argument)
    };
    // Slot 7 is `start_new_session`. CPython converts it with
    // `PyObject_IsTrue`, so any truthy value starts the new session; it is
    // never dropped, and an omitted slot (the common case) stays false.
    let start_new_session = match argument(args, 7) {
        v if v == NONE => false,
        v => value::truthy(v),
    };

    let mut command = match build_command(&argv, &cwd, &env, executable.as_deref()) {
        Ok(command) => command,
        Err(error) => return error,
    };
    // Bottom of the CPython `Popen(..., stdin=...)` matrix: inherit the
    // parent's descriptor, open `/dev/null`, or create a pipe whose parent
    // side becomes `popen.stdin`.
    match stdin {
        StreamMode::Inherit => {}
        StreamMode::DevNull => {
            command.stdin(Stdio::null());
        }
        StreamMode::Pipe => {
            command.stdin(Stdio::piped());
        }
        StreamMode::Stdout => {
            return value_error("subprocess: stdin must not be subprocess.STDOUT");
        }
        StreamMode::Fd(fd) => match fd_stdio(fd) {
            Ok(stdio) => {
                command.stdin(stdio);
            }
            Err(error) => return error,
        },
    }
    // `stderr=STDOUT` with `stdout=PIPE` shares one pipe between the two
    // child descriptors; CPython leaves `popen.stderr` as `None` in that
    // configuration and `popen.stdout` reports the interleaved bytes.
    let mut merged_stdout_parent: Option<OwnedFd> = None;
    if stdout == StreamMode::Pipe && stderr == StreamMode::Stdout {
        let (reader, writer) = match os_pipe() {
            Ok(pair) => pair,
            Err(error) => return io_error(error),
        };
        let second = match writer.try_clone() {
            Ok(clone) => clone,
            Err(error) => return io_error(error),
        };
        command.stdout(Stdio::from(writer));
        command.stderr(Stdio::from(second));
        merged_stdout_parent = Some(OwnedFd::from(reader));
    } else {
        match stdout {
            StreamMode::Inherit => {}
            StreamMode::DevNull => {
                command.stdout(Stdio::null());
            }
            StreamMode::Pipe => {
                command.stdout(Stdio::piped());
            }
            StreamMode::Stdout => {
                return value_error("subprocess: stdout must not be subprocess.STDOUT");
            }
            StreamMode::Fd(fd) => match fd_stdio(fd) {
                Ok(stdio) => {
                    command.stdout(stdio);
                }
                Err(error) => return error,
            },
        }
        match stderr {
            StreamMode::DevNull => {
                command.stderr(Stdio::null());
            }
            StreamMode::Pipe => {
                command.stderr(Stdio::piped());
            }
            StreamMode::Stdout => match stdout {
                StreamMode::Inherit => match duplicate_stdout() {
                    Ok(stdio) => {
                        command.stderr(stdio);
                    }
                    Err(error) => return error,
                },
                StreamMode::DevNull => {
                    command.stderr(Stdio::null());
                }
                // Handled by the shared-pipe branch above.
                StreamMode::Pipe => {
                    command.stderr(Stdio::null());
                }
                StreamMode::Stdout => {}
                StreamMode::Fd(fd) => match fd_stdio(fd) {
                    Ok(stdio) => {
                        command.stderr(stdio);
                    }
                    Err(error) => return error,
                },
            },
            StreamMode::Inherit => {}
            StreamMode::Fd(fd) => match fd_stdio(fd) {
                Ok(stdio) => {
                    command.stderr(stdio);
                }
                Err(error) => return error,
            },
        }
    }

    // `start_new_session=True` puts the child in a fresh session and process
    // group (no controlling terminal), which is what `setsid(2)` does between
    // fork and exec. On targets without `setsid` the flag has no effect, as in
    // CPython's Windows implementation.
    #[cfg(unix)]
    if start_new_session {
        // SAFETY: `setsid` is async-signal-safe and allocates nothing, which
        // is what `pre_exec` requires of the child-side closure.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = start_new_session;

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return io_error(error),
    };
    // Each piped stream stays attached to a runtime file object so callers can
    // read/write it and `select.poll.register(proc.stdout, ...)` can use its
    // descriptor. Text mode switches the objects to text (`str`) I/O; the
    // bytes are UTF-8.
    let stdin_handle = match child.stdin.take() {
        Some(pipe) => pipe_writer_value(pipe, !text_mode),
        None => NONE,
    };
    let stdout_handle = match merged_stdout_parent.take() {
        Some(reader) => pipe_reader_value(reader, !text_mode),
        None => match child.stdout.take() {
            Some(reader) => pipe_reader_value(OwnedFd::from(reader), !text_mode),
            None => NONE,
        },
    };
    let stderr_handle = match child.stderr.take() {
        Some(reader) => pipe_reader_value(OwnedFd::from(reader), !text_mode),
        None => NONE,
    };
    let pid = i64::from(child.id());
    let key = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    children().insert(key, child);

    // The handles are held only in Rust locals until they become instance
    // attributes; keep them (and then the instance) visible to the collector
    // while the attribute assignments allocate.
    let handle_roots = heap::root_values(&[stdin_handle, stdout_handle, stderr_handle]);
    let instance = value::instance_new(popen_class());
    let instance_roots = heap::root_values(std::slice::from_ref(&instance));
    value::instance_setattr(
        instance,
        "args".to_string(),
        args_value(args_argument, &original_argv),
    );
    value::instance_setattr(instance, "returncode".to_string(), NONE);
    value::instance_setattr(instance, "pid".to_string(), int_value(pid));
    value::instance_setattr(
        instance,
        "close_fds".to_string(),
        value::bool_value(close_fds),
    );
    // CPython's `text_mode` is the truthiness of `encoding`/`errors`/`text`;
    // when text mode is on without an explicit codec it records
    // `_text_encoding()`, which Hydro's UTF-8 text path mirrors.
    value::instance_setattr(
        instance,
        "text_mode".to_string(),
        value::bool_value(text_mode),
    );
    // `universal_newlines` is CPython's read-only alias property for
    // `text_mode`; instances expose the same value as a plain attribute.
    value::instance_setattr(
        instance,
        "universal_newlines".to_string(),
        value::bool_value(text_mode),
    );
    let encoding_value = if encoding_argument != NONE {
        encoding_argument
    } else if text_mode {
        value::str_alloc("utf-8".to_string())
    } else {
        NONE
    };
    value::instance_setattr(instance, "encoding".to_string(), encoding_value);
    value::instance_setattr(instance, "errors".to_string(), errors_argument);
    // CPython initializes `stdin`/`stdout`/`stderr` to `None` and fills a
    // stream attribute only when it created a file object.
    value::instance_setattr(instance, "stdin".to_string(), stdin_handle);
    value::instance_setattr(instance, "stdout".to_string(), stdout_handle);
    value::instance_setattr(instance, "stderr".to_string(), stderr_handle);
    value::instance_setattr(instance, ID_ATTR.to_string(), int_value(key as i64));
    heap::release_roots(instance_roots);
    heap::release_roots(handle_roots);
    instance
}

/// Extracts the `CHILDREN` key from a `Popen` instance.
fn process_key(instance: i64) -> Result<u64, i64> {
    if value::as_instance(instance).is_none() {
        return Err(type_error(
            "subprocess: this method requires a subprocess.Popen instance",
        ));
    }
    match value::instance_attr(instance, ID_ATTR).and_then(value::as_integer) {
        Some(key) if key > 0 => Ok(key as u64),
        _ => Err(runtime_error(
            "subprocess: Popen object has no associated process",
        )),
    }
}

/// The return code recorded on an already-reaped instance, if any.
fn recorded_returncode(instance: i64) -> Option<i64> {
    value::instance_attr(instance, "returncode").filter(|&code| code != NONE)
}

/// The cached `subprocess.TimeoutExpired` exception class.
///
/// `TimeoutExpired` subclasses `Exception` (through the runtime's
/// `SubprocessError`-shaped hierarchy) and carries CPython's `cmd`,
/// `timeout`, `output` and `stderr` attributes.
pub(crate) fn timeout_expired_class() -> i64 {
    if let Some(class) = globals::registry_get(TIMEOUT_EXPIRED_KEY) {
        return class;
    }
    let class = value::class_new(
        "TimeoutExpired".to_string(),
        exc::exception_class(EXCEPTION),
        Vec::new(),
    );
    let roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(TIMEOUT_EXPIRED_KEY, class);
    heap::release_roots(roots);
    class
}

/// Raises a `TimeoutExpired` instance for `instance`.
///
/// `output`/`stderr` carry whatever was read before the deadline (raw bytes,
/// matching CPython even in text mode).
fn raise_timeout_expired(
    instance: i64,
    timeout: Option<Duration>,
    output: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
) -> i64 {
    let exception = value::instance_new(timeout_expired_class());
    let cmd = value::instance_attr(instance, "args").unwrap_or(NONE);
    let timeout_value = match timeout {
        Some(timeout) => value::float_new(timeout.as_secs_f64()),
        None => NONE,
    };
    let output_value = output.map(value::bytes_new).unwrap_or(NONE);
    let stderr_value = stderr.map(value::bytes_new).unwrap_or(NONE);
    let roots = heap::root_values(&[exception, cmd, timeout_value, output_value, stderr_value]);
    value::instance_setattr(exception, "cmd".to_string(), cmd);
    value::instance_setattr(exception, "timeout".to_string(), timeout_value);
    value::instance_setattr(exception, "output".to_string(), output_value);
    value::instance_setattr(exception, "stderr".to_string(), stderr_value);
    let result = exc::raise_value(exception);
    heap::release_roots(roots);
    result
}

/// Waits for the child owned by `instance`, cooperating with concurrent
/// `wait`/`poll`/`communicate` calls.
///
/// The `CHILDREN` entry is claimed by exactly one thread while it blocks in
/// `waitpid`; every other caller observes the recorded `returncode` once it
/// is set. That is CPython's `_waitpid_lock`/`returncode` protocol and it is
/// what makes `Popen.wait()` thread-safe (`test_threadsafe_wait`).
fn wait_instance_child(
    instance: i64,
    key: u64,
    timeout: Option<Duration>,
) -> Result<(), WaitError> {
    let deadline = timeout.and_then(|limit| Instant::now().checked_add(limit));
    loop {
        if recorded_returncode(instance).is_some() {
            return Ok(());
        }
        match children().remove(&key) {
            Some(mut child) => {
                let remaining = match deadline {
                    Some(deadline) => {
                        let now = Instant::now();
                        if now >= deadline {
                            children().insert(key, child);
                            return Err(WaitError::Timeout);
                        }
                        Some(deadline - now)
                    }
                    None => None,
                };
                heap::blocking_enter();
                let waited = wait_child(&mut child, remaining, false);
                heap::blocking_leave();
                match waited {
                    Ok(status) => {
                        let code = returncode(status);
                        value::instance_setattr(
                            instance,
                            "returncode".to_string(),
                            int_value(code),
                        );
                        return Ok(());
                    }
                    Err(WaitError::Timeout) => {
                        children().insert(key, child);
                        return Err(WaitError::Timeout);
                    }
                    Err(WaitError::Io(error)) => {
                        children().insert(key, child);
                        return Err(WaitError::Io(error));
                    }
                }
            }
            None => {
                // Another `wait`/`communicate` call claimed the child; wait
                // for its recorded status (or this caller's own deadline).
                match deadline {
                    Some(deadline) if Instant::now() >= deadline => return Err(WaitError::Timeout),
                    _ => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        }
    }
}

fn popen_wait(instance: i64, timeout: Option<Duration>) -> i64 {
    let key = match process_key(instance) {
        Ok(key) => key,
        Err(error) => return error,
    };
    match wait_instance_child(instance, key, timeout) {
        Ok(()) => recorded_returncode(instance).unwrap_or(NONE),
        Err(WaitError::Timeout) => raise_timeout_expired(instance, timeout, None, None),
        Err(WaitError::Io(error)) => io_error(error),
    }
}

enum Polled {
    Exited(i64),
    Running,
    Missing,
    Failed(io::Error),
}

fn popen_poll(instance: i64) -> i64 {
    let key = match process_key(instance) {
        Ok(key) => key,
        Err(error) => return error,
    };
    let mut map = children();
    let polled = match map.get_mut(&key) {
        Some(child) => match child.try_wait() {
            Ok(Some(status)) => Polled::Exited(returncode(status)),
            Ok(None) => Polled::Running,
            Err(error) => Polled::Failed(error),
        },
        None => Polled::Missing,
    };
    // Reap the entry exactly once: a second `poll()` after this point sees no
    // child and answers from the recorded `returncode`.
    if matches!(polled, Polled::Exited(_)) {
        map.remove(&key);
    }
    drop(map);

    match polled {
        Polled::Exited(code) => {
            value::instance_setattr(instance, "returncode".to_string(), int_value(code));
            int_value(code)
        }
        Polled::Running => NONE,
        // A concurrent waiter removed the entry; until it records the status
        // the process is still running, so `poll()` answers `None`.
        Polled::Missing => recorded_returncode(instance).unwrap_or(NONE),
        Polled::Failed(error) => io_error(error),
    }
}

/// `Popen.send_signal(sig)`.
///
/// The child is polled first so an exited process is never signalled (and a
/// recycled pid cannot receive the signal), and a vanished process
/// (`ESRCH`) is ignored, exactly like CPython.
fn popen_send_signal(instance: i64, signal_argument: i64) -> i64 {
    let Some(number) = value::as_integer(signal_argument) else {
        return type_error(&format!(
            "an integer is required (got type {})",
            value::type_name(signal_argument)
        ));
    };
    let signal = match i32::try_from(number) {
        Ok(signal) => signal,
        Err(_) => return exc::raise_with_message(OVERFLOW_ERROR, "signal number out of range"),
    };
    if let Err(error) = process_key(instance) {
        return error;
    }
    let polled = popen_poll(instance);
    if exc::pending() {
        return NONE;
    }
    if polled != NONE {
        return NONE;
    }
    // The pid is read from the instance, not the registry: a concurrent
    // `wait`/`communicate` may have claimed the `Child` while it blocks in
    // `waitpid`, and the signal must still reach the running process.
    let Some(pid) = value::instance_attr(instance, "pid")
        .and_then(value::as_integer)
        .and_then(|pid| i32::try_from(pid).ok())
    else {
        return NONE;
    };
    // SAFETY: `kill` only reads the pid and signal; the child is still owned
    // by the registry (and therefore not reaped) until `wait`/`poll`.
    if unsafe { libc::kill(pid, signal) } == 0 {
        return NONE;
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        NONE
    } else {
        io_error(error)
    }
}

/// A `communicate()` input payload after validation.
enum CommunicateInput {
    Text(String),
    Bytes(Vec<u8>),
}

/// Validates `communicate(input=...)` against the stream's text/binary mode
/// and converts bytes-like values to a contiguous buffer.
fn communicate_input(argument: i64, text_mode: bool) -> Result<Option<CommunicateInput>, i64> {
    if argument == NONE {
        return Ok(None);
    }
    if text_mode {
        return match value::as_str(argument) {
            Some(text) => Ok(Some(CommunicateInput::Text(text.to_string()))),
            None => Err(type_error(&format!(
                "write() argument must be str, not '{}'",
                value::type_name(argument)
            ))),
        };
    }
    if let Some(bytes) = value::as_bytes(argument) {
        return Ok(Some(CommunicateInput::Bytes(bytes.to_vec())));
    }
    if let Some(bytes) = value::as_bytearray(argument) {
        return Ok(Some(CommunicateInput::Bytes(bytes.clone())));
    }
    if let Some(view) = value::as_memoryview(argument) {
        return match value::memoryview_bytes(view) {
            Ok(bytes) => Ok(Some(CommunicateInput::Bytes(bytes))),
            Err(message) => Err(value_error(message)),
        };
    }
    Err(type_error(&format!(
        "a bytes-like object is required, not '{}'",
        value::type_name(argument)
    )))
}

/// Drains a pipe file on a helper thread into a shared byte buffer.
fn spawn_pipe_reader(
    file: &'static value::FileState,
    buffer: Arc<Mutex<Vec<u8>>>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .stack_size(32 * 1024)
        .spawn(move || {
            let Ok(mut data) = file.read_bytes(None) else {
                return;
            };
            if let Ok(mut output) = buffer.lock() {
                output.append(&mut data);
            }
        })
}

/// Writes `payload` to the child's stdin, flushes and closes it.
///
/// Writing on a helper thread keeps a child that fills its output pipe before
/// reading its input from deadlocking the parent, and closing from inside the
/// same thread gives the child EOF as soon as the last byte was written
/// instead of after the child exits. Write errors (notably `EPIPE`) are
/// ignored, exactly as CPython's `_stdin_write` does.
fn spawn_pipe_writer(
    file: &'static value::FileState,
    payload: Option<CommunicateInput>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .stack_size(16 * 1024)
        .spawn(move || {
            if let Some(payload) = payload {
                match payload {
                    CommunicateInput::Text(text) => {
                        let _ = file.write(&text);
                    }
                    CommunicateInput::Bytes(bytes) => {
                        let _ = file.write_bytes(&bytes);
                    }
                }
            }
            let _ = file.flush();
            file.close();
        })
}

/// Starts the reader/writer helpers for one `communicate()` call.
fn start_communication(
    instance: i64,
    input: Option<CommunicateInput>,
) -> Result<Communication, i64> {
    let mut communication = Communication {
        stdout: Arc::new(Mutex::new(Vec::new())),
        stderr: Arc::new(Mutex::new(Vec::new())),
        stdout_reader: None,
        stderr_reader: None,
        stdin_writer: None,
    };
    for (name, buffer) in [
        ("stdout", Arc::clone(&communication.stdout)),
        ("stderr", Arc::clone(&communication.stderr)),
    ] {
        let Some(handle) = stream_handle(instance, name) else {
            continue;
        };
        let Some(file) = value::as_file(handle) else {
            return Err(runtime_error(&format!(
                "subprocess: Popen.{name} is not a pipe handle"
            )));
        };
        if file.closed() {
            return Err(value_error("I/O operation on closed file."));
        }
        let reader = spawn_pipe_reader(file, buffer).map_err(io_error)?;
        if name == "stdout" {
            communication.stdout_reader = Some(reader);
        } else {
            communication.stderr_reader = Some(reader);
        }
    }
    if let Some(handle) = stream_handle(instance, "stdin") {
        let Some(file) = value::as_file(handle) else {
            return Err(runtime_error(
                "subprocess: Popen.stdin is not a pipe handle",
            ));
        };
        if file.closed() {
            if input.is_some() {
                return Err(value_error("I/O operation on closed file."));
            }
        } else {
            let writer = spawn_pipe_writer(file, input).map_err(io_error)?;
            communication.stdin_writer = Some(writer);
        }
    }
    Ok(communication)
}

/// Waits for the child while the helper threads drain the pipes.
fn wait_communication_child(
    instance: i64,
    key: u64,
    timeout: Option<Duration>,
) -> Result<(), WaitError> {
    wait_instance_child(instance, key, timeout)
}

/// Joins the helper threads; every handle returns once the child's pipe ends
/// are closed (the child exited) or the writer hit `EPIPE`.
fn join_communication(communication: &mut Communication) {
    heap::blocking_enter();
    if let Some(reader) = communication.stdout_reader.take() {
        let _ = reader.join();
    }
    if let Some(reader) = communication.stderr_reader.take() {
        let _ = reader.join();
    }
    if let Some(writer) = communication.stdin_writer.take() {
        let _ = writer.join();
    }
    heap::blocking_leave();
}

fn buffer_contents(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    buffer.lock().map(|bytes| bytes.clone()).unwrap_or_default()
}

/// Renders captured pipe bytes as `bytes` or, in text mode, as a `str` with
/// CPython's universal-newline translation.
fn communicate_output(bytes: Vec<u8>, text_mode: bool) -> i64 {
    if !text_mode {
        return value::bytes_new(bytes);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if text.contains('\r') {
        value::str_alloc(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        value::str_alloc(text)
    }
}

/// `Popen.communicate(input=None, timeout=None)`.
fn popen_communicate(instance: i64, input_argument: i64, timeout_argument: i64) -> i64 {
    let key = match process_key(instance) {
        Ok(key) => key,
        Err(error) => return error,
    };
    let timeout = match duration_option(timeout_argument, "timeout") {
        Ok(timeout) => timeout,
        Err(error) => return error,
    };
    if input_argument != NONE
        && value::instance_attr(instance, COMMUNICATION_STARTED_ATTR).is_some_and(value::truthy)
    {
        return value_error("Cannot send input after starting communication");
    }
    let text_mode = value::instance_attr(instance, "text_mode").is_some_and(value::truthy);
    let mut communication = match communications().remove(&key) {
        Some(communication) => communication,
        None => {
            let input = match communicate_input(input_argument, text_mode) {
                Ok(input) => input,
                Err(error) => return error,
            };
            match start_communication(instance, input) {
                Ok(communication) => communication,
                Err(error) => return error,
            }
        }
    };
    value::instance_setattr(
        instance,
        COMMUNICATION_STARTED_ATTR.to_string(),
        value::TRUE,
    );
    match wait_communication_child(instance, key, timeout) {
        Ok(()) => {}
        Err(WaitError::Timeout) => {
            // Keep the in-flight buffers so the next call resumes and returns
            // the complete output, like CPython after TimeoutExpired.
            let output = Some(buffer_contents(&communication.stdout));
            let stderr_output = Some(buffer_contents(&communication.stderr));
            communications().insert(key, communication);
            return raise_timeout_expired(instance, timeout, output, stderr_output);
        }
        Err(WaitError::Io(error)) => {
            communications().insert(key, communication);
            return io_error(error);
        }
    }
    join_communication(&mut communication);
    // CPython closes every stream at the end of communicate().
    close_stream_handle(instance, "stdin");
    close_stream_handle(instance, "stdout");
    close_stream_handle(instance, "stderr");
    let stdout = match stream_handle(instance, "stdout") {
        Some(_) => communicate_output(buffer_contents(&communication.stdout), text_mode),
        None => NONE,
    };
    let stderr = match stream_handle(instance, "stderr") {
        Some(_) => communicate_output(buffer_contents(&communication.stderr), text_mode),
        None => NONE,
    };
    let roots = heap::root_values(&[stdout, stderr]);
    let result = value::tuple_new(vec![stdout, stderr]);
    heap::release_roots(roots);
    result
}

fn popen_method(function: &str, args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("subprocess: instance method called without an instance");
    };
    match function {
        "_popen_wait" => {
            let timeout = match duration_option(argument(args, 1), "timeout") {
                Ok(timeout) => timeout,
                Err(error) => return error,
            };
            popen_wait(instance, timeout)
        }
        "_popen_poll" => popen_poll(instance),
        "_popen_terminate" => popen_send_signal(instance, int_value(i64::from(libc::SIGTERM))),
        "_popen_kill" => popen_send_signal(instance, int_value(i64::from(libc::SIGKILL))),
        "_popen_enter" => instance,
        "_popen_exit" => {
            // CPython closes the output handles, then stdin, and finally
            // waits. Closing the read ends unblocks a child that filled its
            // stdout/stderr pipe and closing stdin gives it EOF; the wait
            // then ends any `communicate()` drain threads still running.
            close_stream_handle(instance, "stdout");
            close_stream_handle(instance, "stderr");
            close_stream_handle(instance, "stdin");
            let _ = popen_wait(instance, None);
            if exc::pending() {
                NONE
            } else {
                value::FALSE
            }
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("subprocess has no native method '{function}'"),
        ),
    }
}

// -------------------------------------------------------------------------
// Dispatch
// -------------------------------------------------------------------------

/// Dispatches a native `subprocess` function or `Popen` method.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "run" => run(args),
        "check_output" => check_output(args),
        "check_call" => check_call(args),
        "call" => call_command(args),
        // CPython's `_cleanup()` runs internal bookkeeping after a fork; Hydro
        // keeps no such state, so it is a documented no-op.
        "_cleanup" => NONE,
        "_args_from_interpreter_flags" | "_optim_args_from_interpreter_flags" => {
            value::list_new(Vec::new())
        }
        "Popen" => popen_new(args),
        "_popen_wait" | "_popen_poll" | "_popen_terminate" | "_popen_kill" | "_popen_enter"
        | "_popen_exit" => popen_method(function, args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'subprocess' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::encode_int;

    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn text(value: &str) -> i64 {
        value::str_alloc(value.to_string())
    }

    fn seq(items: &[&str]) -> i64 {
        value::list_new(items.iter().map(|item| text(item)).collect())
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn attr(namespace: i64, name: &str) -> Option<i64> {
        value::as_namespace(namespace)?
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| *value)
    }

    fn clear_error() {
        if exc::pending() {
            exc::clear();
        }
    }

    fn run_ok(argv: &[&str]) -> i64 {
        exc::init();
        // Capture both streams: the default inherits the test process fds.
        let result = call("run", &[seq(argv), NONE, NONE, NONE, NONE, value::TRUE]);
        assert!(!exc::pending(), "run failed unexpectedly");
        result
    }

    #[test]
    fn run_captures_text_and_bytes() {
        let result = run_ok(&["/bin/echo", "hello"]);
        assert_eq!(
            value::as_bytes(attr(result, "stdout").expect("stdout")).expect("bytes"),
            b"hello\n"
        );
        assert_eq!(
            value::as_integer(attr(result, "returncode").expect("code")),
            Some(0)
        );
        assert_eq!(
            value::as_bytes(attr(result, "stderr").expect("stderr")).expect("bytes"),
            b""
        );

        // text=True decodes the captured output.
        exc::init();
        let result = call(
            "run",
            &[
                seq(&["/bin/echo", "hi"]),
                NONE,
                NONE,
                NONE,
                NONE,
                value::TRUE,
                value::TRUE,
            ],
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(attr(result, "stdout").expect("stdout")),
            Some("hi\n")
        );
    }

    #[test]
    fn run_merges_stderr_into_stdout_pipe() {
        exc::init();
        let result = call(
            "run",
            &[
                seq(&["/bin/sh", "-c", "echo out; echo err >&2"]),
                NONE,
                NONE,
                int(PIPE),
                int(STDOUT),
            ],
        );
        assert!(!exc::pending());
        let stdout = value::as_bytes(attr(result, "stdout").expect("stdout")).expect("bytes");
        assert!(contains(stdout, b"out"));
        assert!(contains(stdout, b"err"));
        assert_eq!(attr(result, "stderr"), Some(NONE));
    }

    #[test]
    fn run_reports_check_failures_and_timeouts() {
        exc::init();
        call(
            "run",
            &[
                seq(&["/bin/sh", "-c", "exit 3"]),
                NONE,
                NONE,
                NONE,
                NONE,
                value::FALSE,
                value::FALSE,
                NONE,
                NONE,
                value::TRUE,
            ],
        );
        assert_eq!(exc::pending_tag(), i64::from(RUNTIME_ERROR));
        clear_error();

        exc::init();
        call(
            "run",
            &[
                seq(&["/bin/sh", "-c", "sleep 5"]),
                NONE,
                NONE,
                NONE,
                NONE,
                value::FALSE,
                value::FALSE,
                NONE,
                value::float_new(0.05),
            ],
        );
        assert_eq!(exc::pending_tag(), i64::from(RUNTIME_ERROR));
        clear_error();
    }

    #[test]
    fn run_honours_cwd_env_and_devnull() {
        exc::init();
        let env = value::dict_new(vec![(text("HYDRO_CHILD_VALUE"), text("ok"))]);
        let result = call(
            "run",
            &[
                seq(&["/bin/sh", "-c", "printf %s \"$HYDRO_CHILD_VALUE\""]),
                NONE,
                NONE,
                int(DEVNULL),
                int(DEVNULL),
                value::FALSE,
                value::FALSE,
                NONE,
                NONE,
                value::FALSE,
                env,
            ],
        );
        assert!(!exc::pending());
        assert_eq!(attr(result, "stdout"), Some(NONE));
        assert_eq!(
            value::as_integer(attr(result, "returncode").expect("code")),
            Some(0)
        );

        exc::init();
        let result = call(
            "run",
            &[
                seq(&["/bin/sh", "-c", "pwd"]),
                NONE,
                NONE,
                int(PIPE),
                NONE,
                value::FALSE,
                value::TRUE,
                text("/tmp"),
            ],
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(attr(result, "stdout").expect("stdout")),
            Some("/tmp\n")
        );
    }

    #[test]
    fn check_output_returns_stdout_and_checks_status() {
        exc::init();
        let output = call("check_output", &[seq(&["/bin/echo", "bytes"])]);
        assert!(!exc::pending());
        assert_eq!(value::as_bytes(output).expect("bytes"), b"bytes\n");

        exc::init();
        let output = call(
            "check_output",
            &[
                seq(&["/bin/echo", "text"]),
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                value::TRUE,
            ],
        );
        assert!(!exc::pending());
        assert_eq!(value::as_str(output), Some("text\n"));

        exc::init();
        call("check_output", &[seq(&["/bin/sh", "-c", "exit 7"])]);
        assert_eq!(exc::pending_tag(), i64::from(RUNTIME_ERROR));
        clear_error();
    }

    #[test]
    fn rejects_malformed_arguments() {
        exc::init();
        call("run", &[int(1)]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        clear_error();

        exc::init();
        call("run", &[value::list_new(Vec::new())]);
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        clear_error();

        exc::init();
        call("run", &[seq(&["/bin/echo"]), int(STDOUT)]);
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        clear_error();

        exc::init();
        call(
            "check_output",
            &[seq(&["/bin/echo"]), int(PIPE), text("input")],
        );
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        clear_error();
    }

    #[test]
    fn popen_wait_poll_and_signal() {
        exc::init();
        let process = call("Popen", &[seq(&["/bin/sh", "-c", "exit 3"])]);
        assert!(!exc::pending());
        assert!(value::as_instance(process).is_some());
        assert!(value::as_integer(value::instance_attr(process, "pid").expect("pid")).is_some());
        assert_eq!(value::as_integer(call("_popen_wait", &[process])), Some(3));
        // Wait records the code; a second call answers without reaping.
        assert_eq!(value::as_integer(call("_popen_wait", &[process])), Some(3));
        assert_eq!(value::as_integer(call("_popen_poll", &[process])), Some(3));
        assert_eq!(call("_popen_terminate", &[process]), NONE);
        assert!(!exc::pending());

        // poll on a live process returns None, then reaps after exit.
        exc::init();
        let process = call("Popen", &[seq(&["/bin/sh", "-c", "sleep 0.2; exit 4"])]);
        assert!(!exc::pending());
        let polled = call("_popen_poll", &[process]);
        // The child may already have exited on a loaded machine.
        if polled == NONE {
            assert_eq!(value::as_integer(call("_popen_wait", &[process])), Some(4));
        } else {
            assert_eq!(value::as_integer(polled), Some(4));
            assert_eq!(value::as_integer(call("_popen_wait", &[process])), Some(4));
        }
        assert!(!exc::pending());
    }

    #[test]
    fn popen_accepts_declared_keywords_and_executable_override() {
        // The keyword binder used by dynamic calls and `functools.partial`
        // must map `stdout` into slot 2; the native function then spawns with
        // the inherited stdout instead of rejecting the keyword.
        exc::init();
        let args = [seq(&["/bin/true"])];
        let stdout = value::str_alloc("stdout".to_string());
        let pipe = int(PIPE);
        let keywords = [stdout, pipe];
        let process = crate::modules::call_native_kw(
            "subprocess",
            "Popen",
            args.len(),
            args.as_ptr(),
            1,
            keywords.as_ptr(),
        );
        assert!(
            !exc::pending(),
            "Popen(stdout=PIPE) failed: {}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(wait_bounded(process), Some(0));

        // `executable=` replaces the program path; the original args[0] is
        // not required to exist.
        let mut slots = vec![NONE; 15];
        slots[0] = seq(&["/nonexistent/hydro-stdin", "-c", "exit 0"]);
        slots[14] = value::str_alloc("/bin/sh".to_string());
        exc::init();
        let process = call("Popen", &slots);
        assert!(
            !exc::pending(),
            "Popen(executable=...) failed: {}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(wait_bounded(process), Some(0));
    }

    #[test]
    fn popen_kill_reports_signal() {
        exc::init();
        let process = call("Popen", &[seq(&["/bin/sh", "-c", "sleep 5"])]);
        assert!(!exc::pending());
        assert_eq!(call("_popen_kill", &[process]), NONE);
        let code = value::as_integer(call("_popen_wait", &[process])).expect("code");
        assert!(
            code < 0,
            "signal-terminated child reports a negative code: {code}"
        );
        assert!(!exc::pending());
    }

    #[test]
    fn popen_records_close_fds_keyword_slot() {
        exc::init();
        // Slot 6 of the Popen argument layout is `close_fds`; an omitted
        // slot defaults to True, and an explicit falsy value is recorded.
        let process = call("Popen", &[seq(&["/bin/true"])]);
        assert!(!exc::pending());
        assert_eq!(
            value::instance_attr(process, "close_fds"),
            Some(value::TRUE)
        );
        let _ = call("_popen_wait", &[process]);

        exc::init();
        let process = call(
            "Popen",
            &[
                seq(&["/bin/true"]),
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                value::FALSE,
            ],
        );
        assert!(!exc::pending());
        assert_eq!(
            value::instance_attr(process, "close_fds"),
            Some(value::FALSE)
        );
        let _ = call("_popen_wait", &[process]);
        assert!(!exc::pending());
    }

    #[test]
    fn popen_rejects_bad_instance() {
        exc::init();
        call("_popen_wait", &[NONE]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        clear_error();
    }

    #[cfg(unix)]
    #[test]
    fn popen_start_new_session_starts_a_new_session() {
        // `sh` reads its own `/proc/self/stat` (field 6 is the session id)
        // and writes the value to `$1`.
        const SESSION_SCRIPT: &str =
            "read _pid _comm _state _ppid _pgrp session _rest < /proc/self/stat; \
             echo \"$session\" > \"$1\"";

        fn session_path(tag: &str) -> String {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            std::env::temp_dir()
                .join(format!("hydro-popen-{tag}-{}-{nanos}", std::process::id()))
                .to_string_lossy()
                .into_owned()
        }

        fn child_session(tag: &str, start_new_session: bool) -> (i64, i64) {
            let path = session_path(tag);
            let argv = vec!["/bin/sh", "-c", SESSION_SCRIPT, "sh", path.as_str()];
            let mut args = vec![seq(&argv), NONE, NONE, NONE, NONE, NONE, NONE];
            if start_new_session {
                args.push(value::TRUE);
            }
            exc::init();
            let process = call("Popen", &args);
            assert!(
                !exc::pending(),
                "Popen failed: {}",
                value::to_string(exc::pending_value())
            );
            let pid =
                value::as_integer(value::instance_attr(process, "pid").expect("pid")).expect("pid");
            let code = value::as_integer(call("_popen_wait", &[process])).expect("exit code");
            assert_eq!(code, 0, "child failed");
            let session = std::fs::read_to_string(&path)
                .expect("child records its session id")
                .trim()
                .parse::<i64>()
                .expect("session id");
            let _ = std::fs::remove_file(&path);
            (pid, session)
        }

        // Without the flag the child stays in the caller's session.
        let (_, inherited) = child_session("inherit", false);
        // SAFETY: `getsid(0)` only reads the calling thread's session id.
        let parent = i64::from(unsafe { libc::getsid(0) });
        assert_eq!(inherited, parent);

        // `setsid` makes the child a session leader, so its session id is its
        // own pid — and therefore differs from the parent's session.
        let (pid, session) = child_session("new", true);
        assert_eq!(session, pid, "the child must lead its own session");
        assert_ne!(session, parent, "the child session must be new");
    }

    /// Waits for `process` with a deadline so a redirection bug shows up as a
    /// failed assertion instead of a hung test run.
    fn wait_bounded(process: i64) -> Option<i64> {
        let code = call("_popen_wait", &[process, value::float_new(5.0)]);
        assert!(
            !exc::pending(),
            "Popen.wait failed: {}",
            value::to_string(exc::pending_value())
        );
        value::as_integer(code)
    }

    #[test]
    fn popen_applies_stdin_redirection() {
        // `read` fails on EOF or on an unreadable descriptor, so the child
        // reports 7 whenever its stdin is `/dev/null` or a closed pipe.
        const READ_SCRIPT: &str = "if read x; then exit 1; else exit 7; fi";

        exc::init();
        // Slot 8 is `stdin`; DEVNULL opens the null device.
        let process = call(
            "Popen",
            &[
                seq(&["/bin/sh", "-c", READ_SCRIPT]),
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                int(DEVNULL),
            ],
        );
        assert!(!exc::pending(), "Popen(DEVNULL) failed");
        assert_eq!(wait_bounded(process), Some(7));

        exc::init();
        // PIPE exposes a writable parent handle (`p.stdin`); closing it gives
        // the child EOF instead of blocking forever.
        let process = call(
            "Popen",
            &[
                seq(&["/bin/sh", "-c", READ_SCRIPT]),
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                int(PIPE),
            ],
        );
        assert!(!exc::pending(), "Popen(PIPE) failed");
        let handle = value::instance_attr(process, "stdin").expect("p.stdin");
        value::as_file(handle).expect("pipe file").close();
        assert_eq!(wait_bounded(process), Some(7));

        exc::init();
        // `stdin=STDOUT` has no meaning (CPython rejects it too).
        call(
            "Popen",
            &[
                seq(&["/bin/true"]),
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                int(STDOUT),
            ],
        );
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        clear_error();

        exc::init();
        // `stdin=PIPE` composes with `stdout`/`stderr` redirection.
        let process = call(
            "Popen",
            &[
                seq(&["/bin/sh", "-c", "echo out; echo err >&2"]),
                NONE,
                int(PIPE),
                int(PIPE),
                NONE,
                NONE,
                NONE,
                NONE,
                int(PIPE),
            ],
        );
        assert!(!exc::pending(), "Popen with piped stdio failed");
        assert_eq!(wait_bounded(process), Some(0));
    }

    #[test]
    fn popen_records_text_encoding_and_errors() {
        fn popen_with(slots: &mut Vec<i64>) -> i64 {
            slots.resize(12, NONE);
            exc::init();
            let process = call("Popen", slots.as_slice());
            assert!(
                !exc::pending(),
                "Popen failed: {}",
                value::to_string(exc::pending_value())
            );
            process
        }

        // Defaults: no text mode, no encoding, no errors, no stdin handle.
        let process = popen_with(&mut vec![seq(&["/bin/true"])]);
        assert_eq!(
            value::instance_attr(process, "text_mode"),
            Some(value::FALSE)
        );
        assert_eq!(value::instance_attr(process, "encoding"), Some(NONE));
        assert_eq!(value::instance_attr(process, "errors"), Some(NONE));
        assert_eq!(value::instance_attr(process, "stdin"), Some(NONE));
        let _ = wait_bounded(process);

        // Slot 9/10: `encoding='utf-8', errors='strict'` turn text mode on.
        let process = popen_with(&mut vec![
            seq(&["/bin/true"]),
            NONE,
            NONE,
            NONE,
            NONE,
            NONE,
            NONE,
            NONE,
            NONE,
            text("utf-8"),
            text("strict"),
        ]);
        assert_eq!(
            value::instance_attr(process, "text_mode"),
            Some(value::TRUE)
        );
        assert_eq!(
            value::as_str(value::instance_attr(process, "encoding").expect("encoding")),
            Some("utf-8")
        );
        assert_eq!(
            value::as_str(value::instance_attr(process, "errors").expect("errors")),
            Some("strict")
        );
        let _ = wait_bounded(process);

        // Slot 4: `text=True` alone implies the runtime's UTF-8 text path.
        let process = popen_with(&mut vec![
            seq(&["/bin/true"]),
            NONE,
            NONE,
            NONE,
            value::TRUE,
        ]);
        assert_eq!(
            value::instance_attr(process, "text_mode"),
            Some(value::TRUE)
        );
        assert_eq!(
            value::as_str(value::instance_attr(process, "encoding").expect("encoding")),
            Some("utf-8")
        );
        assert_eq!(value::instance_attr(process, "errors"), Some(NONE));
        let _ = wait_bounded(process);

        // An empty codec name is falsy in CPython, so it keeps binary mode.
        let process = popen_with(&mut vec![
            seq(&["/bin/true"]),
            NONE,
            NONE,
            NONE,
            NONE,
            NONE,
            NONE,
            NONE,
            NONE,
            text(""),
        ]);
        assert_eq!(
            value::instance_attr(process, "text_mode"),
            Some(value::FALSE)
        );
        assert_eq!(
            value::as_str(value::instance_attr(process, "encoding").expect("encoding")),
            Some("")
        );
        let _ = wait_bounded(process);
    }

    #[test]
    fn popen_rejects_preexec_fn() {
        exc::init();
        // Slot 11 is `preexec_fn`; a Python hook cannot run between `fork`
        // and `exec`, so a supplied one is reported instead of ignored.
        call(
            "Popen",
            &[
                seq(&["/bin/true"]),
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                value::native_function_new("test", "preexec_hook", 0),
            ],
        );
        assert_eq!(exc::pending_tag(), i64::from(NOT_IMPLEMENTED_ERROR));
        clear_error();

        // An explicit `None` is the default and spawns normally.
        exc::init();
        let process = call(
            "Popen",
            &[
                seq(&["/bin/true"]),
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
            ],
        );
        assert!(!exc::pending(), "Popen(preexec_fn=None) failed");
        let _ = wait_bounded(process);
    }

    #[test]
    fn call_returns_the_exit_status() {
        exc::init();
        assert_eq!(call("call", &[seq(&["/bin/sh", "-c", "exit 0"])]), int(0));
        assert_eq!(call("call", &[seq(&["/bin/sh", "-c", "exit 3"])]), int(3));
        assert!(!exc::pending(), "call raised");
    }

    #[cfg(unix)]
    #[test]
    fn call_reports_negative_signal_status() {
        exc::init();
        // SIGKILL is uncatchable, so the status is exactly `-SIGKILL`.
        let status = call("call", &[seq(&["/bin/sh", "-c", "kill -KILL $$"])]);
        assert!(!exc::pending(), "call raised");
        assert_eq!(value::as_integer(status), Some(-i64::from(libc::SIGKILL)));
    }

    #[test]
    fn check_call_returns_zero_or_raises_called_process_error() {
        exc::init();
        let ok = call("check_call", &[seq(&["/bin/sh", "-c", "exit 0"])]);
        assert!(!exc::pending(), "check_call raised on success");
        assert_eq!(value::as_integer(ok), Some(0));

        let failed = call("check_call", &[seq(&["/bin/sh", "-c", "exit 7"])]);
        assert_eq!(failed, NONE);
        let error = exc::pending_value();
        assert_eq!(
            value::instance_class_value(error),
            Some(called_process_error_class())
        );
        assert_eq!(
            value::instance_attr(error, "returncode").and_then(value::as_integer),
            Some(7)
        );
        let command = value::instance_attr(error, "cmd").expect("cmd");
        let words = value::as_list(command).expect("cmd list");
        assert_eq!(
            words
                .iter()
                .map(|word| value::as_str(*word).unwrap_or(""))
                .collect::<Vec<_>>(),
            vec!["/bin/sh", "-c", "exit 7"]
        );
        assert_eq!(value::instance_attr(error, "output"), Some(NONE));
        assert_eq!(value::instance_attr(error, "stderr"), Some(NONE));
        assert_eq!(
            value::exception_data(error).map(|data| data.msg.as_str()),
            Some("Command '['/bin/sh', '-c', 'exit 7']' returned non-zero exit status 7.")
        );
        exc::clear();

        // `shell=True` routes through `/bin/sh -c` like `call`.
        let failed = call(
            "check_call",
            &[text("exit 3"), NONE, NONE, NONE, value::TRUE],
        );
        assert_eq!(failed, NONE);
        let command = value::instance_attr(exc::pending_value(), "cmd").expect("cmd");
        assert_eq!(
            value::as_list(command)
                .expect("cmd list")
                .iter()
                .map(|word| value::as_str(*word).unwrap_or(""))
                .collect::<Vec<_>>(),
            vec!["/bin/sh", "-c", "exit 3"]
        );
        exc::clear();
    }

    #[test]
    fn cleanup_is_a_noop() {
        exc::init();
        assert_eq!(call("_cleanup", &[]), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn call_runs_through_the_shell() {
        exc::init();
        // A string command is handed to `sh -c`.
        let status = call("call", &[text("exit 4"), NONE, NONE, NONE, value::TRUE]);
        assert!(!exc::pending(), "shell call raised");
        assert_eq!(value::as_integer(status), Some(4));

        // A sequence command keeps its arguments after the `-c` script.
        let status = call(
            "call",
            &[seq(&["echo", "hi"]), NONE, NONE, NONE, value::TRUE],
        );
        assert!(!exc::pending(), "shell list call raised");
        assert_eq!(value::as_integer(status), Some(0));
    }

    #[test]
    fn call_drains_piped_streams() {
        exc::init();
        let status = call(
            "call",
            &[
                seq(&["/bin/sh", "-c", "echo out; echo err 1>&2"]),
                NONE,
                int(PIPE),
                int(STDOUT),
            ],
        );
        assert!(!exc::pending(), "piped call raised");
        assert_eq!(value::as_integer(status), Some(0));

        // DEVNULL for both streams spawns normally too.
        let status = call(
            "call",
            &[
                seq(&["/bin/echo", "hi"]),
                int(DEVNULL),
                int(DEVNULL),
                int(DEVNULL),
            ],
        );
        assert!(!exc::pending(), "devnull call raised");
        assert_eq!(value::as_integer(status), Some(0));
    }

    #[test]
    fn call_honours_env() {
        exc::init();
        let env = value::dict_new(vec![(text("HYDRO_CALL_ENV"), text("yes"))]);
        let status = call(
            "call",
            &[
                seq(&["/bin/sh", "-c", "test \"$HYDRO_CALL_ENV\" = yes"]),
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                env,
            ],
        );
        assert!(!exc::pending(), "env call raised");
        assert_eq!(value::as_integer(status), Some(0));
    }

    #[test]
    fn call_validates_its_arguments() {
        exc::init();
        // Missing `args`.
        assert_eq!(call("call", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        clear_error();

        // `stdin` must not select STDOUT.
        assert_eq!(call("call", &[seq(&["/bin/true"]), int(STDOUT)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        clear_error();

        // A cwd that does not exist surfaces as the spawn OSError.
        let missing = text("/hydro-nonexistent-call-cwd");
        assert_eq!(
            call(
                "call",
                &[seq(&["/bin/true"]), NONE, NONE, NONE, NONE, NONE, missing,],
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(OS_ERROR));
        clear_error();
    }
}
