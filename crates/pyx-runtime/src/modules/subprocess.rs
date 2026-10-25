//! `subprocess`: process spawning and simple pipe capture.
//!
//! The module covers the benchmark-facing subset: [`run`], [`check_output`]
//! and a native `Popen` object with `wait`/`poll`/`terminate`/`kill`. There is
//! no `shell=True`, no `communicate()` and no file-object redirection; the
//! stream selectors accept `None`, `PIPE`, `DEVNULL` and (for `stderr`)
//! `STDOUT`.
//!
//! `stdin`/`stdout`/`stderr` are `std::process::Stdio` choices. For
//! `stderr=STDOUT` the child's stderr descriptor is pointed at the same pipe
//! (or at the parent's stdout when nothing is captured), so interleaving is
//! preserved like CPython instead of concatenating two buffers afterwards.
//!
//! Spawned `Popen` processes live in a process-global
//! `Mutex<HashMap<u64, Child>>`; the instance carries the key in a private
//! `$id` attribute. `wait`/`poll` remove and reap the entry, so a second reap
//! is impossible; the recorded `returncode` answers later `poll`/`wait` calls.
//!
//! Blocking waits park the thread with [`heap::blocking_enter`] so a
//! concurrent collection does not wait for a thread sleeping in the kernel;
//! no PyX object is allocated while parked.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::exc::{
    self, ATTRIBUTE_ERROR, OS_ERROR, OVERFLOW_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::value::{self, NONE};

/// Private instance attribute holding the key into [`CHILDREN`].
const ID_ATTR: &str = "$id";

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
}

fn stream_mode(argument: i64, name: &str) -> Result<StreamMode, i64> {
    if argument == NONE {
        return Ok(StreamMode::Inherit);
    }
    match value::as_integer(argument) {
        Some(PIPE) => Ok(StreamMode::Pipe),
        Some(STDOUT) => Ok(StreamMode::Stdout),
        Some(DEVNULL) => Ok(StreamMode::DevNull),
        _ => Err(value_error(&format!(
            "subprocess: {name} must be None, subprocess.PIPE, subprocess.STDOUT or \
             subprocess.DEVNULL, not '{}'",
            value::type_name(argument)
        ))),
    }
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
) -> Result<Command, i64> {
    let Some((program, rest)) = args.split_first() else {
        return Err(value_error("subprocess: args must not be empty"));
    };
    let mut command = Command::new(program);
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
/// reader threads copy bytes only, they never touch PyX objects.
fn execute(plan: &Plan) -> Result<Captured, i64> {
    let mut command = build_command(&plan.args, &plan.cwd, &plan.env)?;

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
        },
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

/// Reads a `Popen` pipe until EOF and drops the bytes on a detached thread.
fn discard<R: Read + Send + 'static>(mut reader: R) {
    let _ = std::thread::Builder::new()
        .stack_size(16 * 1024)
        .spawn(move || {
            let mut sink = [0u8; 4096];
            loop {
                match reader.read(&mut sink) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
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
    let text = flag(argument(args, 6));
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
// Popen
// -------------------------------------------------------------------------

/// Builds the native `Popen` class. A fresh class per instance matches the
/// other native objects in this runtime; the methods are the dispatch names
/// validated by the central spec table.
fn popen_class() -> i64 {
    const METHODS: [(&str, &str, usize); 4] = [
        ("wait", "_popen_wait", 2),
        ("poll", "_popen_poll", 1),
        ("terminate", "_popen_terminate", 1),
        ("kill", "_popen_kill", 1),
    ];
    let mut functions = [NONE; METHODS.len()];
    for (slot, (_, native, arity)) in functions.iter_mut().zip(METHODS) {
        *slot = value::native_function_new("subprocess", native, arity);
    }
    let methods = METHODS
        .iter()
        .zip(functions)
        .map(|((python_name, _, _), function)| ((*python_name).to_string(), function))
        .collect();
    value::class_new("Popen".to_string(), NONE, methods)
}

fn popen_new(args: &[i64]) -> i64 {
    let args_argument = argument(args, 0);
    let argv = match command_args(args_argument) {
        Ok(argv) => argv,
        Err(error) => return error,
    };
    let cwd = match path_option(argument(args, 1), "cwd") {
        Ok(cwd) => cwd,
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

    let mut command = match build_command(&argv, &cwd, &env) {
        Ok(command) => command,
        Err(error) => return error,
    };
    if stdout == StreamMode::DevNull {
        command.stdout(Stdio::null());
    } else if stdout == StreamMode::Pipe {
        command.stdout(Stdio::piped());
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
            // `Popen` exposes no read handle, so a merged pipe would be a
            // discarded pipe either way; use the null device instead of
            // filling an unread pipe buffer.
            StreamMode::Pipe => {
                command.stderr(Stdio::null());
            }
            StreamMode::Stdout => {}
        },
        StreamMode::Inherit => {}
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return io_error(error),
    };
    // `Popen` has no read API in this slice, so the piped descriptors are
    // drained to nowhere; otherwise a chatty child would block on a full pipe
    // and never reach a state where `wait()` can return.
    if let Some(reader) = child.stdout.take() {
        discard(reader);
    }
    if let Some(reader) = child.stderr.take() {
        discard(reader);
    }
    let pid = i64::from(child.id());
    let key = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    children().insert(key, child);

    let instance = value::instance_new(popen_class());
    value::instance_setattr(
        instance,
        "args".to_string(),
        args_value(args_argument, &argv),
    );
    value::instance_setattr(instance, "returncode".to_string(), NONE);
    value::instance_setattr(instance, "pid".to_string(), int_value(pid));
    value::instance_setattr(instance, ID_ATTR.to_string(), int_value(key as i64));
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

fn popen_wait(instance: i64, timeout: Option<Duration>) -> i64 {
    let key = match process_key(instance) {
        Ok(key) => key,
        Err(error) => return error,
    };
    let Some(mut child) = children().remove(&key) else {
        return match recorded_returncode(instance) {
            Some(code) => code,
            None => runtime_error("subprocess: Popen object has no live process"),
        };
    };

    heap::blocking_enter();
    let waited = wait_child(&mut child, timeout, false);
    heap::blocking_leave();

    match waited {
        Ok(status) => {
            let code = returncode(status);
            value::instance_setattr(instance, "returncode".to_string(), int_value(code));
            int_value(code)
        }
        Err(WaitError::Timeout) => {
            children().insert(key, child);
            runtime_error(&format!(
                "Popen.wait() timed out after {} seconds",
                timeout.map(|timeout| timeout.as_secs_f64()).unwrap_or(0.0)
            ))
        }
        Err(WaitError::Io(error)) => {
            children().insert(key, child);
            io_error(error)
        }
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
        Polled::Missing => match recorded_returncode(instance) {
            Some(code) => code,
            None => runtime_error("subprocess: Popen object has no live process"),
        },
        Polled::Failed(error) => io_error(error),
    }
}

fn popen_signal(instance: i64, signal: i32) -> i64 {
    let key = match process_key(instance) {
        Ok(key) => key,
        Err(error) => return error,
    };
    let Some(pid) = children().get(&key).map(|child| child.id() as i32) else {
        return match recorded_returncode(instance) {
            Some(_) => NONE,
            None => runtime_error("subprocess: Popen object has no live process"),
        };
    };
    // SAFETY: `kill` only reads the pid and signal; the child is still owned
    // by the registry (and therefore not reaped) until `wait`/`poll`.
    if unsafe { libc::kill(pid, signal) } == 0 {
        return NONE;
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        runtime_error("subprocess: process already terminated")
    } else {
        io_error(error)
    }
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
        "_popen_terminate" => popen_signal(instance, libc::SIGTERM),
        "_popen_kill" => popen_signal(instance, libc::SIGKILL),
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
        "Popen" => popen_new(args),
        "_popen_wait" | "_popen_poll" | "_popen_terminate" | "_popen_kill" => {
            popen_method(function, args)
        }
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
        let env = value::dict_new(vec![(text("PYX_CHILD_VALUE"), text("ok"))]);
        let result = call(
            "run",
            &[
                seq(&["/bin/sh", "-c", "printf %s \"$PYX_CHILD_VALUE\""]),
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
    fn popen_rejects_bad_instance() {
        exc::init();
        call("_popen_wait", &[NONE]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        clear_error();
    }
}
