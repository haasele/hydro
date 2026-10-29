//! `_signal`: the Linux signal surface behind the standard library's
//! `signal` module.
//!
//! The constant table carries CPython's Linux `_signal` names
//! (`SIGHUP`...`SIGSYS`, the alias names, `SIG_DFL`/`SIG_IGN`, the
//! `SIG_BLOCK`/`SIG_UNBLOCK`/`SIG_SETMASK` operations, the `ITIMER_*` clocks
//! and `NSIG`). `SIGRTMIN`/`SIGRTMAX` are read from libc at runtime, so the
//! range matches the C library the process is linked against; `NSIG` is
//! `SIGRTMAX + 1`, exactly as CPython reports it.
//!
//! # Handler table
//!
//! Like CPython, the module owns the authoritative handler table: `signal()`
//! returns the previously installed handler, `getsignal()` reads the table,
//! and the initial entries mirror CPython's interpreter startup
//! (`SIGINT` -> `default_int_handler`, `SIGPIPE` -> `SIG_IGN`, everything else
//! -> `SIG_DFL`). Callable handlers are kept alive by installing them as
//! process-wide collector roots, so a callback remains valid even when no
//! Python frame mentions it.
//!
//! # Delivery
//!
//! A callable handler is installed in the kernel as a small `extern "C"`
//! trampoline that only records the signal number in an atomic mask; that is
//! the only work that is async-signal-safe in this runtime. Python callbacks
//! therefore run synchronously from the safe points the module itself owns:
//! [`raise_signal`](call) and `pause()` drain the mask immediately after the
//! signal is delivered, and [`dispatch_pending`] is the hook a host (the
//! interpreter loop or an embedding application) can call at any safe point
//! to run callbacks queued by an externally delivered signal. Until such a
//! call happens, an external signal only leaves a pending bit; this module
//! never claims that Python code runs asynchronously inside a signal handler.
//! `SIG_DFL` and `SIG_IGN` are installed directly and need no dispatch.
//!
//! `default_int_handler` raises `KeyboardInterrupt`; the other CPython
//! main-thread restriction on `signal()` is not enforced, because the runtime
//! has no notion of a "main interpreter" thread.

use std::mem;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, KEYBOARD_INTERRUPT, OS_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::value::{self, FunctionBody, NONE};

/// `SIG_DFL`: the default disposition.
const SIG_DFL: i64 = 0;
/// `SIG_IGN`: ignore the signal.
const SIG_IGN: i64 = 1;

/// `pthread_sigmask` operations.
const SIG_BLOCK: i32 = 0;
const SIG_UNBLOCK: i32 = 1;
const SIG_SETMASK: i32 = 2;

/// `setitimer`/`getitimer` clocks.
const ITIMER_REAL: i32 = 0;
const ITIMER_VIRTUAL: i32 = 1;
const ITIMER_PROF: i32 = 2;

/// The kernel numbers whose initial dispositions CPython overrides.
const SIGINT_NUMBER: usize = 2;
const SIGPIPE_NUMBER: usize = 13;
const SIGKILL_NUMBER: i32 = 9;
const SIGSTOP_NUMBER: i32 = 19;

/// Signals 1...=32 live in the low pending word, 33...=64 in the high one.
const PENDING_SPLIT: i32 = 32;

/// The low half of the pending mask (signals 1...=32, bit `signal - 1`).
static PENDING_LOW: AtomicU32 = AtomicU32::new(0);
/// The high half of the pending mask (signals 33...=64, bit `signal - 33`).
static PENDING_HIGH: AtomicU32 = AtomicU32::new(0);

/// `(name, value)` pairs exposed by `_signal`, using the Linux generic
/// numbering shared by x86-64 and AArch64.
const SIGNAL_CONSTANTS: &[(&str, i64)] = &[
    ("SIGHUP", 1),
    ("SIGINT", 2),
    ("SIGQUIT", 3),
    ("SIGILL", 4),
    ("SIGTRAP", 5),
    ("SIGABRT", 6),
    ("SIGIOT", 6),
    ("SIGBUS", 7),
    ("SIGFPE", 8),
    ("SIGKILL", 9),
    ("SIGUSR1", 10),
    ("SIGSEGV", 11),
    ("SIGUSR2", 12),
    ("SIGPIPE", 13),
    ("SIGALRM", 14),
    ("SIGTERM", 15),
    ("SIGSTKFLT", 16),
    ("SIGCHLD", 17),
    ("SIGCLD", 17),
    ("SIGCONT", 18),
    ("SIGSTOP", 19),
    ("SIGTSTP", 20),
    ("SIGTTIN", 21),
    ("SIGTTOU", 22),
    ("SIGURG", 23),
    ("SIGXCPU", 24),
    ("SIGXFSZ", 25),
    ("SIGVTALRM", 26),
    ("SIGPROF", 27),
    ("SIGWINCH", 28),
    ("SIGIO", 29),
    ("SIGPOLL", 29),
    ("SIGPWR", 30),
    ("SIGSYS", 31),
    ("SIG_DFL", SIG_DFL),
    ("SIG_IGN", SIG_IGN),
    ("SIG_BLOCK", SIG_BLOCK as i64),
    ("SIG_UNBLOCK", SIG_UNBLOCK as i64),
    ("SIG_SETMASK", SIG_SETMASK as i64),
    ("ITIMER_REAL", ITIMER_REAL as i64),
    ("ITIMER_VIRTUAL", ITIMER_VIRTUAL as i64),
    ("ITIMER_PROF", ITIMER_PROF as i64),
];

fn encode_int(n: i64) -> i64 {
    value::encode_int(n).unwrap_or(NONE)
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn overflow_error(message: &str) -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, message)
}

fn attribute_error(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '_signal' has no attribute '{name}'"),
    )
}

/// `SIGRTMIN` from the C library (34 on glibc, 35 on musl).
fn sigrtmin() -> i32 {
    libc::SIGRTMIN()
}

/// `SIGRTMAX` from the C library (64 on glibc).
fn sigrtmax() -> i32 {
    libc::SIGRTMAX()
}

/// `NSIG`: one past the highest valid signal number.
fn nsig() -> i32 {
    sigrtmax() + 1
}

/// Whether `signal` is a number CPython accepts for the one-argument
/// functions (`signal()`, `getsignal()`, `strsignal()`, `siginterrupt()`).
fn valid_signal(signal: i32) -> bool {
    (1..nsig()).contains(&signal)
}

/// The C int conversion CPython's argument parsing performs.
///
/// `bool` is accepted (it is an `int` subclass); a value that does not fit a C
/// `int` is an `OverflowError`, and any other type is a `TypeError` naming the
/// type, matching `_signal`'s argument clinic converters.
fn c_int_argument(v: i64) -> Result<i32, i64> {
    match value::as_integer(v) {
        Some(n) if (i32::MIN as i64..=i32::MAX as i64).contains(&n) => Ok(n as i32),
        Some(_) => Err(overflow_error("Python int too large to convert to C int")),
        None if value::type_name(v) == "int" => {
            Err(overflow_error("Python int too large to convert to C int"))
        }
        None => Err(type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(v)
        ))),
    }
}

/// CPython's `[Errno N] message` OSError text.
fn strerror(code: i32) -> String {
    let text = std::io::Error::from_raw_os_error(code).to_string();
    let detail = text.split(" (os error").next().unwrap_or(text.as_str());
    detail.to_string()
}

fn os_errno_error(code: i32) -> i64 {
    exc::raise_with_message(OS_ERROR, &format!("[Errno {code}] {}", strerror(code)))
}

/// `_signal.ItimerError`: an `OSError` subclass, as in CPython.
fn itimer_error_class() -> i64 {
    const KEY: &str = "_signal.ItimerError";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    let class = value::class_new(
        "ItimerError".to_string(),
        exc::exception_class(OS_ERROR),
        Vec::new(),
    );
    globals::registry_set(KEY, class);
    class
}

/// Raises `class(message)` and returns the `None` dummy value.
fn raise_class_error(class: i64, message: &str) -> i64 {
    let message = value::str_alloc(message.to_string());
    let roots = heap::root_values(&[message, class]);
    let instance = crate::builtins::call(class, 1, &message as *const i64);
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let instance_roots = heap::root_values(&[instance]);
    let result = exc::raise_value(instance);
    heap::release_roots(instance_roots);
    heap::release_roots(roots);
    result
}

/// Raises `ItimerError` in CPython's `[Errno N] message` form.
fn itimer_error(code: i32) -> i64 {
    raise_class_error(
        itimer_error_class(),
        &format!("[Errno {code}] {}", strerror(code)),
    )
}

/// Materialises a module constant.
pub(crate) fn constant(name: &str) -> i64 {
    if let Some(entry) = SIGNAL_CONSTANTS.iter().find(|entry| entry.0 == name) {
        return encode_int(entry.1);
    }
    match name {
        "SIGRTMIN" => encode_int(i64::from(sigrtmin())),
        "SIGRTMAX" => encode_int(i64::from(sigrtmax())),
        "NSIG" => encode_int(i64::from(nsig())),
        "ItimerError" => itimer_error_class(),
        _ => attribute_error(name),
    }
}

// -------------------------------------------------------------------------
// Handler table
// -------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Handler {
    Default,
    Ignore,
    /// A callable kept alive by a process-wide collector root.
    Callable(i64),
}

struct State {
    /// Heap generation the entries belong to. A new program run frees the
    /// heap and drops the global roots, so the table starts over with the
    /// CPython startup defaults.
    generation: u64,
    handlers: Vec<Handler>,
    /// Whether the trampoline is installed in the kernel for this signal.
    installed: Vec<bool>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(fresh_state(0)));

fn lock_state() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `_signal.default_int_handler` as a callable value.
fn default_int_handler_value() -> i64 {
    let value = value::native_function_new("_signal", "default_int_handler", 2);
    heap::add_global_root(value);
    value
}

fn fresh_state(generation: u64) -> State {
    let limit = nsig().max(1) as usize;
    let mut handlers = vec![Handler::Default; limit];
    if let Some(slot) = handlers.get_mut(SIGPIPE_NUMBER) {
        *slot = Handler::Ignore;
    }
    if let Some(slot) = handlers.get_mut(SIGINT_NUMBER) {
        *slot = Handler::Callable(default_int_handler_value());
    }
    State {
        generation,
        installed: vec![false; handlers.len()],
        handlers,
    }
}

fn state() -> MutexGuard<'static, State> {
    let generation = heap::generation();
    let mut guard = lock_state();
    if guard.generation != generation {
        *guard = fresh_state(generation);
    }
    guard
}

fn handler_for(signal: i32) -> Handler {
    let guard = state();
    guard
        .handlers
        .get(signal as usize)
        .copied()
        .unwrap_or(Handler::Default)
}

fn is_default_int_handler(v: i64) -> bool {
    let Some(function) = value::as_function(v) else {
        return false;
    };
    matches!(
        &function.body,
        FunctionBody::Native {
            module: "_signal",
            name: "default_int_handler",
        }
    )
}

/// Decodes the `handler` argument of `signal()`.
///
/// CPython accepts the exact integers `SIG_DFL` and `SIG_IGN` (a `bool` is
/// rejected even though it is an `int` subclass) or any callable.
fn decode_handler(v: i64) -> Option<Handler> {
    let type_name = value::type_name(v);
    if type_name == "int" {
        return match value::as_integer(v) {
            Some(number) if number == SIG_DFL => Some(Handler::Default),
            Some(number) if number == SIG_IGN => Some(Handler::Ignore),
            _ => None,
        };
    }
    if crate::builtins::callable(v) == value::TRUE {
        return Some(Handler::Callable(v));
    }
    None
}

/// Installs `handler` for `signal` in the kernel.
fn install_action(signal: i32, handler: Handler) -> Result<(), i32> {
    let mut action: libc::sigaction = unsafe { mem::zeroed() };
    // SAFETY: `action.sa_mask` is a zeroed `sigset_t` owned by this frame and
    // `sigemptyset` only initialises it.
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    action.sa_flags = match handler {
        Handler::Callable(_) => libc::SA_RESTART,
        Handler::Default | Handler::Ignore => 0,
    };
    action.sa_sigaction = match handler {
        Handler::Default => libc::SIG_DFL,
        Handler::Ignore => libc::SIG_IGN,
        Handler::Callable(_) => record_signal as *const () as libc::sighandler_t,
    };
    // SAFETY: `action` is a fully initialised `sigaction` for this process;
    // the kernel copies it and validates the signal number itself.
    let result = unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL))
    }
}

/// Kernel-side trampoline: async-signal-safe bookkeeping only.
extern "C" fn record_signal(signal: libc::c_int) {
    if (1..=PENDING_SPLIT).contains(&signal) {
        PENDING_LOW.fetch_or(1u32 << (signal - 1), Ordering::Relaxed);
    } else if (PENDING_SPLIT + 1..=64).contains(&signal) {
        PENDING_HIGH.fetch_or(1u32 << (signal - PENDING_SPLIT - 1), Ordering::Relaxed);
    }
}

/// Atomically claims every signal recorded by the trampoline.
fn take_pending() -> Vec<i32> {
    let low = PENDING_LOW.swap(0, Ordering::SeqCst);
    let high = PENDING_HIGH.swap(0, Ordering::SeqCst);
    let mut signals = Vec::new();
    for bit in 0..PENDING_SPLIT {
        if low & (1u32 << bit) != 0 {
            signals.push(bit + 1);
        }
    }
    for bit in 0..32 {
        if high & (1u32 << bit) != 0 {
            signals.push(bit + PENDING_SPLIT + 1);
        }
    }
    signals
}

/// Runs the Python handlers for signals recorded by the trampoline.
///
/// This is the documented safe host hook: it must be called from ordinary
/// execution (never from inside a signal handler). `raise_signal()` and
/// `pause()` call it themselves; the interpreter can call it at other safe
/// points to make externally delivered signals visible to Python code.
///
/// A `default_int_handler` registration raises `KeyboardInterrupt`. Any
/// exception raised by a callback stops the drain and stays pending, so the
/// remaining recorded signals are dropped.
pub(crate) fn dispatch_pending() {
    for signal in take_pending() {
        match handler_for(signal) {
            Handler::Callable(function) => {
                if is_default_int_handler(function) {
                    exc::raise_with_message(KEYBOARD_INTERRUPT, "");
                    return;
                }
                let args = [encode_int(i64::from(signal)), NONE];
                crate::builtins::call(function, args.len(), args.as_ptr());
                if exc::pending() {
                    return;
                }
            }
            Handler::Default | Handler::Ignore => {}
        }
    }
}

/// Makes sure the kernel trampoline is installed for a callable entry.
fn ensure_installed(signal: i32) {
    let (handler, already) = {
        let guard = state();
        let handler = guard
            .handlers
            .get(signal as usize)
            .copied()
            .unwrap_or(Handler::Default);
        let installed = guard
            .installed
            .get(signal as usize)
            .copied()
            .unwrap_or(false);
        (handler, installed)
    };
    if already || !matches!(handler, Handler::Callable(_)) {
        return;
    }
    if install_action(signal, handler).is_ok() {
        let mut guard = state();
        if let Some(flag) = guard.installed.get_mut(signal as usize) {
            *flag = true;
        }
    }
}

// -------------------------------------------------------------------------
// Functions
// -------------------------------------------------------------------------

/// `_signal.signal(signalnum, handler)`: installs a disposition and returns
/// the previous handler.
fn signal_call(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return type_error(&format!("signal expected 2 arguments, got {}", args.len()));
    }
    let signal = match c_int_argument(args[0]) {
        Ok(signal) => signal,
        Err(error) => return error,
    };
    if !valid_signal(signal) {
        return value_error("signal number out of range");
    }
    let Some(handler) = decode_handler(args[1]) else {
        return type_error(
            "signal handler must be signal.SIG_IGN, signal.SIG_DFL, or a callable object",
        );
    };
    let previous = handler_for(signal);
    if let Err(code) = install_action(signal, handler) {
        return os_errno_error(code);
    }
    if let Handler::Callable(function) = handler {
        heap::add_global_root(function);
    }
    let installed = matches!(handler, Handler::Callable(_));
    let mut guard = state();
    if let Some(slot) = guard.handlers.get_mut(signal as usize) {
        *slot = handler;
    }
    if let Some(flag) = guard.installed.get_mut(signal as usize) {
        *flag = installed;
    }
    drop(guard);
    match previous {
        Handler::Default => encode_int(SIG_DFL),
        Handler::Ignore => encode_int(SIG_IGN),
        Handler::Callable(function) => function,
    }
}

/// `_signal.getsignal(signalnum)`: the handler this module installed.
fn getsignal_call(args: &[i64]) -> i64 {
    if args.len() != 1 {
        return type_error(&format!(
            "_signal.getsignal() takes exactly one argument ({} given)",
            args.len()
        ));
    }
    let signal = match c_int_argument(args[0]) {
        Ok(signal) => signal,
        Err(error) => return error,
    };
    if !valid_signal(signal) {
        return value_error("signal number out of range");
    }
    match handler_for(signal) {
        Handler::Default => encode_int(SIG_DFL),
        Handler::Ignore => encode_int(SIG_IGN),
        Handler::Callable(function) => function,
    }
}

/// `_signal.raise_signal(signalnum)`: sends the signal to this process.
fn raise_signal_call(args: &[i64]) -> i64 {
    if args.len() != 1 {
        return type_error(&format!(
            "_signal.raise_signal() takes exactly one argument ({} given)",
            args.len()
        ));
    }
    let signal = match c_int_argument(args[0]) {
        Ok(signal) => signal,
        Err(error) => return error,
    };
    ensure_installed(signal);
    // SAFETY: `raise` only takes a signal number; the kernel validates it.
    if unsafe { libc::raise(signal) } != 0 {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL);
        return os_errno_error(code);
    }
    dispatch_pending();
    NONE
}

/// `_signal.strsignal(signalnum)`: the C library's description, or `None`.
fn strsignal_call(args: &[i64]) -> i64 {
    if args.len() != 1 {
        return type_error(&format!(
            "_signal.strsignal() takes exactly one argument ({} given)",
            args.len()
        ));
    }
    let signal = match c_int_argument(args[0]) {
        Ok(signal) => signal,
        Err(error) => return error,
    };
    if !valid_signal(signal) {
        return value_error("signal number out of range");
    }
    // SAFETY: `strsignal` returns either a NUL-terminated static string or
    // NULL; it does not write through any caller pointer.
    let text = unsafe { libc::strsignal(signal) };
    if text.is_null() {
        return NONE;
    }
    // SAFETY: a non-NULL result points at a NUL-terminated C string.
    let text = unsafe { std::ffi::CStr::from_ptr(text) };
    value::str_alloc(text.to_string_lossy().into_owned())
}

/// `_signal.valid_signals()`: every number in `1..NSIG` except `SIGKILL` and
/// `SIGSTOP`, which cannot be caught.
fn valid_signals_call(args: &[i64]) -> i64 {
    if !args.is_empty() {
        return type_error(&format!(
            "_signal.valid_signals() takes no arguments ({} given)",
            args.len()
        ));
    }
    let mut signals = Vec::new();
    for signal in 1..nsig() {
        if signal != SIGKILL_NUMBER && signal != SIGSTOP_NUMBER {
            signals.push(encode_int(i64::from(signal)));
        }
    }
    value::set_new(signals)
}

/// `_signal.sigwait(sigset)`: blocks until one signal from the set is
/// delivered, accepts it, and returns its number (`sigwait(3)`).
fn sigwait_call(args: &[i64]) -> i64 {
    if args.len() != 1 {
        return type_error(&format!("sigwait expected 1 argument, got {}", args.len()));
    }
    let mut set: libc::sigset_t = unsafe { mem::zeroed() };
    // SAFETY: `set` is a zeroed set owned by this frame.
    unsafe { libc::sigemptyset(&mut set) };
    let iterator = crate::iter::iter_of(args[0]);
    if exc::pending() {
        return NONE;
    }
    loop {
        let mut item = NONE;
        let status = crate::iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let signal = match c_int_argument(item) {
            Ok(signal) => signal,
            Err(error) => return error,
        };
        if !valid_signal(signal) {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("signal number {signal} out of range [1; {}]", sigrtmax()),
            );
        }
        // SAFETY: `signal` was validated for this process.
        unsafe { libc::sigaddset(&mut set, signal) };
    }
    let mut received: libc::c_int = 0;
    // SAFETY: `set` holds validated signal numbers; `received` is owned here.
    let result = unsafe { libc::sigwait(&set, &mut received) };
    if result != 0 {
        return os_errno_error(result);
    }
    encode_int(i64::from(received))
}

/// `_signal.pthread_sigmask(how, mask)`: changes the calling thread's mask
/// and returns the previous one.
fn pthread_sigmask_call(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return type_error(&format!(
            "pthread_sigmask expected 2 arguments, got {}",
            args.len()
        ));
    }
    let how = match c_int_argument(args[0]) {
        Ok(how) => how,
        Err(error) => return error,
    };
    let mut set: libc::sigset_t = unsafe { mem::zeroed() };
    // SAFETY: `set` is a zeroed set owned by this frame.
    unsafe { libc::sigemptyset(&mut set) };
    let iterator = crate::iter::iter_of(args[1]);
    if exc::pending() {
        return NONE;
    }
    loop {
        let mut item = NONE;
        let status = crate::iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let signal = match c_int_argument(item) {
            Ok(signal) => signal,
            Err(error) => return error,
        };
        if !valid_signal(signal) {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("signal number {signal} out of range [1; {}]", sigrtmax()),
            );
        }
        // SAFETY: `signal` was validated for this process.
        unsafe { libc::sigaddset(&mut set, signal) };
    }
    let mut previous: libc::sigset_t = unsafe { mem::zeroed() };
    // SAFETY: both sets are valid for the call; `pthread_sigmask` returns the
    // error number directly instead of setting `errno`.
    let result = unsafe { libc::pthread_sigmask(how, &set, &mut previous) };
    if result != 0 {
        return os_errno_error(result);
    }
    let mut signals = Vec::new();
    for signal in 1..nsig() {
        // SAFETY: `previous` was filled in by a successful call.
        if unsafe { libc::sigismember(&previous, signal) } == 1 {
            signals.push(encode_int(i64::from(signal)));
        }
    }
    value::set_new(signals)
}

/// `_signal.sigpending()`: the signals currently pending delivery to the
/// calling thread, as a `set` of numbers.
///
/// The kernel's pending set is the union of the process-directed and the
/// calling-thread-directed queues, which is exactly what CPython reports. As
/// in CPython, the numbers are plain ints; `signal.sigpending()` wraps them
/// into `Signals` members.
fn sigpending_call(args: &[i64]) -> i64 {
    if !args.is_empty() {
        return type_error(&format!(
            "_signal.sigpending() takes no arguments ({} given)",
            args.len()
        ));
    }
    let mut pending: libc::sigset_t = unsafe { mem::zeroed() };
    // SAFETY: `pending` is a valid output set owned by this frame.
    if unsafe { libc::sigpending(&mut pending) } != 0 {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL);
        return os_errno_error(code);
    }
    let mut signals = Vec::new();
    for signal in 1..nsig() {
        // SAFETY: `pending` was filled in by a successful call and `signal`
        // is in range for this process.
        if unsafe { libc::sigismember(&pending, signal) } == 1 {
            signals.push(encode_int(i64::from(signal)));
        }
    }
    value::set_new(signals)
}

/// `_signal.alarm(seconds)`.
fn alarm_call(args: &[i64]) -> i64 {
    if args.len() != 1 {
        return type_error(&format!(
            "_signal.alarm() takes exactly one argument ({} given)",
            args.len()
        ));
    }
    let seconds = match c_int_argument(args[0]) {
        Ok(seconds) => seconds,
        Err(error) => return error,
    };
    // SAFETY: `alarm` only reads the unsigned seconds value.
    let previous = unsafe { libc::alarm(seconds as u32) };
    encode_int(i64::from(previous))
}

/// `_signal.pause()`: blocks until a signal is delivered, then drains the
/// callbacks recorded by the trampoline.
fn pause_call(args: &[i64]) -> i64 {
    if !args.is_empty() {
        return type_error(&format!(
            "_signal.pause() takes no arguments ({} given)",
            args.len()
        ));
    }
    // Only handlers registered through `signal()` have a trampoline; a
    // default-action signal still terminates the process, as in CPython.
    // SAFETY: `pause` takes no arguments and returns `-1`/`EINTR` when a
    // signal arrives.
    unsafe { libc::pause() };
    dispatch_pending();
    NONE
}

/// Converts a timer duration, matching CPython's `PyTime_t` range checks.
fn timer_seconds(v: i64) -> Result<f64, i64> {
    let number = if let Some(number) = value::as_integer(v) {
        number as f64
    } else if value::type_name(v) == "int" {
        return Err(overflow_error("timestamp out of range for C PyTime_t"));
    } else if let Some(float) = value::as_float(v) {
        float
    } else {
        return Err(type_error(&format!(
            "'{}' object cannot be interpreted as an integer or float",
            value::type_name(v)
        )));
    };
    if number.is_nan() {
        return Err(value_error("Invalid value NaN (not a number)"));
    }
    // PyTime_t is a signed 64-bit nanosecond count.
    const MAX_TIMER_SECONDS: f64 = 9_223_372_036.854_775;
    if number.abs() > MAX_TIMER_SECONDS || number.is_nan() {
        return Err(overflow_error("timestamp out of range for C PyTime_t"));
    }
    Ok(number)
}

/// Rounds a duration to microseconds and builds a `timeval`.
fn timeval_of(seconds: f64) -> libc::timeval {
    let micros = (seconds * 1_000_000.0).round() as i64;
    libc::timeval {
        tv_sec: micros.div_euclid(1_000_000),
        tv_usec: micros.rem_euclid(1_000_000) as libc::suseconds_t,
    }
}

fn seconds_of(tv: libc::timeval) -> f64 {
    tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0
}

fn timer_tuple(timer: libc::itimerval) -> i64 {
    value::tuple_new(vec![
        value::float_new(seconds_of(timer.it_value)),
        value::float_new(seconds_of(timer.it_interval)),
    ])
}

/// `_signal.setitimer(which, seconds, interval=0.0)`.
fn setitimer_call(args: &[i64]) -> i64 {
    if args.len() < 2 {
        return type_error(&format!(
            "setitimer expected at least 2 arguments, got {}",
            args.len()
        ));
    }
    if args.len() > 3 {
        return type_error(&format!(
            "setitimer expected at most 3 arguments, got {}",
            args.len()
        ));
    }
    let which = match c_int_argument(args[0]) {
        Ok(which) => which,
        Err(error) => return error,
    };
    let seconds = match timer_seconds(args[1]) {
        Ok(seconds) => seconds,
        Err(error) => return error,
    };
    let interval = if let Some(&argument) = args.get(2) {
        match timer_seconds(argument) {
            Ok(interval) => interval,
            Err(error) => return error,
        }
    } else {
        0.0
    };
    let requested = libc::itimerval {
        it_interval: timeval_of(interval),
        it_value: timeval_of(seconds),
    };
    let mut previous: libc::itimerval = unsafe { mem::zeroed() };
    // SAFETY: `which` is a C int (the kernel validates it) and both structs
    // are fully initialised for the call.
    let result = unsafe { libc::setitimer(which, &requested, &mut previous) };
    if result != 0 {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL);
        return itimer_error(code);
    }
    timer_tuple(previous)
}

/// `_signal.getitimer(which)`.
fn getitimer_call(args: &[i64]) -> i64 {
    if args.len() != 1 {
        return type_error(&format!(
            "_signal.getitimer() takes exactly one argument ({} given)",
            args.len()
        ));
    }
    let which = match c_int_argument(args[0]) {
        Ok(which) => which,
        Err(error) => return error,
    };
    let mut current: libc::itimerval = unsafe { mem::zeroed() };
    // SAFETY: `which` is a C int and `current` is a valid output pointer.
    let result = unsafe { libc::getitimer(which, &mut current) };
    if result != 0 {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL);
        return itimer_error(code);
    }
    timer_tuple(current)
}

/// `_signal.siginterrupt(signalnum, flag)`: toggles `SA_RESTART` on the
/// current disposition.
fn siginterrupt_call(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return type_error(&format!(
            "siginterrupt expected 2 arguments, got {}",
            args.len()
        ));
    }
    let signal = match c_int_argument(args[0]) {
        Ok(signal) => signal,
        Err(error) => return error,
    };
    if !valid_signal(signal) {
        return value_error("signal number out of range");
    }
    let interrupt = value::truthy(args[1]);
    let mut action: libc::sigaction = unsafe { mem::zeroed() };
    // SAFETY: `action` is an output buffer for the current disposition.
    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) } != 0 {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL);
        return os_errno_error(code);
    }
    if interrupt {
        action.sa_flags &= !libc::SA_RESTART;
    } else {
        action.sa_flags |= libc::SA_RESTART;
    }
    // SAFETY: `action` was just read from the kernel for this signal.
    if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL);
        return os_errno_error(code);
    }
    NONE
}

/// `_signal.default_int_handler(signalnum, frame)`: always raises
/// `KeyboardInterrupt`.
fn default_int_handler_call(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return type_error(&format!(
            "default_int_handler expected 2 arguments, got {}",
            args.len()
        ));
    }
    exc::raise_with_message(KEYBOARD_INTERRUPT, "")
}

/// Dispatches a native `_signal` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "signal" => signal_call(args),
        "getsignal" => getsignal_call(args),
        "raise_signal" => raise_signal_call(args),
        "strsignal" => strsignal_call(args),
        "valid_signals" => valid_signals_call(args),
        "pthread_sigmask" => pthread_sigmask_call(args),
        "sigwait" => sigwait_call(args),
        "sigpending" => sigpending_call(args),
        "alarm" => alarm_call(args),
        "pause" => pause_call(args),
        "setitimer" => setitimer_call(args),
        "getitimer" => getitimer_call(args),
        "siginterrupt" => siginterrupt_call(args),
        "default_int_handler" => default_int_handler_call(args),
        _ => attribute_error(function),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable integer")
    }

    fn number(value_arg: i64) -> i64 {
        value::as_integer(value_arg).expect("integer result")
    }

    #[test]
    fn constant_table_matches_linux() {
        exc::init();
        assert_eq!(number(constant("SIGINT")), 2);
        assert_eq!(number(constant("SIGKILL")), 9);
        assert_eq!(number(constant("SIGIOT")), number(constant("SIGABRT")));
        assert_eq!(number(constant("SIGCLD")), number(constant("SIGCHLD")));
        assert_eq!(number(constant("SIGPOLL")), number(constant("SIGIO")));
        assert_eq!(number(constant("SIG_DFL")), 0);
        assert_eq!(number(constant("SIG_IGN")), 1);
        assert_eq!(number(constant("SIG_BLOCK")), 0);
        assert_eq!(number(constant("SIG_UNBLOCK")), 1);
        assert_eq!(number(constant("SIG_SETMASK")), 2);
        assert_eq!(number(constant("ITIMER_REAL")), 0);
        assert_eq!(number(constant("ITIMER_VIRTUAL")), 1);
        assert_eq!(number(constant("ITIMER_PROF")), 2);
        let max = number(constant("SIGRTMAX"));
        let min = number(constant("SIGRTMIN"));
        assert!(min <= max);
        assert_eq!(number(constant("NSIG")), max + 1);
        assert!(!exc::pending());

        let signals = call("valid_signals", &[]);
        assert!(!exc::pending());
        let items = value::as_set(signals).expect("set result");
        assert_eq!(items.len() as i64, max - 2);
        assert!(!items.contains(&int(9)));
        assert!(!items.contains(&int(19)));
        assert!(items.contains(&int(1)));
    }

    #[test]
    fn strsignal_and_signal_number_validation() {
        exc::init();
        let text = call("strsignal", &[int(2)]);
        assert!(!exc::pending());
        assert!(!value::as_str(text).expect("str result").is_empty());

        call("strsignal", &[int(0)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        call("strsignal", &[value::str_alloc("x".to_string())]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        call("getsignal", &[int(0)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        call("getsignal", &[int(i64::from(nsig()))]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        call("signal", &[int(0), int(SIG_DFL)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
    }

    #[test]
    fn handler_table_round_trips_and_raise_dispatches() {
        exc::init();
        let signal = 28; // SIGWINCH: harmless and unused by the runtime.

        assert_eq!(number(call("getsignal", &[int(signal)])), SIG_DFL);
        assert_eq!(
            number(call("signal", &[int(signal), int(SIG_IGN)])),
            SIG_DFL
        );
        assert_eq!(number(call("getsignal", &[int(signal)])), SIG_IGN);

        let callback = value::builtin_function_new(hydro_ir::Builtin::Len);
        assert_eq!(number(call("signal", &[int(signal), callback])), SIG_IGN);
        assert_eq!(call("getsignal", &[int(signal)]), callback);

        // `len` is called with two arguments, so dispatch leaves a TypeError:
        // proof that the registered callback ran synchronously.
        assert_eq!(call("raise_signal", &[int(signal)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(call("signal", &[int(signal), int(SIG_DFL)]), callback);
        assert_eq!(number(call("getsignal", &[int(signal)])), SIG_DFL);

        // SIGKILL cannot be caught: the kernel rejects the disposition and the
        // table is left untouched.
        call("signal", &[int(9), int(SIG_IGN)]);
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        exc::clear();
        assert_eq!(number(call("getsignal", &[int(9)])), SIG_DFL);
    }

    #[test]
    fn bad_handlers_and_arity_are_rejected() {
        exc::init();
        call("signal", &[int(1), int(2)]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        call("signal", &[int(1), value::bool_value(true)]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        call("signal", &[int(1)]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        call("getsignal", &[]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        call("pause", &[int(1)]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        call("valid_signals", &[int(1)]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn masks_timers_alarm_and_default_handler() {
        exc::init();

        let previous = call(
            "pthread_sigmask",
            &[int(i64::from(SIG_BLOCK)), value::list_new(Vec::new())],
        );
        assert!(!exc::pending());
        assert!(value::as_set(previous).is_some());

        call("pthread_sigmask", &[int(99), value::list_new(Vec::new())]);
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        exc::clear();
        call(
            "pthread_sigmask",
            &[int(i64::from(SIG_BLOCK)), value::list_new(vec![int(0)])],
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        // SIGWINCH is harmless (its default action is to ignore the signal).
        call(
            "pthread_sigmask",
            &[int(i64::from(SIG_BLOCK)), value::list_new(vec![int(28)])],
        );
        assert!(!exc::pending());
        call(
            "pthread_sigmask",
            &[int(i64::from(SIG_UNBLOCK)), value::list_new(vec![int(28)])],
        );
        assert!(!exc::pending());

        // An explicit zero interval is accepted and disables the timer.
        call("setitimer", &[int(i64::from(ITIMER_REAL)), int(0), int(0)]);
        assert!(!exc::pending());
        call(
            "setitimer",
            &[
                int(i64::from(ITIMER_REAL)),
                value::str_alloc("x".to_string()),
            ],
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        call("setitimer", &[int(99), int(0)]);
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        exc::clear();

        call("getitimer", &[int(-1)]);
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        exc::clear();
        let timer = call("getitimer", &[int(i64::from(ITIMER_REAL))]);
        assert!(!exc::pending());
        assert_eq!(value::as_tuple(timer).map(|items| items.len()), Some(2));

        assert!(number(call("alarm", &[int(0)])) >= 0);
        assert_eq!(call("raise_signal", &[int(0)]), NONE);
        assert!(!exc::pending());
        call("raise_signal", &[int(65)]);
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        exc::clear();

        call("default_int_handler", &[int(2), NONE]);
        assert_eq!(exc::pending_tag(), KEYBOARD_INTERRUPT as i64);
    }

    #[test]
    fn sigpending_reports_the_thread_pending_set() {
        exc::init();
        let usr1 = i64::from(libc::SIGUSR1);

        // A quiet test thread has nothing pending; the result is a real set,
        // not a list (the `signal` module converts its members to `Signals`).
        let pending = call("sigpending", &[]);
        assert!(!exc::pending());
        let items = value::as_set(pending).expect("set result");
        assert!(!items.contains(&int(usr1)));

        // Block SIGUSR1 in this thread and post it to this thread only, so no
        // other test thread can be terminated by its default action.
        let previous = call(
            "pthread_sigmask",
            &[int(i64::from(SIG_BLOCK)), value::list_new(vec![int(usr1)])],
        );
        assert!(!exc::pending());
        let restore: Vec<i64> = value::as_set(previous).expect("set result").to_vec();
        // SAFETY: `pthread_self` names the calling thread and the signal is
        // blocked, so the default action cannot run.
        let result = unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGUSR1) };
        assert_eq!(result, 0);

        let pending = call("sigpending", &[]);
        assert!(!exc::pending());
        let items = value::as_set(pending).expect("set result");
        assert!(items.contains(&int(usr1)));

        // Consume the pending signal without running its default action:
        // `sigwait` accepts a blocked signal and reports its number.
        let mut set: libc::sigset_t = unsafe { mem::zeroed() };
        // SAFETY: `set` is an output buffer owned by this frame.
        unsafe { libc::sigemptyset(&mut set) };
        unsafe { libc::sigaddset(&mut set, libc::SIGUSR1) };
        let mut received = 0;
        let result = unsafe { libc::sigwait(&set, &mut received) };
        assert_eq!(result, 0);
        assert_eq!(received, libc::SIGUSR1);

        let pending = call("sigpending", &[]);
        assert!(!exc::pending());
        let items = value::as_set(pending).expect("set result");
        assert!(!items.contains(&int(usr1)));

        call(
            "pthread_sigmask",
            &[int(i64::from(SIG_SETMASK)), value::list_new(restore)],
        );
        assert!(!exc::pending());

        call("sigpending", &[int(usr1)]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }
}
