//! `socket`: blocking TCP sockets over `std::net` (AF_INET / SOCK_STREAM).
//!
//! A socket object is a childless heap object holding a mutex-protected
//! [`SocketInner`]. The native kind starts unbound, becomes a [`TcpListener`]
//! after `bind` and a [`TcpStream`] after `connect`/`accept`; dropping the
//! object closes the underlying descriptor.
//!
//! Every operation that may block (`connect`, `accept`, `recv`, `sendall`,
//! host resolution and `poll`) parks the calling thread for the collector via
//! [`heap::blocking_enter`]/[`heap::blocking_leave`]. The socket mutex is
//! never held across such a block: the operation works on a duplicated
//! descriptor (`try_clone`) taken under the lock, so a second thread waiting
//! for the mutex can always reach a GC safepoint.
//!
//! Timeouts are stored on the object and applied to the duplicated stream
//! before each operation. A zero timeout puts the descriptor in non-blocking
//! mode; `accept` uses `poll(2)` for both finite timeouts and the zero case.
//! A timeout raises `OSError("timed out")` (the runtime has no
//! `socket.timeout` class).

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::exc::{self, OS_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

/// Family/type/protocol constants exposed by the module.
const AF_INET: i64 = 2;
const SOCK_STREAM: i64 = 1;
const IPPROTO_TCP: i64 = 6;

const SHUT_RD: i64 = 0;
const SHUT_WR: i64 = 1;
const SHUT_RDWR: i64 = 2;

/// Native socket state.
pub(crate) enum SocketKind {
    /// Created but not bound to an address yet.
    Unbound,
    /// Bound and listening for connections.
    Listener(TcpListener),
    /// Connected (or accepted) stream.
    Stream(TcpStream),
}

/// Mutex-protected payload of a `socket` object.
pub(crate) struct SocketInner {
    kind: SocketKind,
    timeout: Option<Duration>,
    closed: bool,
    /// Values recorded by `setsockopt`, returned by `getsockopt`.
    options: Vec<((i32, i32), i64)>,
}

/// Heap payload of a `socket` object.
pub(crate) struct SocketState {
    inner: Mutex<SocketInner>,
}

impl SocketState {
    /// A freshly created, unbound socket.
    pub(crate) fn unbound(timeout: Option<Duration>) -> SocketState {
        SocketState {
            inner: Mutex::new(SocketInner {
                kind: SocketKind::Unbound,
                timeout,
                closed: false,
                options: Vec::new(),
            }),
        }
    }

    /// A connected stream socket with the given timeout.
    pub(crate) fn stream(stream: TcpStream, timeout: Option<Duration>) -> SocketState {
        SocketState {
            inner: Mutex::new(SocketInner {
                kind: SocketKind::Stream(stream),
                timeout,
                closed: false,
                options: Vec::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, SocketInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Renders the object like CPython's `repr(socket)`.
    pub(crate) fn render(&self) -> String {
        let inner = self.lock();
        if inner.closed {
            return "<socket closed>".to_string();
        }
        match &inner.kind {
            SocketKind::Unbound => "<socket object, unbound>".to_string(),
            SocketKind::Listener(listener) => {
                format!(
                    "<socket object, fd={}, listening>",
                    raw_fd_listener(listener)
                )
            }
            SocketKind::Stream(stream) => {
                format!("<socket object, fd={}>", raw_fd_stream(stream))
            }
        }
    }
}

impl Drop for SocketState {
    fn drop(&mut self) {
        let inner = self
            .inner
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.closed = true;
        // Dropping the payload closes the descriptor.
        inner.kind = SocketKind::Unbound;
    }
}

#[cfg(unix)]
fn raw_fd_listener(listener: &TcpListener) -> i32 {
    use std::os::unix::io::AsRawFd;
    listener.as_raw_fd()
}

#[cfg(not(unix))]
fn raw_fd_listener(_listener: &TcpListener) -> i32 {
    -1
}

#[cfg(unix)]
fn raw_fd_stream(stream: &TcpStream) -> i32 {
    use std::os::unix::io::AsRawFd;
    stream.as_raw_fd()
}

#[cfg(not(unix))]
fn raw_fd_stream(_stream: &TcpStream) -> i32 {
    -1
}

fn os_error(message: &str) -> i64 {
    exc::raise_with_message(OS_ERROR, message)
}

/// Python-style `OSError` text, e.g. `[Errno 111] Connection refused`.
fn io_error(error: io::Error) -> i64 {
    let text = error.to_string();
    match error.raw_os_error() {
        Some(code) => {
            let detail = text.split(" (os error").next().unwrap_or(text.as_str());
            os_error(&format!("[Errno {code}] {detail}"))
        }
        None => os_error(&text),
    }
}

fn timeout_error() -> i64 {
    os_error("timed out")
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn descriptor_error() -> i64 {
    os_error("[Errno 9] Bad file descriptor")
}

fn not_connected_io() -> io::Error {
    io::Error::from_raw_os_error(107)
}

fn invalid_argument_error() -> i64 {
    os_error("[Errno 22] Invalid argument")
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn encode_int(n: i64) -> i64 {
    value::encode_int(n)
        .unwrap_or_else(|| exc::raise_with_message(crate::exc::OVERFLOW_ERROR, "integer overflow"))
}

fn arity(method: &str, args: &[i64], min: usize, max: usize) -> Option<i64> {
    if args.len() >= min && args.len() <= max {
        return None;
    }
    let detail = if min == max {
        if min == 1 {
            "exactly one argument".to_string()
        } else {
            format!("exactly {min} arguments")
        }
    } else {
        format!("{min} to {max} arguments")
    };
    Some(type_error(&format!(
        "socket.{method}() takes {detail} ({} given)",
        args.len()
    )))
}

fn int_argument(v: i64, position: &str) -> Result<i64, i64> {
    value::as_integer(v).ok_or_else(|| {
        type_error(&format!(
            "{position} must be an integer, not {}",
            value::type_name(v)
        ))
    })
}

/// Decodes a `(host, port)` 2-tuple (or 2-list).
fn address_argument(v: i64) -> Result<(String, u16), i64> {
    let items = value::as_tuple(v).or_else(|| value::as_list(v));
    let Some(items) = items else {
        return Err(type_error(&format!(
            "address must be a pair (host, port), not {}",
            value::type_name(v)
        )));
    };
    if items.len() != 2 {
        return Err(os_error("address must be a 2-tuple (host, port)"));
    }
    let host = value::as_str(items[0]).ok_or_else(|| {
        type_error(&format!(
            "host must be str, not {}",
            value::type_name(items[0])
        ))
    })?;
    let port = int_argument(items[1], "port")?;
    let port = u16::try_from(port).map_err(|_| os_error("port must be 0-65535"))?;
    Ok((host.to_string(), port))
}

/// Decodes `None` or a non-negative number of seconds.
fn timeout_argument(v: i64) -> Result<Option<Duration>, i64> {
    if v == NONE {
        return Ok(None);
    }
    let seconds = if let Some(integer) = value::as_integer(v) {
        integer as f64
    } else if let Some(float) = value::as_float(v) {
        float
    } else {
        return Err(type_error(&format!(
            "timeout must be a float or None, not {}",
            value::type_name(v)
        )));
    };
    if seconds.is_nan() || seconds < 0.0 || !seconds.is_finite() || seconds >= u64::MAX as f64 {
        return Err(value_error("Timeout value out of range"));
    }
    Ok(Some(Duration::from_secs_f64(seconds)))
}

fn tuple_addr(addr: SocketAddr) -> i64 {
    let host = value::str_alloc(addr.ip().to_string());
    let port = encode_int(addr.port() as i64);
    value::tuple_new(vec![host, port])
}

/// Resolves `host`/`port`, parking for the collector while the resolver runs.
fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, io::Error> {
    heap::blocking_enter();
    let resolved = (host, port).to_socket_addrs();
    heap::blocking_leave();
    let mut addrs: Vec<SocketAddr> = resolved?.collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Name or service not known: {host}"),
        ));
    }
    addrs.shrink_to_fit();
    Ok(addrs)
}

/// Runs `operation` while parked for the collector unless the timeout is zero
/// (a zero timeout makes the descriptor non-blocking, so the call returns
/// immediately without waiting).
fn parked<T>(timeout: Option<Duration>, operation: impl FnOnce() -> T) -> T {
    if timeout.is_none_or(|limit| !limit.is_zero()) {
        heap::blocking_enter();
        let result = operation();
        heap::blocking_leave();
        result
    } else {
        operation()
    }
}

/// Applies `timeout` to a duplicated stream before a read/write operation.
fn prepare(stream: &TcpStream, timeout: Option<Duration>, read: bool) -> Result<(), i64> {
    let result = match timeout {
        None => {
            let result = stream.set_nonblocking(false);
            result.and_then(|_| {
                if read {
                    stream.set_read_timeout(None)
                } else {
                    stream.set_write_timeout(None)
                }
            })
        }
        Some(limit) if limit.is_zero() => stream.set_nonblocking(true),
        Some(limit) => {
            let result = stream.set_nonblocking(false);
            result.and_then(|_| {
                if read {
                    stream.set_read_timeout(Some(limit))
                } else {
                    stream.set_write_timeout(Some(limit))
                }
            })
        }
    };
    result.map_err(io_error)
}

/// Clones the connected stream and snapshots its timeout.
fn stream_snapshot(state: &SocketState) -> Result<(TcpStream, Option<Duration>), i64> {
    let snapshot = {
        let inner = state.lock();
        if inner.closed {
            None
        } else {
            match &inner.kind {
                SocketKind::Stream(stream) => {
                    Some(stream.try_clone().map(|copy| (copy, inner.timeout)))
                }
                SocketKind::Unbound | SocketKind::Listener(_) => Some(Err(not_connected_io())),
            }
        }
    };
    match snapshot {
        None => Err(descriptor_error()),
        Some(Ok(snapshot)) => Ok(snapshot),
        Some(Err(error)) => Err(io_error(error)),
    }
}

fn map_stream_error(error: io::Error) -> i64 {
    if is_timeout(&error) {
        timeout_error()
    } else {
        io_error(error)
    }
}

/// Connects to the first reachable address, honouring the timeout.
fn connect_stream(
    host: &str,
    port: u16,
    timeout: Option<Duration>,
) -> Result<TcpStream, io::Error> {
    let addrs = resolve(host, port)?;
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no address to connect to");
    match timeout {
        None => {
            heap::blocking_enter();
            for addr in &addrs {
                match TcpStream::connect(*addr) {
                    Ok(stream) => {
                        heap::blocking_leave();
                        return Ok(stream);
                    }
                    Err(error) => last = error,
                }
            }
            heap::blocking_leave();
            Err(last)
        }
        Some(limit) => {
            let deadline = Instant::now().checked_add(limit);
            for addr in &addrs {
                let remaining = match deadline {
                    Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                    None => limit,
                };
                if limit.is_zero() || remaining.is_zero() {
                    if !limit.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
                    }
                    heap::blocking_enter();
                    let result = TcpStream::connect_timeout(addr, Duration::from_micros(1));
                    heap::blocking_leave();
                    match result {
                        Ok(stream) => return Ok(stream),
                        Err(error) => {
                            last = error;
                            continue;
                        }
                    }
                }
                heap::blocking_enter();
                let result = TcpStream::connect_timeout(addr, remaining);
                heap::blocking_leave();
                match result {
                    Ok(stream) => return Ok(stream),
                    Err(error) => last = error,
                }
            }
            if is_timeout(&last) {
                Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"))
            } else {
                Err(last)
            }
        }
    }
}

/// Progress view of `socket()` validation.
fn construct(args: &[i64]) -> i64 {
    let family = match args.first() {
        Some(&v) if v != NONE => match int_argument(v, "family") {
            Ok(family) => family,
            Err(err) => return err,
        },
        _ => AF_INET,
    };
    let kind = match args.get(1) {
        Some(&v) if v != NONE => match int_argument(v, "type") {
            Ok(kind) => kind,
            Err(err) => return err,
        },
        _ => SOCK_STREAM,
    };
    let proto = match args.get(2) {
        Some(&v) if v != NONE => match int_argument(v, "proto") {
            Ok(proto) => proto,
            Err(err) => return err,
        },
        _ => 0,
    };
    if family != AF_INET {
        return os_error("[Errno 97] Address family not supported by protocol");
    }
    if kind != SOCK_STREAM {
        return os_error("[Errno 93] Protocol not supported");
    }
    if proto != 0 && proto != IPPROTO_TCP {
        return os_error("[Errno 93] Protocol not supported");
    }
    value::socket_new(SocketState::unbound(None))
}

/// `socket.create_connection(address, timeout=None)`.
fn create_connection(args: &[i64]) -> i64 {
    let (host, port) = match address_argument(args[0]) {
        Ok(address) => address,
        Err(err) => return err,
    };
    let timeout = match args.get(1) {
        Some(&v) => match timeout_argument(v) {
            Ok(timeout) => timeout,
            Err(err) => return err,
        },
        None => None,
    };
    match connect_stream(&host, port, timeout) {
        Ok(stream) => value::socket_new(SocketState::stream(stream, timeout)),
        Err(error) => io_error(error),
    }
}

/// `socket.gethostname()`: the kernel host name, or `"localhost"`.
fn gethostname() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer and length are valid for the call; `gethostname`
        // writes at most `len` bytes and the result is treated as a C string.
        let result = unsafe {
            libc::gethostname(
                buffer.as_mut_ptr() as *mut libc::c_char,
                buffer.len() as libc::size_t,
            )
        };
        if result == 0 {
            let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
            if end > 0 {
                return String::from_utf8_lossy(&buffer[..end]).into_owned();
            }
        }
    }
    std::env::var("HOSTNAME")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

/// Module-level function dispatch.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "socket" => construct(args),
        "create_connection" => create_connection(args),
        "gethostname" => value::str_alloc(gethostname()),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'socket' has no attribute '{function}'"),
        ),
    }
}

/// Method dispatch for a `socket` object.
pub(crate) fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    let Some(state) = value::as_socket(obj) else {
        return exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("'socket' object has no attribute '{name}'"),
        );
    };
    match name {
        "bind" => {
            if let Some(err) = arity(name, args, 1, 1) {
                return err;
            }
            bind(state, args[0])
        }
        "listen" => {
            if let Some(err) = arity(name, args, 0, 1) {
                return err;
            }
            if let Some(&backlog) = args.first() {
                if value::as_integer(backlog).is_none() {
                    return type_error("backlog must be an integer");
                }
            }
            let (closed, listening) = {
                let inner = state.lock();
                (inner.closed, matches!(&inner.kind, SocketKind::Listener(_)))
            };
            if closed {
                return descriptor_error();
            }
            if listening {
                NONE
            } else {
                invalid_argument_error()
            }
        }
        "accept" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            accept(state)
        }
        "connect" => {
            if let Some(err) = arity(name, args, 1, 1) {
                return err;
            }
            let (host, port) = match address_argument(args[0]) {
                Ok(address) => address,
                Err(err) => return err,
            };
            connect(state, &host, port)
        }
        "connect_ex" => {
            if let Some(err) = arity(name, args, 1, 1) {
                return err;
            }
            let (host, port) = match address_argument(args[0]) {
                Ok(address) => address,
                Err(_) => return encode_int(libc_einval()),
            };
            connect_ex(state, &host, port)
        }
        "send" => {
            if let Some(err) = arity(name, args, 1, 2) {
                return err;
            }
            send(state, args)
        }
        "sendall" => {
            if let Some(err) = arity(name, args, 1, 2) {
                return err;
            }
            send_all(state, args)
        }
        "recv" => {
            if let Some(err) = arity(name, args, 1, 2) {
                return err;
            }
            recv(state, args)
        }
        "close" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            let mut inner = state.lock();
            inner.closed = true;
            inner.kind = SocketKind::Unbound;
            NONE
        }
        "shutdown" => {
            if let Some(err) = arity(name, args, 1, 1) {
                return err;
            }
            shutdown(state, args[0])
        }
        "settimeout" => {
            if let Some(err) = arity(name, args, 1, 1) {
                return err;
            }
            let timeout = match timeout_argument(args[0]) {
                Ok(timeout) => timeout,
                Err(err) => return err,
            };
            state.lock().timeout = timeout;
            NONE
        }
        "gettimeout" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            let timeout = state.lock().timeout;
            match timeout {
                Some(limit) => value::float_new(limit.as_secs_f64()),
                None => NONE,
            }
        }
        "setsockopt" => {
            if let Some(err) = arity(name, args, 3, 3) {
                return err;
            }
            let level = match int_argument(args[0], "level") {
                Ok(level) => level as i32,
                Err(err) => return err,
            };
            let option = match int_argument(args[1], "optname") {
                Ok(option) => option as i32,
                Err(err) => return err,
            };
            let stored = if let Some(integer) = value::as_integer(args[2]) {
                integer
            } else if value::as_bytes(args[2]).is_some() {
                1
            } else {
                return type_error(&format!(
                    "a bytes-like object or integer is required, not {}",
                    value::type_name(args[2])
                ));
            };
            let mut inner = state.lock();
            if inner.closed {
                drop(inner);
                return descriptor_error();
            }
            let existing = inner
                .options
                .iter()
                .position(|((stored_level, stored_option), _)| {
                    *stored_level == level && *stored_option == option
                });
            match existing {
                Some(index) => inner.options[index].1 = stored,
                None => inner.options.push(((level, option), stored)),
            }
            NONE
        }
        "getsockopt" => {
            if let Some(err) = arity(name, args, 2, 2) {
                return err;
            }
            let level = match int_argument(args[0], "level") {
                Ok(level) => level as i32,
                Err(err) => return err,
            };
            let option = match int_argument(args[1], "optname") {
                Ok(option) => option as i32,
                Err(err) => return err,
            };
            let stored = {
                let inner = state.lock();
                inner
                    .options
                    .iter()
                    .find(|((stored_level, stored_option), _)| {
                        *stored_level == level && *stored_option == option
                    })
                    .map(|(_, value)| *value)
                    .unwrap_or(0)
            };
            encode_int(stored)
        }
        "getsockname" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            let result: Result<SocketAddr, Option<io::Error>> = {
                let inner = state.lock();
                if inner.closed {
                    Err(None)
                } else {
                    match &inner.kind {
                        SocketKind::Unbound => Ok(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))),
                        SocketKind::Listener(listener) => listener.local_addr().map_err(Some),
                        SocketKind::Stream(stream) => stream.local_addr().map_err(Some),
                    }
                }
            };
            match result {
                Ok(addr) => tuple_addr(addr),
                Err(None) => descriptor_error(),
                Err(Some(error)) => io_error(error),
            }
        }
        "getpeername" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            let result: Result<SocketAddr, Option<io::Error>> = {
                let inner = state.lock();
                if inner.closed {
                    Err(None)
                } else {
                    match &inner.kind {
                        SocketKind::Stream(stream) => stream.peer_addr().map_err(Some),
                        _ => Err(Some(not_connected_io())),
                    }
                }
            };
            match result {
                Ok(addr) => tuple_addr(addr),
                Err(None) => descriptor_error(),
                Err(Some(error)) => io_error(error),
            }
        }
        "fileno" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            let fd = {
                let inner = state.lock();
                if inner.closed {
                    None
                } else {
                    match &inner.kind {
                        SocketKind::Unbound => None,
                        SocketKind::Listener(listener) => Some(raw_fd_listener(listener)),
                        SocketKind::Stream(stream) => Some(raw_fd_stream(stream)),
                    }
                }
            };
            match fd {
                Some(fd) => encode_int(fd as i64),
                None => descriptor_error(),
            }
        }
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("'socket' object has no attribute '{name}'"),
        ),
    }
}

fn libc_einval() -> i64 {
    22
}

fn bind(state: &SocketState, address: i64) -> i64 {
    let (host, port) = match address_argument(address) {
        Ok(address) => address,
        Err(err) => return err,
    };
    let host = if host.is_empty() {
        "0.0.0.0".to_string()
    } else {
        host
    };
    let mut addrs = match resolve(&host, port) {
        Ok(addrs) => addrs,
        Err(error) => return io_error(error),
    };
    let addr = addrs.remove(0);
    let mut inner = state.lock();
    if inner.closed {
        drop(inner);
        return descriptor_error();
    }
    if !matches!(&inner.kind, SocketKind::Unbound) {
        drop(inner);
        return invalid_argument_error();
    }
    match TcpListener::bind(addr) {
        Ok(listener) => {
            inner.kind = SocketKind::Listener(listener);
            NONE
        }
        Err(error) => {
            drop(inner);
            io_error(error)
        }
    }
}

fn accept(state: &SocketState) -> i64 {
    let snapshot = {
        let inner = state.lock();
        if inner.closed {
            None
        } else {
            match &inner.kind {
                SocketKind::Listener(listener) => {
                    Some(listener.try_clone().map(|copy| (copy, inner.timeout)))
                }
                _ => Some(Err(io::Error::from_raw_os_error(22))),
            }
        }
    };
    let (listener, timeout) = match snapshot {
        None => return descriptor_error(),
        Some(Ok(snapshot)) => snapshot,
        Some(Err(error)) => return io_error(error),
    };
    let result = match timeout {
        None => parked(None, || listener.accept()),
        Some(limit) => accept_with_timeout(&listener, limit),
    };
    match result {
        Ok((stream, addr)) => {
            let sock = value::socket_new(SocketState::stream(stream, None));
            let address = tuple_addr(addr);
            value::tuple_new(vec![sock, address])
        }
        Err(error) => map_stream_error(error),
    }
}

/// Accepts with a finite (possibly zero) timeout by polling the listener.
fn accept_with_timeout(
    listener: &TcpListener,
    limit: Duration,
) -> io::Result<(TcpStream, SocketAddr)> {
    let deadline = Instant::now().checked_add(limit);
    loop {
        let wait = match deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => limit,
        };
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline && !limit.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
            }
        }
        let ready = poll_readable(listener, wait)?;
        if ready {
            return parked(None, || listener.accept());
        }
        if limit.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        }
    }
}

/// Waits until the listener is readable or `timeout` expires.
fn poll_readable(listener: &TcpListener, timeout: Duration) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = listener.as_raw_fd();
        let millis = timeout
            .as_nanos()
            .saturating_add(999_999)
            .checked_div(1_000_000)
            .unwrap_or(0)
            .min(i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            heap::blocking_enter();
            // SAFETY: `pollfd` is a valid single-element array.
            let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
            heap::blocking_leave();
            if result > 0 {
                return Ok(pollfd.revents & libc::POLLIN != 0);
            }
            if result == 0 {
                return Ok(false);
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = listener;
        parked(Some(timeout), || std::thread::sleep(timeout));
        Ok(true)
    }
}

fn connect(state: &SocketState, host: &str, port: u16) -> i64 {
    let (unbound, connected, timeout) = {
        let inner = state.lock();
        if inner.closed {
            drop(inner);
            return descriptor_error();
        }
        (
            matches!(&inner.kind, SocketKind::Unbound),
            matches!(&inner.kind, SocketKind::Stream(_)),
            inner.timeout,
        )
    };
    if connected {
        return os_error("[Errno 106] Transport endpoint is already connected");
    }
    if !unbound {
        return invalid_argument_error();
    }
    let stream = match connect_stream(host, port, timeout) {
        Ok(stream) => stream,
        Err(error) => return io_error(error),
    };
    let mut inner = state.lock();
    if inner.closed {
        drop(inner);
        return descriptor_error();
    }
    inner.kind = SocketKind::Stream(stream);
    NONE
}

fn connect_ex(state: &SocketState, host: &str, port: u16) -> i64 {
    let timeout = {
        let inner = state.lock();
        if inner.closed {
            return encode_int(9);
        }
        match &inner.kind {
            SocketKind::Unbound => inner.timeout,
            SocketKind::Stream(_) => return encode_int(106),
            SocketKind::Listener(_) => return encode_int(95),
        }
    };
    match connect_stream(host, port, timeout) {
        Ok(stream) => {
            let mut inner = state.lock();
            if inner.closed {
                return encode_int(9);
            }
            inner.kind = SocketKind::Stream(stream);
            encode_int(0)
        }
        Err(error) => encode_int(error.raw_os_error().unwrap_or(22) as i64),
    }
}

/// Validates an optional flags argument; only 0 is supported.
fn flags_argument(args: &[i64]) -> Result<(), i64> {
    let Some(&flags) = args.get(1) else {
        return Ok(());
    };
    match value::as_integer(flags) {
        None => Err(type_error("flags must be an integer")),
        Some(0) => Ok(()),
        Some(_) => Err(os_error("[Errno 95] Operation not supported")),
    }
}

fn send(state: &SocketState, args: &[i64]) -> i64 {
    if let Err(err) = flags_argument(args) {
        return err;
    }
    let Some(data) = value::as_bytes(args[0]) else {
        return type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(args[0])
        ));
    };
    let data = data.to_vec();
    let (mut stream, timeout) = match stream_snapshot(state) {
        Ok(snapshot) => snapshot,
        Err(err) => return err,
    };
    if let Err(err) = prepare(&stream, timeout, false) {
        return err;
    }
    match parked(timeout, || stream.write(&data)) {
        Ok(written) => encode_int(written as i64),
        Err(error) => map_stream_error(error),
    }
}

fn send_all(state: &SocketState, args: &[i64]) -> i64 {
    if let Err(err) = flags_argument(args) {
        return err;
    }
    let Some(data) = value::as_bytes(args[0]) else {
        return type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(args[0])
        ));
    };
    let data = data.to_vec();
    let (mut stream, timeout) = match stream_snapshot(state) {
        Ok(snapshot) => snapshot,
        Err(err) => return err,
    };
    if let Err(err) = prepare(&stream, timeout, false) {
        return err;
    }
    match parked(timeout, || stream.write_all(&data)) {
        Ok(()) => NONE,
        Err(error) => map_stream_error(error),
    }
}

fn recv(state: &SocketState, args: &[i64]) -> i64 {
    if let Err(err) = flags_argument(args) {
        return err;
    }
    let size = match int_argument(args[0], "buffersize") {
        Ok(size) => size,
        Err(err) => return err,
    };
    let size = match usize::try_from(size) {
        Ok(size) => size,
        Err(_) => return value_error("negative buffersize in recv"),
    };
    let (mut stream, timeout) = match stream_snapshot(state) {
        Ok(snapshot) => snapshot,
        Err(err) => return err,
    };
    if size == 0 {
        return value::bytes_new(Vec::new());
    }
    if let Err(err) = prepare(&stream, timeout, true) {
        return err;
    }
    let mut buffer = vec![0u8; size];
    match parked(timeout, || stream.read(&mut buffer)) {
        Ok(read) => {
            buffer.truncate(read);
            value::bytes_new(buffer)
        }
        Err(error) => map_stream_error(error),
    }
}

fn shutdown(state: &SocketState, how: i64) -> i64 {
    let mode = match how {
        SHUT_RD => std::net::Shutdown::Read,
        SHUT_WR => std::net::Shutdown::Write,
        SHUT_RDWR => std::net::Shutdown::Both,
        _ => return invalid_argument_error(),
    };
    let outcome = {
        let inner = state.lock();
        if inner.closed {
            Err(None)
        } else {
            match &inner.kind {
                SocketKind::Stream(stream) => stream.shutdown(mode).map_err(Some),
                _ => Err(Some(not_connected_io())),
            }
        }
    };
    match outcome {
        Ok(()) => NONE,
        Err(None) => descriptor_error(),
        Err(Some(error)) => io_error(error),
    }
}
