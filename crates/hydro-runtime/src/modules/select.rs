//! `select`, `selectors` and the `poll` readiness primitives.
//!
//! `select.select` and `select.poll` drive `poll(2)` on Unix and park the
//! calling thread for the collector while the wait is blocking; the descriptor
//! set and the result vector are allocated before [`heap::blocking_enter`] and
//! after [`heap::blocking_leave`], so a collection never sees a parked thread
//! allocating.
//!
//! `selectors.DefaultSelector` is a native object: its state lives in ordinary
//! instance attributes (lists of registered descriptors and `SelectorKey`
//! instances), so no new `ObjData` kind is needed. It exposes the subset the
//! standard library's event loops use: `register`, `unregister`, `modify`,
//! `select`, `get_key`, `get_map`, `close`, and the context-manager methods.
//!
//! File objects are accepted as integers or as native `socket` objects.
//! Arbitrary objects with a `fileno()` method are not called (the runtime has
//! no way to invoke a user method from here); callers must pass `sock.fileno()`
//! or the socket itself.

use std::io;
use std::time::{Duration, Instant};

use crate::core::methods;
use crate::exc::{
    self, ATTRIBUTE_ERROR, KEY_ERROR, OS_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::value::{self, NONE};

pub(crate) const POLLIN: i64 = 1;
pub(crate) const POLLPRI: i64 = 2;
pub(crate) const POLLOUT: i64 = 4;
pub(crate) const POLLERR: i64 = 8;
pub(crate) const POLLHUP: i64 = 16;
pub(crate) const POLLNVAL: i64 = 32;

/// `selectors.EVENT_*`.
const EVENT_READ: i64 = 1;
const EVENT_WRITE: i64 = 2;

fn encode_int(n: i64) -> i64 {
    value::encode_int(n)
        .unwrap_or_else(|| exc::raise_with_message(crate::exc::OVERFLOW_ERROR, "integer overflow"))
}

fn os_error(message: &str) -> i64 {
    exc::raise_with_message(OS_ERROR, message)
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn key_error(message: &str) -> i64 {
    exc::raise_with_message(KEY_ERROR, message)
}

fn attribute_error(module: &str, name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '{module}' has no attribute '{name}'"),
    )
}

/// Module constants for `select`.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "POLLIN" => encode_int(POLLIN),
        "POLLPRI" => encode_int(POLLPRI),
        "POLLOUT" => encode_int(POLLOUT),
        "POLLERR" => encode_int(POLLERR),
        "POLLHUP" => encode_int(POLLHUP),
        "POLLNVAL" => encode_int(POLLNVAL),
        "error" => exc::exception_class(OS_ERROR),
        _ => epoll_constant(name),
    }
}

/// `select.epoll` and its `EPOLL*` masks on the platforms that have epoll.
fn epoll_constant(name: &str) -> i64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        match name {
            "epoll" => return epoll_class(),
            "EPOLLIN" => return encode_int(libc::EPOLLIN as i64),
            "EPOLLPRI" => return encode_int(libc::EPOLLPRI as i64),
            "EPOLLOUT" => return encode_int(libc::EPOLLOUT as i64),
            "EPOLLERR" => return encode_int(libc::EPOLLERR as i64),
            "EPOLLHUP" => return encode_int(libc::EPOLLHUP as i64),
            "EPOLLRDHUP" => return encode_int(libc::EPOLLRDHUP as i64),
            "EPOLLET" => return encode_int(libc::EPOLLET as i64),
            "EPOLLONESHOT" => return encode_int(libc::EPOLLONESHOT as i64),
            "EPOLLWAKEUP" => return encode_int(libc::EPOLLWAKEUP as i64),
            "EPOLLEXCLUSIVE" => return encode_int(libc::EPOLLEXCLUSIVE as i64),
            "EPOLL_CLOEXEC" => return encode_int(libc::EPOLL_CLOEXEC as i64),
            "EPOLL_CTL_ADD" => return encode_int(libc::EPOLL_CTL_ADD as i64),
            "EPOLL_CTL_DEL" => return encode_int(libc::EPOLL_CTL_DEL as i64),
            "EPOLL_CTL_MOD" => return encode_int(libc::EPOLL_CTL_MOD as i64),
            _ => {}
        }
    }
    attribute_error("select", name)
}

/// Module constants for `selectors`.
pub(crate) fn selectors_constant(name: &str) -> i64 {
    match name {
        "EVENT_READ" => encode_int(EVENT_READ),
        "EVENT_WRITE" => encode_int(EVENT_WRITE),
        _ => attribute_error("selectors", name),
    }
}

/// Resolves a descriptor from an integer, a native socket object, or any
/// object exposing `fileno()`.
fn file_descriptor(item: i64) -> Result<i32, i64> {
    if let Some(number) = value::as_integer(item) {
        if number < 0 {
            return Err(value_error("file descriptor cannot be a negative integer"));
        }
        return i32::try_from(number).map_err(|_| value_error("file descriptor is out of range"));
    }
    if let Some(state) = value::as_socket(item) {
        return crate::modules::socket::descriptor(state)
            .ok_or_else(|| os_error("[Errno 9] Bad file descriptor"));
    }
    // CPython's `PyObject_AsFileDescriptor` calls a user `fileno()` method;
    // the same path serves file objects and test helpers.
    let method = methods::getattr(item, b"fileno".as_ptr(), b"fileno".len());
    if exc::pending() {
        exc::clear();
        return Err(type_error(
            "argument must be an int, or have a fileno() method",
        ));
    }
    let result = crate::builtins::call(method, 0, std::ptr::null());
    if exc::pending() {
        return Err(NONE);
    }
    match value::as_integer(result) {
        Some(number) if number >= 0 && number <= i32::MAX as i64 => Ok(number as i32),
        _ => Err(type_error(&format!(
            "fileno() returned a non-integer ({})",
            value::type_name(result)
        ))),
    }
}

/// One descriptor sequence argument of `select.select`.
///
/// A list is read live (index by index, re-borrowing each step) because a
/// user `fileno()` may resize it during the scan, which is exactly the
/// behaviour CPython documents for a mutated argument. A tuple is snapshotted.
enum SequenceSource {
    List(i64),
    Items(Vec<i64>),
}

impl SequenceSource {
    fn from_value(v: i64) -> Result<Self, i64> {
        if value::as_list(v).is_some() {
            return Ok(Self::List(v));
        }
        if let Some(items) = value::as_tuple(v) {
            return Ok(Self::Items(items.to_vec()));
        }
        Err(type_error(&format!(
            "expected a list or tuple, not {}",
            value::type_name(v)
        )))
    }

    fn len(&self) -> usize {
        match self {
            Self::List(list) => value::as_list(*list).map(<[i64]>::len).unwrap_or(0),
            Self::Items(items) => items.len(),
        }
    }

    fn get(&self, index: usize) -> Option<i64> {
        match self {
            Self::List(list) => value::as_list(*list).and_then(|items| items.get(index).copied()),
            Self::Items(items) => items.get(index).copied(),
        }
    }
}

fn seconds_argument(v: i64) -> Result<Option<Duration>, i64> {
    if v == NONE {
        return Ok(None);
    }
    let seconds = if let Some(integer) = value::as_integer(v) {
        integer as f64
    } else if let Some(float) = value::as_float(v) {
        float
    } else {
        return Err(type_error("timeout must be a float or None"));
    };
    if seconds.is_nan() || seconds < 0.0 || !seconds.is_finite() || seconds >= u64::MAX as f64 {
        return Err(value_error("timeout must be non-negative"));
    }
    Ok(Some(Duration::from_secs_f64(seconds)))
}

/// `poll()` timeout: milliseconds, `None`/negative meaning infinite.
///
/// An integer that does not fit the C `int` timeout is an `OverflowError`,
/// like CPython; floats are accepted and truncated.
fn milliseconds_argument(v: i64) -> Result<Option<Duration>, i64> {
    if v == NONE {
        return Ok(None);
    }
    if let Some(integer) = value::as_integer(v) {
        if integer < i32::MIN as i64 || integer > i32::MAX as i64 {
            return Err(exc::raise_with_message(
                OVERFLOW_ERROR,
                "timeout is out of range",
            ));
        }
        if integer < 0 {
            return Ok(None);
        }
        return Ok(Some(Duration::from_millis(integer as u64)));
    }
    if value::as_bigint(v).is_some() {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "timeout is out of range",
        ));
    }
    let Some(float) = value::as_float(v) else {
        return Err(type_error("timeout must be an int, float or None"));
    };
    if !float.is_finite() {
        if float > 0.0 {
            return Err(exc::raise_with_message(
                OVERFLOW_ERROR,
                "timeout is out of range",
            ));
        }
        return Ok(None);
    }
    if float <= 0.0 {
        return Ok(None);
    }
    Ok(Some(Duration::from_millis(float as u64)))
}

/// Decodes a `poll` event mask (`unsigned short` in CPython).
fn event_mask(function: &str, argument: i64) -> Result<i16, i64> {
    let Some(number) = value::as_integer(argument) else {
        if value::as_bigint(argument).is_some() {
            return Err(exc::raise_with_message(
                OVERFLOW_ERROR,
                &format!("{function}() eventmask is out of range"),
            ));
        }
        return Err(type_error(&format!(
            "{function}() argument must be an int, not {}",
            value::type_name(argument)
        )));
    };
    if number < 0 {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            &format!("{function}() eventmask must be non-negative"),
        ));
    }
    if number > 0xffff {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            &format!("{function}() eventmask is out of range"),
        ));
    }
    Ok(number as i16)
}

/// Waits for `entries` and returns the descriptors with a non-zero `revents`.
///
/// The `pollfd` vector is built by the caller; this only parks around the
/// system call, so no allocation happens while the thread is parked.
#[cfg(unix)]
fn poll_entries(
    entries: &[(i32, i16)],
    timeout: Option<Duration>,
) -> Result<Vec<(i32, i16)>, io::Error> {
    let mut fds: Vec<libc::pollfd> = entries
        .iter()
        .map(|&(fd, events)| libc::pollfd {
            fd,
            events,
            revents: 0,
        })
        .collect();
    let deadline = timeout.and_then(|limit| Instant::now().checked_add(limit));
    loop {
        let millis = match timeout {
            None => -1,
            Some(limit) if limit.is_zero() => 0,
            Some(_) => {
                let remaining = match deadline {
                    Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                    None => Duration::ZERO,
                };
                if remaining.is_zero() {
                    return Ok(Vec::new());
                }
                remaining
                    .as_nanos()
                    .saturating_add(999_999)
                    .checked_div(1_000_000)
                    .unwrap_or(0)
                    .min(i32::MAX as u128) as i32
            }
        };
        let blocking = timeout != Some(Duration::ZERO);
        if blocking {
            heap::blocking_enter();
        }
        // SAFETY: `fds` is a valid mutable array of `len` pollfd elements.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
        if blocking {
            heap::blocking_leave();
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            return Ok(Vec::new());
        }
        let mut ready = Vec::new();
        for (index, entry) in fds.iter().enumerate() {
            if entry.revents != 0 {
                ready.push((entries[index].0, entry.revents));
            }
        }
        return Ok(ready);
    }
}

#[cfg(not(unix))]
fn poll_entries(
    entries: &[(i32, i16)],
    timeout: Option<Duration>,
) -> Result<Vec<(i32, i16)>, io::Error> {
    // There is no portable `poll` here; sleeping and reporting every
    // descriptor ready is only a compile-time fallback for non-Unix hosts.
    if let Some(limit) = timeout {
        if !limit.is_zero() {
            heap::blocking_enter();
            std::thread::sleep(limit);
            heap::blocking_leave();
        }
    }
    Ok(entries.iter().map(|&(fd, events)| (fd, events)).collect())
}

/// Scans one sequence argument, merging its descriptors into the `pollfd`
/// set and remembering the (item, fd) pairs for result construction.
fn collect_descriptors(
    source: &SequenceSource,
    mask: i16,
    events: &mut Vec<(i32, i16)>,
    pairs: &mut Vec<(i64, i32)>,
) -> Result<(), i64> {
    let mut index = 0;
    while index < source.len() {
        let Some(item) = source.get(index) else {
            break;
        };
        // The length is re-read every iteration, so a `fileno()` that shrinks
        // the list ends the scan exactly like CPython's index loop.
        index += 1;
        let fd = file_descriptor(item)?;
        match events.iter_mut().find(|(existing, _)| *existing == fd) {
            Some(entry) => entry.1 |= mask,
            None => events.push((fd, mask)),
        }
        pairs.push((item, fd));
    }
    Ok(())
}

/// Builds the ready list for one sequence argument from the recorded pairs.
fn ready_items(pairs: &[(i64, i32)], ready: &[(i32, i16)], mask: i16) -> i64 {
    let mut out = Vec::new();
    for &(item, fd) in pairs {
        if ready
            .iter()
            .find(|(ready_fd, _)| *ready_fd == fd)
            .is_some_and(|(_, revents)| revents & mask != 0)
        {
            out.push(item);
        }
    }
    value::list_new(out)
}

fn select_call(args: &[i64]) -> i64 {
    let readables = match SequenceSource::from_value(args[0]) {
        Ok(items) => items,
        Err(err) => return err,
    };
    let writables = match SequenceSource::from_value(args[1]) {
        Ok(items) => items,
        Err(err) => return err,
    };
    let exceptionals = match SequenceSource::from_value(args[2]) {
        Ok(items) => items,
        Err(err) => return err,
    };
    let timeout = match args.get(3) {
        Some(&value) => match seconds_argument(value) {
            Ok(timeout) => timeout,
            Err(err) => return err,
        },
        None => None,
    };
    let mut events: Vec<(i32, i16)> = Vec::new();
    let mut read_pairs: Vec<(i64, i32)> = Vec::new();
    let mut write_pairs: Vec<(i64, i32)> = Vec::new();
    let mut exception_pairs: Vec<(i64, i32)> = Vec::new();
    if let Err(err) = collect_descriptors(
        &readables,
        POLLIN as i16,
        &mut events,
        &mut read_pairs,
    ) {
        return err;
    }
    if let Err(err) = collect_descriptors(
        &writables,
        POLLOUT as i16,
        &mut events,
        &mut write_pairs,
    ) {
        return err;
    }
    if let Err(err) = collect_descriptors(
        &exceptionals,
        POLLPRI as i16,
        &mut events,
        &mut exception_pairs,
    ) {
        return err;
    }
    let ready = match poll_entries(&events, timeout) {
        Ok(ready) => ready,
        Err(error) => return os_error(&error.to_string()),
    };
    // `select(2)` reports a closed descriptor as `EBADF`; the `poll(2)`
    // backend underneath signals the same condition with `POLLNVAL`.
    if ready
        .iter()
        .any(|(_, revents)| revents & POLLNVAL as i16 != 0)
    {
        return os_error("[Errno 9] Bad file descriptor");
    }
    let readable_mask = (POLLIN | POLLERR | POLLHUP) as i16;
    let writable_mask = (POLLOUT | POLLERR | POLLHUP) as i16;
    let exceptional_mask = (POLLPRI | POLLERR) as i16;
    let read = ready_items(&read_pairs, &ready, readable_mask);
    let write = ready_items(&write_pairs, &ready, writable_mask);
    let exceptional = ready_items(&exception_pairs, &ready, exceptional_mask);
    value::tuple_new(vec![read, write, exceptional])
}

fn instance_list(obj: i64, name: &str) -> Vec<i64> {
    value::instance_attr(obj, name)
        .and_then(value::as_list)
        .map(|items| items.to_vec())
        .unwrap_or_default()
}

fn set_instance_attr(obj: i64, name: &str, item: i64) {
    value::instance_setattr(obj, name.to_string(), item);
}

fn is_closed(obj: i64) -> bool {
    value::instance_attr(obj, "_closed")
        .map(value::truthy)
        .unwrap_or(false)
}

// -------------------------------------------------------------------------
// select.poll
// -------------------------------------------------------------------------

fn poll_entries_attr(obj: i64) -> Vec<(i32, i16)> {
    let mut entries = Vec::new();
    for item in instance_list(obj, "_entries") {
        let Some(pair) = value::as_list(item) else {
            continue;
        };
        if pair.len() != 2 {
            continue;
        }
        let (Some(fd), Some(mask)) = (value::as_integer(pair[0]), value::as_integer(pair[1]))
        else {
            continue;
        };
        if let Ok(fd) = i32::try_from(fd) {
            entries.push((fd, mask as i16));
        }
    }
    entries
}

fn store_poll_entries(obj: i64, entries: Vec<(i32, i16)>) {
    let items: Vec<i64> = entries
        .into_iter()
        .map(|(fd, mask)| value::list_new(vec![encode_int(fd as i64), encode_int(mask as i64)]))
        .collect();
    set_instance_attr(obj, "_entries", value::list_new(items));
}

fn poll_new() -> i64 {
    let methods = vec![
        (
            "register".to_string(),
            value::native_function_new("select", "_poll_register", 3),
        ),
        (
            "unregister".to_string(),
            value::native_function_new("select", "_poll_unregister", 2),
        ),
        (
            "modify".to_string(),
            value::native_function_new("select", "_poll_modify", 3),
        ),
        (
            "poll".to_string(),
            value::native_function_new("select", "_poll_poll", 2),
        ),
        (
            "close".to_string(),
            value::native_function_new("select", "_poll_close", 1),
        ),
        (
            "__enter__".to_string(),
            value::native_function_new("select", "_poll_enter", 1),
        ),
        (
            "__exit__".to_string(),
            value::native_function_new("select", "_poll_exit", 4),
        ),
        (
            "__init__".to_string(),
            value::native_function_new("select", "_poll_init", 1),
        ),
    ];
    let class = value::class_new("poll".to_string(), NONE, methods);
    let object = value::instance_new(class);
    set_instance_attr(object, "_entries", value::list_new(Vec::new()));
    set_instance_attr(object, "_closed", value::FALSE);
    object
}

fn poll_register(args: &[i64]) -> i64 {
    let object = args[0];
    if is_closed(object) {
        return value_error("I/O operation on closed poll object");
    }
    let fd = match file_descriptor(args[1]) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let mask = match args.get(2) {
        Some(&value) if value != NONE => match event_mask("register", value) {
            Ok(mask) => mask,
            Err(err) => return err,
        },
        _ => (POLLIN | POLLPRI | POLLOUT) as i16,
    };
    let mut entries = poll_entries_attr(object);
    match entries.iter_mut().find(|(existing, _)| *existing == fd) {
        Some(entry) => entry.1 = mask,
        None => entries.push((fd, mask)),
    }
    store_poll_entries(object, entries);
    NONE
}

fn poll_unregister(args: &[i64]) -> i64 {
    let object = args[0];
    if is_closed(object) {
        return value_error("I/O operation on closed poll object");
    }
    let fd = match file_descriptor(args[1]) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let mut entries = poll_entries_attr(object);
    let Some(index) = entries.iter().position(|(existing, _)| *existing == fd) else {
        return key_error(&format!("fd {fd} not registered"));
    };
    entries.remove(index);
    store_poll_entries(object, entries);
    NONE
}

fn poll_modify(args: &[i64]) -> i64 {
    let object = args[0];
    if is_closed(object) {
        return value_error("I/O operation on closed poll object");
    }
    let fd = match file_descriptor(args[1]) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let mask = match event_mask("modify", args[2]) {
        Ok(mask) => mask,
        Err(err) => return err,
    };
    let mut entries = poll_entries_attr(object);
    let Some(index) = entries.iter().position(|(existing, _)| *existing == fd) else {
        return key_error(&format!("fd {fd} not registered"));
    };
    entries[index].1 = mask;
    store_poll_entries(object, entries);
    NONE
}

fn poll_poll(args: &[i64]) -> i64 {
    let object = args[0];
    if is_closed(object) {
        return value_error("I/O operation on closed poll object");
    }
    // CPython keeps one `pollfd` array per object and reports a concurrent
    // `poll()` call on the same object as a `RuntimeError`; the descriptor
    // set cannot be read safely while another thread is inside `poll(2)`.
    if value::instance_attr(object, "_polling")
        .map(value::truthy)
        .unwrap_or(false)
    {
        return exc::raise_with_message(
            exc::RUNTIME_ERROR,
            "poll() is not thread-safe; use select()",
        );
    }
    let timeout = match args.get(1) {
        Some(&value) => match milliseconds_argument(value) {
            Ok(timeout) => timeout,
            Err(err) => return err,
        },
        None => None,
    };
    let entries = poll_entries_attr(object);
    set_instance_attr(object, "_polling", value::TRUE);
    let ready = poll_entries(&entries, timeout);
    set_instance_attr(object, "_polling", value::FALSE);
    let ready = match ready {
        Ok(ready) => ready,
        Err(error) => return os_error(&error.to_string()),
    };
    let items: Vec<i64> = ready
        .into_iter()
        .map(|(fd, mask)| value::tuple_new(vec![encode_int(fd as i64), encode_int(mask as i64)]))
        .collect();
    value::list_new(items)
}

/// `poll.__init__`: a poll object is only created by `select.poll()`.
fn poll_init(_args: &[i64]) -> i64 {
    type_error("cannot create 'poll' instances")
}

fn poll_close(args: &[i64]) -> i64 {
    let object = args[0];
    set_instance_attr(object, "_entries", value::list_new(Vec::new()));
    set_instance_attr(object, "_closed", value::TRUE);
    NONE
}

// -------------------------------------------------------------------------
// select.epoll (Linux / Android)
// -------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_class() -> i64 {
    const KEY: &str = "select.epoll.class";
    if let Some(class) = crate::core::globals::registry_get(KEY) {
        return class;
    }
    let native = |name: &'static str, arity: usize| value::native_function_new("select", name, arity);
    let methods = vec![
        ("__init__".to_string(), native("_epoll_init", 3)),
        ("register".to_string(), native("_epoll_register", 3)),
        ("unregister".to_string(), native("_epoll_unregister", 2)),
        ("modify".to_string(), native("_epoll_modify", 3)),
        ("poll".to_string(), native("_epoll_poll", 3)),
        ("fileno".to_string(), native("_epoll_fileno", 1)),
        ("close".to_string(), native("_epoll_close", 1)),
        ("__enter__".to_string(), native("_epoll_enter", 1)),
        ("__exit__".to_string(), native("_epoll_exit", 4)),
    ];
    // `fromfd` is a class-level function: a class attribute holding a plain
    // function value, so `epoll.fromfd(fd)` passes the descriptor only.
    let attrs = vec![(
        "fromfd".to_string(),
        value::native_function_new("select", "_epoll_fromfd", 1),
    )];
    let methods = methods
        .into_iter()
        .map(|(name, function)| (name, function, hydro_ir::MethodKind::Normal))
        .collect();
    let class = value::class_new_full("epoll".to_string(), NONE, methods, attrs);
    crate::core::globals::registry_set(KEY, class);
    class
}

/// Formats a failed syscall as CPython renders `OSError`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_io_error(error: std::io::Error) -> i64 {
    let code = error.raw_os_error().unwrap_or(0);
    let text = error.to_string();
    let detail = text.split(" (os error").next().unwrap_or(text.as_str());
    os_error(&format!("[Errno {code}] {detail}"))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_instance(fd: i32) -> i64 {
    let instance = value::instance_new(epoll_class());
    set_instance_attr(instance, "_fd", encode_int(i64::from(fd)));
    set_instance_attr(instance, "closed", value::FALSE);
    instance
}

/// The epoll descriptor of a live object, or `ValueError` once closed.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_fd(object: i64) -> Result<i32, i64> {
    if value::instance_attr(object, "closed")
        .map(value::truthy)
        .unwrap_or(false)
    {
        return Err(value_error("I/O operation on closed epoll object"));
    }
    value::instance_attr(object, "_fd")
        .and_then(value::as_integer)
        .and_then(|fd| i32::try_from(fd).ok())
        .ok_or_else(|| value_error("I/O operation on closed epoll object"))
}

/// Converts `epoll.poll`'s timeout (seconds) to milliseconds.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_timeout(value_arg: i64) -> Result<i32, i64> {
    if value_arg == NONE {
        return Ok(-1);
    }
    if let Some(integer) = value::as_integer(value_arg) {
        if integer < 0 {
            return Ok(-1);
        }
        return Ok(integer.min(i64::from(i32::MAX)) as i32);
    }
    if value::as_bigint(value_arg).is_some() {
        return Ok(i32::MAX);
    }
    let Some(seconds) = value::as_float(value_arg) else {
        return Err(type_error("timeout must be an int, float or None"));
    };
    if !(seconds > 0.0) {
        return Ok(-1);
    }
    let millis = (seconds * 1000.0).ceil();
    Ok(if millis >= f64::from(i32::MAX) {
        i32::MAX
    } else {
        millis as i32
    })
}

/// `epoll.__init__(sizehint=-1, flags=0)`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_init(args: &[i64]) -> i64 {
    let object = args[0];
    let mut sizehint = -1i64;
    if let Some(&argument) = args.get(1) {
        if argument != NONE {
            let Some(number) = value::as_integer(argument) else {
                if value::as_bigint(argument).is_some() {
                    return exc::raise_with_message(
                        OVERFLOW_ERROR,
                        "sizehint is out of range",
                    );
                }
                return type_error(&format!(
                    "argument 1 must be an integer, not {}",
                    value::type_name(argument)
                ));
            };
            sizehint = number;
        }
    }
    if sizehint <= 0 && sizehint != -1 {
        return value_error("sizehint must be greater than zero");
    }
    let mut flags = 0i64;
    if let Some(&argument) = args.get(2) {
        if argument != NONE {
            let Some(number) = value::as_integer(argument) else {
                if value::as_bigint(argument).is_some() {
                    return exc::raise_with_message(
                        OVERFLOW_ERROR,
                        "flags is out of range",
                    );
                }
                return type_error(&format!(
                    "argument 2 must be an integer, not {}",
                    value::type_name(argument)
                ));
            };
            flags = number;
        }
    }
    let Ok(flags) = i32::try_from(flags) else {
        return exc::raise_with_message(OVERFLOW_ERROR, "flags is out of range");
    };
    // CPython always creates the epoll descriptor close-on-exec.
    // SAFETY: `epoll_create1` only reads its flag argument.
    let fd = unsafe { libc::epoll_create1(flags | libc::EPOLL_CLOEXEC) };
    if fd < 0 {
        return epoll_io_error(std::io::Error::last_os_error());
    }
    set_instance_attr(object, "_fd", encode_int(i64::from(fd)));
    set_instance_attr(object, "closed", value::FALSE);
    NONE
}

/// `epoll.register(fd, eventmask=EPOLLIN)`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_register(args: &[i64]) -> i64 {
    let object = args[0];
    let epfd = match epoll_fd(object) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let target = match file_descriptor(args[1]) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let mask = match args.get(2) {
        Some(&argument) if argument != NONE => match event_mask("register", argument) {
            Ok(mask) => mask as u32,
            Err(err) => return err,
        },
        _ => libc::EPOLLIN as u32,
    };
    let mut event = libc::epoll_event {
        events: mask,
        u64: target as u64,
    };
    // SAFETY: `event` is a valid, fully initialised `epoll_event`.
    let result =
        unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, target, &mut event) };
    if result < 0 {
        return epoll_io_error(std::io::Error::last_os_error());
    }
    NONE
}

/// `epoll.modify(fd, eventmask)`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_modify(args: &[i64]) -> i64 {
    let object = args[0];
    let epfd = match epoll_fd(object) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let target = match file_descriptor(args[1]) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let mask = match event_mask("modify", args[2]) {
        Ok(mask) => mask as u32,
        Err(err) => return err,
    };
    let mut event = libc::epoll_event {
        events: mask,
        u64: target as u64,
    };
    // SAFETY: `event` is a valid, fully initialised `epoll_event`.
    let result =
        unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_MOD, target, &mut event) };
    if result < 0 {
        return epoll_io_error(std::io::Error::last_os_error());
    }
    NONE
}

/// `epoll.unregister(fd)`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_unregister(args: &[i64]) -> i64 {
    let object = args[0];
    let epfd = match epoll_fd(object) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let target = match file_descriptor(args[1]) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    // SAFETY: `EPOLL_CTL_DEL` ignores the event argument on Linux; a null
    // pointer is accepted (kernel >= 2.6.9).
    let result = unsafe {
        libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, target, std::ptr::null_mut())
    };
    if result < 0 {
        return epoll_io_error(std::io::Error::last_os_error());
    }
    NONE
}

/// `epoll.poll(timeout=-1, maxevents=-1)`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_poll(args: &[i64]) -> i64 {
    let object = args[0];
    let epfd = match epoll_fd(object) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let timeout = match args.get(1) {
        Some(&argument) => match epoll_timeout(argument) {
            Ok(timeout) => timeout,
            Err(err) => return err,
        },
        None => -1,
    };
    let maxevents = match args.get(2) {
        Some(&argument) if argument != NONE => {
            let Some(number) = value::as_integer(argument) else {
                if value::as_bigint(argument).is_some() {
                    return exc::raise_with_message(
                        OVERFLOW_ERROR,
                        "maxevents is out of range",
                    );
                }
                return type_error(&format!(
                    "maxevents must be an integer, not {}",
                    value::type_name(argument)
                ));
            };
            if number <= 0 {
                return value_error("maxevents must be greater than zero");
            }
            number.min(1 << 20) as usize
        }
        _ => 128,
    };
    let mut events = vec![unsafe { std::mem::zeroed::<libc::epoll_event>() }; maxevents];
    let blocking = timeout != 0;
    if blocking {
        heap::blocking_enter();
    }
    // SAFETY: `events` has room for `maxevents` entries.
    let count = unsafe {
        libc::epoll_wait(
            epfd,
            events.as_mut_ptr(),
            maxevents as libc::c_int,
            timeout,
        )
    };
    if blocking {
        heap::blocking_leave();
    }
    if count < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return value::list_new(Vec::new());
        }
        return epoll_io_error(error);
    }
    let items: Vec<i64> = events[..count as usize]
        .iter()
        .map(|event| {
            value::tuple_new(vec![
                encode_int(event.u64 as i64),
                encode_int(i64::from(event.events)),
            ])
        })
        .collect();
    value::list_new(items)
}

/// `epoll.fileno()`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_fileno(args: &[i64]) -> i64 {
    match epoll_fd(args[0]) {
        Ok(fd) => encode_int(i64::from(fd)),
        Err(err) => err,
    }
}

/// `epoll.close()`; idempotent.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_close(args: &[i64]) -> i64 {
    let object = args[0];
    if value::instance_attr(object, "closed")
        .map(value::truthy)
        .unwrap_or(false)
    {
        return NONE;
    }
    if let Some(fd) = value::instance_attr(object, "_fd").and_then(value::as_integer) {
        if let Ok(fd) = i32::try_from(fd) {
            // SAFETY: closing our own descriptor; errors are irrelevant to the
            // idempotent `close()` contract.
            unsafe {
                libc::close(fd);
            }
        }
    }
    set_instance_attr(object, "closed", value::TRUE);
    NONE
}

/// `epoll.__enter__()`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_enter(args: &[i64]) -> i64 {
    match epoll_fd(args[0]) {
        Ok(_) => args[0],
        Err(err) => err,
    }
}

/// `epoll.__exit__(...)` closes the object.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_exit(args: &[i64]) -> i64 {
    epoll_close(args)
}

/// `epoll.fromfd(fd)`: a new epoll object over a duplicate of `fd`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn epoll_fromfd(args: &[i64]) -> i64 {
    let Some(number) = value::as_integer(args[0]) else {
        if value::as_bigint(args[0]).is_some() {
            return exc::raise_with_message(OVERFLOW_ERROR, "fd is out of range");
        }
        return type_error(&format!(
            "argument must be an int, not {}",
            value::type_name(args[0])
        ));
    };
    if number < 0 {
        return value_error("file descriptor cannot be a negative integer");
    }
    let Ok(fd) = i32::try_from(number) else {
        return exc::raise_with_message(OVERFLOW_ERROR, "fd is out of range");
    };
    // SAFETY: `dup` only reads the descriptor number.
    let duplicate = unsafe { libc::dup(fd) };
    if duplicate < 0 {
        return epoll_io_error(std::io::Error::last_os_error());
    }
    // SAFETY: setting close-on-exec on our own duplicate.
    unsafe {
        libc::fcntl(duplicate, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    epoll_instance(duplicate)
}

// -------------------------------------------------------------------------
// selectors
// -------------------------------------------------------------------------

struct Registration {
    fd: i32,
    events: i16,
    key: i64,
}

fn selector_key_class(selector: i64) -> i64 {
    if let Some(class) = value::instance_attr(selector, "_key_class") {
        return class;
    }
    let class = value::class_new("SelectorKey".to_string(), NONE, Vec::new());
    set_instance_attr(selector, "_key_class", class);
    class
}

fn new_key(selector: i64, fd: i32, fileobj: i64, events: i64, data: i64) -> i64 {
    let key = value::instance_new(selector_key_class(selector));
    set_instance_attr(key, "fd", encode_int(fd as i64));
    set_instance_attr(key, "fileobj", fileobj);
    set_instance_attr(key, "events", encode_int(events));
    set_instance_attr(key, "data", data);
    key
}

fn selector_entries(selector: i64) -> Vec<Registration> {
    let mut entries = Vec::new();
    for item in instance_list(selector, "_entries") {
        let Some(entry) = value::as_list(item) else {
            continue;
        };
        if entry.len() != 3 {
            continue;
        }
        let (Some(fd), Some(events), key) = (
            value::as_integer(entry[0]),
            value::as_integer(entry[1]),
            entry[2],
        ) else {
            continue;
        };
        if let Ok(fd) = i32::try_from(fd) {
            entries.push(Registration {
                fd,
                events: events as i16,
                key,
            });
        }
    }
    entries
}

fn store_selector_entries(selector: i64, entries: Vec<Registration>) {
    let items: Vec<i64> = entries
        .into_iter()
        .map(|entry| {
            value::list_new(vec![
                encode_int(entry.fd as i64),
                encode_int(entry.events as i64),
                entry.key,
            ])
        })
        .collect();
    set_instance_attr(selector, "_entries", value::list_new(items));
}

fn selector_object(name: &str) -> i64 {
    let methods = vec![
        (
            "register".to_string(),
            value::native_function_new("selectors", "_selector_register", 4),
        ),
        (
            "unregister".to_string(),
            value::native_function_new("selectors", "_selector_unregister", 2),
        ),
        (
            "modify".to_string(),
            value::native_function_new("selectors", "_selector_modify", 4),
        ),
        (
            "select".to_string(),
            value::native_function_new("selectors", "_selector_select", 2),
        ),
        (
            "get_key".to_string(),
            value::native_function_new("selectors", "_selector_get_key", 2),
        ),
        (
            "get_map".to_string(),
            value::native_function_new("selectors", "_selector_get_map", 1),
        ),
        (
            "close".to_string(),
            value::native_function_new("selectors", "_selector_close", 1),
        ),
        (
            "__enter__".to_string(),
            value::native_function_new("selectors", "_selector_enter", 1),
        ),
        (
            "__exit__".to_string(),
            value::native_function_new("selectors", "_selector_exit", 4),
        ),
    ];
    let class = value::class_new(name.to_string(), NONE, methods);
    let selector = value::instance_new(class);
    set_instance_attr(selector, "_entries", value::list_new(Vec::new()));
    set_instance_attr(selector, "_closed", value::FALSE);
    selector
}

fn selector_events_mask(events: i64) -> i16 {
    let mut mask = 0i16;
    if events & EVENT_READ != 0 {
        mask |= POLLIN as i16;
    }
    if events & EVENT_WRITE != 0 {
        mask |= POLLOUT as i16;
    }
    mask
}

fn selector_revents(revents: i16) -> i64 {
    let mut events = 0i64;
    if revents & (POLLIN | POLLPRI | POLLERR | POLLHUP | POLLNVAL) as i16 != 0 {
        events |= EVENT_READ;
    }
    if revents & (POLLOUT | POLLERR | POLLHUP | POLLNVAL) as i16 != 0 {
        events |= EVENT_WRITE;
    }
    events
}

fn selector_register(args: &[i64]) -> i64 {
    let selector = args[0];
    if is_closed(selector) {
        return value_error("I/O operation on closed selector");
    }
    let fd = match file_descriptor(args[1]) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let Some(events) = value::as_integer(args[2]) else {
        return type_error("events must be an integer");
    };
    let data = args.get(3).copied().unwrap_or(NONE);
    let mut entries = selector_entries(selector);
    if entries.iter().any(|entry| entry.fd == fd) {
        return key_error(&format!("fd {fd} is already registered"));
    }
    let key = new_key(selector, fd, args[1], events, data);
    entries.push(Registration {
        fd,
        events: events as i16,
        key,
    });
    store_selector_entries(selector, entries);
    key
}

fn selector_unregister(args: &[i64]) -> i64 {
    let selector = args[0];
    if is_closed(selector) {
        return value_error("I/O operation on closed selector");
    }
    let fd = match file_descriptor(args[1]) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let mut entries = selector_entries(selector);
    let Some(index) = entries.iter().position(|entry| entry.fd == fd) else {
        return key_error(&format!("fd {fd} is not registered"));
    };
    let key = entries.remove(index).key;
    store_selector_entries(selector, entries);
    key
}

fn selector_modify(args: &[i64]) -> i64 {
    let selector = args[0];
    if is_closed(selector) {
        return value_error("I/O operation on closed selector");
    }
    let fd = match file_descriptor(args[1]) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let Some(events) = value::as_integer(args[2]) else {
        return type_error("events must be an integer");
    };
    let data = args.get(3).copied().unwrap_or(NONE);
    let mut entries = selector_entries(selector);
    let Some(index) = entries.iter().position(|entry| entry.fd == fd) else {
        return key_error(&format!("fd {fd} is not registered"));
    };
    let key = new_key(selector, fd, args[1], events, data);
    entries[index].events = events as i16;
    entries[index].key = key;
    store_selector_entries(selector, entries);
    key
}

fn selector_select(args: &[i64]) -> i64 {
    let selector = args[0];
    if is_closed(selector) {
        return value_error("I/O operation on closed selector");
    }
    let timeout = match args.get(1) {
        Some(&value) => match seconds_argument(value) {
            Ok(timeout) => timeout,
            Err(err) => return err,
        },
        None => None,
    };
    let entries = selector_entries(selector);
    let mut poll_set: Vec<(i32, i16)> = Vec::new();
    for entry in &entries {
        let mask = selector_events_mask(entry.events as i64);
        match poll_set.iter_mut().find(|(fd, _)| *fd == entry.fd) {
            Some(existing) => existing.1 |= mask,
            None => poll_set.push((entry.fd, mask)),
        }
    }
    let ready = match poll_entries(&poll_set, timeout) {
        Ok(ready) => ready,
        Err(error) => return os_error(&error.to_string()),
    };
    let mut results = Vec::new();
    for (fd, revents) in ready {
        if let Some(entry) = entries.iter().find(|entry| entry.fd == fd) {
            results.push(value::tuple_new(vec![
                entry.key,
                encode_int(selector_revents(revents)),
            ]));
        }
    }
    value::list_new(results)
}

fn selector_get_key(args: &[i64]) -> i64 {
    let selector = args[0];
    let fd = match file_descriptor(args[1]) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    match selector_entries(selector)
        .into_iter()
        .find(|entry| entry.fd == fd)
    {
        Some(entry) => entry.key,
        None => key_error(&format!("fd {fd} is not registered")),
    }
}

fn selector_get_map(args: &[i64]) -> i64 {
    let selector = args[0];
    let pairs: Vec<(i64, i64)> = selector_entries(selector)
        .into_iter()
        .map(|entry| (encode_int(entry.fd as i64), entry.key))
        .collect();
    value::dict_new(pairs)
}

fn selector_close(args: &[i64]) -> i64 {
    let selector = args[0];
    set_instance_attr(selector, "_entries", value::list_new(Vec::new()));
    set_instance_attr(selector, "_closed", value::TRUE);
    NONE
}

/// Module-level dispatch for `select`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "select" => select_call(args),
        "poll" => poll_new(),
        "_poll_register" => poll_register(args),
        "_poll_unregister" => poll_unregister(args),
        "_poll_modify" => poll_modify(args),
        "_poll_poll" => poll_poll(args),
        "_poll_close" => poll_close(args),
        "_poll_enter" => {
            if is_closed(args[0]) {
                value_error("I/O operation on closed poll object")
            } else {
                args[0]
            }
        }
        "_poll_exit" => poll_close(args),
        "_poll_init" => poll_init(args),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "_epoll_init" => epoll_init(args),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "_epoll_register" => epoll_register(args),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "_epoll_unregister" => epoll_unregister(args),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "_epoll_modify" => epoll_modify(args),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "_epoll_poll" => epoll_poll(args),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "_epoll_fileno" => epoll_fileno(args),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "_epoll_close" => epoll_close(args),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "_epoll_enter" => epoll_enter(args),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "_epoll_exit" => epoll_exit(args),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "_epoll_fromfd" => epoll_fromfd(args),
        _ => attribute_error("select", function),
    }
}

/// Keyword-aware `select.select(..., timeout=...)`.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: Vec<(String, i64)>) -> i64 {
    if function != "select" {
        return type_error(&format!(
            "select.{function}() does not accept keyword arguments"
        ));
    }
    let mut bound = args.to_vec();
    for (name, item) in keywords {
        if name != "timeout" {
            return type_error(&format!(
                "select.select() got an unexpected keyword argument '{name}'"
            ));
        }
        while bound.len() <= 3 {
            bound.push(NONE);
        }
        if bound[3] != NONE {
            return type_error("select.select() got multiple values for argument 'timeout'");
        }
        bound[3] = item;
    }
    call("select", &bound)
}

/// Module-level dispatch for `selectors`.
pub(crate) fn selectors_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "DefaultSelector" | "SelectSelector" | "PollSelector" => selector_object(function),
        "_selector_register" => selector_register(args),
        "_selector_unregister" => selector_unregister(args),
        "_selector_modify" => selector_modify(args),
        "_selector_select" => selector_select(args),
        "_selector_get_key" => selector_get_key(args),
        "_selector_get_map" => selector_get_map(args),
        "_selector_close" => selector_close(args),
        "_selector_enter" => {
            if is_closed(args[0]) {
                value_error("I/O operation on closed selector")
            } else {
                args[0]
            }
        }
        "_selector_exit" => selector_close(args),
        _ => attribute_error("selectors", function),
    }
}
