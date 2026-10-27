//! Integration tests for the TCP/UDP `socket` module (blocking and
//! non-blocking paths, IPv6, socketpair and address helpers).
//!
//! The heap and thread registry are process-global, so every test takes
//! `TEST_LOCK` and starts from `hydro_runtime_init`. The server thread is a
//! `_thread` worker (the shared heap makes the objects visible to both
//! threads); client and server operations carry timeouts so a regression
//! fails instead of hanging.
//!
//! Values are tagged `i64`s; helpers below encode integers with `<< 3` and
//! decode with `>> 3`, exactly like the generated code.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hydro_runtime::abi::*;

/// Serialises the tests in this binary: the shared heap is process-global.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn int(n: i64) -> i64 {
    n << 3
}

fn decode(value: i64) -> i64 {
    hydro_to_int(value) >> 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn bytes(value: &[u8]) -> i64 {
    hydro_bytes_new(value.as_ptr(), value.len())
}

fn list(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
}

fn tuple(items: &[i64]) -> i64 {
    hydro_tuple_new(items.len(), items.as_ptr())
}

fn module(module: &str, function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn module_get(module: &str, name: &str) -> i64 {
    hydro_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    hydro_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len(), 0)
}

fn start(func: i64, args: i64) -> i64 {
    module("_thread", "start_new_thread", &[func, args])
}

fn join(handle: i64) -> i64 {
    module("_thread", "join", &[handle])
}

fn eq(a: i64, b: i64) -> bool {
    hydro_binop(7, a, b) == hydro_true()
}

const OS_ERROR: i64 = 10;

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn error_message() -> i64 {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    hydro_to_str(hydro_exc_value())
}

fn clear_error() {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    hydro_exc_clear();
}

fn assert_bytes(actual: i64, expected: &[u8]) {
    assert_no_error();
    assert!(
        eq(actual, bytes(expected)),
        "bytes mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn bind_loopback(socket: i64) -> i64 {
    let address = tuple(&[text("127.0.0.1"), int(0)]);
    assert_eq!(method(socket, "bind", &[address]), hydro_none());
    assert_no_error();
    let name = method(socket, "getsockname", &[]);
    assert_no_error();
    decode(hydro_subscript(name, int(1)))
}

fn new_socket() -> i64 {
    let socket = module("socket", "socket", &[]);
    assert_no_error();
    socket
}

fn settimeout(socket: i64, seconds: f64) {
    assert_eq!(
        method(socket, "settimeout", &[hydro_float_new(seconds)]),
        hydro_none()
    );
    assert_no_error();
}

fn address(port: i64) -> i64 {
    tuple(&[text("127.0.0.1"), int(port)])
}

// -------------------------------------------------------------------------
// Worker (generated code has the same `extern "C"` shape)
// -------------------------------------------------------------------------

/// `client(host, port, payload, results)`: connects, sends `payload`, stores
/// the echo in `results[0]` and closes.
extern "C" fn client_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly four arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 4) };
    let (host, port, payload, results) = (args[0], args[1], args[2], args[3]);
    let socket = module("socket", "socket", &[]);
    let peer = tuple(&[host, port]);
    method(socket, "settimeout", &[hydro_float_new(5.0)]);
    method(socket, "connect", &[peer]);
    method(socket, "sendall", &[payload]);
    let echoed = method(socket, "recv", &[int(4)]);
    hydro_list_append(results, echoed);
    method(socket, "close", &[]);
    hydro_none()
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[test]
fn loopback_echo_over_threads() {
    let _guard = guard();
    hydro_runtime_init();

    let server = new_socket();
    let port = bind_loopback(server);
    assert!(port > 0);
    assert_eq!(method(server, "listen", &[int(1)]), hydro_none());
    assert_no_error();
    settimeout(server, 5.0);

    let results = list(&[]);
    let client = start(
        function(client_worker, 4, "client"),
        list(&[text("127.0.0.1"), int(port), bytes(b"ping"), results]),
    );
    assert_no_error();

    let accepted = method(server, "accept", &[]);
    assert_no_error();
    let connection = hydro_subscript(accepted, int(0));
    let peer = hydro_subscript(accepted, int(1));
    assert!(eq(hydro_subscript(peer, int(0)), text("127.0.0.1")));
    assert!(decode(hydro_subscript(peer, int(1))) > 0);

    settimeout(connection, 5.0);
    let received = method(connection, "recv", &[int(4)]);
    assert_bytes(received, b"ping");
    assert_eq!(
        method(connection, "sendall", &[bytes(b"pong")]),
        hydro_none()
    );
    assert_no_error();
    assert_eq!(method(connection, "close", &[]), hydro_none());

    assert_eq!(join(client), hydro_none());
    assert_no_error();
    assert_bytes(hydro_subscript(results, int(0)), b"pong");

    assert_eq!(method(server, "close", &[]), hydro_none());
    assert_no_error();
}

#[test]
fn create_connection_returns_stream() {
    let _guard = guard();
    hydro_runtime_init();

    let server = new_socket();
    let port = bind_loopback(server);
    assert_eq!(method(server, "listen", &[int(1)]), hydro_none());
    settimeout(server, 5.0);

    let client = module(
        "socket",
        "create_connection",
        &[address(port), hydro_float_new(5.0)],
    );
    assert_no_error();

    let accepted = method(server, "accept", &[]);
    assert_no_error();
    let connection = hydro_subscript(accepted, int(0));
    settimeout(connection, 5.0);

    assert_eq!(method(client, "sendall", &[bytes(b"hello")]), hydro_none());
    assert_bytes(method(connection, "recv", &[int(5)]), b"hello");

    // `gettimeout` reflects the timeout passed to `create_connection`.
    let timeout = method(client, "gettimeout", &[]);
    assert_no_error();
    assert!(eq(timeout, hydro_float_new(5.0)));

    assert_eq!(method(client, "close", &[]), hydro_none());
    assert_eq!(method(connection, "close", &[]), hydro_none());
    assert_eq!(method(server, "close", &[]), hydro_none());
    assert_no_error();
}

#[test]
fn recv_timeout_and_flags_errors() {
    let _guard = guard();
    hydro_runtime_init();

    let server = new_socket();
    let port = bind_loopback(server);
    assert_eq!(method(server, "listen", &[int(1)]), hydro_none());
    settimeout(server, 5.0);

    let client = new_socket();
    settimeout(client, 5.0);
    assert_eq!(method(client, "connect", &[address(port)]), hydro_none());
    assert_no_error();

    let accepted = method(server, "accept", &[]);
    assert_no_error();
    let connection = hydro_subscript(accepted, int(0));

    // No data is sent: the finite read timeout must fire.
    settimeout(client, 0.05);
    let started = Instant::now();
    assert_eq!(method(client, "recv", &[int(8)]), hydro_none());
    assert!(eq(error_message(), text("timed out")));
    clear_error();
    assert!(started.elapsed() < Duration::from_secs(5));

    // Flags must be integers.
    assert_eq!(
        method(connection, "send", &[bytes(b"x"), text("peek")]),
        hydro_none()
    );
    clear_error();

    assert_eq!(method(connection, "close", &[]), hydro_none());
    assert_eq!(method(client, "close", &[]), hydro_none());
    assert_eq!(method(server, "close", &[]), hydro_none());
}

#[test]
fn refused_and_connect_ex_errors() {
    let _guard = guard();
    hydro_runtime_init();

    // Reserve a port, then close the listener so nothing accepts there.
    let probe = new_socket();
    let port = bind_loopback(probe);
    assert_eq!(method(probe, "close", &[]), hydro_none());

    let client = new_socket();
    settimeout(client, 1.0);
    assert_eq!(method(client, "connect", &[address(port)]), hydro_none());
    assert_eq!(hydro_exc_tag(), OS_ERROR);
    clear_error();

    let retry = new_socket();
    settimeout(retry, 1.0);
    let errno = method(retry, "connect_ex", &[address(port)]);
    assert_no_error();
    assert_ne!(decode(errno), 0);

    assert_eq!(method(client, "close", &[]), hydro_none());
    assert_eq!(method(retry, "close", &[]), hydro_none());
}

#[test]
fn close_is_idempotent_and_closed_ops_fail() {
    let _guard = guard();
    hydro_runtime_init();

    let socket = new_socket();
    assert_eq!(method(socket, "close", &[]), hydro_none());
    assert_eq!(method(socket, "close", &[]), hydro_none());
    assert_no_error();

    assert_eq!(method(socket, "bind", &[address(0)]), hydro_none());
    clear_error();
    assert_eq!(method(socket, "recv", &[int(1)]), hydro_none());
    clear_error();
    assert_eq!(method(socket, "fileno", &[]), hydro_none());
    clear_error();
    assert!(eq(hydro_to_str(socket), text("<socket closed>")));

    // Unknown methods and bad arity are rejected without panicking.
    let unbound = new_socket();
    assert_eq!(method(unbound, "nope", &[]), hydro_none());
    clear_error();
    assert_eq!(method(unbound, "settimeout", &[]), hydro_none());
    clear_error();
    assert_eq!(
        method(unbound, "settimeout", &[hydro_float_new(-1.0)]),
        hydro_none()
    );
    clear_error();
    // An unbound socket has no descriptor but still reports the wildcard
    // address, like CPython.
    let name = method(unbound, "getsockname", &[]);
    assert_no_error();
    assert!(eq(hydro_subscript(name, int(0)), text("0.0.0.0")));
    assert_eq!(decode(hydro_subscript(name, int(1))), 0);
    assert_eq!(method(unbound, "close", &[]), hydro_none());
}

#[test]
fn constants_and_sockopts() {
    let _guard = guard();
    hydro_runtime_init();

    assert_eq!(module_get("socket", "AF_INET"), int(2));
    assert_eq!(module_get("socket", "SOCK_STREAM"), int(1));
    assert_eq!(module_get("socket", "SOL_SOCKET"), int(1));
    assert_eq!(module_get("socket", "SO_REUSEADDR"), int(2));
    assert_eq!(module_get("socket", "SHUT_RD"), int(0));
    assert_eq!(module_get("socket", "SHUT_WR"), int(1));
    assert_eq!(module_get("socket", "SHUT_RDWR"), int(2));
    assert_no_error();

    let socket = new_socket();
    assert_eq!(
        method(socket, "setsockopt", &[int(1), int(2), int(1)],),
        hydro_none()
    );
    assert_no_error();
    let value = method(socket, "getsockopt", &[int(1), int(2)]);
    assert_eq!(decode(value), 1);

    // Unsupported family/type are rejected; AF_INET6 and SOCK_DGRAM are not.
    assert_eq!(
        module("socket", "socket", &[int(99), int(1), int(0)]),
        hydro_none()
    );
    clear_error();
    assert_eq!(
        module("socket", "socket", &[int(2), int(3), int(0)]),
        hydro_none()
    );
    clear_error();
    assert_eq!(module_get("socket", "AF_INET6"), int(10));
    assert_eq!(module_get("socket", "SOCK_DGRAM"), int(2));
    assert_eq!(module_get("socket", "IPPROTO_UDP"), int(17));
    assert_eq!(method(socket, "close", &[]), hydro_none());

    let host = module("socket", "gethostname", &[]);
    assert_no_error();
    assert!(decode(hydro_len(hydro_to_str(host))) > 0);
}

#[test]
fn gc_collects_closed_sockets_without_panicking() {
    let _guard = guard();
    hydro_runtime_init();

    for _ in 0..128 {
        let socket = new_socket();
        let port = bind_loopback(socket);
        assert!(port > 0);
        assert_eq!(method(socket, "close", &[]), hydro_none());
    }
    assert_no_error();
    let _ = hydro_gc_collect();
    assert_no_error();
}

#[test]
fn datagram_echo_uses_udp_and_addresses() {
    let _guard = guard();
    hydro_runtime_init();

    let first = module("socket", "socket", &[int(2), int(2), int(0)]);
    assert_no_error();
    let second = module("socket", "socket", &[int(2), int(2), int(0)]);
    assert_no_error();

    let second_address = tuple(&[text("127.0.0.1"), int(0)]);
    assert_eq!(method(second, "bind", &[second_address]), hydro_none());
    assert_no_error();
    let second_name = method(second, "getsockname", &[]);
    assert_no_error();
    let second_port = decode(hydro_subscript(second_name, int(1)));
    assert!(second_port > 0);

    let first_address = tuple(&[text("127.0.0.1"), int(0)]);
    assert_eq!(method(first, "bind", &[first_address]), hydro_none());
    assert_no_error();

    let written = method(
        first,
        "sendto",
        &[
            bytes(b"udp-ping"),
            tuple(&[text("127.0.0.1"), int(second_port)]),
        ],
    );
    assert_no_error();
    assert_eq!(decode(written), 8);

    settimeout(second, 2.0);
    let received = method(second, "recvfrom", &[int(64)]);
    assert_no_error();
    assert_bytes(hydro_subscript(received, int(0)), b"udp-ping");
    let peer = hydro_subscript(received, int(1));
    assert!(eq(hydro_subscript(peer, int(0)), text("127.0.0.1")));
    let first_name = method(first, "getsockname", &[]);
    assert_no_error();
    assert_eq!(
        decode(hydro_subscript(peer, int(1))),
        decode(hydro_subscript(first_name, int(1)))
    );

    assert_eq!(method(first, "close", &[]), hydro_none());
    assert_eq!(method(second, "close", &[]), hydro_none());
    assert_no_error();
}

#[test]
fn nonblocking_recv_reports_blocking_io_error() {
    let _guard = guard();
    hydro_runtime_init();

    let server = new_socket();
    let port = bind_loopback(server);
    assert_eq!(method(server, "listen", &[int(1)]), hydro_none());
    settimeout(server, 5.0);

    let client = new_socket();
    settimeout(client, 5.0);
    assert_eq!(method(client, "connect", &[address(port)]), hydro_none());
    assert_no_error();
    let accepted = method(server, "accept", &[]);
    assert_no_error();
    let connection = hydro_subscript(accepted, int(0));
    settimeout(connection, 5.0);

    assert_eq!(
        method(client, "setblocking", &[hydro_false()]),
        hydro_none()
    );
    assert_eq!(method(client, "getblocking", &[]), hydro_false());
    let started = Instant::now();
    assert_eq!(method(client, "recv", &[int(8)]), hydro_none());
    assert!(eq(
        error_message(),
        text("[Errno 11] Resource temporarily unavailable")
    ));
    clear_error();
    assert!(started.elapsed() < Duration::from_secs(2));

    assert_eq!(method(client, "setblocking", &[hydro_true()]), hydro_none());
    assert_eq!(method(client, "getblocking", &[]), hydro_true());

    assert_eq!(
        method(connection, "sendall", &[bytes(b"data")]),
        hydro_none()
    );
    assert_bytes(method(client, "recv", &[int(4)]), b"data");

    assert_eq!(method(connection, "close", &[]), hydro_none());
    assert_eq!(method(client, "close", &[]), hydro_none());
    assert_eq!(method(server, "close", &[]), hydro_none());
}

#[test]
fn socketpair_returns_connected_streams() {
    let _guard = guard();
    hydro_runtime_init();

    let pair = module("socket", "socketpair", &[]);
    assert_no_error();
    let left = hydro_subscript(pair, int(0));
    let right = hydro_subscript(pair, int(1));
    settimeout(left, 2.0);
    settimeout(right, 2.0);

    assert_eq!(method(left, "sendall", &[bytes(b"pair")]), hydro_none());
    assert_no_error();
    assert_bytes(method(right, "recv", &[int(4)]), b"pair");

    assert_eq!(method(left, "close", &[]), hydro_none());
    assert_eq!(method(right, "close", &[]), hydro_none());
}

#[test]
fn address_helpers_convert_and_resolve() {
    let _guard = guard();
    hydro_runtime_init();

    assert_eq!(module_get("socket", "AF_INET6"), int(10));
    assert_eq!(module_get("socket", "SOCK_DGRAM"), int(2));
    assert_eq!(module_get("socket", "IPPROTO_TCP"), int(6));
    assert_eq!(module_get("socket", "IPPROTO_UDP"), int(17));

    let packed = module("socket", "inet_pton", &[int(2), text("127.0.0.1")]);
    assert_bytes(packed, &[127, 0, 0, 1]);
    assert!(eq(
        module("socket", "inet_ntop", &[int(2), packed]),
        text("127.0.0.1")
    ));

    let packed6 = module("socket", "inet_pton", &[int(10), text("::1")]);
    assert_no_error();
    assert!(eq(
        module("socket", "inet_ntop", &[int(10), packed6]),
        text("::1")
    ));

    let entries = module(
        "socket",
        "getaddrinfo",
        &[text("127.0.0.1"), int(80), int(2), int(1), int(6)],
    );
    assert_no_error();
    assert!(decode(hydro_len(entries)) >= 1);
    let first = hydro_subscript(entries, int(0));
    assert_eq!(decode(hydro_subscript(first, int(0))), 2);
    assert_eq!(decode(hydro_subscript(first, int(1))), 1);
    assert_eq!(decode(hydro_subscript(first, int(2))), 6);
    let sockaddr = hydro_subscript(first, int(4));
    assert!(eq(hydro_subscript(sockaddr, int(0)), text("127.0.0.1")));
    assert_eq!(decode(hydro_subscript(sockaddr, int(1))), 80);

    assert!(eq(
        module("socket", "gethostbyname", &[text("127.0.0.1")]),
        text("127.0.0.1")
    ));
    assert_no_error();
}

#[test]
fn ipv6_loopback_echo_when_available() {
    let _guard = guard();
    hydro_runtime_init();

    let server = module("socket", "socket", &[int(10), int(1), int(0)]);
    assert_no_error();
    let bound = method(server, "bind", &[tuple(&[text("::1"), int(0)])]);
    if hydro_exc_pending() == 1 {
        // No IPv6 loopback on this host: the bind failure is the skip signal.
        clear_error();
        assert_eq!(method(server, "close", &[]), hydro_none());
        return;
    }
    assert_eq!(bound, hydro_none());
    let name = method(server, "getsockname", &[]);
    assert_no_error();
    assert_eq!(decode(hydro_len(name)), 4);
    assert!(eq(hydro_subscript(name, int(0)), text("::1")));
    let port = decode(hydro_subscript(name, int(1)));
    assert!(port > 0);

    assert_eq!(method(server, "listen", &[int(1)]), hydro_none());
    settimeout(server, 5.0);

    let client = module("socket", "socket", &[int(10), int(1), int(0)]);
    settimeout(client, 5.0);
    assert_eq!(
        method(client, "connect", &[tuple(&[text("::1"), int(port)])]),
        hydro_none()
    );
    assert_no_error();
    let accepted = method(server, "accept", &[]);
    assert_no_error();
    let connection = hydro_subscript(accepted, int(0));
    settimeout(connection, 5.0);
    assert_eq!(method(client, "sendall", &[bytes(b"v6")]), hydro_none());
    assert_bytes(method(connection, "recv", &[int(2)]), b"v6");

    assert_eq!(method(client, "close", &[]), hydro_none());
    assert_eq!(method(connection, "close", &[]), hydro_none());
    assert_eq!(method(server, "close", &[]), hydro_none());
}

#[test]
fn context_manager_enter_and_exit() {
    let _guard = guard();
    hydro_runtime_init();

    let socket = new_socket();
    assert!(eq(method(socket, "__enter__", &[]), socket));
    assert_no_error();
    assert!(eq(hydro_to_str(socket), text("<socket object, unbound>")));
    assert_eq!(
        method(
            socket,
            "__exit__",
            &[hydro_none(), hydro_none(), hydro_none()]
        ),
        hydro_none()
    );
    assert_no_error();
    assert!(eq(hydro_to_str(socket), text("<socket closed>")));
    assert_eq!(method(socket, "recv", &[int(1)]), hydro_none());
    clear_error();
}

#[test]
fn accelerator_alias_exposes_the_socket_surface() {
    let _guard = guard();
    hydro_runtime_init();

    // Constants resolve through the accelerator spelling with the same value.
    for name in [
        "AF_INET",
        "AF_INET6",
        "SOCK_STREAM",
        "SOCK_DGRAM",
        "SOL_SOCKET",
        "SO_REUSEADDR",
        "SHUT_RDWR",
        "IPPROTO_TCP",
    ] {
        assert!(
            eq(module_get("_socket", name), module_get("socket", name)),
            "{name}"
        );
        assert_no_error();
    }

    // Calls dispatch to the same implementation: a socket created through
    // `_socket.socket` binds, reports its address and closes normally.
    let socket = module("_socket", "socket", &[]);
    assert_no_error();
    let port = bind_loopback(socket);
    assert!(port > 0);

    // `SocketType` is the class `type()` reports for a socket, so identity and
    // `isinstance` work through the accelerator alias.
    let socket_type = module_get("_socket", "SocketType");
    assert_no_error();
    assert!(eq(socket_type, module_get("socket", "SocketType")));
    assert_no_error();
    let type_fn = hydro_builtin_function(b"type".as_ptr(), 4);
    let actual_class = hydro_call(type_fn, 1, [socket].as_ptr());
    assert_no_error();
    assert!(eq(actual_class, socket_type));
    let isinstance_fn = hydro_builtin_function(b"isinstance".as_ptr(), 10);
    assert_eq!(
        hydro_call(
            isinstance_fn,
            2,
            [socket, module_get("_socket", "SocketType")].as_ptr()
        ),
        hydro_true()
    );
    assert_no_error();
    assert_eq!(method(socket, "close", &[]), hydro_none());
    assert_no_error();

    // The class value itself constructs a socket, like `SocketType()`.
    let made = hydro_call(socket_type, 0, std::ptr::null());
    assert_no_error();
    assert_eq!(method(made, "close", &[]), hydro_none());
    assert_no_error();

    // The other module-level helpers reach the socket implementation.
    let host = module("_socket", "gethostname", &[]);
    assert_no_error();
    assert!(decode(hydro_len(hydro_to_str(host))) > 0);
    assert!(eq(
        module("_socket", "inet_pton", &[int(2), text("127.0.0.1")]),
        module("socket", "inet_pton", &[int(2), text("127.0.0.1")])
    ));
    assert_no_error();

    // pydantic's probe: `getattr(_socket, "CAPI", None)` is `None` because
    // Hydro exposes no CPython CAPI capsule.
    let module_value = hydro_module_value(b"_socket".as_ptr(), 7);
    assert_no_error();
    let getattr_fn = hydro_builtin_function(b"getattr".as_ptr(), 7);
    let capi = hydro_call(
        getattr_fn,
        3,
        [module_value, text("CAPI"), hydro_none()].as_ptr(),
    );
    assert_no_error();
    assert_eq!(capi, hydro_none());

    // Without a default the miss still raises `AttributeError`.
    assert_eq!(module_get("_socket", "CAPI"), hydro_none());
    assert_eq!(hydro_exc_pending(), 1);
    hydro_exc_clear();
}
