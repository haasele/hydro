//! `resource`: process resource limits and usage (Unix only).
//!
//! `getrlimit`/`setrlimit` wrap `getrlimit(2)`/`setrlimit(2)`, `getrusage`
//! wraps `getrusage(2)`, and `getpagesize` reports the virtual memory page
//! size. Limits use CPython's representation: an unlimited value is `-1`
//! (aliased by `RLIM_INFINITY`), so a limit tuple round-trips through
//! `getrlimit`/`setrlimit`.
//!
//! `getrusage` returns a `struct_rusage`-shaped namespace whose `ru_utime`
//! and `ru_stime` are floats in seconds and whose remaining fields are the
//! raw kernel counters, in CPython's field order.
//!
//! Failure paths raise `TypeError`/`ValueError`/`OverflowError` for bad
//! arguments and `OSError` for syscall failures; no user input can panic.
//! On non-Unix platforms every function raises `RuntimeError`.

use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR};
#[cfg(unix)]
use crate::heap;
#[cfg(unix)]
use crate::value::{self, NONE};

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Encodes an integer, promoting past the tagged range.
#[cfg(unix)]
fn int_value(number: i64) -> i64 {
    value::encode_int(number).unwrap_or_else(|| value::bigint_new(number.into()))
}

/// Appends one field to a rooted namespace, keeping the value alive across
/// the mutation.
#[cfg(unix)]
fn record_field(namespace: i64, name: &str, field_value: i64) {
    let roots = heap::root_values(&[field_value]);
    if let Some(mut state) = value::namespace_pairs_mut(namespace) {
        state.pairs.push((name.to_string(), field_value));
    }
    heap::write_barrier_edge(namespace, field_value);
    heap::release_roots(roots);
}

/// Raises `OSError` for the current errno.
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

/// Extracts a required integer argument for `function`.
#[cfg(unix)]
fn int_arg(function: &str, args: &[i64], index: usize, name: &str) -> Option<i64> {
    match args.get(index) {
        Some(&value) => match value::as_integer(value) {
            Some(number) => Some(number),
            None => {
                type_error(&format!(
                    "{function}() argument '{name}' must be an integer, not {}",
                    value::type_name(value)
                ));
                None
            }
        },
        None => {
            type_error(&format!(
                "{function}() missing required argument '{name}' (pos {})",
                index + 1
            ));
            None
        }
    }
}

/// `resource.getrlimit(resource)`: the `(soft, hard)` pair.
#[cfg(unix)]
fn getrlimit(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "getrlimit() takes exactly 1 argument ({} given)",
            args.len()
        ));
    }
    let Some(resource) = int_arg("getrlimit", args, 0, "resource") else {
        return NONE;
    };
    let Ok(resource) = libc::c_int::try_from(resource) else {
        return exc::raise_with_message(OVERFLOW_ERROR, "getrlimit(): resource is out of range");
    };
    let mut limits: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: `limits` is a valid `struct rlimit` the kernel fills in.
    let result = unsafe { libc::getrlimit(resource as _, &mut limits) };
    if result != 0 {
        return raise_last_os_error("getrlimit");
    }
    let soft = rlim_to_value(limits.rlim_cur);
    let hard = rlim_to_value(limits.rlim_max);
    value::tuple_new(vec![int_value(soft), int_value(hard)])
}

/// Maps a raw `rlim_t` to CPython's limit representation (`-1` = unlimited).
#[cfg(unix)]
fn rlim_to_value(limit: libc::rlim_t) -> i64 {
    if limit == libc::RLIM_INFINITY {
        -1
    } else {
        i64::try_from(limit).unwrap_or(i64::MAX)
    }
}

/// Maps CPython's limit representation to a raw `rlim_t`; `None` for a
/// negative value other than `-1`.
#[cfg(unix)]
fn value_to_rlim(number: i64) -> Option<libc::rlim_t> {
    if number == -1 || number as u64 == libc::RLIM_INFINITY {
        return Some(libc::RLIM_INFINITY);
    }
    if number < 0 {
        return None;
    }
    libc::rlim_t::try_from(number as u64).ok()
}

/// `resource.setrlimit(resource, limits)`: installs a `(soft, hard)` pair.
#[cfg(unix)]
fn setrlimit(args: &[i64]) -> i64 {
    if args.len() > 2 {
        return type_error(&format!(
            "setrlimit() takes exactly 2 arguments ({} given)",
            args.len()
        ));
    }
    let Some(resource) = int_arg("setrlimit", args, 0, "resource") else {
        return NONE;
    };
    let Some(&limits_value) = args.get(1) else {
        return type_error("setrlimit() missing required argument 'limits' (pos 2)");
    };
    let Ok(resource) = libc::c_int::try_from(resource) else {
        return exc::raise_with_message(OVERFLOW_ERROR, "setrlimit(): resource is out of range");
    };
    let values: Vec<i64> = if let Some(items) = value::as_tuple(limits_value) {
        items.to_vec()
    } else if let Some(items) = value::as_list(limits_value) {
        items.to_vec()
    } else {
        return type_error(&format!(
            "setrlimit() argument 2 must be a tuple of 2 items, not {}",
            value::type_name(limits_value)
        ));
    };
    if values.len() != 2 {
        return type_error(&format!(
            "setrlimit() argument 2 must be a tuple of 2 items, not a sequence of {}",
            values.len()
        ));
    }
    let mut parsed = [0i64; 2];
    for (index, item) in values.iter().enumerate() {
        let Some(number) = value::as_integer(*item) else {
            return type_error("setrlimit() argument 2 must be a tuple of two integers");
        };
        parsed[index] = number;
    }
    let mut limits: libc::rlimit = unsafe { std::mem::zeroed() };
    let Some(soft) = value_to_rlim(parsed[0]) else {
        return exc::raise_with_message(VALUE_ERROR, "not a valid value for a resource limit");
    };
    let Some(hard) = value_to_rlim(parsed[1]) else {
        return exc::raise_with_message(VALUE_ERROR, "not a valid value for a resource limit");
    };
    limits.rlim_cur = soft;
    limits.rlim_max = hard;
    // SAFETY: `limits` is a fully initialised `struct rlimit`.
    let result = unsafe { libc::setrlimit(resource as _, &limits) };
    if result != 0 {
        return raise_last_os_error("setrlimit");
    }
    NONE
}

/// `resource.getrusage(who)`: usage counters for the current process, its
/// children or (where available) the calling thread.
#[cfg(unix)]
fn getrusage(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "getrusage() takes exactly 1 argument ({} given)",
            args.len()
        ));
    }
    let Some(who) = int_arg("getrusage", args, 0, "who") else {
        return NONE;
    };
    let Ok(who) = libc::c_int::try_from(who) else {
        return exc::raise_with_message(OVERFLOW_ERROR, "getrusage(): who is out of range");
    };
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a valid `struct rusage` the kernel fills in.
    let result = unsafe { libc::getrusage(who, &mut usage) };
    if result != 0 {
        return raise_last_os_error("getrusage");
    }
    let namespace = value::namespace_new(Vec::new());
    let roots = heap::root_values(&[namespace]);
    record_field(
        namespace,
        "ru_utime",
        value::float_new(seconds(usage.ru_utime)),
    );
    record_field(
        namespace,
        "ru_stime",
        value::float_new(seconds(usage.ru_stime)),
    );
    record_field(namespace, "ru_maxrss", int_value(usage.ru_maxrss));
    record_field(namespace, "ru_ixrss", int_value(usage.ru_ixrss));
    record_field(namespace, "ru_idrss", int_value(usage.ru_idrss));
    record_field(namespace, "ru_isrss", int_value(usage.ru_isrss));
    record_field(namespace, "ru_minflt", int_value(usage.ru_minflt));
    record_field(namespace, "ru_majflt", int_value(usage.ru_majflt));
    record_field(namespace, "ru_nswap", int_value(usage.ru_nswap));
    record_field(namespace, "ru_inblock", int_value(usage.ru_inblock));
    record_field(namespace, "ru_oublock", int_value(usage.ru_oublock));
    record_field(namespace, "ru_msgsnd", int_value(usage.ru_msgsnd));
    record_field(namespace, "ru_msgrcv", int_value(usage.ru_msgrcv));
    record_field(namespace, "ru_nsignals", int_value(usage.ru_nsignals));
    record_field(namespace, "ru_nvcsw", int_value(usage.ru_nvcsw));
    record_field(namespace, "ru_nivcsw", int_value(usage.ru_nivcsw));
    heap::release_roots(roots);
    namespace
}

/// Seconds of a `struct timeval` as a float.
#[cfg(unix)]
fn seconds(time: libc::timeval) -> f64 {
    time.tv_sec as f64 + time.tv_usec as f64 / 1_000_000.0
}

/// `resource.getpagesize()`: the virtual memory page size.
#[cfg(target_os = "linux")]
fn page_size() -> Option<i64> {
    // glibc and musl both define `_SC_PAGESIZE` as 30; the libc crate does
    // not bind the constant for Linux targets.
    const SC_PAGESIZE: libc::c_int = 30;
    // SAFETY: `sysconf` with a valid name is a pure query.
    let size = unsafe { libc::sysconf(SC_PAGESIZE) };
    (size > 0).then_some(size)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn page_size() -> Option<i64> {
    // SAFETY: `sysconf(_SC_PAGESIZE)` is a pure query.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (size > 0).then(|| i64::from(size))
}

/// `resource.getpagesize()`.
#[cfg(unix)]
fn getpagesize(args: &[i64]) -> i64 {
    if !args.is_empty() {
        return type_error(&format!(
            "getpagesize() takes no arguments ({} given)",
            args.len()
        ));
    }
    match page_size() {
        Some(size) => int_value(size),
        None => raise_last_os_error("getpagesize"),
    }
}

#[cfg(unix)]
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "getrlimit" => getrlimit(args),
        "setrlimit" => setrlimit(args),
        "getrusage" => getrusage(args),
        "getpagesize" => getpagesize(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'resource' has no attribute '{function}'"),
        ),
    }
}

#[cfg(not(unix))]
pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
    exc::raise_with_message(
        exc::RUNTIME_ERROR,
        &format!("resource.{function} is not available on this platform"),
    )
}

/// Module integer constants: `RLIM_INFINITY`, every `RLIMIT_*` and the
/// `RUSAGE_*` selectors.
#[cfg(unix)]
pub(crate) fn constant(name: &str) -> Option<i64> {
    // Limits that every supported Unix defines.
    let raw: i64 = match name {
        "RLIM_INFINITY" => libc::RLIM_INFINITY as i64,
        "RLIMIT_CPU" => libc::RLIMIT_CPU as i64,
        "RLIMIT_FSIZE" => libc::RLIMIT_FSIZE as i64,
        "RLIMIT_DATA" => libc::RLIMIT_DATA as i64,
        "RLIMIT_STACK" => libc::RLIMIT_STACK as i64,
        "RLIMIT_CORE" => libc::RLIMIT_CORE as i64,
        "RLIMIT_AS" => libc::RLIMIT_AS as i64,
        "RLIMIT_NOFILE" => libc::RLIMIT_NOFILE as i64,
        // Limits missing from some Unix C libraries (Solaris has no `RSS`,
        // Haiku no `MEMLOCK`, ...).
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
            target_os = "aix"
        ))]
        "RLIMIT_RSS" => libc::RLIMIT_RSS as i64,
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
            target_os = "aix"
        ))]
        "RLIMIT_NPROC" => libc::RLIMIT_NPROC as i64,
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        "RLIMIT_MEMLOCK" => libc::RLIMIT_MEMLOCK as i64,
        // Linux/Android extras.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "RLIMIT_LOCKS" => libc::RLIMIT_LOCKS as i64,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "RLIMIT_SIGPENDING" => libc::RLIMIT_SIGPENDING as i64,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "RLIMIT_MSGQUEUE" => libc::RLIMIT_MSGQUEUE as i64,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "RLIMIT_NICE" => libc::RLIMIT_NICE as i64,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "RLIMIT_RTPRIO" => libc::RLIMIT_RTPRIO as i64,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "RLIMIT_RTTIME" => libc::RLIMIT_RTTIME as i64,
        // `getrusage` selectors (libc does not declare every one everywhere).
        "RUSAGE_SELF" => 0,
        "RUSAGE_CHILDREN" => -1,
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "aix"
        ))]
        "RUSAGE_THREAD" => 1,
        _ => return None,
    };
    Some(int_value(raw))
}

#[cfg(not(unix))]
pub(crate) fn constant(_name: &str) -> Option<i64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{encode_int, NONE};

    fn int(n: i64) -> i64 {
        encode_int(n).expect("small integer")
    }

    fn field(entry: i64, name: &str) -> i64 {
        value::as_namespace(entry)
            .and_then(|pairs| pairs.iter().find(|(key, _)| key == name))
            .map(|(_, item)| *item)
            .unwrap_or(NONE)
    }

    fn message() -> String {
        value::to_string(exc::pending_value())
    }

    #[cfg(unix)]
    #[test]
    fn getpagesize_reports_a_positive_size() {
        exc::init();
        let size = call("getpagesize", &[]);
        assert!(!exc::pending(), "getpagesize failed: {}", message());
        let size = value::as_integer(size).expect("integer page size");
        assert!(size > 0);
    }

    #[cfg(unix)]
    #[test]
    fn getrlimit_and_setrlimit_round_trip() {
        exc::init();
        let limits = call("getrlimit", &[int(i64::from(libc::RLIMIT_NOFILE as i32))]);
        assert!(!exc::pending(), "getrlimit failed: {}", message());
        let items = value::as_tuple(limits).expect("(soft, hard) tuple");
        assert_eq!(items.len(), 2);
        let soft = value::as_integer(items[0]).expect("soft limit");
        let hard = value::as_integer(items[1]).expect("hard limit");
        assert!(soft >= -1);
        assert!(hard >= soft || soft == -1);

        // Writing back the values just read is always allowed.
        let values = value::tuple_new(vec![int(soft), int(hard)]);
        let result = call(
            "setrlimit",
            &[int(i64::from(libc::RLIMIT_NOFILE as i32)), values],
        );
        assert!(!exc::pending(), "setrlimit failed: {}", message());
        assert_eq!(result, NONE);
    }

    #[cfg(unix)]
    #[test]
    fn getrusage_returns_cpython_fields() {
        exc::init();
        let usage = call("getrusage", &[constant("RUSAGE_SELF").expect("constant")]);
        assert!(!exc::pending(), "getrusage failed: {}", message());
        for name in [
            "ru_utime",
            "ru_stime",
            "ru_maxrss",
            "ru_ixrss",
            "ru_idrss",
            "ru_isrss",
            "ru_minflt",
            "ru_majflt",
            "ru_nswap",
            "ru_inblock",
            "ru_oublock",
            "ru_msgsnd",
            "ru_msgrcv",
            "ru_nsignals",
            "ru_nvcsw",
            "ru_nivcsw",
        ] {
            assert_ne!(field(usage, name), NONE, "missing {name}");
        }
        assert!(value::as_float(field(usage, "ru_utime")).is_some());
        assert!(value::as_integer(field(usage, "ru_maxrss")).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn argument_validation_raises_the_documented_errors() {
        exc::init();
        assert_eq!(call("getrlimit", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("getrlimit", &[value::str_alloc("cpu".into())]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        // An unknown resource reaches the kernel and fails with EINVAL.
        assert_eq!(call("getrlimit", &[int(9_999)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(OS_ERROR));
        exc::init();

        assert_eq!(call("setrlimit", &[int(0)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        // A 1-item sequence is rejected before the syscall.
        assert_eq!(
            call(
                "setrlimit",
                &[
                    int(i64::from(libc::RLIMIT_NOFILE as i32)),
                    value::tuple_new(vec![int(1)])
                ]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        // Negative values other than -1 are invalid.
        assert_eq!(
            call(
                "setrlimit",
                &[
                    int(i64::from(libc::RLIMIT_NOFILE as i32)),
                    value::tuple_new(vec![int(-5), int(-1)])
                ]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        exc::init();

        assert_eq!(call("getrusage", &[value::str_alloc("self".into())]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("getpagesize", &[int(0)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("nope", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));
        exc::init();
    }

    #[cfg(unix)]
    #[test]
    fn constants_match_the_kernel_headers() {
        assert_eq!(
            constant("RLIM_INFINITY"),
            Some(int_value(libc::RLIM_INFINITY as i64))
        );
        assert_eq!(
            constant("RLIMIT_NOFILE"),
            Some(int_value(libc::RLIMIT_NOFILE as i64))
        );
        assert_eq!(constant("RUSAGE_SELF"), Some(int(0)));
        assert_eq!(constant("RUSAGE_CHILDREN"), Some(int(-1)));
        assert_eq!(constant("NOT_A_LIMIT"), None);
    }
}
