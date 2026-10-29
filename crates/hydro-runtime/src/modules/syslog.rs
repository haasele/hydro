//! `syslog`: the Unix system logger (Unix only).
//!
//! `openlog`/`syslog`/`closelog`/`setlogmask` wrap the C library logger.
//! `syslog` accepts CPython's two spellings:
//!
//! * `syslog(message)` logs at `LOG_INFO`;
//! * `syslog(priority, message)` logs at the given priority.
//!
//! The message is always passed through a `"%s"` format so a `%` in user text
//! cannot be reinterpreted as a printf directive. `openlog`'s identifier is
//! copied into a process-wide `CString` that stays alive for the lifetime of
//! the logger, because libc keeps the pointer.
//!
//! Argument errors raise `TypeError`/`ValueError`/`OverflowError`; the logger
//! itself has no error channel, so the calls return `None` after invoking
//! libc. No user input can panic. On non-Unix platforms every function raises
//! `RuntimeError`.

use std::ffi::CString;
use std::sync::{Mutex, MutexGuard};

use crate::exc::{self, ATTRIBUTE_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

/// `(name, value)` pairs of the `LOG_*` constants.
///
/// The priority, facility and option values are fixed by POSIX/SUS and equal
/// on every Unix this runtime supports, so they are spelled numerically
/// rather than pulled from platform-specific libc bindings.
const CONSTANTS: &[(&str, i64)] = &[
    // Priorities.
    ("LOG_EMERG", 0),
    ("LOG_ALERT", 1),
    ("LOG_CRIT", 2),
    ("LOG_ERR", 3),
    ("LOG_WARNING", 4),
    ("LOG_NOTICE", 5),
    ("LOG_INFO", 6),
    ("LOG_DEBUG", 7),
    // Facilities.
    ("LOG_KERN", 0),
    ("LOG_USER", 1 << 3),
    ("LOG_MAIL", 2 << 3),
    ("LOG_DAEMON", 3 << 3),
    ("LOG_AUTH", 4 << 3),
    ("LOG_SYSLOG", 5 << 3),
    ("LOG_LPR", 6 << 3),
    ("LOG_NEWS", 7 << 3),
    ("LOG_UUCP", 8 << 3),
    ("LOG_CRON", 9 << 3),
    ("LOG_AUTHPRIV", 10 << 3),
    ("LOG_FTP", 11 << 3),
    ("LOG_LOCAL0", 16 << 3),
    ("LOG_LOCAL1", 17 << 3),
    ("LOG_LOCAL2", 18 << 3),
    ("LOG_LOCAL3", 19 << 3),
    ("LOG_LOCAL4", 20 << 3),
    ("LOG_LOCAL5", 21 << 3),
    ("LOG_LOCAL6", 22 << 3),
    ("LOG_LOCAL7", 23 << 3),
    // `openlog` options.
    ("LOG_PID", 0x01),
    ("LOG_CONS", 0x02),
    ("LOG_ODELAY", 0x04),
    ("LOG_NDELAY", 0x08),
    ("LOG_NOWAIT", 0x10),
    ("LOG_PERROR", 0x20),
];

/// The identifier last passed to `openlog`, kept alive for libc.
#[cfg(unix)]
static IDENT: Mutex<Option<CString>> = Mutex::new(None);

/// `LOG_USER`, the default facility of `openlog`/`syslog`.
const LOG_USER: libc::c_int = 1 << 3;

/// `LOG_INFO`, the default priority of a one-argument `syslog`.
const LOG_INFO: libc::c_int = 6;

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

#[cfg(unix)]
fn ident_lock() -> MutexGuard<'static, Option<CString>> {
    IDENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Parses an optional integer argument, raising on a wrong type and
/// defaulting to `fallback`.
#[cfg(unix)]
fn optional_int(
    function: &str,
    args: &[i64],
    index: usize,
    fallback: libc::c_int,
) -> Option<libc::c_int> {
    match args.get(index) {
        None => Some(fallback),
        Some(&argument) => match value::as_integer(argument) {
            Some(number) => match libc::c_int::try_from(number) {
                Ok(number) => Some(number),
                Err(_) => {
                    exc::raise_with_message(
                        OVERFLOW_ERROR,
                        &format!("{function}(): argument {} is out of range", index + 1),
                    );
                    None
                }
            },
            None => {
                type_error(&format!(
                    "{function}() argument {} must be an integer, not {}",
                    index + 1,
                    value::type_name(argument)
                ));
                None
            }
        },
    }
}

/// `syslog.openlog(ident=None, logoption=0, facility=LOG_USER)`.
#[cfg(unix)]
fn openlog(args: &[i64]) -> i64 {
    if args.len() > 3 {
        return type_error(&format!(
            "openlog() takes at most 3 arguments ({} given)",
            args.len()
        ));
    }
    let ident = match args.first().copied() {
        None | Some(NONE) => None,
        Some(argument) => match value::as_str(argument) {
            Some(text) => match CString::new(text) {
                Ok(text) => Some(text),
                Err(_) => return exc::raise_with_message(VALUE_ERROR, "embedded null character"),
            },
            None => {
                return type_error(&format!(
                    "openlog() argument 'ident' must be str or None, not {}",
                    value::type_name(argument)
                ))
            }
        },
    };
    let Some(logoption) = optional_int("openlog", args, 1, 0) else {
        return NONE;
    };
    let Some(facility) = optional_int("openlog", args, 2, LOG_USER) else {
        return NONE;
    };
    // Replace the stored identifier before calling libc; the guard keeps the
    // buffer alive and unmodified while the C library retains the pointer.
    let mut stored = ident_lock();
    *stored = ident;
    let pointer = stored
        .as_ref()
        .map_or(std::ptr::null(), |text| text.as_ptr());
    // SAFETY: `pointer` is null or points into the live `CString`; libc copies
    // what it needs for the duration of the call.
    unsafe { libc::openlog(pointer, logoption, facility) };
    NONE
}

/// `syslog.syslog([priority, ]message)`.
#[cfg(unix)]
fn syslog(args: &[i64]) -> i64 {
    if args.is_empty() {
        return type_error("syslog() missing required argument 'message'");
    }
    if args.len() > 2 {
        return type_error(&format!(
            "syslog() takes at most 2 arguments ({} given)",
            args.len()
        ));
    }
    let (priority, message) = if args.len() == 1 {
        (LOG_INFO, args[0])
    } else {
        let Some(priority) = value::as_integer(args[0]) else {
            return type_error(&format!(
                "syslog() argument 'priority' must be an integer, not {}",
                value::type_name(args[0])
            ));
        };
        let Ok(priority) = libc::c_int::try_from(priority) else {
            return exc::raise_with_message(OVERFLOW_ERROR, "syslog(): priority is out of range");
        };
        (priority, args[1])
    };
    let Some(text) = value::as_str(message) else {
        return type_error(&format!(
            "syslog() argument 'message' must be str, not {}",
            value::type_name(message)
        ));
    };
    let Ok(text) = CString::new(text) else {
        return exc::raise_with_message(VALUE_ERROR, "embedded null character");
    };
    // SAFETY: `"%s"` is a valid format string and `text` a valid
    // NUL-terminated argument, so user text is never parsed as a directive.
    unsafe { libc::syslog(priority, c"%s".as_ptr(), text.as_ptr()) };
    NONE
}

/// `syslog.closelog()`.
#[cfg(unix)]
fn closelog(args: &[i64]) -> i64 {
    if !args.is_empty() {
        return type_error(&format!(
            "closelog() takes no arguments ({} given)",
            args.len()
        ));
    }
    // SAFETY: `closelog` has no arguments and no failure mode.
    unsafe { libc::closelog() };
    NONE
}

/// `syslog.setlogmask(mask)`: installs a priority mask, returning the previous
/// one.
#[cfg(unix)]
fn setlogmask(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "setlogmask() takes exactly 1 argument ({} given)",
            args.len()
        ));
    }
    let Some(&mask) = args.first() else {
        return type_error("setlogmask() missing required argument 'mask' (pos 1)");
    };
    let Some(mask) = value::as_integer(mask) else {
        return type_error(&format!(
            "setlogmask() argument 'mask' must be an integer, not {}",
            value::type_name(mask)
        ));
    };
    let Ok(mask) = libc::c_int::try_from(mask) else {
        return exc::raise_with_message(OVERFLOW_ERROR, "setlogmask(): mask is out of range");
    };
    // SAFETY: `setlogmask` only stores the integer mask.
    let previous = unsafe { libc::setlogmask(mask) };
    // SAFETY: the previous mask always fits a Python integer.
    value::encode_int(i64::from(previous)).unwrap_or(NONE)
}

#[cfg(unix)]
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "openlog" => openlog(args),
        "syslog" => syslog(args),
        "closelog" => closelog(args),
        "setlogmask" => setlogmask(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'syslog' has no attribute '{function}'"),
        ),
    }
}

#[cfg(not(unix))]
pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
    exc::raise_with_message(
        exc::RUNTIME_ERROR,
        &format!("syslog.{function} is not available on this platform"),
    )
}

/// Module integer constants: `LOG_*` priorities, facilities and options.
pub(crate) fn constant(name: &str) -> Option<i64> {
    let &(_, raw) = CONSTANTS.iter().find(|(key, _)| *key == name)?;
    Some(value::encode_int(raw).unwrap_or_else(|| value::bigint_new(raw.into())))
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

    #[test]
    fn constants_match_cpython() {
        assert_eq!(constant("LOG_EMERG"), Some(int(0)));
        assert_eq!(constant("LOG_DEBUG"), Some(int(7)));
        assert_eq!(constant("LOG_INFO"), Some(int(6)));
        assert_eq!(constant("LOG_USER"), Some(int(8)));
        assert_eq!(constant("LOG_LOCAL7"), Some(int(184)));
        assert_eq!(constant("LOG_PID"), Some(int(1)));
        assert_eq!(constant("LOG_PERROR"), Some(int(32)));
        assert_eq!(constant("NOT_A_FLAG"), None);
    }

    #[cfg(unix)]
    #[test]
    fn logging_round_trip_does_not_raise() {
        exc::init();
        let pid = constant("LOG_PID").expect("LOG_PID");
        let user = constant("LOG_USER").expect("LOG_USER");
        let info = constant("LOG_INFO").expect("LOG_INFO");

        assert_eq!(call("openlog", &[NONE, int(pid), int(user)]), NONE);
        assert!(!exc::pending(), "openlog failed: {}", message());
        assert_eq!(call("syslog", &[text("hydro syslog test")]), NONE);
        assert!(!exc::pending(), "syslog failed: {}", message());
        assert_eq!(
            call("syslog", &[int(info), text("hydro syslog test 100%")]),
            NONE
        );
        assert!(!exc::pending(), "syslog failed: {}", message());

        // `setlogmask()` returns the previous mask; save and restore the
        // process mask so the test leaves no trace.
        let previous = call("setlogmask", &[int(0xff)]);
        assert!(!exc::pending(), "setlogmask failed: {}", message());
        let previous = value::as_integer(previous).expect("previous mask");
        // The mask installed just above was `0xff`, so restoring returns it.
        let restored = call("setlogmask", &[int(previous)]);
        assert!(!exc::pending(), "setlogmask failed: {}", message());
        assert_eq!(value::as_integer(restored), Some(0xff));

        assert_eq!(call("closelog", &[]), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn argument_validation_raises_documented_errors() {
        exc::init();
        assert_eq!(call("openlog", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("openlog", &[NONE, text("pid")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("openlog", &[NONE, int(0), int(0), int(0)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("syslog", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("syslog", &[int(6), int(7)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        // With two arguments the first must be the integer priority.
        assert_eq!(call("syslog", &[text("message"), text("x")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("syslog", &[text("a"), text("b"), text("c")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("setlogmask", &[text("all")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("closelog", &[int(0)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("system", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));
        exc::init();
    }
}
