//! `socket`: TCP, UDP and client TLS sockets over `std::net` and `rustls`.
//!
//! A socket object is a childless heap object holding a mutex-protected
//! [`SocketInner`]. The native kind starts unbound, becomes a
//! [`TcpListener`] after `bind` (stream sockets) or [`UdpSocket`] after
//! `bind`/`sendto` (datagram sockets), a [`TcpStream`] after
//! `connect`/`accept`, and a [`TlsStream`] after `ssl.SSLContext.wrap_socket`;
//! dropping the object closes the underlying descriptor.
//!
//! Every operation that may block (`connect`, `accept`, `recv`, `sendall`,
//! host resolution, `poll`, the TLS handshake and TLS record I/O) parks the
//! calling thread for the collector via [`heap::blocking_enter`]/
//! [`heap::blocking_leave`]. The socket mutex is never held across such a
//! block: stream and datagram operations work on a duplicated descriptor
//! (`try_clone`) taken under the lock, and the TLS session is moved out of the
//! state for the duration of the operation. A second thread waiting for the
//! mutex can therefore always reach a GC safepoint, and no Python object is
//! allocated while the thread is parked.
//!
//! Timeouts are stored on the object. `timeout=None` blocks, a finite timeout
//! applies `SO_RCVTIMEO`/`SO_SNDTIMEO` and a zero timeout puts the descriptor
//! in non-blocking mode. Non-blocking readiness failures (`EAGAIN`/`EWOULDBLOCK`)
//! raise `OSError` with errno 11 (`BlockingIOError` semantics); finite timeouts
//! raise `OSError("timed out")`. The runtime's exception registry has no
//! dedicated `BlockingIOError`/`socket.timeout`/`SSLError` tags (only the
//! fixed `ExcKind` set in `hydro-ir`), so TLS failures are reported as `OSError`
//! carrying an `[SSL] ...` message.
//!
//! IPv6 sockets bind and format with the usual `(host, port, flowinfo,
//! scopeid)` 4-tuple; host strings in brackets (`[::1]`) are accepted
//! everywhere a host is accepted.
//!
//! `ssl.SSLContext` follows CPython's rules: `verify_mode` and
//! `check_hostname` stay consistent (`CERT_NONE` is rejected while host-name
//! checking is enabled), `load_cert_chain`/`load_verify_locations` read real
//! PEM/DER files, `set_alpn_protocols` feeds rustls' client hello and
//! `post_handshake_auth` is stored for `http.client`. rustls clients never
//! perform post-handshake authentication and server-side TLS is not
//! implemented, so `SSLContext(PROTOCOL_TLS_SERVER).wrap_socket` reports that
//! explicitly. `_create_stdlib_context`/`_create_unverified_context` and
//! `_create_default_https_context` mirror the CPython factories the stdlib
//! uses to build client contexts.
//!
//! Host lookup covers `gethostname`, `gethostbyname`, `getaddrinfo` and
//! `getfqdn`; the last one reverse-resolves the name it is given (the local
//! host name when the argument is empty), so it reports the resolver's
//! canonical name instead of echoing its argument.

use std::io::{self, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
    ServerName, UnixTime,
};

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, TYPE_ERROR, UNICODE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

/// Address families exposed by the module.
pub(crate) const AF_UNSPEC: i64 = 0;
pub(crate) const AF_UNIX: i64 = 1;
pub(crate) const AF_INET: i64 = 2;
pub(crate) const AF_INET6: i64 = 10;

/// Socket types.
pub(crate) const SOCK_STREAM: i64 = 1;
pub(crate) const SOCK_DGRAM: i64 = 2;

/// Protocol numbers.
pub(crate) const IPPROTO_IP: i64 = 0;
pub(crate) const IPPROTO_TCP: i64 = 6;
pub(crate) const IPPROTO_UDP: i64 = 17;

const SOL_SOCKET: i64 = 1;

const SHUT_RD: i64 = 0;
const SHUT_WR: i64 = 1;
const SHUT_RDWR: i64 = 2;

/// TLS verification modes, mirroring `ssl.CERT_*`.
const CERT_NONE: i64 = 0;
const CERT_OPTIONAL: i64 = 1;
const CERT_REQUIRED: i64 = 2;

/// `ssl.OP_ALL`: the compatibility options `SSLContext` reports.
const OP_ALL: i64 = 0x80000;

/// ALPN protocol names are length-prefixed with one byte, so a name is
/// 1..=255 bytes long.
const ALPN_MAX_LENGTH: usize = 255;

/// `SSLContext` protocol selectors.
const PROTOCOL_TLS: i64 = 2;
const PROTOCOL_TLS_CLIENT: i64 = 16;
const PROTOCOL_TLS_SERVER: i64 = 17;

/// Native socket state.
pub(crate) enum SocketKind {
    /// Created but not bound to an address yet.
    Unbound,
    /// Bound and listening for connections.
    Listener(TcpListener),
    /// Connected (or accepted) stream.
    Stream(TcpStream),
    /// A non-blocking connect that has not completed yet.
    Connecting(TcpStream),
    /// A datagram socket; `None` is an unbound descriptor-like socket that is
    /// bound implicitly on the first `bind`/`sendto`/`connect`.
    Datagram(Option<UdpSocket>),
    /// A client TLS session. `None` means an operation currently owns the
    /// session after moving it out of the mutex.
    Tls(Option<Box<TlsStream>>),
    /// Ownership temporarily moved out by a TLS operation.
    Busy,
}

/// Mutex-protected payload of a `socket` object.
pub(crate) struct SocketInner {
    kind: SocketKind,
    family: i64,
    sock_type: i64,
    timeout: Option<Duration>,
    closed: bool,
    /// Values recorded by `setsockopt`, returned by `getsockopt`.
    options: Vec<((i32, i32), i64)>,
}

/// Heap payload of a `socket` object.
pub(crate) struct SocketState {
    inner: Mutex<SocketInner>,
}

/// A client-side TLS session layered over a connected stream.
pub(crate) struct TlsStream {
    conn: ClientConnection,
    tcp: TcpStream,
}

impl SocketState {
    /// A freshly created, unbound socket.
    pub(crate) fn unbound(family: i64, sock_type: i64, timeout: Option<Duration>) -> SocketState {
        SocketState {
            inner: Mutex::new(SocketInner {
                kind: SocketKind::Unbound,
                family,
                sock_type,
                timeout,
                closed: false,
                options: Vec::new(),
            }),
        }
    }

    /// A connected stream socket with the given timeout.
    pub(crate) fn stream(stream: TcpStream, family: i64, timeout: Option<Duration>) -> SocketState {
        SocketState {
            inner: Mutex::new(SocketInner {
                kind: SocketKind::Stream(stream),
                family,
                sock_type: SOCK_STREAM,
                timeout,
                closed: false,
                options: Vec::new(),
            }),
        }
    }

    /// An unbound datagram socket.
    fn datagram(udp: Option<UdpSocket>, family: i64, timeout: Option<Duration>) -> SocketState {
        SocketState {
            inner: Mutex::new(SocketInner {
                kind: SocketKind::Datagram(udp),
                family,
                sock_type: SOCK_DGRAM,
                timeout,
                closed: false,
                options: Vec::new(),
            }),
        }
    }

    fn from_inner(inner: SocketInner) -> SocketState {
        SocketState {
            inner: Mutex::new(inner),
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
            SocketKind::Stream(stream) | SocketKind::Connecting(stream) => {
                format!("<socket object, fd={}>", raw_fd_stream(stream))
            }
            SocketKind::Datagram(Some(udp)) => {
                format!("<socket object, fd={}>", raw_fd_udp(udp))
            }
            SocketKind::Datagram(None) => "<socket object, unbound>".to_string(),
            SocketKind::Tls(Some(tls)) => {
                format!("<socket object, fd={}, TLS>", raw_fd_stream(&tls.tcp))
            }
            SocketKind::Tls(None) | SocketKind::Busy => "<socket object, busy>".to_string(),
        }
    }
}

impl SocketInner {
    fn fd(&self) -> Option<i32> {
        match &self.kind {
            SocketKind::Listener(listener) => Some(raw_fd_listener(listener)),
            SocketKind::Stream(stream) | SocketKind::Connecting(stream) => {
                Some(raw_fd_stream(stream))
            }
            SocketKind::Datagram(Some(udp)) => Some(raw_fd_udp(udp)),
            SocketKind::Tls(Some(tls)) => Some(raw_fd_stream(&tls.tcp)),
            SocketKind::Unbound
            | SocketKind::Datagram(None)
            | SocketKind::Tls(None)
            | SocketKind::Busy => None,
        }
    }

    fn nonblocking(&self) -> bool {
        self.timeout == Some(Duration::ZERO)
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

/// Descriptor number of a live socket, used by `select`.
pub(crate) fn descriptor(state: &SocketState) -> Option<i32> {
    let inner = state.lock();
    if inner.closed {
        return None;
    }
    inner.fd()
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

#[cfg(unix)]
fn raw_fd_udp(udp: &UdpSocket) -> i32 {
    use std::os::unix::io::AsRawFd;
    udp.as_raw_fd()
}

#[cfg(not(unix))]
fn raw_fd_udp(_udp: &UdpSocket) -> i32 {
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

/// `BlockingIOError`-style failure. The exception registry has no dedicated
/// tag, so this is an `OSError` carrying errno 11, which `except OSError`
/// catches exactly like CPython's `BlockingIOError` subclass.
fn blocking_error() -> i64 {
    os_error("[Errno 11] Resource temporarily unavailable")
}

fn ssl_error(message: &str) -> i64 {
    os_error(&format!("[SSL] {message}"))
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Maps a network error using the socket's timeout mode: a non-blocking
/// descriptor reports `BlockingIOError` semantics, a finite timeout reports
/// "timed out", and any other failure keeps its errno text.
fn map_net_error(error: io::Error, timeout: Option<Duration>) -> i64 {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            if timeout == Some(Duration::ZERO)
                || (error.kind() == io::ErrorKind::WouldBlock && timeout.is_none())
            {
                blocking_error()
            } else {
                timeout_error()
            }
        }
        _ => io_error(error),
    }
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

fn optional_int(args: &[i64], index: usize, name: &str, default: i64) -> Result<i64, i64> {
    match args.get(index) {
        Some(&v) if v != NONE => int_argument(v, name),
        _ => Ok(default),
    }
}

/// A decoded socket address: host text plus optional IPv6 scope id.
struct Address {
    host: String,
    port: u16,
    scope_id: u32,
}

/// Decodes a 2-tuple `(host, port)` or a 4-tuple `(host, port, flowinfo,
/// scopeid)` (which is what IPv6 `getsockname`/`getpeername` return).
fn address_argument(v: i64) -> Result<Address, i64> {
    let items = value::as_tuple(v).or_else(|| value::as_list(v));
    let Some(items) = items else {
        return Err(type_error(&format!(
            "address must be a pair (host, port), not {}",
            value::type_name(v)
        )));
    };
    if items.len() != 2 && items.len() != 4 {
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
    let scope_id = if items.len() == 4 {
        let scope = int_argument(items[3], "scopeid")?;
        u32::try_from(scope).map_err(|_| os_error("scopeid must be 0-4294967295"))?
    } else {
        0
    };
    Ok(Address {
        host: host.to_string(),
        port,
        scope_id,
    })
}

/// Decodes `None` or a non-negative number of seconds.
fn timeout_argument(v: i64) -> Result<Option<Duration>, i64> {
    // `_GLOBAL_DEFAULT_TIMEOUT` is a sentinel, not a duration: passing it
    // means "use the module default", exactly like `None`.
    if v == NONE || v == global_default_timeout() {
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
    match addr {
        SocketAddr::V4(_) => value::tuple_new(vec![host, port]),
        SocketAddr::V6(v6) => value::tuple_new(vec![
            host,
            port,
            encode_int(v6.flowinfo() as i64),
            encode_int(v6.scope_id() as i64),
        ]),
    }
}

fn family_of(addr: &SocketAddr) -> i64 {
    if addr.is_ipv4() {
        AF_INET
    } else {
        AF_INET6
    }
}

fn family_matches(ip: IpAddr, family: i64) -> bool {
    match family {
        AF_INET => ip.is_ipv4(),
        AF_INET6 => ip.is_ipv6(),
        _ => true,
    }
}

fn wildcard(family: i64) -> &'static str {
    if family == AF_INET6 {
        "::"
    } else {
        "0.0.0.0"
    }
}

/// Strips the brackets CPython accepts around IPv6 literals (`[::1]`).
fn strip_brackets(host: &str) -> &str {
    let bytes = host.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'[' && bytes[bytes.len() - 1] == b']' {
        &host[1..host.len() - 1]
    } else {
        host
    }
}

/// Resolves `host`/`port`, parking for the collector while the resolver runs.
fn resolve(host: &str, port: u16, family: i64) -> Result<Vec<SocketAddr>, io::Error> {
    let host = strip_brackets(host);
    let host = if host.is_empty() {
        wildcard(family)
    } else {
        host
    };
    // A literal address needs no resolver, and `ToSocketAddrs` would accept it
    // anyway; handling it directly keeps the common case allocation-light and
    // preserves malformed-literal errors.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if family_matches(ip, family) || family == AF_UNSPEC {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Name or service not known: {host}"),
        ));
    }
    heap::blocking_enter();
    let resolved = (host, port).to_socket_addrs();
    heap::blocking_leave();
    let mut addrs: Vec<SocketAddr> = resolved?
        .filter(|addr| family_matches(addr.ip(), family))
        .collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Name or service not known: {host}"),
        ));
    }
    addrs.shrink_to_fit();
    Ok(addrs)
}

fn scope_address(addr: SocketAddr, scope_id: u32) -> SocketAddr {
    match addr {
        SocketAddr::V6(mut v6) if scope_id != 0 => {
            v6.set_scope_id(scope_id);
            SocketAddr::V6(v6)
        }
        other => other,
    }
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

/// Applies the timeout to both directions, as the TLS handshake and record
/// layer may read and write in the same call.
fn prepare_both(stream: &TcpStream, timeout: Option<Duration>) -> Result<(), i64> {
    prepare(stream, timeout, true)?;
    prepare(stream, timeout, false)
}

fn prepare_udp(udp: &UdpSocket, timeout: Option<Duration>, read: bool) -> Result<(), i64> {
    let result = match timeout {
        None => {
            let result = udp.set_nonblocking(false);
            result.and_then(|_| {
                if read {
                    udp.set_read_timeout(None)
                } else {
                    udp.set_write_timeout(None)
                }
            })
        }
        Some(limit) if limit.is_zero() => udp.set_nonblocking(true),
        Some(limit) => {
            let result = udp.set_nonblocking(false);
            result.and_then(|_| {
                if read {
                    udp.set_read_timeout(Some(limit))
                } else {
                    udp.set_write_timeout(Some(limit))
                }
            })
        }
    };
    result.map_err(io_error)
}

enum NonblockingConnect {
    Connected(TcpStream),
    Pending(TcpStream),
}

#[cfg(unix)]
fn raw_sockaddr(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            // SAFETY: `storage` is large enough for a `sockaddr_in`.
            let sin = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in) };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            // Network byte order stored in memory.
            sin.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            // SAFETY: `storage` is large enough for a `sockaddr_in6`.
            let sin6 = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in6) };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_flowinfo = v6.flowinfo();
            sin6.sin6_addr.s6_addr = v6.ip().octets();
            sin6.sin6_scope_id = v6.scope_id();
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

#[cfg(unix)]
fn connect_nonblocking(addr: SocketAddr) -> io::Result<NonblockingConnect> {
    use std::os::unix::io::FromRawFd;
    let domain = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    // SAFETY: `socket` returns an owned descriptor or -1.
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh owned stream descriptor.
    let stream = unsafe { TcpStream::from_raw_fd(fd) };
    stream.set_nonblocking(true)?;
    let (storage, length) = raw_sockaddr(addr);
    // SAFETY: `storage` holds an initialized sockaddr of `length` bytes.
    let result =
        unsafe { libc::connect(fd, &storage as *const _ as *const libc::sockaddr, length) };
    if result == 0 {
        return Ok(NonblockingConnect::Connected(stream));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EINPROGRESS) {
        Ok(NonblockingConnect::Pending(stream))
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
fn connect_nonblocking(addr: SocketAddr) -> io::Result<NonblockingConnect> {
    match TcpStream::connect_timeout(&addr, Duration::from_micros(1)) {
        Ok(stream) => Ok(NonblockingConnect::Connected(stream)),
        Err(error) => Err(error),
    }
}

/// Connects to the first reachable address, honouring the timeout.
fn connect_stream(
    host: &str,
    port: u16,
    family: i64,
    timeout: Option<Duration>,
) -> io::Result<NonblockingConnect> {
    let addrs = resolve(host, port, family)?;
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no address to connect to");
    match timeout {
        None => {
            heap::blocking_enter();
            for addr in &addrs {
                match TcpStream::connect(*addr) {
                    Ok(stream) => {
                        heap::blocking_leave();
                        return Ok(NonblockingConnect::Connected(stream));
                    }
                    Err(error) => last = error,
                }
            }
            heap::blocking_leave();
            Err(last)
        }
        Some(limit) if limit.is_zero() => {
            for addr in &addrs {
                match connect_nonblocking(*addr) {
                    Ok(outcome) => return Ok(outcome),
                    Err(error) => last = error,
                }
            }
            Err(last)
        }
        Some(limit) => {
            let deadline = Instant::now().checked_add(limit);
            for addr in &addrs {
                let remaining = match deadline {
                    Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                    None => limit,
                };
                if remaining.is_zero() {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
                }
                heap::blocking_enter();
                let result = TcpStream::connect_timeout(addr, remaining);
                heap::blocking_leave();
                match result {
                    Ok(stream) => return Ok(NonblockingConnect::Connected(stream)),
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
fn construct_kind(family: i64, sock_type: i64, proto: i64, timeout: Option<Duration>) -> i64 {
    let base_type = sock_type & 0xF;
    if family != AF_INET && family != AF_INET6 {
        return os_error("[Errno 97] Address family not supported by protocol");
    }
    if base_type != SOCK_STREAM && base_type != SOCK_DGRAM {
        return os_error("[Errno 93] Protocol not supported");
    }
    let expected = if base_type == SOCK_DGRAM {
        IPPROTO_UDP
    } else {
        IPPROTO_TCP
    };
    if proto != 0 && proto != expected {
        return os_error("[Errno 93] Protocol not supported");
    }
    if base_type == SOCK_DGRAM {
        value::socket_new(SocketState::datagram(None, family, timeout))
    } else {
        value::socket_new(SocketState::unbound(family, SOCK_STREAM, timeout))
    }
}

fn construct(args: &[i64]) -> i64 {
    let family = match args.first() {
        Some(&v) if v != NONE => match int_argument(v, "family") {
            Ok(family) => family,
            Err(err) => return err,
        },
        _ => AF_INET,
    };
    let raw_type = match args.get(1) {
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
    // `SOCK_NONBLOCK`/`SOCK_CLOEXEC` may be OR-ed into the type; the runtime
    // only models the non-blocking bit, as a zero timeout.
    let nonblock = raw_type & 0o4000 != 0;
    let timeout = nonblock.then_some(Duration::ZERO);
    construct_kind(family, raw_type, proto, timeout)
}

/// `socket.create_connection(address, timeout=None)`.
fn create_connection(args: &[i64]) -> i64 {
    let Some(&address_value) = args.first() else {
        return type_error("create_connection() missing required argument 'address'");
    };
    let address = match address_argument(address_value) {
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
    let family = if address.host.contains(':') || address.host.starts_with('[') {
        AF_INET6
    } else {
        AF_UNSPEC
    };
    match connect_stream(&address.host, address.port, family, timeout) {
        Ok(NonblockingConnect::Connected(stream)) => {
            let family = family_of(
                &stream
                    .local_addr()
                    .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)),
            );
            value::socket_new(SocketState::stream(stream, family, timeout))
        }
        Ok(NonblockingConnect::Pending(_)) => blocking_error(),
        Err(error) => io_error(error),
    }
}

/// Keyword-only options of `socket.create_server`. The module call path
/// (`create_server(address)`) leaves the CPython defaults in place; the
/// keyword path in [`call_kw`] fills them.
#[derive(Clone, Copy)]
struct ServerOptions {
    family: i64,
    /// `None` means `SOMAXCONN`, like CPython's `backlog=None`.
    backlog: Option<i64>,
    reuse_port: bool,
    dualstack_ipv6: bool,
}

impl ServerOptions {
    const DEFAULT: ServerOptions = ServerOptions {
        family: AF_INET,
        backlog: None,
        reuse_port: false,
        dualstack_ipv6: false,
    };
}

/// `socket.create_server(address, *, family=AF_INET, backlog=None,
/// reuse_port=False, dualstack_ipv6=False)`.
///
/// Creates, configures, binds and listens on a TCP server socket, returning
/// it in the listening state — exactly the sequence of the CPython helper:
/// `SO_REUSEADDR` is always set before `bind`, `SO_REUSEPORT` is set before
/// `bind` when `reuse_port` is true, and `IPV6_V6ONLY` is written for IPv6
/// sockets (0 for a dual-stack server, 1 otherwise). Any failure closes the
/// descriptor and raises the underlying `OSError`/`ValueError`.
fn create_server(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "create_server() takes 1 positional argument but {} were given",
            args.len()
        ));
    }
    let Some(&address) = args.first() else {
        return type_error("create_server() missing 1 required positional argument: 'address'");
    };
    create_server_impl(address, ServerOptions::DEFAULT)
}

fn create_server_impl(address_value: i64, options: ServerOptions) -> i64 {
    if options.dualstack_ipv6 && options.family != AF_INET6 {
        return exc::raise_with_message(VALUE_ERROR, "dualstack_ipv6 requires AF_INET6 family");
    }
    let address = match address_argument(address_value) {
        Ok(address) => address,
        Err(err) => return err,
    };
    let addrs = match resolve(&address.host, address.port, options.family) {
        Ok(addrs) => addrs,
        Err(error) => return io_error(error),
    };
    let addr = scope_address(addrs[0], address.scope_id);
    let backlog = match options.backlog {
        None => libc::SOMAXCONN as i64,
        Some(backlog) => match libc::c_int::try_from(backlog) {
            Ok(backlog) => i64::from(backlog),
            Err(_) => {
                return exc::raise_with_message(
                    crate::exc::OVERFLOW_ERROR,
                    "Python int too large to convert to C int",
                )
            }
        },
    };
    create_listener(addr, options, backlog as libc::c_int)
}

#[cfg(unix)]
fn create_listener(addr: SocketAddr, options: ServerOptions, backlog: libc::c_int) -> i64 {
    use std::os::unix::io::FromRawFd;

    const IPPROTO_IPV6_LEVEL: i32 = 41;
    const IPV6_V6ONLY: i32 = 26;

    let domain = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    // SAFETY: `socket` returns an owned descriptor or -1.
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return io_error(io::Error::last_os_error());
    }
    let mut stored: Vec<((i32, i32), i64)> = Vec::new();
    let mut configure = |level: i32, option: i32, value: i64| -> io::Result<()> {
        apply_option(fd, level, option, value)?;
        stored.push(((level, option), value));
        Ok(())
    };
    // CPython always requests address reuse before binding; `reuse_port` must
    // also reach the kernel before `bind` to participate in the port group.
    if let Err(error) = configure(libc::SOL_SOCKET, libc::SO_REUSEADDR, 1) {
        // SAFETY: `fd` is still owned here and has not been handed to a Rust
        // wrapper; closing it releases the failed socket.
        unsafe { libc::close(fd) };
        return io_error(error);
    }
    if options.reuse_port {
        if let Err(error) = configure(libc::SOL_SOCKET, libc::SO_REUSEPORT, 1) {
            // SAFETY: as above.
            unsafe { libc::close(fd) };
            return io_error(error);
        }
    }
    if !addr.is_ipv4() {
        let dual_stack = i64::from(options.dualstack_ipv6);
        if let Err(error) = configure(IPPROTO_IPV6_LEVEL, IPV6_V6ONLY, dual_stack) {
            // SAFETY: as above.
            unsafe { libc::close(fd) };
            return io_error(error);
        }
    }
    let (storage, length) = raw_sockaddr(addr);
    // SAFETY: `storage` holds an initialized sockaddr of `length` bytes and
    // `fd` is a live stream descriptor.
    let bound = unsafe { libc::bind(fd, &storage as *const _ as *const libc::sockaddr, length) };
    if bound != 0 {
        let error = io::Error::last_os_error();
        // SAFETY: as above.
        unsafe { libc::close(fd) };
        return io_error(error);
    }
    // SAFETY: `fd` is a bound stream descriptor owned by this function.
    let listening = unsafe { libc::listen(fd, backlog) };
    if listening != 0 {
        let error = io::Error::last_os_error();
        // SAFETY: as above.
        unsafe { libc::close(fd) };
        return io_error(error);
    }
    // SAFETY: ownership of the configured, bound, listening descriptor moves
    // into the `TcpListener`; the runtime state closes it on drop.
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    let family = family_of(&addr);
    value::socket_new(SocketState::from_inner(SocketInner {
        kind: SocketKind::Listener(listener),
        family,
        sock_type: SOCK_STREAM,
        timeout: None,
        closed: false,
        options: stored,
    }))
}

#[cfg(not(unix))]
fn create_listener(addr: SocketAddr, options: ServerOptions, backlog: libc::c_int) -> i64 {
    // No descriptor-level API on this target: bind through `std` and report
    // the reuse options as stored socket options. `std` already binds with
    // safe reuse defaults, so the listening socket is truthful; the
    // `SO_REUSEPORT` group semantics are the documented limitation.
    let listener = match TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(error) => return io_error(error),
    };
    let _ = backlog;
    let mut stored = vec![((1, 2), 1)];
    if options.reuse_port {
        stored.push(((1, 15), 1));
    }
    let family = family_of(&addr);
    value::socket_new(SocketState::from_inner(SocketInner {
        kind: SocketKind::Listener(listener),
        family,
        sock_type: SOCK_STREAM,
        timeout: None,
        closed: false,
        options: stored,
    }))
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

/// `socket.socketpair(family=AF_UNIX, type=SOCK_STREAM, proto=0)`.
fn socketpair(args: &[i64]) -> i64 {
    let family = match optional_int(args, 0, "family", AF_UNIX) {
        Ok(family) => family,
        Err(err) => return err,
    };
    let sock_type = match optional_int(args, 1, "type", SOCK_STREAM) {
        Ok(kind) => kind,
        Err(err) => return err,
    };
    let proto = match optional_int(args, 2, "proto", 0) {
        Ok(proto) => proto,
        Err(err) => return err,
    };
    if family != AF_UNIX {
        return os_error("[Errno 97] Address family not supported by protocol");
    }
    if sock_type & 0xF != SOCK_STREAM || (proto != 0 && proto != IPPROTO_TCP) {
        return os_error("[Errno 93] Protocol not supported");
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a valid two-element array for `socketpair`.
        let result =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        if result != 0 {
            return io_error(io::Error::last_os_error());
        }
        // SAFETY: both descriptors are fresh and owned by the two streams.
        let first = unsafe { TcpStream::from_raw_fd(fds[0]) };
        // SAFETY: see above.
        let second = unsafe { TcpStream::from_raw_fd(fds[1]) };
        let left = value::socket_new(SocketState::stream(first, AF_UNIX, None));
        let right = value::socket_new(SocketState::stream(second, AF_UNIX, None));
        value::tuple_new(vec![left, right])
    }
    #[cfg(not(unix))]
    {
        os_error("[Errno 95] Operation not supported")
    }
}

/// `socket.getaddrinfo(host, port, family=0, type=0, proto=0, flags=0)`.
fn getaddrinfo(args: &[i64]) -> i64 {
    let family = match optional_int(args, 2, "family", AF_UNSPEC) {
        Ok(family) => family,
        Err(err) => return err,
    };
    let sock_type = match optional_int(args, 3, "type", 0) {
        Ok(kind) => kind,
        Err(err) => return err,
    };
    let proto = match optional_int(args, 4, "proto", 0) {
        Ok(proto) => proto,
        Err(err) => return err,
    };
    let host = match args[0] {
        v if v == NONE => wildcard(family).to_string(),
        v => match value::as_str(v) {
            Some(host) if !host.is_empty() => host.to_string(),
            Some(_) => wildcard(family).to_string(),
            None => {
                return type_error(&format!(
                    "host must be str or None, not {}",
                    value::type_name(v)
                ))
            }
        },
    };
    let port = match args[1] {
        v if v == NONE => 0u16,
        v => match int_argument(v, "port") {
            Ok(port) => match u16::try_from(port) {
                Ok(port) => port,
                Err(_) => return os_error("port must be 0-65535"),
            },
            Err(_) => match value::as_str(v).and_then(|text| text.parse::<u16>().ok()) {
                Some(port) => port,
                None => return os_error("Servname not supported for ai_socktype"),
            },
        },
    };
    let addrs = match resolve(&host, port, family) {
        Ok(addrs) => addrs,
        Err(error) => return io_error(error),
    };
    let effective_type = if sock_type == 0 {
        SOCK_STREAM
    } else {
        sock_type & 0xF
    };
    let effective_proto = if proto != 0 {
        proto
    } else if effective_type == SOCK_DGRAM {
        IPPROTO_UDP
    } else {
        IPPROTO_TCP
    };
    let mut entries = Vec::with_capacity(addrs.len());
    for addr in addrs {
        entries.push(value::tuple_new(vec![
            encode_int(family_of(&addr)),
            encode_int(effective_type),
            encode_int(effective_proto),
            value::str_alloc(String::new()),
            tuple_addr(addr),
        ]));
    }
    value::list_new(entries)
}

/// `socket.gethostbyname(host)`.
fn gethostbyname(host: &str) -> i64 {
    if let Ok(ip) = strip_brackets(host).parse::<Ipv4Addr>() {
        return value::str_alloc(ip.to_string());
    }
    match resolve(host, 0, AF_INET) {
        Ok(addrs) => match addrs.iter().find(|addr| addr.is_ipv4()) {
            Some(addr) => value::str_alloc(addr.ip().to_string()),
            None => os_error("[Errno -2] Name or service not known"),
        },
        Err(_) => os_error("[Errno -2] Name or service not known"),
    }
}

// The reverse resolver `libc` does not declare for linux-gnu:
// `getnameinfo(3)` is the call CPython's `gethostbyaddr` ends in. The symbol
// exists in every Unix libc and its ABI is fixed, so the declaration lives
// here instead of being faked with a forward lookup.
#[cfg(unix)]
extern "C" {
    fn getnameinfo(
        addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
        host: *mut libc::c_char,
        hostlen: libc::socklen_t,
        serv: *mut libc::c_char,
        servlen: libc::socklen_t,
        flags: libc::c_int,
    ) -> libc::c_int;
}

/// Reverse-resolves `host` the way `gethostbyaddr` does: turn the text into an
/// address (a literal needs no lookup), then ask the resolver for the name
/// recorded for that address.
///
/// The alias list `gethostbyaddr` also returns is not reachable through
/// `getnameinfo`, so the official name is the only candidate and
/// `getfqdn`'s "prefer the first alias containing a dot" step reduces to it.
/// `None` means the lookup failed, which `getfqdn` treats like CPython's
/// `gaierror` fallback.
#[cfg(unix)]
fn reverse_lookup(host: &str) -> Option<String> {
    let addr = *resolve(host, 0, AF_UNSPEC).ok()?.first()?;
    let (storage, length) = raw_sockaddr(addr);
    // `NI_MAXHOST` is 1025 in glibc and 255 in musl; one buffer covers both.
    let mut buffer = [0u8; 1025];
    heap::blocking_enter();
    // SAFETY: `storage` holds an initialized sockaddr of `length` bytes and
    // `buffer` is a writable array of `buffer.len()` bytes.
    let status = unsafe {
        getnameinfo(
            &storage as *const libc::sockaddr_storage as *const libc::sockaddr,
            length,
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            0,
        )
    };
    heap::blocking_leave();
    if status != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).into_owned();
    // Without `NI_NAMEREQD` a missing PTR record yields the numeric host;
    // CPython's `gethostbyaddr` raises `gaierror` there instead.
    if name.is_empty() || name == addr.ip().to_string() {
        return None;
    }
    Some(name)
}

#[cfg(not(unix))]
fn reverse_lookup(_host: &str) -> Option<String> {
    None
}

/// `socket.getfqdn(name='')`: the fully qualified name the resolver records
/// for `name`, or the local host name when the argument is empty (CPython
/// also treats the `0.0.0.0` wildcard that way). A failed lookup returns the
/// name unchanged, exactly like CPython's `gaierror` fallback.
fn getfqdn(name: &str) -> String {
    let mut host = name.trim().to_string();
    if host.is_empty() || host == "0.0.0.0" {
        host = gethostname();
    }
    reverse_lookup(&host).unwrap_or(host)
}

fn inet_pton(args: &[i64]) -> i64 {
    let family = match int_argument(args[0], "family") {
        Ok(family) => family,
        Err(err) => return err,
    };
    let Some(text) = value::as_str(args[1]) else {
        return type_error(&format!(
            "inet_pton() argument 2 must be str, not {}",
            value::type_name(args[1])
        ));
    };
    match family {
        AF_INET => match text.parse::<Ipv4Addr>() {
            Ok(ip) => value::bytes_new(ip.octets().to_vec()),
            Err(_) => os_error("[Errno 22] Invalid argument"),
        },
        AF_INET6 => match text.parse::<Ipv6Addr>() {
            Ok(ip) => value::bytes_new(ip.octets().to_vec()),
            Err(_) => os_error("[Errno 22] Invalid argument"),
        },
        _ => os_error("[Errno 97] Address family not supported by protocol"),
    }
}

fn inet_ntop(args: &[i64]) -> i64 {
    let family = match int_argument(args[0], "family") {
        Ok(family) => family,
        Err(err) => return err,
    };
    let Some(packed) = value::as_bytes(args[1]) else {
        return type_error(&format!(
            "inet_ntop() argument 2 must be bytes, not {}",
            value::type_name(args[1])
        ));
    };
    match family {
        AF_INET if packed.len() == 4 => {
            value::str_alloc(Ipv4Addr::new(packed[0], packed[1], packed[2], packed[3]).to_string())
        }
        AF_INET6 if packed.len() == 16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(packed);
            value::str_alloc(Ipv6Addr::from(octets).to_string())
        }
        AF_INET | AF_INET6 => os_error("[Errno 22] Invalid argument"),
        _ => os_error("[Errno 97] Address family not supported by protocol"),
    }
}

/// Module-level function dispatch shared by `socket` and `_socket`.
pub(crate) fn call(module: &str, function: &str, args: &[i64]) -> i64 {
    match function {
        "socket" => construct(args),
        "create_connection" => create_connection(args),
        "create_server" => create_server(args),
        "gethostname" => value::str_alloc(gethostname()),
        "socketpair" => socketpair(args),
        "getaddrinfo" => getaddrinfo(args),
        "gethostbyname" => match value::as_str(args[0]) {
            Some(host) => gethostbyname(host),
            None => type_error("gethostbyname() argument must be str"),
        },
        // `getfqdn` is pure Python upstream and its first statement is
        // `name.strip()`, so a non-str argument fails with `AttributeError`
        // rather than a `TypeError`.
        "getfqdn" => match args.first() {
            None => value::str_alloc(getfqdn("")),
            Some(&v) => match value::as_str(v) {
                Some(name) => value::str_alloc(getfqdn(name)),
                None => exc::raise_with_message(
                    ATTRIBUTE_ERROR,
                    &format!("'{}' object has no attribute 'strip'", value::type_name(v)),
                ),
            },
        },
        "inet_pton" => inet_pton(args),
        "inet_ntop" => inet_ntop(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{module}' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware module calls: `socket.socket(family=..., type=..., proto=...)`
/// and `socket.create_connection(address=..., timeout=...)`.
pub(crate) fn call_kw(
    module: &str,
    function: &str,
    args: &[i64],
    keywords: Vec<(String, i64)>,
) -> i64 {
    // `create_server`'s options are keyword-only. They are bound here instead
    // of into positional slots, so a second positional argument (which CPython
    // rejects) cannot be mistaken for `family`.
    if function == "create_server" {
        if args.len() > 1 {
            return type_error(&format!(
                "create_server() takes 1 positional argument but {} were given",
                args.len()
            ));
        }
        let Some(&address) = args.first() else {
            return type_error("create_server() missing 1 required positional argument: 'address'");
        };
        let mut options = ServerOptions::DEFAULT;
        for (name, item) in keywords {
            match name.as_str() {
                "family" => match int_argument(item, "family") {
                    Ok(family) => options.family = family,
                    Err(err) => return err,
                },
                "backlog" => {
                    options.backlog = if item == NONE {
                        None
                    } else {
                        match value::as_integer(item) {
                            Some(backlog) => Some(backlog),
                            None => {
                                return type_error(&format!(
                                    "'{}' object cannot be interpreted as an integer",
                                    value::type_name(item)
                                ))
                            }
                        }
                    }
                }
                "reuse_port" => options.reuse_port = value::truthy(item),
                "dualstack_ipv6" => options.dualstack_ipv6 = value::truthy(item),
                _ => {
                    return type_error(&format!(
                        "{module}.{function}() got an unexpected keyword argument '{name}'"
                    ))
                }
            }
        }
        return create_server_impl(address, options);
    }
    let mut bound = args.to_vec();
    for (name, item) in keywords {
        let index = match (function, name.as_str()) {
            ("socket", "family") => 0,
            ("socket", "type") => 1,
            ("socket", "proto") => 2,
            ("create_connection", "address") => 0,
            ("create_connection", "timeout") => 1,
            ("getfqdn", "name") => 0,
            _ => {
                return type_error(&format!(
                    "{module}.{function}() got an unexpected keyword argument '{name}'"
                ))
            }
        };
        while bound.len() <= index {
            bound.push(NONE);
        }
        if bound[index] != NONE {
            return type_error(&format!(
                "{module}.{function}() got multiple values for argument '{name}'"
            ));
        }
        bound[index] = item;
    }
    call(module, function, &bound)
}

/// `socket._GLOBAL_DEFAULT_TIMEOUT`: the cached sentinel `CPython` uses when a
/// wrapper's timeout argument was not supplied.
///
/// The value is a plain `object()` instance stored in the globals registry, so
/// every module read returns the same identity. It is never interpreted as a
/// duration: [`timeout_argument`] maps it to `None`.
fn global_default_timeout() -> i64 {
    const KEY: &str = "socket._GLOBAL_DEFAULT_TIMEOUT";
    if let Some(value) = globals::registry_get(KEY) {
        return value;
    }
    let sentinel = value::instance_new(value::object_class());
    let roots = heap::root_values(&[sentinel]);
    globals::registry_set(KEY, sentinel);
    heap::release_roots(roots);
    sentinel
}

/// Module-level constants shared by `socket` and `_socket`.
pub(crate) fn constant(module: &str, name: &str) -> i64 {
    match name {
        "_GLOBAL_DEFAULT_TIMEOUT" => global_default_timeout(),
        "AF_UNSPEC" => encode_int(AF_UNSPEC),
        "AF_UNIX" => encode_int(AF_UNIX),
        "AF_INET" => encode_int(AF_INET),
        "AF_INET6" => encode_int(AF_INET6),
        "SOCK_STREAM" => encode_int(SOCK_STREAM),
        "SOCK_DGRAM" => encode_int(SOCK_DGRAM),
        "SOCK_NONBLOCK" => encode_int(0o4000),
        "SOCK_CLOEXEC" => encode_int(0o2000000),
        "SOL_SOCKET" => encode_int(SOL_SOCKET),
        "SO_REUSEADDR" => encode_int(2),
        "SO_REUSEPORT" => encode_int(15),
        "SO_ERROR" => encode_int(4),
        "IPPROTO_IP" => encode_int(IPPROTO_IP),
        "IPPROTO_TCP" => encode_int(IPPROTO_TCP),
        "IPPROTO_UDP" => encode_int(IPPROTO_UDP),
        // TCP-level options. `TCP_NODELAY` is what `socketserver` and
        // `http.client` set so a small write is not held back by Nagle; the
        // remaining entries are the Linux/Android knobs long-lived clients
        // tune. Every value comes from libc, so it follows the target ABI
        // instead of being restated here.
        #[cfg(unix)]
        "TCP_NODELAY" => encode_int(libc::TCP_NODELAY as i64),
        // Winsock's `TCP_NODELAY` is the same bit.
        #[cfg(not(unix))]
        "TCP_NODELAY" => encode_int(1),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_MAXSEG" => encode_int(libc::TCP_MAXSEG as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_CORK" => encode_int(libc::TCP_CORK as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_KEEPIDLE" => encode_int(libc::TCP_KEEPIDLE as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_KEEPINTVL" => encode_int(libc::TCP_KEEPINTVL as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_KEEPCNT" => encode_int(libc::TCP_KEEPCNT as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_SYNCNT" => encode_int(libc::TCP_SYNCNT as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_LINGER2" => encode_int(libc::TCP_LINGER2 as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_DEFER_ACCEPT" => encode_int(libc::TCP_DEFER_ACCEPT as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_WINDOW_CLAMP" => encode_int(libc::TCP_WINDOW_CLAMP as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_INFO" => encode_int(libc::TCP_INFO as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_QUICKACK" => encode_int(libc::TCP_QUICKACK as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_CONGESTION" => encode_int(libc::TCP_CONGESTION as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_USER_TIMEOUT" => encode_int(libc::TCP_USER_TIMEOUT as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_FASTOPEN" => encode_int(libc::TCP_FASTOPEN as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_FASTOPEN_CONNECT" => encode_int(libc::TCP_FASTOPEN_CONNECT as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_ULP" => encode_int(libc::TCP_ULP as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_NOTSENT_LOWAT" => encode_int(libc::TCP_NOTSENT_LOWAT as i64),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "TCP_INQ" => encode_int(libc::TCP_INQ as i64),
        "SHUT_RD" => encode_int(SHUT_RD),
        "SHUT_WR" => encode_int(SHUT_WR),
        "SHUT_RDWR" => encode_int(SHUT_RDWR),
        "MSG_OOB" => encode_int(1),
        "MSG_PEEK" => encode_int(2),
        "MSG_DONTWAIT" => encode_int(64),
        "MSG_WAITALL" => encode_int(256),
        "MSG_NOSIGNAL" => encode_int(16384),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{module}' has no attribute '{name}'"),
        ),
    }
}

/// Class values exported by `socket` and `_socket`.
///
/// `socket`/`SocketType` resolve to the cached class `type()` reports for a
/// socket object, so identity, `isinstance` and construction through the
/// class all work. Hydro has a single tagged `OSError` family for socket
/// failures (including resolution and timeout errors), so `error`,
/// `gaierror` and `timeout` all resolve to that class.
pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    if !matches!(module, "socket" | "_socket") {
        return None;
    }
    match name {
        "socket" | "SocketType" => Some(value::socket_class()),
        "error" | "gaierror" | "timeout" => Some(exc::exception_class(OS_ERROR)),
        _ => None,
    }
}

/// Method dispatch for a `socket` object.
pub(crate) fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    let Some(state) = value::as_socket(obj) else {
        return exc::raise_with_message(
            ATTRIBUTE_ERROR,
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
            let inner = state.lock();
            if inner.closed {
                return descriptor_error();
            }
            match &inner.kind {
                SocketKind::Listener(_) => NONE,
                SocketKind::Datagram(_) => os_error("[Errno 95] Operation not supported"),
                _ => invalid_argument_error(),
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
            let address = match address_argument(args[0]) {
                Ok(address) => address,
                Err(err) => return err,
            };
            connect(state, address)
        }
        "connect_ex" => {
            if let Some(err) = arity(name, args, 1, 1) {
                return err;
            }
            let address = match address_argument(args[0]) {
                Ok(address) => address,
                Err(_) => return encode_int(libc_einval()),
            };
            connect_ex(state, address)
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
        "sendto" => {
            if let Some(err) = arity(name, args, 2, 3) {
                return err;
            }
            send_to(state, args)
        }
        "recv" => {
            if let Some(err) = arity(name, args, 1, 2) {
                return err;
            }
            recv(state, args)
        }
        "recvfrom" => {
            if let Some(err) = arity(name, args, 1, 2) {
                return err;
            }
            recv_from(state, args)
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
        "setblocking" => {
            if let Some(err) = arity(name, args, 1, 1) {
                return err;
            }
            let blocking = value::truthy(args[0]);
            state.lock().timeout = if blocking { None } else { Some(Duration::ZERO) };
            NONE
        }
        "getblocking" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            let nonblocking = state.lock().nonblocking();
            value::bool_value(!nonblocking)
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
            // Apply socket-level options to the live descriptor immediately
            // where one exists (SO_REUSEADDR must reach the kernel before a
            // later bind, so binding code re-applies every stored option).
            if let Some(fd) = inner.fd() {
                if let Err(error) = apply_option(fd, level, option, stored) {
                    drop(inner);
                    return io_error(error);
                }
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
            getsockname(state)
        }
        "getpeername" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            getpeername(state)
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
                    inner.fd()
                }
            };
            match fd {
                Some(fd) => encode_int(fd as i64),
                None => descriptor_error(),
            }
        }
        "detach" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            detach(state)
        }
        "dup" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            duplicate(state)
        }
        "unwrap" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            unwrap_tls(obj, state)
        }
        "__enter__" => {
            if let Some(err) = arity(name, args, 0, 0) {
                return err;
            }
            if state.lock().closed {
                return descriptor_error();
            }
            obj
        }
        "__exit__" => {
            if let Some(err) = arity(name, args, 0, 3) {
                return err;
            }
            let mut inner = state.lock();
            inner.closed = true;
            inner.kind = SocketKind::Unbound;
            NONE
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("'socket' object has no attribute '{name}'"),
        ),
    }
}

fn libc_einval() -> i64 {
    22
}

#[cfg(unix)]
fn apply_option(fd: i32, level: i32, option: i32, stored: i64) -> io::Result<()> {
    let value: i32 = stored as i32;
    // SAFETY: `value` points to an `i32` of the length passed.
    let result = unsafe {
        libc::setsockopt(
            fd,
            level,
            option,
            &value as *const i32 as *const libc::c_void,
            std::mem::size_of::<i32>() as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn apply_option(_fd: i32, _level: i32, _option: i32, _stored: i64) -> io::Result<()> {
    Ok(())
}

fn apply_stored_options(fd: i32, options: &[((i32, i32), i64)]) -> io::Result<()> {
    for &((level, option), stored) in options {
        apply_option(fd, level, option, stored)?;
    }
    Ok(())
}

fn bind(state: &SocketState, address: i64) -> i64 {
    let address = match address_argument(address) {
        Ok(address) => address,
        Err(err) => return err,
    };
    let inner = state.lock();
    if inner.closed {
        drop(inner);
        return descriptor_error();
    }
    if !matches!(
        &inner.kind,
        SocketKind::Unbound | SocketKind::Datagram(None)
    ) {
        drop(inner);
        return invalid_argument_error();
    }
    let family = inner.family;
    let sock_type = inner.sock_type;
    let options = inner.options.clone();
    drop(inner);
    let addrs = match resolve(&address.host, address.port, family) {
        Ok(addrs) => addrs,
        Err(error) => return io_error(error),
    };
    let addr = scope_address(addrs[0], address.scope_id);
    let mut inner = state.lock();
    if inner.closed {
        drop(inner);
        return descriptor_error();
    }
    if !matches!(
        &inner.kind,
        SocketKind::Unbound | SocketKind::Datagram(None)
    ) {
        drop(inner);
        return invalid_argument_error();
    }
    match sock_type {
        SOCK_STREAM => match TcpListener::bind(addr) {
            Ok(listener) => {
                let rebuild = {
                    let fd = raw_fd_listener(&listener);
                    apply_stored_options(fd, &options).err()
                };
                if let Some(error) = rebuild {
                    drop(inner);
                    return io_error(error);
                }
                inner.kind = SocketKind::Listener(listener);
                NONE
            }
            Err(error) => {
                drop(inner);
                io_error(error)
            }
        },
        _ => match UdpSocket::bind(addr) {
            Ok(udp) => {
                if let Err(error) = apply_stored_options(raw_fd_udp(&udp), &options) {
                    drop(inner);
                    return io_error(error);
                }
                inner.kind = SocketKind::Datagram(Some(udp));
                NONE
            }
            Err(error) => {
                drop(inner);
                io_error(error)
            }
        },
    }
}

fn ensure_datagram_bound(inner: &mut SocketInner) -> io::Result<()> {
    if let SocketKind::Datagram(None) = &inner.kind {
        // Binding a wildcard literal needs no resolver, and this runs under
        // the socket mutex: never park (or allocate through DNS) here.
        let addr = if inner.family == AF_INET6 {
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        };
        let udp = UdpSocket::bind(addr)?;
        inner.kind = SocketKind::Datagram(Some(udp));
    }
    Ok(())
}

fn accept(state: &SocketState) -> i64 {
    let snapshot = {
        let inner = state.lock();
        if inner.closed {
            None
        } else {
            match &inner.kind {
                SocketKind::Listener(listener) => Some(
                    listener
                        .try_clone()
                        .map(|copy| (copy, inner.timeout, inner.family)),
                ),
                SocketKind::Datagram(_) => Some(Err(io::Error::from_raw_os_error(95))),
                _ => Some(Err(io::Error::from_raw_os_error(22))),
            }
        }
    };
    let (listener, timeout, family) = match snapshot {
        None => return descriptor_error(),
        Some(Ok(snapshot)) => snapshot,
        Some(Err(error)) => return io_error(error),
    };
    let result = match timeout {
        None => parked(None, || listener.accept()),
        Some(limit) if limit.is_zero() => {
            let _ = listener.set_nonblocking(true);
            let result = listener.accept();
            // The duplicate shares the listening descriptor, so a zero-timeout
            // accept must not leave it permanently non-blocking.
            let _ = listener.set_nonblocking(false);
            result
        }
        Some(limit) => accept_with_timeout(&listener, limit),
    };
    match result {
        Ok((stream, addr)) => {
            let sock = value::socket_new(SocketState::stream(stream, family, None));
            let address = tuple_addr(addr);
            value::tuple_new(vec![sock, address])
        }
        Err(error) => map_net_error(error, timeout),
    }
}

/// Accepts with a finite timeout by polling the listener.
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
            return Err(io::Error::from_raw_os_error(11));
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
            let blocking = !timeout.is_zero();
            if blocking {
                heap::blocking_enter();
            }
            // SAFETY: `pollfd` is a valid single-element array.
            let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
            if blocking {
                heap::blocking_leave();
            }
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

fn connect(state: &SocketState, address: Address) -> i64 {
    let (kind, family, timeout) = {
        let inner = state.lock();
        if inner.closed {
            drop(inner);
            return descriptor_error();
        }
        let kind = match &inner.kind {
            SocketKind::Unbound => 0,
            SocketKind::Datagram(_) => 1,
            SocketKind::Stream(_) => 2,
            SocketKind::Listener(_) => 3,
            SocketKind::Connecting(_) => 4,
            SocketKind::Tls(_) | SocketKind::Busy => 5,
        };
        (kind, inner.family, inner.timeout)
    };
    match kind {
        2 => return os_error("[Errno 106] Transport endpoint is already connected"),
        3 => return os_error("[Errno 95] Operation not supported"),
        4 => return os_error("[Errno 114] Operation already in progress"),
        5 => {
            return exc::raise_with_message(
                crate::exc::RUNTIME_ERROR,
                "socket is busy with a TLS operation",
            )
        }
        1 => return connect_datagram(state, &address),
        _ => {}
    }
    match connect_stream(&address.host, address.port, family, timeout) {
        Ok(NonblockingConnect::Connected(stream)) => {
            let mut inner = state.lock();
            if inner.closed {
                return descriptor_error();
            }
            inner.kind = SocketKind::Stream(stream);
            NONE
        }
        Ok(NonblockingConnect::Pending(stream)) => {
            let mut inner = state.lock();
            if inner.closed {
                return descriptor_error();
            }
            inner.kind = SocketKind::Connecting(stream);
            blocking_error()
        }
        Err(error) => io_error(error),
    }
}

fn connect_datagram(state: &SocketState, address: &Address) -> i64 {
    let snapshot = {
        let mut inner = state.lock();
        if inner.closed {
            return descriptor_error();
        }
        if let Err(error) = ensure_datagram_bound(&mut inner) {
            return io_error(error);
        }
        match &inner.kind {
            SocketKind::Datagram(Some(udp)) => udp
                .try_clone()
                .map(|copy| (copy, inner.family, inner.timeout))
                .map_err(io_error),
            _ => Err(invalid_argument_error()),
        }
    };
    let (udp, family, timeout) = match snapshot {
        Ok(snapshot) => snapshot,
        Err(err) => return err,
    };
    let addrs = match resolve(&address.host, address.port, family) {
        Ok(addrs) => addrs,
        Err(error) => return io_error(error),
    };
    let addr = scope_address(addrs[0], address.scope_id);
    match parked(timeout, || udp.connect(addr)) {
        Ok(()) => NONE,
        Err(error) => map_net_error(error, timeout),
    }
}

fn connect_ex(state: &SocketState, address: Address) -> i64 {
    let (kind, family, timeout) = {
        let inner = state.lock();
        if inner.closed {
            return encode_int(9);
        }
        let kind = match &inner.kind {
            SocketKind::Unbound => 0,
            SocketKind::Datagram(_) => 1,
            SocketKind::Stream(_) => 2,
            SocketKind::Listener(_) => 3,
            SocketKind::Connecting(_) => 4,
            SocketKind::Tls(_) | SocketKind::Busy => 5,
        };
        (kind, inner.family, inner.timeout)
    };
    match kind {
        2 => return encode_int(106),
        3 => return encode_int(95),
        5 => return encode_int(16),
        4 => {
            let mut inner = state.lock();
            let previous = std::mem::replace(&mut inner.kind, SocketKind::Unbound);
            match previous {
                SocketKind::Connecting(stream) => {
                    if let Some(error) = stream.take_error().ok().flatten() {
                        let code = errno_of(&error);
                        inner.kind = SocketKind::Unbound;
                        return encode_int(code);
                    }
                    if stream.peer_addr().is_ok() {
                        inner.kind = SocketKind::Stream(stream);
                        return encode_int(0);
                    }
                    inner.kind = SocketKind::Connecting(stream);
                    return encode_int(libc_einprogress());
                }
                other => {
                    inner.kind = other;
                    return encode_int(22);
                }
            }
        }
        1 => {
            let _ = connect_datagram(state, &address);
            if exc::pending() {
                let tag = exc::pending_tag();
                exc::clear();
                return encode_int(if tag == OS_ERROR as i64 { 111 } else { 22 });
            }
            return encode_int(0);
        }
        _ => {}
    }
    match connect_stream(&address.host, address.port, family, timeout) {
        Ok(NonblockingConnect::Connected(stream)) => {
            let mut inner = state.lock();
            if inner.closed {
                return encode_int(9);
            }
            inner.kind = SocketKind::Stream(stream);
            encode_int(0)
        }
        Ok(NonblockingConnect::Pending(stream)) => {
            let mut inner = state.lock();
            if inner.closed {
                return encode_int(9);
            }
            inner.kind = SocketKind::Connecting(stream);
            encode_int(libc_einprogress())
        }
        Err(error) => encode_int(errno_of(&error)),
    }
}

fn errno_of(error: &io::Error) -> i64 {
    match error.kind() {
        io::ErrorKind::TimedOut => 110,
        io::ErrorKind::WouldBlock => 11,
        io::ErrorKind::Interrupted => 4,
        _ => error.raw_os_error().unwrap_or(22) as i64,
    }
}

fn libc_einprogress() -> i64 {
    115
}

fn flags_value(args: &[i64]) -> Result<i64, i64> {
    match args.get(1) {
        None => Ok(0),
        Some(&flags) => {
            value::as_integer(flags).ok_or_else(|| type_error("flags must be an integer"))
        }
    }
}

enum Target {
    Stream(TcpStream, Option<Duration>),
    Datagram(UdpSocket, Option<Duration>),
    Tls(Box<TlsStream>, Option<Duration>),
}

fn take_target(state: &SocketState) -> Result<Target, i64> {
    let mut inner = state.lock();
    if inner.closed {
        return Err(descriptor_error());
    }
    let timeout = inner.timeout;
    let tls = matches!(&inner.kind, SocketKind::Tls(_));
    if tls {
        let previous = std::mem::replace(&mut inner.kind, SocketKind::Busy);
        return match previous {
            SocketKind::Tls(Some(tls)) => Ok(Target::Tls(tls, timeout)),
            other => {
                inner.kind = other;
                Err(io_error(not_connected_io()))
            }
        };
    }
    match &inner.kind {
        SocketKind::Stream(stream) => match stream.try_clone() {
            Ok(copy) => Ok(Target::Stream(copy, timeout)),
            Err(error) => Err(io_error(error)),
        },
        SocketKind::Datagram(Some(udp)) => match udp.try_clone() {
            Ok(copy) => Ok(Target::Datagram(copy, timeout)),
            Err(error) => Err(io_error(error)),
        },
        SocketKind::Datagram(None) => Err(io_error(not_connected_io())),
        SocketKind::Tls(_) | SocketKind::Busy => Err(blocking_error()),
        // A non-blocking connect may have completed at the TCP level without a
        // second `connect_ex` call; a successful `peer_addr` proves it.
        SocketKind::Connecting(stream) => {
            if stream.peer_addr().is_err() {
                return Err(io_error(not_connected_io()));
            }
            match stream.try_clone() {
                Ok(copy) => Ok(Target::Stream(copy, timeout)),
                Err(error) => Err(io_error(error)),
            }
        }
        SocketKind::Listener(_) | SocketKind::Unbound => Err(io_error(not_connected_io())),
    }
}

fn restore_tls(state: &SocketState, tls: Box<TlsStream>) {
    let mut inner = state.lock();
    if !inner.closed && matches!(&inner.kind, SocketKind::Busy) {
        inner.kind = SocketKind::Tls(Some(tls));
    }
}

#[cfg(unix)]
fn send_flags_stream(
    stream: &TcpStream,
    data: &[u8],
    flags: i64,
    timeout: Option<Duration>,
) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;
    let mut os_flags = flags as i32 | libc::MSG_NOSIGNAL;
    if timeout == Some(Duration::ZERO) {
        os_flags |= libc::MSG_DONTWAIT;
    }
    let fd = stream.as_raw_fd();
    parked(timeout, || {
        // SAFETY: `data` is a valid readable slice and `fd` is open.
        let result = unsafe {
            libc::send(
                fd,
                data.as_ptr() as *const libc::c_void,
                data.len(),
                os_flags,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    })
}

#[cfg(not(unix))]
fn send_flags_stream(
    stream: &TcpStream,
    data: &[u8],
    flags: i64,
    timeout: Option<Duration>,
) -> io::Result<usize> {
    let _ = (stream, flags);
    parked(timeout, || Ok(data.len()))
}

#[cfg(unix)]
fn recv_flags_stream(
    stream: &TcpStream,
    size: usize,
    flags: i64,
    timeout: Option<Duration>,
) -> io::Result<Vec<u8>> {
    use std::os::unix::io::AsRawFd;
    let mut os_flags = flags as i32;
    if timeout == Some(Duration::ZERO) {
        os_flags |= libc::MSG_DONTWAIT;
    }
    let fd = stream.as_raw_fd();
    let mut buffer = vec![0u8; size];
    parked(timeout, || {
        // SAFETY: `buffer` is valid for writes of `size` bytes and `fd` is open.
        let result =
            unsafe { libc::recv(fd, buffer.as_mut_ptr() as *mut libc::c_void, size, os_flags) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            let read = result as usize;
            buffer.truncate(read);
            Ok(buffer)
        }
    })
}

#[cfg(not(unix))]
fn recv_flags_stream(
    stream: &TcpStream,
    size: usize,
    flags: i64,
    timeout: Option<Duration>,
) -> io::Result<Vec<u8>> {
    let _ = (stream, flags);
    parked(timeout, || Ok(vec![0u8; size]))
}

#[cfg(unix)]
fn send_flags_datagram(
    udp: &UdpSocket,
    data: &[u8],
    flags: i64,
    timeout: Option<Duration>,
) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;
    let mut os_flags = flags as i32 | libc::MSG_NOSIGNAL;
    if timeout == Some(Duration::ZERO) {
        os_flags |= libc::MSG_DONTWAIT;
    }
    let fd = udp.as_raw_fd();
    parked(timeout, || {
        // SAFETY: `data` is valid and `fd` is an open datagram descriptor.
        let result = unsafe {
            libc::send(
                fd,
                data.as_ptr() as *const libc::c_void,
                data.len(),
                os_flags,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    })
}

#[cfg(not(unix))]
fn send_flags_datagram(
    _udp: &UdpSocket,
    data: &[u8],
    _flags: i64,
    timeout: Option<Duration>,
) -> io::Result<usize> {
    parked(timeout, || Ok(data.len()))
}

#[cfg(unix)]
fn recv_flags_datagram(
    udp: &UdpSocket,
    size: usize,
    flags: i64,
    timeout: Option<Duration>,
) -> io::Result<Vec<u8>> {
    use std::os::unix::io::AsRawFd;
    let mut os_flags = flags as i32;
    if timeout == Some(Duration::ZERO) {
        os_flags |= libc::MSG_DONTWAIT;
    }
    let fd = udp.as_raw_fd();
    let mut buffer = vec![0u8; size];
    parked(timeout, || {
        // SAFETY: `buffer` is valid for writes and `fd` is an open descriptor.
        let result =
            unsafe { libc::recv(fd, buffer.as_mut_ptr() as *mut libc::c_void, size, os_flags) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            let read = result as usize;
            buffer.truncate(read);
            Ok(buffer)
        }
    })
}

#[cfg(not(unix))]
fn recv_flags_datagram(
    _udp: &UdpSocket,
    size: usize,
    _flags: i64,
    timeout: Option<Duration>,
) -> io::Result<Vec<u8>> {
    parked(timeout, || Ok(vec![0u8; size]))
}

fn stream_write_all_flags(
    stream: &TcpStream,
    data: &[u8],
    flags: i64,
    timeout: Option<Duration>,
) -> io::Result<()> {
    let mut written = 0;
    while written < data.len() {
        let count = send_flags_stream(stream, &data[written..], flags, timeout)?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write"));
        }
        written += count;
    }
    Ok(())
}

fn tls_write_all(tls: &mut TlsStream, data: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < data.len() {
        let count = tls.write(&data[written..])?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write"));
        }
        written += count;
    }
    Ok(())
}

fn send(state: &SocketState, args: &[i64]) -> i64 {
    let flags = match flags_value(args) {
        Ok(flags) => flags,
        Err(err) => return err,
    };
    let Some(data) = value::as_bytes(args[0]) else {
        return type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(args[0])
        ));
    };
    let data = data.to_vec();
    let target = match take_target(state) {
        Ok(target) => target,
        Err(err) => return err,
    };
    match target {
        Target::Stream(mut stream, timeout) => {
            if let Err(err) = prepare(&stream, timeout, false) {
                return err;
            }
            let result = if flags == 0 {
                parked(timeout, || stream.write(&data))
            } else {
                send_flags_stream(&stream, &data, flags, timeout)
            };
            match result {
                Ok(written) => encode_int(written as i64),
                Err(error) => map_net_error(error, timeout),
            }
        }
        Target::Datagram(udp, timeout) => {
            if let Err(err) = prepare_udp(&udp, timeout, false) {
                return err;
            }
            let result = if flags == 0 {
                parked(timeout, || udp.send(&data))
            } else {
                send_flags_datagram(&udp, &data, flags, timeout)
            };
            match result {
                Ok(written) => encode_int(written as i64),
                Err(error) => map_net_error(error, timeout),
            }
        }
        Target::Tls(mut tls, timeout) => {
            if flags != 0 {
                restore_tls(state, tls);
                return os_error("[Errno 95] Flags are not supported on TLS sockets");
            }
            if let Err(err) = prepare_both(&tls.tcp, timeout) {
                restore_tls(state, tls);
                return err;
            }
            let result = parked(timeout, || tls.write(&data));
            restore_tls(state, tls);
            match result {
                Ok(written) => encode_int(written as i64),
                Err(error) => map_net_error(error, timeout),
            }
        }
    }
}

fn send_all(state: &SocketState, args: &[i64]) -> i64 {
    let flags = match flags_value(args) {
        Ok(flags) => flags,
        Err(err) => return err,
    };
    let Some(data) = value::as_bytes(args[0]) else {
        return type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(args[0])
        ));
    };
    let data = data.to_vec();
    let target = match take_target(state) {
        Ok(target) => target,
        Err(err) => return err,
    };
    match target {
        Target::Stream(mut stream, timeout) => {
            if let Err(err) = prepare(&stream, timeout, false) {
                return err;
            }
            let result = if flags == 0 {
                parked(timeout, || stream.write_all(&data))
            } else {
                stream_write_all_flags(&stream, &data, flags, timeout)
            };
            match result {
                Ok(()) => NONE,
                Err(error) => map_net_error(error, timeout),
            }
        }
        Target::Datagram(_, _) => os_error("[Errno 95] Operation not supported"),
        Target::Tls(mut tls, timeout) => {
            if flags != 0 {
                restore_tls(state, tls);
                return os_error("[Errno 95] Flags are not supported on TLS sockets");
            }
            if let Err(err) = prepare_both(&tls.tcp, timeout) {
                restore_tls(state, tls);
                return err;
            }
            let result = parked(timeout, || tls_write_all(&mut tls, &data));
            restore_tls(state, tls);
            match result {
                Ok(()) => NONE,
                Err(error) => map_net_error(error, timeout),
            }
        }
    }
}

fn recv(state: &SocketState, args: &[i64]) -> i64 {
    let flags = match flags_value(args) {
        Ok(flags) => flags,
        Err(err) => return err,
    };
    let size = match int_argument(args[0], "buffersize") {
        Ok(size) => size,
        Err(err) => return err,
    };
    let size = match usize::try_from(size) {
        Ok(size) => size,
        Err(_) => return value_error("negative buffersize in recv"),
    };
    if size == 0 {
        // Match CPython: zero-length receive returns immediately without
        // touching the descriptor, but a closed socket still fails.
        if state.lock().closed {
            return descriptor_error();
        }
        return value::bytes_new(Vec::new());
    }
    let target = match take_target(state) {
        Ok(target) => target,
        Err(err) => return err,
    };
    match target {
        Target::Stream(mut stream, timeout) => {
            if let Err(err) = prepare(&stream, timeout, true) {
                return err;
            }
            if flags == 0 {
                let mut buffer = vec![0u8; size];
                match parked(timeout, || stream.read(&mut buffer)) {
                    Ok(read) => {
                        buffer.truncate(read);
                        value::bytes_new(buffer)
                    }
                    Err(error) => map_net_error(error, timeout),
                }
            } else {
                match recv_flags_stream(&stream, size, flags, timeout) {
                    Ok(buffer) => value::bytes_new(buffer),
                    Err(error) => map_net_error(error, timeout),
                }
            }
        }
        Target::Datagram(udp, timeout) => {
            if let Err(err) = prepare_udp(&udp, timeout, true) {
                return err;
            }
            if flags == 0 {
                let mut buffer = vec![0u8; size];
                match parked(timeout, || udp.recv(&mut buffer)) {
                    Ok(read) => {
                        buffer.truncate(read);
                        value::bytes_new(buffer)
                    }
                    Err(error) => map_net_error(error, timeout),
                }
            } else {
                match recv_flags_datagram(&udp, size, flags, timeout) {
                    Ok(buffer) => value::bytes_new(buffer),
                    Err(error) => map_net_error(error, timeout),
                }
            }
        }
        Target::Tls(mut tls, timeout) => {
            if flags != 0 {
                restore_tls(state, tls);
                return os_error("[Errno 95] Flags are not supported on TLS sockets");
            }
            if let Err(err) = prepare_both(&tls.tcp, timeout) {
                restore_tls(state, tls);
                return err;
            }
            let mut buffer = vec![0u8; size];
            let result = parked(timeout, || tls.read(&mut buffer));
            restore_tls(state, tls);
            match result {
                Ok(read) => {
                    buffer.truncate(read);
                    value::bytes_new(buffer)
                }
                Err(error) => map_net_error(error, timeout),
            }
        }
    }
}

fn send_to(state: &SocketState, args: &[i64]) -> i64 {
    // `sendto(data, address)` or `sendto(data, flags, address)`.
    let (flags, address_arg) = if args.len() == 2 {
        (0, args[1])
    } else {
        let flags = match value::as_integer(args[1]) {
            Some(flags) => flags,
            None => return type_error("flags must be an integer"),
        };
        (flags, args[2])
    };
    let Some(data) = value::as_bytes(args[0]) else {
        return type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(args[0])
        ));
    };
    let data = data.to_vec();
    let address = match address_argument(address_arg) {
        Ok(address) => address,
        Err(err) => return err,
    };
    let snapshot = {
        let mut inner = state.lock();
        if inner.closed {
            return descriptor_error();
        }
        match &inner.kind {
            SocketKind::Datagram(_) => {}
            SocketKind::Unbound => {}
            _ => return os_error("[Errno 95] Operation not supported"),
        }
        if let Err(error) = ensure_datagram_bound(&mut inner) {
            return io_error(error);
        }
        match &inner.kind {
            SocketKind::Datagram(Some(udp)) => udp
                .try_clone()
                .map(|copy| (copy, inner.family, inner.timeout))
                .map_err(io_error),
            _ => Err(invalid_argument_error()),
        }
    };
    let (udp, family, timeout) = match snapshot {
        Ok(snapshot) => snapshot,
        Err(err) => return err,
    };
    let addrs = match resolve(&address.host, address.port, family) {
        Ok(addrs) => addrs,
        Err(error) => return io_error(error),
    };
    let addr = scope_address(addrs[0], address.scope_id);
    if flags != 0 {
        return os_error("[Errno 95] sendto() flags are not supported");
    }
    if let Err(err) = prepare_udp(&udp, timeout, false) {
        return err;
    }
    let result = parked(timeout, || udp.send_to(&data, addr));
    match result {
        Ok(written) => encode_int(written as i64),
        Err(error) => map_net_error(error, timeout),
    }
}

fn recv_from(state: &SocketState, args: &[i64]) -> i64 {
    let flags = match flags_value(args) {
        Ok(flags) => flags,
        Err(err) => return err,
    };
    if flags != 0 {
        return os_error("[Errno 95] recvfrom() flags are not supported");
    }
    let size = match int_argument(args[0], "buffersize") {
        Ok(size) => size,
        Err(err) => return err,
    };
    let size = match usize::try_from(size) {
        Ok(size) => size,
        Err(_) => return value_error("negative buffersize in recvfrom"),
    };
    let snapshot = {
        let inner = state.lock();
        if inner.closed {
            return descriptor_error();
        }
        match &inner.kind {
            SocketKind::Datagram(Some(udp)) => udp
                .try_clone()
                .map(|copy| (copy, inner.timeout))
                .map_err(io_error),
            SocketKind::Datagram(None) => Err(io_error(not_connected_io())),
            _ => Err(os_error("[Errno 95] Operation not supported")),
        }
    };
    let (udp, timeout) = match snapshot {
        Ok(snapshot) => snapshot,
        Err(err) => return err,
    };
    if let Err(err) = prepare_udp(&udp, timeout, true) {
        return err;
    }
    let mut buffer = vec![0u8; size];
    match parked(timeout, || udp.recv_from(&mut buffer)) {
        Ok((read, addr)) => {
            buffer.truncate(read);
            let data = value::bytes_new(buffer);
            value::tuple_new(vec![data, tuple_addr(addr)])
        }
        Err(error) => map_net_error(error, timeout),
    }
}

fn shutdown(state: &SocketState, how: i64) -> i64 {
    let mode = match how {
        SHUT_RD => std::net::Shutdown::Read,
        SHUT_WR => std::net::Shutdown::Write,
        SHUT_RDWR => std::net::Shutdown::Both,
        _ => return invalid_argument_error(),
    };
    // TLS sessions are moved out for the call, as with read/write.
    let tls = {
        let mut inner = state.lock();
        if inner.closed {
            return descriptor_error();
        }
        if matches!(&inner.kind, SocketKind::Tls(_)) {
            match std::mem::replace(&mut inner.kind, SocketKind::Busy) {
                SocketKind::Tls(Some(tls)) => Some((tls, inner.timeout)),
                other => {
                    inner.kind = other;
                    return os_error("[Errno 95] Operation not supported");
                }
            }
        } else {
            None
        }
    };
    if let Some((mut tls, timeout)) = tls {
        tls.conn.send_close_notify();
        let result = parked(timeout, || tls.flush());
        let _ = tls.tcp.shutdown(mode);
        restore_tls(state, tls);
        return match result {
            Ok(()) => NONE,
            Err(error) => io_error(error),
        };
    }
    let outcome = {
        let inner = state.lock();
        if inner.closed {
            Err(None)
        } else {
            match &inner.kind {
                SocketKind::Stream(stream) | SocketKind::Connecting(stream) => {
                    stream.shutdown(mode).map_err(Some)
                }
                SocketKind::Datagram(_) => Err(Some(io::Error::from_raw_os_error(95))),
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

fn getsockname(state: &SocketState) -> i64 {
    let result: Result<SocketAddr, Option<io::Error>> = {
        let inner = state.lock();
        if inner.closed {
            Err(None)
        } else {
            match &inner.kind {
                SocketKind::Unbound | SocketKind::Datagram(None) => {
                    if inner.family == AF_INET6 {
                        Ok(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)))
                    } else {
                        Ok(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
                    }
                }
                SocketKind::Listener(listener) => listener.local_addr().map_err(Some),
                SocketKind::Stream(stream) | SocketKind::Connecting(stream) => {
                    stream.local_addr().map_err(Some)
                }
                SocketKind::Datagram(Some(udp)) => udp.local_addr().map_err(Some),
                SocketKind::Tls(Some(tls)) => tls.tcp.local_addr().map_err(Some),
                SocketKind::Tls(None) | SocketKind::Busy => Err(Some(not_connected_io())),
            }
        }
    };
    match result {
        Ok(addr) => tuple_addr(addr),
        Err(None) => descriptor_error(),
        Err(Some(error)) => io_error(error),
    }
}

fn getpeername(state: &SocketState) -> i64 {
    let result: Result<SocketAddr, Option<io::Error>> = {
        let inner = state.lock();
        if inner.closed {
            Err(None)
        } else {
            match &inner.kind {
                SocketKind::Stream(stream) | SocketKind::Connecting(stream) => {
                    stream.peer_addr().map_err(Some)
                }
                SocketKind::Datagram(Some(udp)) => udp.peer_addr().map_err(Some),
                SocketKind::Tls(Some(tls)) => tls.tcp.peer_addr().map_err(Some),
                SocketKind::Tls(None) | SocketKind::Busy => Err(Some(not_connected_io())),
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

fn detach(state: &SocketState) -> i64 {
    let mut inner = state.lock();
    if inner.closed {
        return descriptor_error();
    }
    let kind = std::mem::replace(&mut inner.kind, SocketKind::Unbound);
    let fd = match kind {
        #[cfg(unix)]
        SocketKind::Listener(listener) => {
            use std::os::unix::io::IntoRawFd;
            listener.into_raw_fd()
        }
        #[cfg(not(unix))]
        SocketKind::Listener(_) => -1,
        #[cfg(unix)]
        SocketKind::Stream(stream) | SocketKind::Connecting(stream) => {
            use std::os::unix::io::IntoRawFd;
            stream.into_raw_fd()
        }
        #[cfg(not(unix))]
        SocketKind::Stream(_) | SocketKind::Connecting(_) => -1,
        #[cfg(unix)]
        SocketKind::Datagram(Some(udp)) => {
            use std::os::unix::io::IntoRawFd;
            udp.into_raw_fd()
        }
        #[cfg(not(unix))]
        SocketKind::Datagram(Some(_)) => -1,
        SocketKind::Datagram(None) => -1,
        SocketKind::Tls(Some(tls)) => {
            #[cfg(unix)]
            {
                use std::os::unix::io::IntoRawFd;
                let TlsStream { tcp, .. } = *tls;
                tcp.into_raw_fd()
            }
            #[cfg(not(unix))]
            {
                let _ = tls;
                -1
            }
        }
        other @ (SocketKind::Tls(None) | SocketKind::Busy) => {
            inner.kind = other;
            return blocking_error();
        }
        SocketKind::Unbound => -1,
    };
    inner.closed = true;
    if fd < 0 {
        return descriptor_error();
    }
    encode_int(fd as i64)
}

fn duplicate(state: &SocketState) -> i64 {
    let (kind, family, sock_type, timeout, options) = {
        let inner = state.lock();
        if inner.closed {
            return descriptor_error();
        }
        let kind = match &inner.kind {
            SocketKind::Unbound => SocketKind::Unbound,
            SocketKind::Listener(listener) => match listener.try_clone() {
                Ok(copy) => SocketKind::Listener(copy),
                Err(error) => return io_error(error),
            },
            SocketKind::Stream(stream) => match stream.try_clone() {
                Ok(copy) => SocketKind::Stream(copy),
                Err(error) => return io_error(error),
            },
            SocketKind::Connecting(stream) => match stream.try_clone() {
                Ok(copy) => SocketKind::Connecting(copy),
                Err(error) => return io_error(error),
            },
            SocketKind::Datagram(Some(udp)) => match udp.try_clone() {
                Ok(copy) => SocketKind::Datagram(Some(copy)),
                Err(error) => return io_error(error),
            },
            SocketKind::Datagram(None) => SocketKind::Datagram(None),
            SocketKind::Tls(Some(_)) | SocketKind::Tls(None) => {
                return os_error("[Errno 95] Operation not supported for TLS sockets")
            }
            SocketKind::Busy => return blocking_error(),
        };
        (
            kind,
            inner.family,
            inner.sock_type,
            inner.timeout,
            inner.options.clone(),
        )
    };
    let copy = SocketInner {
        kind,
        family,
        sock_type,
        timeout,
        closed: false,
        options,
    };
    value::socket_new(SocketState::from_inner(copy))
}

fn unwrap_tls(_obj: i64, state: &SocketState) -> i64 {
    let taken = {
        let mut inner = state.lock();
        if inner.closed {
            return descriptor_error();
        }
        let timeout = inner.timeout;
        match std::mem::replace(&mut inner.kind, SocketKind::Busy) {
            SocketKind::Tls(Some(tls)) => Some((tls, timeout)),
            other => {
                inner.kind = other;
                None
            }
        }
    };
    let Some((mut tls, timeout)) = taken else {
        return os_error("[Errno 95] Socket is not a TLS socket");
    };
    tls.conn.send_close_notify();
    let _ = parked(timeout, || tls.flush());
    let TlsStream { tcp, .. } = *tls;
    let mut inner = state.lock();
    if !inner.closed && matches!(&inner.kind, SocketKind::Busy) {
        inner.kind = SocketKind::Stream(tcp);
    }
    NONE
}

// -------------------------------------------------------------------------
// TLS (client side)
// -------------------------------------------------------------------------

/// A verifier used for `CERT_NONE`: it accepts any certificate but still
/// verifies handshake signatures, so `SOCK_STREAM` framing is authentic.
#[derive(Debug)]
struct NoCertificateVerification(Arc<CryptoProvider>);

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_io_error(error: rustls::Error) -> io::Error {
    io::Error::other(format!("[SSL] {error}"))
}

impl TlsStream {
    /// Drives the handshake until rustls stops handshaking.
    fn handshake(&mut self) -> io::Result<()> {
        while self.conn.is_handshaking() {
            while self.conn.wants_write() {
                self.conn.write_tls(&mut self.tcp)?;
            }
            if self.conn.wants_read() {
                if self.conn.read_tls(&mut self.tcp)? == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "peer closed during the TLS handshake",
                    ));
                }
                self.conn.process_new_packets().map_err(tls_io_error)?;
            }
        }
        self.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            self.conn.write_tls(&mut self.tcp)?;
        }
        Ok(())
    }

    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // A previous `write` may have left records buffered; flush them before
        // waiting for the peer (ignoring a non-blocking stall is not possible:
        // the caller must retry, matching SSLWantWrite semantics).
        if self.conn.wants_write() {
            self.flush()?;
        }
        loop {
            match self.conn.reader().read(buffer) {
                Ok(0) => return Ok(0),
                Ok(read) => return Ok(read),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            if self.conn.wants_read() {
                if self.conn.read_tls(&mut self.tcp)? == 0 {
                    // EOF: let `process_new_packets` update the reader state so
                    // the next `read` reports close_notify or unexpected EOF.
                    self.conn.process_new_packets().map_err(tls_io_error)?;
                } else {
                    self.conn.process_new_packets().map_err(tls_io_error)?;
                }
                continue;
            }
            return Ok(0);
        }
    }

    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // Flush bytes buffered by an earlier call before accepting more, so a
        // non-blocking retry can never duplicate application data.
        self.flush()?;
        let written = self.conn.writer().write(data)?;
        match self.flush() {
            Ok(()) => Ok(written),
            // The record is already accepted into rustls' outbound buffer; the
            // next call flushes it. Reporting success avoids a retry that would
            // duplicate the plaintext.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(written)
            }
            Err(error) => Err(error),
        }
    }
}

fn parse_pem_certificates(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut certificates: Vec<Vec<u8>> = CertificateDer::pem_slice_iter(bytes)
        .filter_map(Result::ok)
        .map(|certificate| certificate.as_ref().to_vec())
        .collect();
    // `ssl.load_verify_locations(cadata=...)` also accepts raw DER bytes.
    if certificates.is_empty()
        && !bytes.is_empty()
        && !bytes.windows(11).any(|w| w == &b"-----BEGIN "[..])
    {
        certificates.push(bytes.to_vec());
    }
    certificates
}

fn parse_certificate_file(path: &str) -> Result<Vec<Vec<u8>>, i64> {
    let bytes = std::fs::read(path).map_err(io_error)?;
    let certificates: Vec<Vec<u8>> = CertificateDer::pem_slice_iter(&bytes)
        .filter_map(Result::ok)
        .map(|certificate| certificate.as_ref().to_vec())
        .collect();
    if certificates.is_empty() {
        return Err(ssl_error(&format!("no certificates found in {path}")));
    }
    Ok(certificates)
}

fn context_attr(ctx: i64, name: &str) -> Option<i64> {
    value::instance_attr(ctx, name)
}

fn context_int(ctx: i64, name: &str, default: i64) -> i64 {
    context_attr(ctx, name)
        .and_then(value::as_integer)
        .unwrap_or(default)
}

fn context_bool(ctx: i64, name: &str, default: bool) -> bool {
    context_attr(ctx, name)
        .map(value::truthy)
        .unwrap_or(default)
}

fn set_context_attr(ctx: i64, name: &str, value: i64) {
    value::instance_setattr(ctx, name.to_string(), value);
}

/// Stored `verify_mode` (`CERT_*`) of a context instance.
fn context_verify_mode(ctx: i64) -> i64 {
    context_int(ctx, "_verify_mode", CERT_NONE)
}

/// Stored `check_hostname` flag of a context instance.
fn context_check_hostname(ctx: i64) -> bool {
    context_bool(ctx, "_check_hostname", false)
}

/// Stored `protocol` selector of a context instance.
fn context_protocol(ctx: i64) -> i64 {
    context_int(ctx, "_protocol", PROTOCOL_TLS)
}

/// ALPN protocol names configured through `set_alpn_protocols`.
fn context_alpn(ctx: i64) -> Vec<Vec<u8>> {
    let mut protocols = Vec::new();
    if let Some(items) = context_attr(ctx, "_alpn").and_then(value::as_list) {
        for &item in items {
            if let Some(bytes) = value::as_bytes(item) {
                protocols.push(bytes.to_vec());
            }
        }
    }
    protocols
}

/// Applies CPython's `verify_mode` rules: only `CERT_NONE`, `CERT_OPTIONAL`
/// and `CERT_REQUIRED` are accepted, and `CERT_NONE` is rejected while
/// host-name checking is enabled.
fn set_context_verify_mode(ctx: i64, mode: i64) -> Result<(), i64> {
    if !matches!(mode, CERT_NONE | CERT_OPTIONAL | CERT_REQUIRED) {
        return Err(value_error("invalid verify_mode"));
    }
    if mode == CERT_NONE && context_check_hostname(ctx) {
        return Err(value_error(
            "Cannot set verify_mode to CERT_NONE when check_hostname is enabled.",
        ));
    }
    set_context_attr(ctx, "_verify_mode", encode_int(mode));
    Ok(())
}

/// Applies CPython's `check_hostname` rules: enabling it while `verify_mode`
/// is `CERT_NONE` is rejected.
fn set_context_check_hostname(ctx: i64, enabled: bool) -> Result<(), i64> {
    if enabled && context_verify_mode(ctx) == CERT_NONE {
        return Err(value_error(
            "Cannot set check_hostname to True when verify_mode is CERT_NONE",
        ));
    }
    set_context_attr(ctx, "_check_hostname", value::bool_value(enabled));
    Ok(())
}

/// The purpose selector passed to `create_default_context()` /
/// `_create_stdlib_context()`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PurposeKind {
    ServerAuth,
    ClientAuth,
}

/// `ssl.Purpose`: the X.509 extended-key-usage selectors the stdlib passes
/// when it builds a context.
fn purpose_class() -> i64 {
    if let Some(class) = globals::registry_get("ssl.Purpose") {
        return class;
    }
    let class = value::class_new("Purpose".to_string(), NONE, Vec::new());
    globals::registry_set("ssl.Purpose", class);
    let server = purpose_member(class, PurposeKind::ServerAuth);
    let client = purpose_member(class, PurposeKind::ClientAuth);
    value::class_set_attr(class, "SERVER_AUTH".to_string(), server);
    value::class_set_attr(class, "CLIENT_AUTH".to_string(), client);
    class
}

fn purpose_member(class: i64, kind: PurposeKind) -> i64 {
    let key = match kind {
        PurposeKind::ServerAuth => "ssl.Purpose.SERVER_AUTH",
        PurposeKind::ClientAuth => "ssl.Purpose.CLIENT_AUTH",
    };
    if let Some(member) = globals::registry_get(key) {
        return member;
    }
    let (shortname, longname, oid) = match kind {
        PurposeKind::ServerAuth => (
            "serverAuth",
            "TLS Web Server Authentication",
            "1.3.6.1.5.5.7.3.1",
        ),
        PurposeKind::ClientAuth => (
            "clientAuth",
            "TLS Web Client Authentication",
            "1.3.6.1.5.5.7.3.2",
        ),
    };
    let member = value::instance_new(class);
    set_context_attr(member, "shortname", value::str_alloc(shortname.to_string()));
    set_context_attr(member, "longname", value::str_alloc(longname.to_string()));
    set_context_attr(member, "oid", value::str_alloc(oid.to_string()));
    globals::registry_set(key, member);
    member
}

/// Decodes a `purpose` argument: `None` means `Purpose.SERVER_AUTH`, and a
/// value that is not one of the two `Purpose` members is rejected like
/// CPython's `isinstance(purpose, _ASN1Object)` check.
fn purpose_argument(purpose: i64) -> Result<PurposeKind, i64> {
    if purpose == NONE {
        return Ok(PurposeKind::ServerAuth);
    }
    let class = purpose_class();
    if purpose == purpose_member(class, PurposeKind::ServerAuth) {
        return Ok(PurposeKind::ServerAuth);
    }
    if purpose == purpose_member(class, PurposeKind::ClientAuth) {
        return Ok(PurposeKind::ClientAuth);
    }
    Err(type_error(&format!(
        "purpose must be a Purpose member, not {}",
        value::type_name(purpose)
    )))
}

/// Wraps a native entry point as a class-method value.
fn ssl_method(entry: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    value::function_new(entry as *const () as usize, arity, name.to_string())
}

/// Wraps a native entry point whose argument count is dynamic (the first
/// element of the argument buffer is the tagged count).
fn ssl_varargs_method(entry: extern "C" fn(*const i64) -> i64, name: &str) -> i64 {
    value::function_new_with_flags(
        entry as *const () as usize,
        1,
        name.to_string(),
        value::FUNCTION_FLAG_VARARGS,
    )
}

extern "C" fn verify_mode_get(argv: *const i64) -> i64 {
    // SAFETY: a bound method call passes `self` first.
    let ctx = unsafe { *argv };
    encode_int(context_verify_mode(ctx))
}

extern "C" fn verify_mode_set(argv: *const i64) -> i64 {
    // SAFETY: an assignment through a property passes `self` and the value.
    let ctx = unsafe { *argv };
    let new_value = unsafe { *argv.add(1) };
    let Some(mode) = value::as_integer(new_value) else {
        return type_error("verify_mode must be an integer");
    };
    match set_context_verify_mode(ctx, mode) {
        Ok(()) => NONE,
        Err(err) => err,
    }
}

extern "C" fn check_hostname_get(argv: *const i64) -> i64 {
    // SAFETY: a bound method call passes `self` first.
    let ctx = unsafe { *argv };
    value::bool_value(context_check_hostname(ctx))
}

extern "C" fn check_hostname_set(argv: *const i64) -> i64 {
    // SAFETY: an assignment through a property passes `self` and the value.
    let ctx = unsafe { *argv };
    let enabled = value::truthy(unsafe { *argv.add(1) });
    match set_context_check_hostname(ctx, enabled) {
        Ok(()) => NONE,
        Err(err) => err,
    }
}

extern "C" fn post_handshake_auth_get(argv: *const i64) -> i64 {
    // SAFETY: a bound method call passes `self` first.
    let ctx = unsafe { *argv };
    value::bool_value(context_bool(ctx, "_post_handshake_auth", false))
}

extern "C" fn post_handshake_auth_set(argv: *const i64) -> i64 {
    // SAFETY: an assignment through a property passes `self` and the value.
    let ctx = unsafe { *argv };
    let enabled = value::truthy(unsafe { *argv.add(1) });
    set_context_attr(ctx, "_post_handshake_auth", value::bool_value(enabled));
    NONE
}

extern "C" fn protocol_get(argv: *const i64) -> i64 {
    // SAFETY: a bound method call passes `self` first.
    let ctx = unsafe { *argv };
    encode_int(context_protocol(ctx))
}

extern "C" fn options_get(argv: *const i64) -> i64 {
    // SAFETY: a bound method call passes `self` first.
    let ctx = unsafe { *argv };
    encode_int(context_int(ctx, "_options", OP_ALL))
}

extern "C" fn options_set(argv: *const i64) -> i64 {
    // SAFETY: an assignment through a property passes `self` and the value.
    let ctx = unsafe { *argv };
    let Some(options) = value::as_integer(unsafe { *argv.add(1) }) else {
        return type_error("options must be an integer");
    };
    set_context_attr(ctx, "_options", encode_int(options));
    NONE
}

/// `SSLContext.set_alpn_protocols(protocols)`: stores the names handed to
/// rustls for the client hello. CPython's Python-level wrapper accepts
/// ASCII strings, so both `str` and `bytes` items are accepted here.
extern "C" fn set_alpn_protocols(argv: *const i64) -> i64 {
    // SAFETY: a bound method call passes `self` then the sequence.
    let ctx = unsafe { *argv };
    let protocols = unsafe { *argv.add(1) };
    let items = value::as_list(protocols)
        .or_else(|| value::as_tuple(protocols))
        .map(|items| items.to_vec());
    let Some(items) = items else {
        return type_error(&format!(
            "set_alpn_protocols() argument must be a sequence, not {}",
            value::type_name(protocols)
        ));
    };
    let mut encoded: Vec<Vec<u8>> = Vec::with_capacity(items.len());
    for item in items {
        let bytes = if let Some(text) = value::as_str(item) {
            if !text.is_ascii() {
                return exc::raise_with_message(
                    UNICODE_ERROR,
                    "'ascii' codec can't encode non-ASCII ALPN protocol name",
                );
            }
            text.as_bytes().to_vec()
        } else if let Some(bytes) = value::as_bytes(item) {
            bytes.to_vec()
        } else {
            return type_error(&format!(
                "protocol name must be str or bytes, not {}",
                value::type_name(item)
            ));
        };
        if bytes.is_empty() || bytes.len() > ALPN_MAX_LENGTH {
            return ssl_error("ALPN protocols must be 1 to 255 in length");
        }
        encoded.push(bytes);
    }
    let stored: Vec<i64> = encoded.into_iter().map(value::bytes_new).collect();
    set_context_attr(ctx, "_alpn", value::list_new(stored));
    NONE
}

/// `SSLContext.load_default_certs(purpose=Purpose.SERVER_AUTH)`: marks the
/// Mozilla (webpki) trust anchors as enabled. rustls ships one trust store,
/// so the purpose only validates the selector.
extern "C" fn load_default_certs(argv: *const i64) -> i64 {
    // A dynamic (varargs) wrapper receives the tagged argument count first.
    // SAFETY: the caller supplies the count followed by the arguments.
    let count = value::as_integer(unsafe { *argv }).unwrap_or(1);
    if count > 2 {
        return type_error(&format!(
            "load_default_certs() takes at most 2 arguments ({count} given)"
        ));
    }
    let ctx = unsafe { *argv.add(1) };
    let purpose = if count > 1 {
        unsafe { *argv.add(2) }
    } else {
        NONE
    };
    match purpose_argument(purpose) {
        Ok(_) => {
            set_context_attr(ctx, "_default_roots", value::TRUE);
            NONE
        }
        Err(err) => err,
    }
}

/// The cached `ssl.SSLContext` class shared by every context instance.
fn ssl_context_class() -> i64 {
    if let Some(class) = globals::registry_get("ssl.SSLContext") {
        return class;
    }
    let methods = vec![
        (
            "wrap_socket".to_string(),
            value::native_function_new("ssl", "_ssl_context_wrap_socket", 3),
            MethodKind::Normal,
        ),
        (
            "load_verify_locations".to_string(),
            value::native_function_new("ssl", "_ssl_context_load_verify_locations", 4),
            MethodKind::Normal,
        ),
        (
            "load_cert_chain".to_string(),
            value::native_function_new("ssl", "_ssl_context_load_cert_chain", 4),
            MethodKind::Normal,
        ),
        (
            "load_default_certs".to_string(),
            ssl_varargs_method(load_default_certs, "load_default_certs"),
            MethodKind::Normal,
        ),
        (
            "set_alpn_protocols".to_string(),
            ssl_method(set_alpn_protocols, 2, "set_alpn_protocols"),
            MethodKind::Normal,
        ),
    ];
    let attrs = vec![
        (
            "verify_mode".to_string(),
            value::property_new(
                ssl_method(verify_mode_get, 1, "verify_mode"),
                ssl_method(verify_mode_set, 2, "verify_mode"),
                NONE,
            ),
        ),
        (
            "check_hostname".to_string(),
            value::property_new(
                ssl_method(check_hostname_get, 1, "check_hostname"),
                ssl_method(check_hostname_set, 2, "check_hostname"),
                NONE,
            ),
        ),
        (
            "post_handshake_auth".to_string(),
            value::property_new(
                ssl_method(post_handshake_auth_get, 1, "post_handshake_auth"),
                ssl_method(post_handshake_auth_set, 2, "post_handshake_auth"),
                NONE,
            ),
        ),
        (
            "protocol".to_string(),
            value::property_new(ssl_method(protocol_get, 1, "protocol"), NONE, NONE),
        ),
        (
            "options".to_string(),
            value::property_new(
                ssl_method(options_get, 1, "options"),
                ssl_method(options_set, 2, "options"),
                NONE,
            ),
        ),
    ];
    let class = value::class_new_full("SSLContext".to_string(), NONE, methods, attrs);
    globals::registry_set("ssl.SSLContext", class);
    class
}

/// The cached `ssl.SSLSocket` class. Hydro's TLS state lives inside the same
/// `socket` object (`wrap_socket` returns its argument), so this value exists
/// for the stdlib's `isinstance(conn, ssl.SSLSocket)` checks and as a
/// subclass of `socket.socket`; wrapped objects keep their `socket.socket`
/// class and never match it.
fn ssl_socket_class() -> i64 {
    if let Some(class) = globals::registry_get("ssl.SSLSocket") {
        return class;
    }
    let class = value::class_new("SSLSocket".to_string(), value::socket_class(), Vec::new());
    globals::registry_set("ssl.SSLSocket", class);
    class
}

fn context_roots(ctx: i64) -> Vec<Vec<u8>> {
    let mut roots = Vec::new();
    if let Some(items) = context_attr(ctx, "_roots").and_then(value::as_list) {
        for &item in items {
            if let Some(bytes) = value::as_bytes(item) {
                roots.push(bytes.to_vec());
            }
        }
    }
    roots
}

fn set_context_roots(ctx: i64, roots: Vec<Vec<u8>>) {
    let items: Vec<i64> = roots.into_iter().map(value::bytes_new).collect();
    set_context_attr(ctx, "_roots", value::list_new(items));
}

fn context_cert_chain(ctx: i64) -> Vec<Vec<u8>> {
    let mut chain = Vec::new();
    if let Some(items) = context_attr(ctx, "_cert_chain").and_then(value::as_list) {
        for &item in items {
            if let Some(bytes) = value::as_bytes(item) {
                chain.push(bytes.to_vec());
            }
        }
    }
    chain
}

fn build_client_config(ctx: i64) -> Result<Arc<ClientConfig>, i64> {
    let verify_mode = context_verify_mode(ctx);
    let default_roots = context_bool(ctx, "_default_roots", false);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|error| ssl_error(&error.to_string()))?;
    let builder = if verify_mode == CERT_NONE {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification(provider)))
    } else {
        let mut store = RootCertStore::empty();
        if default_roots {
            store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let roots = context_roots(ctx);
        let (added, ignored) =
            store.add_parsable_certificates(roots.iter().cloned().map(CertificateDer::from));
        if ignored > 0 {
            return Err(ssl_error("one or more CA certificates could not be parsed"));
        }
        if added == 0 && !default_roots {
            return Err(ssl_error("no trust anchors loaded"));
        }
        builder.with_root_certificates(store)
    };
    let chain = context_cert_chain(ctx);
    let key = context_attr(ctx, "_key").filter(|&key| key != NONE);
    let mut config = match key {
        Some(key) => {
            let Some(key_bytes) = value::as_bytes(key) else {
                return Err(ssl_error("invalid private key"));
            };
            let kind = context_int(ctx, "_key_kind", 2);
            let key = match kind {
                0 => PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(key_bytes.to_vec())),
                1 => PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(key_bytes.to_vec())),
                _ => PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes.to_vec())),
            };
            let certificates: Vec<CertificateDer<'static>> =
                chain.into_iter().map(CertificateDer::from).collect();
            builder
                .with_client_auth_cert(certificates, key)
                .map_err(|error| ssl_error(&error.to_string()))?
        }
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = context_alpn(ctx);
    Ok(Arc::new(config))
}

fn ssl_context(args: &[i64]) -> i64 {
    let protocol = match args.first() {
        Some(&v) if v != NONE => match int_argument(v, "protocol") {
            Ok(protocol) => protocol,
            Err(err) => return err,
        },
        _ => PROTOCOL_TLS,
    };
    if !matches!(
        protocol,
        PROTOCOL_TLS | PROTOCOL_TLS_CLIENT | PROTOCOL_TLS_SERVER | 5
    ) {
        return value_error(&format!("Unsupported protocol version 0x{protocol:x}"));
    }
    // `PROTOCOL_TLS_CLIENT` verifies the peer and its host name;
    // `PROTOCOL_TLS_SERVER` and the generic `PROTOCOL_TLS` verify nothing,
    // exactly like CPython's `_ssl._SSLContext`.
    let client = protocol == PROTOCOL_TLS_CLIENT;
    let context = value::instance_new(ssl_context_class());
    set_context_attr(
        context,
        "_verify_mode",
        encode_int(if client { CERT_REQUIRED } else { CERT_NONE }),
    );
    set_context_attr(context, "_check_hostname", value::bool_value(client));
    // Only `create_default_context()`/`load_default_certs()` pre-load the
    // Mozilla roots; a bare `SSLContext(...)` starts with an empty trust
    // store, like CPython.
    set_context_attr(context, "_default_roots", value::FALSE);
    set_context_attr(context, "_post_handshake_auth", value::FALSE);
    set_context_attr(context, "_roots", value::list_new(Vec::new()));
    set_context_attr(context, "_cert_chain", value::list_new(Vec::new()));
    set_context_attr(context, "_key", NONE);
    set_context_attr(context, "_key_kind", encode_int(2));
    set_context_attr(context, "_alpn", value::list_new(Vec::new()));
    set_context_attr(context, "_options", encode_int(OP_ALL));
    set_context_attr(
        context,
        "_server_side",
        value::bool_value(protocol == PROTOCOL_TLS_SERVER),
    );
    set_context_attr(context, "_protocol", encode_int(protocol));
    context
}

fn create_default_context(args: &[i64]) -> i64 {
    let purpose = match purpose_argument(args.first().copied().unwrap_or(NONE)) {
        Ok(purpose) => purpose,
        Err(err) => return err,
    };
    let protocol = match purpose {
        PurposeKind::ServerAuth => PROTOCOL_TLS_CLIENT,
        PurposeKind::ClientAuth => PROTOCOL_TLS_SERVER,
    };
    let context = ssl_context(&[encode_int(protocol)]);
    if exc::pending() {
        return context;
    }
    let cafile = args.get(1).copied().unwrap_or(NONE);
    let capath = args.get(2).copied().unwrap_or(NONE);
    let cadata = args.get(3).copied().unwrap_or(NONE);
    if cafile != NONE || capath != NONE || cadata != NONE {
        let result = context_load_verify_locations(&[context, cafile, capath, cadata]);
        if exc::pending() {
            return result;
        }
    } else if context_verify_mode(context) != CERT_NONE {
        set_context_attr(context, "_default_roots", value::TRUE);
    }
    context
}

/// `ssl._create_stdlib_context(...)`/`_create_unverified_context(...)`:
/// CPython's backward-compatible context factory used by `ftplib`,
/// `imaplib`, `poplib` and `smtplib`.
///
/// The defaults are CPython 3.14's (`cert_reqs=CERT_NONE`,
/// `check_hostname=False`, `purpose=Purpose.SERVER_AUTH`); an explicitly
/// supplied `None` keeps the protocol default, which is how the stdlib
/// modules build unverified contexts while still allowing callers to ask for
/// verification.
fn create_stdlib_context(args: &[i64]) -> i64 {
    let purpose = match purpose_argument(args.get(3).copied().unwrap_or(NONE)) {
        Ok(purpose) => purpose,
        Err(err) => return err,
    };
    let protocol = match args.first().copied().unwrap_or(NONE) {
        NONE => match purpose {
            PurposeKind::ServerAuth => PROTOCOL_TLS_CLIENT,
            PurposeKind::ClientAuth => PROTOCOL_TLS_SERVER,
        },
        v => match value::as_integer(v) {
            Some(protocol) => protocol,
            None => {
                return type_error(&format!(
                    "protocol must be an integer, not {}",
                    value::type_name(v)
                ))
            }
        },
    };
    let context = ssl_context(&[encode_int(protocol)]);
    if exc::pending() {
        return context;
    }
    // CPython assigns `check_hostname` first so a `CERT_NONE` context with
    // host-name checking fails with the intended `ValueError`.
    let check_hostname = args.get(2).copied().map(value::truthy).unwrap_or(false);
    if let Err(err) = set_context_check_hostname(context, check_hostname) {
        return err;
    }
    match args.get(1).copied() {
        // The keyword-only default is `cert_reqs=CERT_NONE`.
        None => {
            if let Err(err) = set_context_verify_mode(context, CERT_NONE) {
                return err;
            }
        }
        // An explicit `None` keeps the protocol default, like CPython.
        Some(NONE) => {}
        Some(v) => {
            let Some(mode) = value::as_integer(v) else {
                return type_error("cert_reqs must be an integer");
            };
            if let Err(err) = set_context_verify_mode(context, mode) {
                return err;
            }
        }
    }
    let certfile = args.get(4).copied().unwrap_or(NONE);
    let keyfile = args.get(5).copied().unwrap_or(NONE);
    if keyfile != NONE && certfile == NONE {
        return value_error("certfile must be specified");
    }
    if certfile != NONE || keyfile != NONE {
        let result = context_load_cert_chain(&[context, certfile, keyfile, NONE]);
        if exc::pending() {
            return result;
        }
    }
    let cafile = args.get(6).copied().unwrap_or(NONE);
    let capath = args.get(7).copied().unwrap_or(NONE);
    let cadata = args.get(8).copied().unwrap_or(NONE);
    if cafile != NONE || capath != NONE || cadata != NONE {
        let result = context_load_verify_locations(&[context, cafile, capath, cadata]);
        if exc::pending() {
            return result;
        }
    } else if context_verify_mode(context) != CERT_NONE {
        set_context_attr(context, "_default_roots", value::TRUE);
    }
    context
}

fn context_load_verify_locations(args: &[i64]) -> i64 {
    let ctx = args[0];
    let cafile = args.get(1).copied().unwrap_or(NONE);
    let capath = args.get(2).copied().unwrap_or(NONE);
    let cadata = args.get(3).copied().unwrap_or(NONE);
    if cafile == NONE && capath == NONE && cadata == NONE {
        return type_error("cafile, capath and cadata cannot be all omitted");
    }
    let mut loaded: Vec<Vec<u8>> = Vec::new();
    if cadata != NONE {
        if let Some(text) = value::as_str(cadata) {
            loaded.extend(parse_pem_certificates(text.as_bytes()));
        } else if let Some(bytes) = value::as_bytes(cadata) {
            loaded.extend(parse_pem_certificates(bytes));
        } else {
            return type_error("cadata should be a string or bytes");
        }
    }
    if cafile != NONE {
        let Some(path) = value::as_str(cafile) else {
            return type_error("cafile must be a string");
        };
        match parse_certificate_file(path) {
            Ok(certificates) => loaded.extend(certificates),
            Err(err) => return err,
        }
    }
    if capath != NONE {
        let Some(path) = value::as_str(capath) else {
            return type_error("capath must be a string");
        };
        match std::fs::read_dir(path) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if let Ok(bytes) = std::fs::read(entry.path()) {
                        loaded.extend(parse_pem_certificates(&bytes));
                    }
                }
            }
            Err(error) => return io_error(error),
        }
    }
    if loaded.is_empty() {
        return ssl_error("no certificates loaded");
    }
    let mut roots = context_roots(ctx);
    roots.extend(loaded);
    set_context_roots(ctx, roots);
    NONE
}

fn context_load_cert_chain(args: &[i64]) -> i64 {
    let ctx = args[0];
    let Some(certfile) = args.get(1).and_then(|&v| value::as_str(v)) else {
        return type_error("certfile must be a string");
    };
    let keyfile = match args.get(2) {
        Some(&v) if v != NONE => match value::as_str(v) {
            Some(path) => path.to_string(),
            None => return type_error("keyfile must be a string or None"),
        },
        _ => certfile.to_string(),
    };
    if let Some(&password) = args.get(3) {
        if password != NONE {
            return ssl_error("encrypted private keys are not supported");
        }
    }
    let certificates = match parse_certificate_file(certfile) {
        Ok(certificates) => certificates,
        Err(err) => return err,
    };
    let key_bytes = match std::fs::read(&keyfile) {
        Ok(bytes) => bytes,
        Err(error) => return io_error(error),
    };
    let key = match PrivateKeyDer::from_pem_slice(&key_bytes) {
        Ok(key) => key,
        Err(_) => return ssl_error("could not load the private key"),
    };
    let (kind, der) = match key {
        PrivateKeyDer::Pkcs1(key) => (0, key.secret_pkcs1_der().to_vec()),
        PrivateKeyDer::Sec1(key) => (1, key.secret_sec1_der().to_vec()),
        PrivateKeyDer::Pkcs8(key) => (2, key.secret_pkcs8_der().to_vec()),
        _ => return ssl_error("unsupported private key format"),
    };
    let cert_values: Vec<i64> = certificates.into_iter().map(value::bytes_new).collect();
    set_context_attr(ctx, "_cert_chain", value::list_new(cert_values));
    set_context_attr(ctx, "_key", value::bytes_new(der));
    set_context_attr(ctx, "_key_kind", encode_int(kind));
    NONE
}

/// Host text of a connected stream's peer, used when a verifying context is
/// wrapped without an explicit `server_hostname` (no SNI).
fn peer_host(state: &SocketState) -> Option<String> {
    let inner = state.lock();
    match &inner.kind {
        SocketKind::Stream(stream) => stream.peer_addr().ok().map(|addr| addr.ip().to_string()),
        _ => None,
    }
}

/// Wraps `sock` in a client TLS session, performing the handshake now.
fn wrap_tls(context: i64, sock: i64, server_hostname: i64) -> i64 {
    let Some(state) = value::as_socket(sock) else {
        return type_error("wrap_socket() argument 1 must be a socket");
    };
    if context_bool(context, "_server_side", false) {
        return ssl_error("server-side TLS is not supported by this runtime");
    }
    let verify_mode = context_verify_mode(context);
    let check_hostname = context_check_hostname(context);
    let hostname = if server_hostname != NONE {
        match value::as_str(server_hostname) {
            Some(host) if !host.is_empty() => Some(host.to_string()),
            Some(_) => None,
            None => return type_error("server_hostname must be a string or None"),
        }
    } else {
        None
    };
    let hostname = match hostname {
        Some(host) => strip_brackets(&host).to_string(),
        None if check_hostname => return value_error("check_hostname requires server_hostname"),
        // Without SNI CPython verifies against (and sends no name for) the
        // connected peer, so an explicit name is only needed for SNI.
        None if verify_mode != CERT_NONE => {
            peer_host(state).unwrap_or_else(|| "localhost".to_string())
        }
        None => "localhost".to_string(),
    };
    let config = match build_client_config(context) {
        Ok(config) => config,
        Err(err) => return err,
    };
    let server_name = match ServerName::try_from(hostname.clone()) {
        Ok(name) => name,
        Err(_) => return ssl_error("invalid server hostname"),
    };
    let (tcp, timeout) = {
        let mut inner = state.lock();
        if inner.closed {
            return descriptor_error();
        }
        let timeout = inner.timeout;
        match std::mem::replace(&mut inner.kind, SocketKind::Busy) {
            SocketKind::Stream(stream) => (stream, timeout),
            other => {
                inner.kind = other;
                return ssl_error("socket is not a connected stream");
            }
        }
    };
    if let Err(err) = prepare_both(&tcp, timeout) {
        let mut inner = state.lock();
        if !inner.closed && matches!(&inner.kind, SocketKind::Busy) {
            inner.kind = SocketKind::Stream(tcp);
        }
        return err;
    }
    let connection = match ClientConnection::new(config, server_name) {
        Ok(connection) => connection,
        Err(error) => {
            let mut inner = state.lock();
            if !inner.closed && matches!(&inner.kind, SocketKind::Busy) {
                inner.kind = SocketKind::Stream(tcp);
            }
            return ssl_error(&error.to_string());
        }
    };
    let mut tls = Box::new(TlsStream {
        conn: connection,
        tcp,
    });
    let handshake = parked(timeout, || tls.handshake());
    match handshake {
        Ok(()) => {
            let mut inner = state.lock();
            if inner.closed {
                return descriptor_error();
            }
            inner.kind = SocketKind::Tls(Some(tls));
            sock
        }
        Err(error) => {
            let TlsStream { tcp, .. } = *tls;
            let mut inner = state.lock();
            if !inner.closed && matches!(&inner.kind, SocketKind::Busy) {
                inner.kind = SocketKind::Stream(tcp);
            }
            map_net_error(error, timeout)
        }
    }
}

fn ssl_wrap_socket(args: &[i64]) -> i64 {
    let Some(&sock) = args.first() else {
        return type_error("wrap_socket() missing required argument 'sock'");
    };
    let hostname = args.get(1).copied().unwrap_or(NONE);
    // The legacy `ssl.wrap_socket` disables certificate verification.
    let context = ssl_context(&[encode_int(PROTOCOL_TLS_CLIENT)]);
    if exc::pending() {
        return context;
    }
    set_context_attr(context, "_verify_mode", encode_int(CERT_NONE));
    set_context_attr(context, "_check_hostname", value::FALSE);
    wrap_tls(context, sock, hostname)
}

/// Module-level `ssl` dispatch.
pub(crate) fn ssl_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "SSLContext" => ssl_context(args),
        "create_default_context" | "_create_default_https_context" => create_default_context(args),
        "_create_stdlib_context" | "_create_unverified_context" => create_stdlib_context(args),
        "wrap_socket" => ssl_wrap_socket(args),
        "_ssl_context_wrap_socket" => {
            let hostname = args.get(2).copied().unwrap_or(NONE);
            wrap_tls(args[0], args[1], hostname)
        }
        "_ssl_context_load_verify_locations" => context_load_verify_locations(args),
        "_ssl_context_load_cert_chain" => context_load_cert_chain(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'ssl' has no attribute '{function}'"),
        ),
    }
}

/// Keyword binding for `ssl` module calls.
pub(crate) fn ssl_call_kw(function: &str, args: &[i64], keywords: Vec<(String, i64)>) -> i64 {
    let mut bound = args.to_vec();
    for (name, item) in keywords {
        let index = match (function, name.as_str()) {
            ("SSLContext", "protocol") => 0,
            ("create_default_context" | "_create_default_https_context", "purpose") => 0,
            ("create_default_context" | "_create_default_https_context", "cafile") => 1,
            ("create_default_context" | "_create_default_https_context", "capath") => 2,
            ("create_default_context" | "_create_default_https_context", "cadata") => 3,
            ("_create_stdlib_context" | "_create_unverified_context", "protocol") => 0,
            ("_create_stdlib_context" | "_create_unverified_context", "cert_reqs") => 1,
            ("_create_stdlib_context" | "_create_unverified_context", "check_hostname") => 2,
            ("_create_stdlib_context" | "_create_unverified_context", "purpose") => 3,
            ("_create_stdlib_context" | "_create_unverified_context", "certfile") => 4,
            ("_create_stdlib_context" | "_create_unverified_context", "keyfile") => 5,
            ("_create_stdlib_context" | "_create_unverified_context", "cafile") => 6,
            ("_create_stdlib_context" | "_create_unverified_context", "capath") => 7,
            ("_create_stdlib_context" | "_create_unverified_context", "cadata") => 8,
            ("wrap_socket", "sock") => 0,
            ("wrap_socket", "server_hostname") => 1,
            _ => {
                return type_error(&format!(
                    "ssl.{function}() got an unexpected keyword argument '{name}'"
                ))
            }
        };
        while bound.len() <= index {
            bound.push(NONE);
        }
        if bound[index] != NONE {
            return type_error(&format!(
                "ssl.{function}() got multiple values for argument '{name}'"
            ));
        }
        bound[index] = item;
    }
    ssl_call(function, &bound)
}

/// Constants of the native `ssl` module.
pub(crate) fn ssl_constant(name: &str) -> i64 {
    match name {
        "CERT_NONE" => encode_int(CERT_NONE),
        "CERT_OPTIONAL" => encode_int(1),
        "CERT_REQUIRED" => encode_int(CERT_REQUIRED),
        "PROTOCOL_TLS" => encode_int(PROTOCOL_TLS),
        "PROTOCOL_TLS_CLIENT" => encode_int(PROTOCOL_TLS_CLIENT),
        "PROTOCOL_TLS_SERVER" => encode_int(PROTOCOL_TLS_SERVER),
        "PROTOCOL_TLSv1_2" => encode_int(5),
        "OP_ALL" => encode_int(OP_ALL),
        "HAS_SNI" => value::TRUE,
        "HAS_TLSv1_3" => value::TRUE,
        "Purpose" => purpose_class(),
        "SSLSocket" => ssl_socket_class(),
        // The class value behind `SSLContext(...)`; module attribute lookup
        // currently prefers the spec function, so `mod.rs` needs a
        // class-value hook to expose this (see the hand-off notes).
        "SSLContext" => ssl_context_class(),
        // No dedicated `SSLError` tag exists; the closest catchable class is
        // `OSError`, which is what every TLS failure in this runtime raises.
        "SSLError" => exc::exception_class(OS_ERROR),
        "OPENSSL_VERSION" => value::str_alloc("rustls 0.23".to_string()),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'ssl' has no attribute '{name}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_default_timeout_is_a_cached_object_sentinel() {
        exc::init();
        let first = constant("socket", "_GLOBAL_DEFAULT_TIMEOUT");
        let second = constant("_socket", "_GLOBAL_DEFAULT_TIMEOUT");
        assert!(!exc::pending());
        assert_eq!(first, second, "sentinel identity must be stable");
        assert!(value::as_instance(first).is_some());
        assert!(!exc::pending());
    }

    #[test]
    fn global_default_timeout_is_not_a_duration() {
        exc::init();
        let sentinel = global_default_timeout();
        assert_eq!(timeout_argument(sentinel), Ok(None));
        assert!(!exc::pending());
        // `create_connection` accepts the sentinel where `timeout` belongs.
        assert_eq!(timeout_argument(NONE), Ok(None));
        assert!(!exc::pending());
    }

    #[test]
    fn create_server_binds_listens_and_records_reuse_options() {
        exc::init();
        let address = value::tuple_new(vec![
            value::str_alloc("127.0.0.1".to_string()),
            encode_int(0),
        ]);
        let keywords = vec![
            ("backlog".to_string(), encode_int(8)),
            ("reuse_port".to_string(), value::TRUE),
        ];
        let server = call_kw("socket", "create_server", &[address], keywords);
        assert!(!exc::pending());
        assert!(value::as_socket(server).is_some());

        let name = method(server, "getsockname", &[]);
        assert!(!exc::pending());
        let parts = value::as_tuple(name).expect("getsockname tuple");
        let port = value::as_integer(parts[1]).expect("bound port");
        assert!(port > 0, "ephemeral port must be assigned");
        assert_eq!(value::as_str(parts[0]), Some("127.0.0.1"));

        // `SO_REUSEADDR` is always set before bind; `SO_REUSEPORT` because the
        // keyword requested it. getsockopt reports the recorded values.
        let reuseaddr = method(server, "getsockopt", &[encode_int(1), encode_int(2)]);
        assert_eq!(value::as_integer(reuseaddr), Some(1));
        let reuseport = method(server, "getsockopt", &[encode_int(1), encode_int(15)]);
        assert_eq!(value::as_integer(reuseport), Some(1));

        // The socket is really listening: a client connects and `accept`
        // returns the connected stream.
        let client = TcpStream::connect(("127.0.0.1", port as u16)).expect("client connect");
        let accepted = method(server, "accept", &[]);
        assert!(!exc::pending());
        let pair = value::as_tuple(accepted).expect("accept tuple");
        assert!(value::as_socket(pair[0]).is_some());
        drop(client);
    }

    #[test]
    fn create_server_rejects_positional_options_and_bad_keywords() {
        exc::init();
        let address = value::tuple_new(vec![
            value::str_alloc("127.0.0.1".to_string()),
            encode_int(0),
        ]);
        let _ = call("socket", "create_server", &[address, encode_int(AF_INET)]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(value::to_string(exc::pending_value())
            .contains("create_server() takes 1 positional argument but 2 were given"));
        exc::clear();

        exc::init();
        let dual_stack = vec![("dualstack_ipv6".to_string(), value::TRUE)];
        let _ = call_kw("socket", "create_server", &[address], dual_stack);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert!(value::to_string(exc::pending_value())
            .contains("dualstack_ipv6 requires AF_INET6 family"));
        exc::clear();

        exc::init();
        let unknown = vec![("bogus".to_string(), encode_int(1))];
        let _ = call_kw("socket", "create_server", &[address], unknown);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(
            value::to_string(exc::pending_value()).contains("unexpected keyword argument 'bogus'")
        );
        exc::clear();

        exc::init();
        let _ = call("socket", "create_server", &[]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(value::to_string(exc::pending_value())
            .contains("missing 1 required positional argument: 'address'"));
        exc::clear();
    }

    fn fqdn_text(args: &[i64]) -> String {
        let result = call("socket", "getfqdn", args);
        assert!(!exc::pending(), "getfqdn failed unexpectedly");
        value::as_str(result)
            .expect("getfqdn returns str")
            .to_string()
    }

    #[test]
    fn getfqdn_strips_the_name_and_defaults_to_the_local_host() {
        exc::init();
        let empty = fqdn_text(&[]);
        assert!(!empty.is_empty(), "getfqdn() must name the local host");
        assert!(!empty.chars().any(char::is_whitespace), "{empty}");

        // Surrounding whitespace is stripped, and `0.0.0.0` means "this
        // host" just like an empty argument.
        let blank = fqdn_text(&[value::str_alloc("   ".to_string())]);
        assert_eq!(blank, empty);
        let wildcard = fqdn_text(&[value::str_alloc(" 0.0.0.0 ".to_string())]);
        assert_eq!(wildcard, empty);
        // Passing the host name itself goes through the same reverse lookup.
        let hostname = fqdn_text(&[value::str_alloc(gethostname())]);
        assert_eq!(hostname, empty);
    }

    #[test]
    fn getfqdn_reverses_loopback_and_localhost() {
        exc::init();
        let localhost = fqdn_text(&[value::str_alloc("localhost".to_string())]);
        assert!(localhost.contains("localhost"), "{localhost}");

        // The v4 and v6 loopback entries resolve to the machine's own
        // localhost name; a host without a PTR record keeps the literal.
        let v4 = fqdn_text(&[value::str_alloc("127.0.0.1".to_string())]);
        assert!(!v4.is_empty());
        assert!(v4.contains("localhost") || v4 == "127.0.0.1", "{v4}");
        let v6 = fqdn_text(&[value::str_alloc("::1".to_string())]);
        assert!(v6.contains("localhost") || v6 == "::1", "{v6}");

        // `name` is a positional-or-keyword parameter upstream, and the
        // keyword form binds to the same slot.
        let keywords = vec![(
            "name".to_string(),
            value::str_alloc("localhost".to_string()),
        )];
        let keyword_call = call_kw("socket", "getfqdn", &[], keywords);
        assert_eq!(value::as_str(keyword_call), Some(localhost.as_str()));

        // CPython's `getfqdn` starts with `name.strip()`, so a non-str
        // argument raises `AttributeError`, not `TypeError`.
        exc::init();
        let _ = call("socket", "getfqdn", &[encode_int(1)]);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert!(value::to_string(exc::pending_value())
            .contains("'int' object has no attribute 'strip'"));
        exc::clear();
    }

    #[test]
    fn tcp_nodelay_constant_reaches_the_kernel() {
        exc::init();
        let nodelay = constant("socket", "TCP_NODELAY");
        assert!(!exc::pending());
        #[cfg(unix)]
        assert_eq!(
            value::as_integer(nodelay),
            Some(i64::from(libc::TCP_NODELAY))
        );
        // `_socket` is the same native module under its accelerator name.
        assert_eq!(
            value::as_integer(constant("_socket", "TCP_NODELAY")),
            value::as_integer(nodelay)
        );
        // Every `TCP_*` option is a tagged Python int, never a raw C number.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        for name in [
            "TCP_MAXSEG",
            "TCP_KEEPIDLE",
            "TCP_USER_TIMEOUT",
            "TCP_FASTOPEN",
            "TCP_INQ",
        ] {
            let option = constant("socket", name);
            assert!(
                value::as_integer(option).is_some(),
                "socket.{name} is not a tagged int"
            );
        }

        // A bound stream socket owns a descriptor, so the option reaches the
        // kernel: a wrong constant number fails with ENOPROTOOPT instead of
        // being recorded silently.
        let sock = call(
            "socket",
            "socket",
            &[encode_int(AF_INET), encode_int(SOCK_STREAM)],
        );
        assert!(!exc::pending());
        let address = value::tuple_new(vec![
            value::str_alloc("127.0.0.1".to_string()),
            encode_int(0),
        ]);
        let _ = method(sock, "bind", &[address]);
        assert!(
            !exc::pending(),
            "bind failed: {}",
            value::to_string(exc::pending_value())
        );
        let level = encode_int(IPPROTO_TCP);
        assert_eq!(
            method(sock, "setsockopt", &[level, nodelay, encode_int(1)]),
            NONE
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(method(sock, "getsockopt", &[level, nodelay])),
            Some(1)
        );
    }

    // --- ssl.SSLContext / stdlib context factories -----------------------

    /// A certificate/key pair used by the file-backed `ssl` tests
    /// (`CN=localhost`, self-signed, test data only).
    const TEST_CA_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIBkTCCATegAwIBAgIUPqDSi0O65gIvseWR4wdYILNxClIwCgYIKoZIzj0EAwIw
FjEUMBIGA1UEAwwLUHlYIFRlc3QgQ0EwHhcNMjYxMDI1MjEwODQzWhcNMzYxMDIy
MjEwODQzWjAWMRQwEgYDVQQDDAtQeVggVGVzdCBDQTBZMBMGByqGSM49AgEGCCqG
SM49AwEHA0IABKdTR5KrBiJF8cUlL/EtVQmFRdm+SfG8tGiCb6zLzpj86JZhYUOI
IH/3xL6jy0cMM9X98bDeQxezSGcLDIe0jZGjYzBhMB0GA1UdDgQWBBS8GSQ6Jzww
MhXkMB3Bf1H82R1IpzAfBgNVHSMEGDAWgBS8GSQ6JzwwMhXkMB3Bf1H82R1IpzAP
BgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIBBjAKBggqhkjOPQQDAgNIADBF
AiAiNg1QVfYNPIMHV5DAhydW04m11vc/HQnlOi/UniwH/AIhAIFyOF0aRbQOiNRl
qu1DRtUYcaAjRxO7epvoE/5g3rZf
-----END CERTIFICATE-----
";

    const TEST_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIBvjCCAWWgAwIBAgIUP9Qi57ffGs1l9SGvBbAg2yBHrLgwCgYIKoZIzj0EAwIw
FjEUMBIGA1UEAwwLUHlYIFRlc3QgQ0EwHhcNMjYxMDI1MjEwODQzWhcNMzYxMDIy
MjEwODQzWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO
PQMBBwNCAAQd3u7zRhk7+BASAbxK7WwLExCm5Dp+71j8tiiLp+I9mY09AZQ9a0zN
J1s0W6b/JtOS+WMzlYxDm5x0bvGxQqbSo4GSMIGPMBoGA1UdEQQTMBGCCWxvY2Fs
aG9zdIcEfwAAATAMBgNVHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUE
DDAKBggrBgEFBQcDATAdBgNVHQ4EFgQUt4vz30/o5JQFkxcdLBzsuuZNl1YwHwYD
VR0jBBgwFoAUvBkkOic8MDIV5DAdwX9R/NkdSKcwCgYIKoZIzj0EAwIDRwAwRAIg
MMtdgSH5E4RTe+6ZN1LnDk14EXGy1HG3NEA/Q34mg5QCIBg6iDOH9QWMzwHnujWB
3KtoWZ/dHdSIL9Nv+HV1m32P
-----END CERTIFICATE-----
";

    const TEST_KEY_PEM: &str = "\
-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIErV9AbQP8JYpvIVWvfAlYMHOI2fQi5HC+82OxiZ5QeEoAoGCCqGSM49
AwEHoUQDQgAEHd7u80YZO/gQEgG8Su1sCxMQpuQ6fu9Y/LYoi6fiPZmNPQGUPWtM
zSdbNFum/ybTkvljM5WMQ5ucdG7xsUKm0g==
-----END EC PRIVATE KEY-----
";

    fn ssl_temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("hydro-ssl-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temporary directory");
        dir
    }

    fn write_fixture(dir: &std::path::Path, name: &str, contents: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("fixture file");
        path.to_string_lossy().into_owned()
    }

    fn ssl_text(text: &str) -> i64 {
        value::str_alloc(text.to_string())
    }

    fn new_context(protocol: i64) -> i64 {
        let context = ssl_call("SSLContext", &[encode_int(protocol)]);
        assert!(
            !exc::pending(),
            "SSLContext({protocol}) raised: {}",
            value::to_string(exc::pending_value())
        );
        context
    }

    fn context_get(context: i64, name: &str) -> i64 {
        let result = crate::methods::getattr(context, name.as_ptr(), name.len());
        assert!(
            !exc::pending(),
            "reading {name} raised: {}",
            value::to_string(exc::pending_value())
        );
        result
    }

    fn context_set(context: i64, name: &str, new_value: i64) -> i64 {
        crate::methods::setattr(context, name.as_ptr(), name.len(), new_value)
    }

    fn context_method(context: i64, name: &str, args: &[i64]) -> i64 {
        crate::methods::call(
            context,
            name.as_ptr(),
            name.len(),
            args.len(),
            args.as_ptr(),
        )
    }

    fn context_error_message() -> String {
        value::to_string(exc::pending_value())
    }

    #[test]
    fn ssl_context_defaults_follow_the_protocol() {
        exc::init();
        let client = new_context(PROTOCOL_TLS_CLIENT);
        assert_eq!(
            value::as_integer(context_get(client, "verify_mode")),
            Some(CERT_REQUIRED)
        );
        assert_eq!(context_get(client, "check_hostname"), value::TRUE);
        assert_eq!(
            value::as_integer(context_get(client, "protocol")),
            Some(PROTOCOL_TLS_CLIENT)
        );
        assert_eq!(context_get(client, "post_handshake_auth"), value::FALSE);
        assert_eq!(
            value::as_integer(context_get(client, "options")),
            Some(OP_ALL)
        );
        assert_eq!(
            value::instance_class_value(client).and_then(value::class_display_name),
            Some("SSLContext")
        );

        // `SSLContext()` without a protocol is CPython's generic
        // `PROTOCOL_TLS`, which verifies nothing.
        let generic = ssl_call("SSLContext", &[]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(context_get(generic, "protocol")),
            Some(PROTOCOL_TLS)
        );
        assert_eq!(
            value::as_integer(context_get(generic, "verify_mode")),
            Some(CERT_NONE)
        );
        assert_eq!(context_get(generic, "check_hostname"), value::FALSE);

        let server = new_context(PROTOCOL_TLS_SERVER);
        assert_eq!(
            value::as_integer(context_get(server, "verify_mode")),
            Some(CERT_NONE)
        );
        assert_eq!(context_get(server, "check_hostname"), value::FALSE);

        exc::init();
        let _ = ssl_call("SSLContext", &[encode_int(99)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert!(context_error_message().contains("Unsupported protocol version"));
        exc::clear();
    }

    #[test]
    fn check_hostname_and_verify_mode_stay_consistent() {
        exc::init();
        let context = new_context(PROTOCOL_TLS_CLIENT);

        // CERT_NONE is rejected while host-name checking is enabled.
        assert_eq!(
            context_set(context, "verify_mode", encode_int(CERT_NONE)),
            NONE
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert!(context_error_message()
            .contains("Cannot set verify_mode to CERT_NONE when check_hostname is enabled."));
        exc::clear();

        // Out-of-range and non-integer modes are rejected.
        assert_eq!(context_set(context, "verify_mode", encode_int(7)), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert!(context_error_message().contains("invalid verify_mode"));
        exc::clear();

        assert_eq!(
            context_set(context, "verify_mode", ssl_text("unverified")),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(context_error_message().contains("verify_mode must be an integer"));
        exc::clear();

        // Disabling host-name checking frees CERT_NONE, and re-enabling it
        // afterwards is then rejected.
        assert_eq!(context_set(context, "check_hostname", value::FALSE), NONE);
        assert!(!exc::pending());
        assert_eq!(
            context_set(context, "verify_mode", encode_int(CERT_NONE)),
            NONE
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(context_get(context, "verify_mode")),
            Some(CERT_NONE)
        );
        assert_eq!(context_set(context, "check_hostname", value::TRUE), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert!(context_error_message()
            .contains("Cannot set check_hostname to True when verify_mode is CERT_NONE"));
        exc::clear();

        // CERT_OPTIONAL keeps host-name checking available.
        let optional = new_context(PROTOCOL_TLS_CLIENT);
        assert_eq!(
            context_set(optional, "verify_mode", encode_int(CERT_OPTIONAL)),
            NONE
        );
        assert!(!exc::pending());
        assert_eq!(context_get(optional, "check_hostname"), value::TRUE);
        assert_eq!(
            value::as_integer(context_get(optional, "verify_mode")),
            Some(CERT_OPTIONAL)
        );
    }

    #[test]
    fn load_verify_locations_reads_real_ca_files() {
        exc::init();
        let dir = ssl_temp_dir("ca");
        let ca_file = write_fixture(&dir, "ca.pem", TEST_CA_PEM);
        let context = new_context(PROTOCOL_TLS_CLIENT);
        assert_eq!(
            ssl_call(
                "_ssl_context_load_verify_locations",
                &[context, ssl_text(&ca_file), NONE, NONE],
            ),
            NONE
        );
        assert!(!exc::pending());
        assert_eq!(context_roots(context).len(), 1);

        // `cadata` loads the same PEM text from memory.
        let keyword_context = new_context(PROTOCOL_TLS_CLIENT);
        let loaded = context_method(
            keyword_context,
            "load_verify_locations",
            &[NONE, NONE, ssl_text(TEST_CA_PEM)],
        );
        assert_eq!(loaded, NONE);
        assert!(!exc::pending());
        assert_eq!(context_roots(keyword_context).len(), 1);

        // All three sources omitted is CPython's TypeError.
        exc::init();
        let _ = ssl_call(
            "_ssl_context_load_verify_locations",
            &[context, NONE, NONE, NONE],
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(context_error_message().contains("cannot be all omitted"));
        exc::clear();

        // A missing file keeps the OS errno text, and a file without a
        // certificate is an SSL-tagged OSError.
        let missing = dir.join("missing.pem");
        let _ = ssl_call(
            "_ssl_context_load_verify_locations",
            &[context, ssl_text(&missing.to_string_lossy()), NONE, NONE],
        );
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        assert!(context_error_message().contains("No such file"));
        exc::clear();

        let garbage = write_fixture(&dir, "garbage.pem", "not a certificate\n");
        let _ = ssl_call(
            "_ssl_context_load_verify_locations",
            &[context, ssl_text(&garbage), NONE, NONE],
        );
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        assert!(context_error_message().contains("no certificates found"));
        exc::clear();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_cert_chain_reads_real_certificate_and_key_files() {
        exc::init();
        let dir = ssl_temp_dir("chain");
        let cert_file = write_fixture(&dir, "cert.pem", TEST_CERT_PEM);
        let key_file = write_fixture(&dir, "key.pem", TEST_KEY_PEM);
        let context = new_context(PROTOCOL_TLS_CLIENT);
        assert_eq!(
            ssl_call(
                "_ssl_context_load_cert_chain",
                &[context, ssl_text(&cert_file), ssl_text(&key_file), NONE],
            ),
            NONE
        );
        assert!(!exc::pending());
        assert_eq!(context_cert_chain(context).len(), 1);
        let key = context_attr(context, "_key").expect("private key stored");
        assert!(value::as_bytes(key).is_some_and(|bytes| !bytes.is_empty()));

        // The second argument is an unsupported encrypted-key password.
        let password_context = new_context(PROTOCOL_TLS_CLIENT);
        let _ = ssl_call(
            "_ssl_context_load_cert_chain",
            &[
                password_context,
                ssl_text(&cert_file),
                ssl_text(&key_file),
                ssl_text("secret"),
            ],
        );
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        assert!(context_error_message().contains("encrypted private keys"));
        exc::clear();

        // A missing certificate file and an unparsable key both fail.
        let missing = dir.join("missing.pem");
        let _ = ssl_call(
            "_ssl_context_load_cert_chain",
            &[context, ssl_text(&missing.to_string_lossy()), NONE, NONE],
        );
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        exc::clear();

        let bad_key = write_fixture(&dir, "bad-key.pem", "not a private key\n");
        let _ = ssl_call(
            "_ssl_context_load_cert_chain",
            &[context, ssl_text(&cert_file), ssl_text(&bad_key), NONE],
        );
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        assert!(context_error_message().contains("could not load the private key"));
        exc::clear();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_alpn_protocols_validates_and_stores_names() {
        exc::init();
        let context = new_context(PROTOCOL_TLS_CLIENT);
        let protocols =
            value::list_new(vec![ssl_text("h2"), value::bytes_new(b"http/1.1".to_vec())]);
        assert_eq!(
            context_method(context, "set_alpn_protocols", &[protocols]),
            NONE
        );
        assert!(!exc::pending());
        let stored = context_alpn(context);
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0], b"h2".to_vec());
        assert_eq!(stored[1], b"http/1.1".to_vec());

        // An empty and an over-long name are rejected with `SSLError`
        // (`OSError` in this runtime).
        for bad in [b"".to_vec(), vec![b'a'; 256]] {
            let names = value::list_new(vec![value::bytes_new(bad)]);
            assert_eq!(
                context_method(context, "set_alpn_protocols", &[names]),
                NONE
            );
            assert_eq!(exc::pending_tag(), OS_ERROR as i64);
            assert!(context_error_message().contains("ALPN protocols must be 1 to 255"));
            exc::clear();
        }

        // A non-bytes item and a non-sequence argument are `TypeError`.
        let mixed = value::list_new(vec![encode_int(1)]);
        assert_eq!(
            context_method(context, "set_alpn_protocols", &[mixed]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(context_error_message().contains("protocol name must be str or bytes"));
        exc::clear();

        assert_eq!(
            context_method(context, "set_alpn_protocols", &[encode_int(1)]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(context_error_message().contains("must be a sequence"));
        exc::clear();

        // An empty sequence clears the list again.
        let empty = value::list_new(Vec::new());
        assert_eq!(
            context_method(context, "set_alpn_protocols", &[empty]),
            NONE
        );
        assert!(!exc::pending());
        assert!(context_alpn(context).is_empty());
    }

    #[test]
    fn post_handshake_auth_and_options_are_writable_flags() {
        exc::init();
        let context = new_context(PROTOCOL_TLS_CLIENT);
        assert_eq!(context_get(context, "post_handshake_auth"), value::FALSE);
        assert_eq!(
            context_set(context, "post_handshake_auth", value::TRUE),
            NONE
        );
        assert!(!exc::pending());
        assert_eq!(context_get(context, "post_handshake_auth"), value::TRUE);
        // Truthiness, not identity, decides the stored flag.
        assert_eq!(
            context_set(context, "post_handshake_auth", encode_int(0)),
            NONE
        );
        assert_eq!(context_get(context, "post_handshake_auth"), value::FALSE);
        // rustls has no post-handshake authentication for clients; the flag
        // is stored (CPython reads it in `http.client`) but never used.

        assert_eq!(context_set(context, "options", encode_int(3)), NONE);
        assert!(!exc::pending());
        assert_eq!(value::as_integer(context_get(context, "options")), Some(3));
        assert_eq!(context_set(context, "options", ssl_text("opts")), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn stdlib_context_matches_cpython_defaults() {
        exc::init();
        // `ssl._create_stdlib_context()` is the unverified context the stdlib
        // mail/FTP clients ask for by default.
        let unverified = ssl_call_kw("_create_stdlib_context", &[], Vec::new());
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(context_get(unverified, "protocol")),
            Some(PROTOCOL_TLS_CLIENT)
        );
        assert_eq!(
            value::as_integer(context_get(unverified, "verify_mode")),
            Some(CERT_NONE)
        );
        assert_eq!(context_get(unverified, "check_hostname"), value::FALSE);
        // No certificate store is touched without verification.
        assert!(!context_bool(unverified, "_default_roots", false));

        // `_create_unverified_context` is the same factory under its real
        // CPython name.
        let alias = ssl_call("_create_unverified_context", &[]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(context_get(alias, "verify_mode")),
            Some(CERT_NONE)
        );

        // Explicit verification loads the Mozilla roots.
        let verified = ssl_call_kw(
            "_create_stdlib_context",
            &[],
            vec![
                ("cert_reqs".to_string(), encode_int(CERT_REQUIRED)),
                ("check_hostname".to_string(), value::TRUE),
            ],
        );
        assert!(
            !exc::pending(),
            "verified context raised: {}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(
            value::as_integer(context_get(verified, "verify_mode")),
            Some(CERT_REQUIRED)
        );
        assert_eq!(context_get(verified, "check_hostname"), value::TRUE);
        assert!(context_bool(verified, "_default_roots", false));

        // The CLIENT_AUTH purpose selects the server protocol, which this
        // runtime reports as unsupported when wrapping.
        let purposes = ssl_constant("Purpose");
        let client_auth =
            crate::methods::getattr(purposes, b"CLIENT_AUTH".as_ptr(), "CLIENT_AUTH".len());
        assert!(!exc::pending());
        let server_context = ssl_call_kw(
            "_create_stdlib_context",
            &[],
            vec![("purpose".to_string(), client_auth)],
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(context_get(server_context, "protocol")),
            Some(PROTOCOL_TLS_SERVER)
        );
        let socket = call(
            "socket",
            "socket",
            &[encode_int(AF_INET), encode_int(SOCK_STREAM)],
        );
        assert!(!exc::pending());
        let wrapped = ssl_call("_ssl_context_wrap_socket", &[server_context, socket, NONE]);
        assert_eq!(wrapped, NONE);
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        assert!(context_error_message().contains("server-side TLS is not supported"));
        exc::clear();
        let _ = method(socket, "close", &[]);

        // A bogus purpose is `TypeError`, and `keyfile` requires `certfile`.
        exc::init();
        let _ = ssl_call_kw(
            "_create_stdlib_context",
            &[],
            vec![("purpose".to_string(), ssl_text("bogus"))],
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        exc::init();
        let _ = ssl_call_kw(
            "_create_stdlib_context",
            &[],
            vec![("keyfile".to_string(), ssl_text("key.pem"))],
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert!(context_error_message().contains("certfile must be specified"));
        exc::clear();

        // Explicit CA files really load.
        let dir = ssl_temp_dir("stdlib");
        let ca_file = write_fixture(&dir, "ca.pem", TEST_CA_PEM);
        let context = ssl_call_kw(
            "_create_stdlib_context",
            &[],
            vec![("cafile".to_string(), ssl_text(&ca_file))],
        );
        assert!(!exc::pending());
        assert_eq!(context_roots(context).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_https_context_is_create_default_context() {
        exc::init();
        let context = ssl_call("_create_default_https_context", &[]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(context_get(context, "protocol")),
            Some(PROTOCOL_TLS_CLIENT)
        );
        assert_eq!(
            value::as_integer(context_get(context, "verify_mode")),
            Some(CERT_REQUIRED)
        );
        assert_eq!(context_get(context, "check_hostname"), value::TRUE);
        assert!(context_bool(context, "_default_roots", false));

        let keyword = ssl_call_kw("_create_default_https_context", &[], Vec::new());
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(context_get(keyword, "verify_mode")),
            Some(CERT_REQUIRED)
        );

        // `create_default_context` validates its purpose selector too.
        let purposes = ssl_constant("Purpose");
        let client_auth =
            crate::methods::getattr(purposes, b"CLIENT_AUTH".as_ptr(), "CLIENT_AUTH".len());
        let server = ssl_call("create_default_context", &[client_auth]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(context_get(server, "protocol")),
            Some(PROTOCOL_TLS_SERVER)
        );
        assert_eq!(
            value::as_integer(context_get(server, "verify_mode")),
            Some(CERT_NONE)
        );
    }

    #[test]
    fn ssl_exports_ssl_socket_and_purpose_values() {
        exc::init();
        let ssl_socket = ssl_constant("SSLSocket");
        assert!(!exc::pending());
        assert!(value::as_class(ssl_socket).is_some());
        assert!(value::is_subclass_of(ssl_socket, value::socket_class()));

        let purposes = ssl_constant("Purpose");
        assert!(!exc::pending());
        assert!(value::as_class(purposes).is_some());
        for name in ["SERVER_AUTH", "CLIENT_AUTH"] {
            let member = crate::methods::getattr(purposes, name.as_ptr(), name.len());
            assert!(!exc::pending(), "Purpose.{name} must resolve");
            assert_eq!(
                value::instance_class_value(member).and_then(value::class_display_name),
                Some("Purpose")
            );
            let oid = crate::methods::getattr(member, b"oid".as_ptr(), 3);
            assert!(!exc::pending());
            assert!(value::as_str(oid).is_some_and(|oid| oid.starts_with("1.3.6.1.5.5.7.3.")));
        }
        let server_auth =
            crate::methods::getattr(purposes, b"SERVER_AUTH".as_ptr(), "SERVER_AUTH".len());
        let client_auth =
            crate::methods::getattr(purposes, b"CLIENT_AUTH".as_ptr(), "CLIENT_AUTH".len());
        assert_ne!(server_auth, client_auth);
    }

    #[test]
    fn wrap_socket_reports_unsupported_and_missing_hostname() {
        exc::init();
        let context = new_context(PROTOCOL_TLS_CLIENT);
        let socket = call(
            "socket",
            "socket",
            &[encode_int(AF_INET), encode_int(SOCK_STREAM)],
        );
        assert!(!exc::pending());

        // `check_hostname` is on for a client context, so a wrap without a
        // server name is CPython's `ValueError`.
        let _ = ssl_call("_ssl_context_wrap_socket", &[context, socket, NONE]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert!(context_error_message().contains("check_hostname requires server_hostname"));
        exc::clear();

        // A non-socket argument is rejected before any TLS work.
        let _ = ssl_call(
            "_ssl_context_wrap_socket",
            &[context, encode_int(1), ssl_text("localhost")],
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(context_error_message().contains("must be a socket"));
        exc::clear();

        let _ = method(socket, "close", &[]);
    }
}
