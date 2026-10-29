//! `fcntl`: file descriptor locking and control (Unix only).
//!
//! `fcntl(fd, cmd[, arg])` and `ioctl(fd, request[, arg[, mutate_flag]])`
//! accept CPython's two argument forms:
//!
//! * an integer, passed as the C `int` argument (the return value of the call
//!   is returned as the Python result);
//! * a bytes-like object (`bytes`, `bytearray`, `memoryview`, and — as a
//!   convenience — `str`, encoded as UTF-8), copied into a mutable buffer
//!   whose address is passed to the C call and whose contents are returned as
//!   `bytes`.
//!
//! `flock` locks a whole file; `lockf` applies a POSIX record lock. Failures
//! raise `OSError` (syscall), `TypeError` (argument type), `ValueError`
//! (invalid `whence`) or `OverflowError` (integer out of range); no user
//! input can panic. On non-Unix platforms every function raises
//! `RuntimeError`.

use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR};
#[cfg(unix)]
use crate::value::{self, NONE};

/// A type error raised from an argument check.
fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Raises `OSError` for the current errno, without a `Hydro`-specific prefix
/// beyond the function name.
#[cfg(unix)]
fn raise_last_os_error(function: &str) -> i64 {
    let error = std::io::Error::last_os_error();
    let text = error.to_string();
    let detail = text.split(" (os error").next().unwrap_or(text.as_str());
    match error.raw_os_error() {
        Some(code) => {
            exc::raise_with_message(OS_ERROR, &format!("{function}: [Errno {code}] {detail}"))
        }
        None => exc::raise_with_message(OS_ERROR, &format!("{function}: {detail}")),
    }
}

/// Parses a required `int` argument that must fit in a C `int`.
#[cfg(unix)]
fn int32_arg(function: &str, args: &[i64], index: usize, name: &str) -> Option<libc::c_int> {
    let Some(&argument) = args.get(index) else {
        type_error(&format!(
            "{function}() missing required argument '{name}' (pos {})",
            index + 1
        ));
        return None;
    };
    match value::as_integer(argument) {
        Some(number) => match libc::c_int::try_from(number) {
            Ok(number) => Some(number),
            Err(_) => {
                exc::raise_with_message(
                    OVERFLOW_ERROR,
                    &format!("{function}(): argument '{name}' is out of range"),
                );
                None
            }
        },
        None if value::as_bigint(argument).is_some() => {
            exc::raise_with_message(
                OVERFLOW_ERROR,
                &format!("{function}(): argument '{name}' is out of range"),
            );
            None
        }
        None => {
            type_error(&format!(
                "{function}() argument '{name}' must be an integer, not {}",
                value::type_name(argument)
            ));
            None
        }
    }
}

/// Copies a bytes-like value (or `str`) into an owned byte buffer.
#[cfg(unix)]
fn buffer_value(argument: i64) -> Option<Vec<u8>> {
    if let Some(text) = value::as_str(argument) {
        return Some(text.as_bytes().to_vec());
    }
    if let Some(bytes) = value::as_bytes(argument) {
        return Some(bytes.to_vec());
    }
    if let Some(bytes) = value::as_bytearray(argument) {
        return Some(bytes.clone());
    }
    let state = value::as_memoryview(argument)?;
    value::memoryview_bytes(state).ok()
}

/// Returns the `bytes` a buffer-form call produces, or raises a `TypeError`
/// describing the offending argument.
#[cfg(unix)]
fn buffer_or_type_error(function: &str, argument: i64) -> Option<Vec<u8>> {
    match buffer_value(argument) {
        Some(buffer) => Some(buffer),
        None => {
            type_error(&format!(
                "{function}() argument must be int or a bytes-like object, not {}",
                value::type_name(argument)
            ));
            None
        }
    }
}

/// `fcntl.fcntl(fd, cmd[, arg])`.
#[cfg(unix)]
fn fcntl_call(args: &[i64]) -> i64 {
    if args.len() < 2 {
        return type_error("fcntl() missing required arguments 'fd' and 'cmd'");
    }
    if args.len() > 3 {
        return type_error(&format!(
            "fcntl() takes at most 3 arguments ({} given)",
            args.len()
        ));
    }
    let Some(fd) = int32_arg("fcntl", args, 0, "fd") else {
        return NONE;
    };
    let Some(cmd) = int32_arg("fcntl", args, 1, "cmd") else {
        return NONE;
    };
    let Some(&argument) = args.get(2) else {
        // SAFETY: `fcntl` with two arguments is the variadic "no argument"
        // form, valid for commands such as `F_GETFD`.
        let result = unsafe { libc::fcntl(fd, cmd) };
        return if result < 0 {
            raise_last_os_error("fcntl")
        } else {
            value::encode_int(i64::from(result)).unwrap_or(NONE)
        };
    };
    if let Some(number) = value::as_integer(argument) {
        let Ok(raw) = libc::c_int::try_from(number) else {
            return exc::raise_with_message(OVERFLOW_ERROR, "fcntl(): argument 3 is out of range");
        };
        // SAFETY: a variadic call with one `int` argument.
        let result = unsafe { libc::fcntl(fd, cmd, raw) };
        return if result < 0 {
            raise_last_os_error("fcntl")
        } else {
            value::encode_int(i64::from(result)).unwrap_or(NONE)
        };
    }
    if value::as_bigint(argument).is_some() {
        return exc::raise_with_message(OVERFLOW_ERROR, "fcntl(): argument 3 is out of range");
    }
    let Some(mut buffer) = buffer_or_type_error("fcntl", argument) else {
        return NONE;
    };
    let length = buffer.len();
    if buffer.is_empty() {
        buffer.push(0);
    }
    // SAFETY: `buffer` is a mutable allocation of at least one byte and the
    // kernel writes no more than the command's own structure (the caller
    // sized the buffer for it, exactly as CPython documents).
    let result = unsafe { libc::fcntl(fd, cmd, buffer.as_mut_ptr() as *mut libc::c_void) };
    if result < 0 {
        return raise_last_os_error("fcntl");
    }
    value::bytes_new(buffer[..length].to_vec())
}

/// `fcntl.ioctl(fd, request[, arg[, mutate_flag]])`.
#[cfg(unix)]
fn ioctl_call(args: &[i64]) -> i64 {
    if args.len() < 2 {
        return type_error("ioctl() missing required arguments 'fd' and 'request'");
    }
    if args.len() > 4 {
        return type_error(&format!(
            "ioctl() takes at most 4 arguments ({} given)",
            args.len()
        ));
    }
    let Some(fd) = int32_arg("ioctl", args, 0, "fd") else {
        return NONE;
    };
    let Some(&request_value) = args.get(1) else {
        return type_error("ioctl() missing required argument 'request' (pos 2)");
    };
    let Some(request) = value::as_integer(request_value) else {
        return type_error(&format!(
            "ioctl() argument 'request' must be an integer, not {}",
            value::type_name(request_value)
        ));
    };
    // CPython passes the request through its unsigned 32-bit form, so the
    // low 32 bits are kept regardless of the Python integer's sign.
    let request32 = request as u32;
    let Some(&argument) = args.get(2) else {
        // SAFETY: the no-argument form passes the null pointer CPython uses
        // for the variadic slot.
        let result =
            unsafe { libc::ioctl(fd, request32 as _, std::ptr::null_mut::<libc::c_void>()) };
        return ioctl_int_result(result);
    };
    if let Some(number) = value::as_integer(argument) {
        // SAFETY: the C call receives the integer widened to `c_int`.
        let result = unsafe { libc::ioctl(fd, request32 as _, number as libc::c_int) };
        return ioctl_int_result(result);
    }
    if value::as_bigint(argument).is_some() {
        return exc::raise_with_message(OVERFLOW_ERROR, "ioctl(): argument 3 is out of range");
    }
    let Some(mut buffer) = buffer_or_type_error("ioctl", argument) else {
        return NONE;
    };
    let length = buffer.len();
    if buffer.is_empty() {
        buffer.push(0);
    }
    // SAFETY: `buffer` is a mutable allocation of at least one byte; the
    // request's structure lives inside the caller-provided length.
    let result =
        unsafe { libc::ioctl(fd, request32 as _, buffer.as_mut_ptr() as *mut libc::c_void) };
    if result < 0 {
        return raise_last_os_error("ioctl");
    }
    value::bytes_new(buffer[..length].to_vec())
}

/// Shared result handling for the integer forms of `ioctl`.
#[cfg(unix)]
fn ioctl_int_result(result: libc::c_int) -> i64 {
    if result < 0 {
        raise_last_os_error("ioctl")
    } else {
        value::encode_int(i64::from(result)).unwrap_or(NONE)
    }
}

/// `fcntl.flock(fd, operation)`.
#[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
fn flock_call(args: &[i64]) -> i64 {
    if args.len() < 2 {
        return type_error("flock() missing required arguments 'fd' and 'operation'");
    }
    if args.len() > 2 {
        return type_error(&format!(
            "flock() takes exactly 2 arguments ({} given)",
            args.len()
        ));
    }
    let Some(fd) = int32_arg("flock", args, 0, "fd") else {
        return NONE;
    };
    let Some(operation) = int32_arg("flock", args, 1, "operation") else {
        return NONE;
    };
    // SAFETY: `flock` only reads the descriptor and the operation bits.
    let result = unsafe { libc::flock(fd, operation) };
    if result < 0 {
        return raise_last_os_error("flock");
    }
    NONE
}

/// Solaris has no `flock(2)`.
#[cfg(any(target_os = "solaris", target_os = "illumos"))]
fn flock_call(_args: &[i64]) -> i64 {
    exc::raise_with_message(
        exc::RUNTIME_ERROR,
        "flock() is not supported on this platform",
    )
}

/// `fcntl.lockf(fd, cmd, len=0, start=0, whence=0)`.
///
/// Like CPython, `start` is validated but `lockf(3)` itself has no offset
/// parameter, so only the descriptor, command and length reach the kernel.
#[cfg(unix)]
fn lockf_call(args: &[i64]) -> i64 {
    if args.len() < 2 {
        return type_error("lockf() missing required arguments 'fd' and 'cmd'");
    }
    if args.len() > 5 {
        return type_error(&format!(
            "lockf() takes at most 5 arguments ({} given)",
            args.len()
        ));
    }
    let Some(fd) = int32_arg("lockf", args, 0, "fd") else {
        return NONE;
    };
    let Some(cmd) = int32_arg("lockf", args, 1, "cmd") else {
        return NONE;
    };
    let length = match args.get(2).copied() {
        None => 0,
        Some(value) => match value::as_integer(value) {
            Some(number) if number >= 0 => number,
            Some(_) => {
                return exc::raise_with_message(OVERFLOW_ERROR, "lockf(): len must be non-negative")
            }
            None => return type_error("lockf() argument 'len' must be an integer"),
        },
    };
    match args.get(3).copied() {
        None => {}
        Some(value) => match value::as_integer(value) {
            Some(number) if number >= 0 => {}
            Some(_) => {
                return exc::raise_with_message(
                    OVERFLOW_ERROR,
                    "lockf(): start must be non-negative",
                )
            }
            None => return type_error("lockf() argument 'start' must be an integer"),
        },
    }
    let whence = match args.get(4).copied() {
        None => 0,
        Some(value) => match value::as_integer(value) {
            Some(number) => number,
            None => return type_error("lockf() argument 'whence' must be an integer"),
        },
    };
    if !matches!(whence, 0..=2) {
        return exc::raise_with_message(VALUE_ERROR, "invalid whence");
    }
    // `off_t` is 64-bit on LP64 targets and 32-bit on ILP32 ones; compare
    // against its maximum in `u64` so an oversized `len` still raises instead
    // of silently truncating. `len` is non-negative here, and going through
    // `u64` keeps the check meaningful at both widths without an irrefutable
    // `TryFrom` pattern or an always-false `i64` comparison.
    if length as u64 > libc::off_t::MAX as u64 {
        return exc::raise_with_message(OVERFLOW_ERROR, "lockf(): len is out of range");
    }
    let length = length as libc::off_t;
    // SAFETY: `lockf` only reads the descriptor, command and length.
    let result = unsafe { libc::lockf(fd, cmd, length) };
    if result < 0 {
        return raise_last_os_error("lockf");
    }
    NONE
}

#[cfg(unix)]
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "fcntl" => fcntl_call(args),
        "ioctl" => ioctl_call(args),
        "flock" => flock_call(args),
        "lockf" => lockf_call(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'fcntl' has no attribute '{function}'"),
        ),
    }
}

#[cfg(not(unix))]
pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
    exc::raise_with_message(
        exc::RUNTIME_ERROR,
        &format!("fcntl.{function} is not available on this platform"),
    )
}

/// Module integer constants: the `F_*` commands, `LOCK_*` operations,
/// `FD_CLOEXEC`, and the `I_*` STREAMS commands where the platform has them.
#[cfg(unix)]
pub(crate) fn constant(name: &str) -> Option<i64> {
    let raw: i64 = match name {
        "F_DUPFD" => libc::F_DUPFD as i64,
        "F_GETFD" => libc::F_GETFD as i64,
        "F_SETFD" => libc::F_SETFD as i64,
        "F_GETFL" => libc::F_GETFL as i64,
        "F_SETFL" => libc::F_SETFL as i64,
        "F_GETLK" => libc::F_GETLK as i64,
        "F_SETLK" => libc::F_SETLK as i64,
        "F_SETLKW" => libc::F_SETLKW as i64,
        "F_GETOWN" => libc::F_GETOWN as i64,
        "F_SETOWN" => libc::F_SETOWN as i64,
        // Linux-specific commands, whose values are stable in the kernel ABI.
        // libc does not declare all of them on every target.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "F_GETSIG" => 11,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "F_SETSIG" => 10,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "F_SETLEASE" => 1024,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "F_GETLEASE" => 1025,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "F_NOTIFY" => 1026,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "F_DUPFD_CLOEXEC" => 1030,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "F_SETPIPE_SZ" => 1031,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "F_GETPIPE_SZ" => 1032,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "F_ADD_SEALS" => 1033,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "F_GET_SEALS" => 1034,
        // `F_DUPFD_CLOEXEC` also exists on the BSDs and macOS.
        #[cfg(any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        "F_DUPFD_CLOEXEC" => libc::F_DUPFD_CLOEXEC as i64,
        // STREAMS commands exist only in the Solaris-family libc, where `pty`
        // pushes the `ptem`/`ldterm` modules after opening a terminal by hand.
        // Linux has no STREAMS, so these names stay absent there exactly as in
        // CPython's `fcntl`; the runtime does not invent values for them.
        #[cfg(any(target_os = "solaris", target_os = "illumos"))]
        "I_PUSH" => libc::I_PUSH as i64,
        #[cfg(any(target_os = "solaris", target_os = "illumos"))]
        "I_POP" => libc::I_POP as i64,
        #[cfg(any(target_os = "solaris", target_os = "illumos"))]
        "I_LOOK" => libc::I_LOOK as i64,
        #[cfg(any(target_os = "solaris", target_os = "illumos"))]
        "I_FLUSH" => libc::I_FLUSH as i64,
        // Portable values: `fdflag` and `flock(2)` operation bits.
        "FD_CLOEXEC" => 1,
        "LOCK_SH" => 1,
        "LOCK_EX" => 2,
        "LOCK_NB" => 4,
        "LOCK_UN" => 8,
        _ => return None,
    };
    Some(value::encode_int(raw).unwrap_or_else(|| value::bigint_new(raw.into())))
}

#[cfg(not(unix))]
pub(crate) fn constant(_name: &str) -> Option<i64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{encode_int, str_alloc, NONE};

    fn int(n: i64) -> i64 {
        encode_int(n).expect("small integer")
    }

    fn text(item: &str) -> i64 {
        str_alloc(item.to_string())
    }

    fn message() -> String {
        value::to_string(exc::pending_value())
    }

    #[cfg(unix)]
    fn dev_null() -> std::fs::File {
        std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .expect("/dev/null")
    }

    #[cfg(unix)]
    fn scratch_file() -> (std::path::PathBuf, std::fs::File) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "hydro-fcntl-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("scratch file");
        (path, file)
    }

    #[cfg(unix)]
    #[test]
    fn constants_match_the_platform_headers() {
        assert_eq!(constant("F_GETFD"), Some(int(i64::from(libc::F_GETFD))));
        assert_eq!(constant("F_SETFL"), Some(int(i64::from(libc::F_SETFL))));
        assert_eq!(constant("FD_CLOEXEC"), Some(int(1)));
        assert_eq!(constant("LOCK_EX"), Some(int(2)));
        assert_eq!(constant("LOCK_NB"), Some(int(4)));
        assert_eq!(constant("NOPE"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn i_push_is_absent_without_streams() {
        // Linux has no STREAMS and CPython's `fcntl` exposes no `I_PUSH`
        // there, so `pty` never reaches the guarded push and the module must
        // not invent a value.
        assert_eq!(constant("I_PUSH"), None);
        assert_eq!(constant("I_POP"), None);
        assert_eq!(constant("NOPE"), None);
    }

    #[cfg(any(target_os = "solaris", target_os = "illumos"))]
    #[test]
    fn i_push_matches_libc_where_streams_exist() {
        assert_eq!(constant("I_PUSH"), Some(int(i64::from(libc::I_PUSH))));
        assert_eq!(constant("I_POP"), Some(int(i64::from(libc::I_POP))));
    }

    #[cfg(unix)]
    #[test]
    fn fcntl_reads_and_sets_descriptor_flags() {
        use std::os::unix::io::AsRawFd;
        exc::init();
        let file = dev_null();
        let fd = file.as_raw_fd();
        let getfd = constant("F_GETFD").expect("F_GETFD");
        let setfd = constant("F_SETFD").expect("F_SETFD");
        let flags = call("fcntl", &[int(i64::from(fd)), getfd]);
        assert!(!exc::pending(), "F_GETFD failed: {}", message());
        value::as_integer(flags).expect("integer flags");

        // Clear `FD_CLOEXEC`, then set it again and observe the bit.
        let result = call("fcntl", &[int(i64::from(fd)), setfd, int(0)]);
        assert!(!exc::pending(), "F_SETFD failed: {}", message());
        assert_eq!(result, int(0));
        let flags = call("fcntl", &[int(i64::from(fd)), getfd]);
        assert!(!exc::pending());
        assert_eq!(value::as_integer(flags).expect("integer flags") & 1, 0);

        let result = call(
            "fcntl",
            &[int(i64::from(fd)), setfd, int(i64::from(libc::FD_CLOEXEC))],
        );
        assert!(!exc::pending(), "F_SETFD failed: {}", message());
        assert_eq!(result, int(0));

        let flags = call("fcntl", &[int(i64::from(fd)), getfd]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(flags).expect("integer flags") & 1,
            i64::from(libc::FD_CLOEXEC) & 1
        );

        // A bytes-like third argument is copied and returned as `bytes`.
        let returned = call("fcntl", &[int(i64::from(fd)), getfd, text("x")]);
        assert!(!exc::pending(), "buffer form failed: {}", message());
        assert_eq!(value::as_bytes(returned), Some(&b"x"[..]));

        let returned = call(
            "fcntl",
            &[int(i64::from(fd)), getfd, value::bytes_from_slice(b"ab")],
        );
        assert!(!exc::pending());
        assert_eq!(value::as_bytes(returned), Some(&b"ab"[..]));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn ioctl_buffer_form_fills_the_buffer() {
        exc::init();
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `pipe` fills both descriptor slots.
        let created = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(created, 0, "pipe failed");
        let payload = b"abc";
        // SAFETY: the write descriptor and payload are valid.
        let written = unsafe {
            libc::write(
                fds[1],
                payload.as_ptr() as *const libc::c_void,
                payload.len(),
            )
        };
        assert_eq!(written as usize, payload.len());

        let request = i64::from(libc::FIONREAD as u32);
        let buffer = value::bytes_from_slice(&[0u8; 4]);
        let returned = call("ioctl", &[int(i64::from(fds[0])), int(request), buffer]);
        assert!(!exc::pending(), "ioctl failed: {}", message());
        let returned = value::as_bytes(returned).expect("bytes result");
        assert_eq!(returned.len(), 4);
        let available = i32::from_ne_bytes([returned[0], returned[1], returned[2], returned[3]]);
        assert_eq!(available, payload.len() as i32);

        // SAFETY: both descriptors were created above and are closed once.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    #[test]
    fn flock_locks_and_unlocks_a_file() {
        use std::os::unix::io::AsRawFd;
        exc::init();
        let (path, file) = scratch_file();
        let fd = int(i64::from(file.as_raw_fd()));
        let operation = int(1 | 4); // LOCK_EX | LOCK_NB
        assert_eq!(call("flock", &[fd, operation]), NONE);
        assert!(!exc::pending(), "flock failed: {}", message());
        assert_eq!(call("flock", &[fd, int(8)]), NONE);
        assert!(!exc::pending());
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn lockf_locks_and_unlocks_a_file() {
        use std::os::unix::io::AsRawFd;
        exc::init();
        let (path, file) = scratch_file();
        let fd = int(i64::from(file.as_raw_fd()));
        assert_eq!(call("lockf", &[fd, int(2), int(0)]), NONE);
        assert!(!exc::pending(), "lockf failed: {}", message());
        assert_eq!(call("lockf", &[fd, int(0), int(0)]), NONE);
        assert!(!exc::pending());
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn argument_validation_raises_documented_errors() {
        exc::init();
        assert_eq!(call("fcntl", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("fcntl", &[text("fd"), int(1)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        // A closed descriptor reports OSError.
        let closed = -1;
        let getfd = constant("F_GETFD").expect("F_GETFD");
        assert_eq!(call("fcntl", &[int(closed), getfd]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(OS_ERROR));
        exc::init();

        // A terminal query on a non-terminal descriptor is an errno failure.
        use std::os::unix::io::AsRawFd;
        let null = dev_null();
        let winsize = i64::from(libc::TIOCGWINSZ as u32);
        assert_eq!(
            call("ioctl", &[int(i64::from(null.as_raw_fd())), int(winsize)]),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(OS_ERROR));
        exc::init();

        assert_eq!(call("flock", &[int(0)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(
            call("lockf", &[int(0), int(2), int(0), int(0), int(7)]),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        assert_eq!(message(), "invalid whence");
        exc::init();

        assert_eq!(call("lockf", &[int(0), int(2), int(-1)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(OVERFLOW_ERROR));
        exc::init();

        assert_eq!(call("lockf", &[int(0), text("lock")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("nope", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));
        exc::init();
    }
}
