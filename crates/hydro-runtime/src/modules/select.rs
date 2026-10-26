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

use crate::exc::{self, ATTRIBUTE_ERROR, KEY_ERROR, OS_ERROR, TYPE_ERROR, VALUE_ERROR};
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
        _ => attribute_error("select", name),
    }
}

/// Module constants for `selectors`.
pub(crate) fn selectors_constant(name: &str) -> i64 {
    match name {
        "EVENT_READ" => encode_int(EVENT_READ),
        "EVENT_WRITE" => encode_int(EVENT_WRITE),
        _ => attribute_error("selectors", name),
    }
}

/// Resolves a descriptor from an integer or a native socket object.
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
    Err(type_error(
        "argument must be an int, or a socket object with a file descriptor",
    ))
}

fn sequence(v: i64) -> Result<Vec<i64>, i64> {
    if let Some(items) = value::as_list(v) {
        return Ok(items.to_vec());
    }
    if let Some(items) = value::as_tuple(v) {
        return Ok(items.to_vec());
    }
    Err(type_error(&format!(
        "expected a list or tuple, not {}",
        value::type_name(v)
    )))
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
fn milliseconds_argument(v: i64) -> Result<Option<Duration>, i64> {
    if v == NONE {
        return Ok(None);
    }
    let millis = if let Some(integer) = value::as_integer(v) {
        integer as f64
    } else if let Some(float) = value::as_float(v) {
        float
    } else {
        return Err(type_error("timeout must be an int, float or None"));
    };
    if millis.is_nan() || millis < 0.0 || !millis.is_finite() || millis >= u64::MAX as f64 {
        return Ok(None);
    }
    Ok(Some(Duration::from_millis(millis as u64)))
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

fn add_event(events: &mut Vec<(i32, i16)>, item: i64, mask: i16) -> Result<(), i64> {
    let fd = file_descriptor(item)?;
    match events.iter_mut().find(|(existing, _)| *existing == fd) {
        Some(entry) => entry.1 |= mask,
        None => events.push((fd, mask)),
    }
    Ok(())
}

fn select_call(args: &[i64]) -> i64 {
    let readables = match sequence(args[0]) {
        Ok(items) => items,
        Err(err) => return err,
    };
    let writables = match sequence(args[1]) {
        Ok(items) => items,
        Err(err) => return err,
    };
    let exceptionals = match sequence(args[2]) {
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
    for &item in &readables {
        if let Err(err) = add_event(&mut events, item, POLLIN as i16) {
            return err;
        }
    }
    for &item in &writables {
        if let Err(err) = add_event(&mut events, item, POLLOUT as i16) {
            return err;
        }
    }
    for &item in &exceptionals {
        if let Err(err) = add_event(&mut events, item, POLLPRI as i16) {
            return err;
        }
    }
    let ready = match poll_entries(&events, timeout) {
        Ok(ready) => ready,
        Err(error) => return os_error(&error.to_string()),
    };
    let revents = |fd: i32| {
        ready
            .iter()
            .find(|(ready_fd, _)| *ready_fd == fd)
            .map(|(_, mask)| *mask)
    };
    let readable_mask = (POLLIN | POLLERR | POLLHUP | POLLNVAL) as i16;
    let writable_mask = (POLLOUT | POLLERR | POLLHUP | POLLNVAL) as i16;
    let exceptional_mask = (POLLPRI | POLLERR | POLLNVAL) as i16;
    let filter = |items: &[i64], mask: i16| -> Result<i64, i64> {
        let mut out = Vec::new();
        for &item in items {
            let fd = file_descriptor(item)?;
            if revents(fd).is_some_and(|found| found & mask != 0) {
                out.push(item);
            }
        }
        Ok(value::list_new(out))
    };
    let read = match filter(&readables, readable_mask) {
        Ok(list) => list,
        Err(err) => return err,
    };
    let write = match filter(&writables, writable_mask) {
        Ok(list) => list,
        Err(err) => return err,
    };
    let exceptional = match filter(&exceptionals, exceptional_mask) {
        Ok(list) => list,
        Err(err) => return err,
    };
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
        Some(&value) => match value::as_integer(value) {
            Some(mask) => mask as i16,
            None => return type_error("eventmask must be an integer"),
        },
        None => (POLLIN | POLLPRI | POLLOUT) as i16,
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
    let Some(mask) = value::as_integer(args[2]) else {
        return type_error("eventmask must be an integer");
    };
    let mut entries = poll_entries_attr(object);
    let Some(index) = entries.iter().position(|(existing, _)| *existing == fd) else {
        return key_error(&format!("fd {fd} not registered"));
    };
    entries[index].1 = mask as i16;
    store_poll_entries(object, entries);
    NONE
}

fn poll_poll(args: &[i64]) -> i64 {
    let object = args[0];
    if is_closed(object) {
        return value_error("I/O operation on closed poll object");
    }
    let timeout = match args.get(1) {
        Some(&value) => match milliseconds_argument(value) {
            Ok(timeout) => timeout,
            Err(err) => return err,
        },
        None => None,
    };
    let entries = poll_entries_attr(object);
    let ready = match poll_entries(&entries, timeout) {
        Ok(ready) => ready,
        Err(error) => return os_error(&error.to_string()),
    };
    let items: Vec<i64> = ready
        .into_iter()
        .map(|(fd, mask)| value::tuple_new(vec![encode_int(fd as i64), encode_int(mask as i64)]))
        .collect();
    value::list_new(items)
}

fn poll_close(args: &[i64]) -> i64 {
    let object = args[0];
    set_instance_attr(object, "_entries", value::list_new(Vec::new()));
    set_instance_attr(object, "_closed", value::TRUE);
    NONE
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
