//! Integration tests for `select`, `select.poll` and `selectors`.
//!
//! Like the socket tests, every test takes `TEST_LOCK` and initialises the
//! process-global heap. All descriptors are loopback sockets; no test touches
//! the external network.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hydro_runtime::abi::*;

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

fn method_kw(obj: i64, name: &str, args: &[i64], keywords: &[(&str, i64)]) -> i64 {
    let mut words = Vec::with_capacity(keywords.len() * 2);
    for (keyword, item) in keywords {
        words.push(text(keyword));
        words.push(*item);
    }
    hydro_method_kw(
        obj,
        name.as_ptr(),
        name.len(),
        args.len(),
        args.as_ptr(),
        keywords.len(),
        words.as_ptr(),
    )
}

fn module_kw(module: &str, function: &str, args: &[i64], keywords: &[(&str, i64)]) -> i64 {
    let mut words = Vec::with_capacity(keywords.len() * 2);
    for (keyword, item) in keywords {
        words.push(text(keyword));
        words.push(*item);
    }
    hydro_module_call_kw(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
        keywords.len(),
        words.as_ptr(),
    )
}

fn eq(a: i64, b: i64) -> bool {
    hydro_binop(7, a, b) == hydro_true()
}

fn getattr(obj: i64, name: &str) -> i64 {
    hydro_getattr(obj, name.as_ptr(), name.len())
}

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

fn new_stream() -> i64 {
    let socket = module("socket", "socket", &[]);
    assert_no_error();
    socket
}

fn new_datagram() -> i64 {
    let socket = module("socket", "socket", &[int(2), int(2), int(0)]);
    assert_no_error();
    socket
}

fn bind_loopback(socket: i64) -> i64 {
    let address = tuple(&[text("127.0.0.1"), int(0)]);
    assert_eq!(method(socket, "bind", &[address]), hydro_none());
    assert_no_error();
    let name = method(socket, "getsockname", &[]);
    assert_no_error();
    decode(hydro_subscript(name, int(1)))
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

#[test]
fn select_reports_udp_readiness_and_timeouts() {
    let _guard = guard();
    hydro_runtime_init();

    let receiver = new_datagram();
    let receiver_port = bind_loopback(receiver);
    let sender = new_datagram();
    bind_loopback(sender);
    settimeout(receiver, 2.0);

    // Nothing is queued yet: a zero timeout returns three empty lists.
    let empty = module(
        "select",
        "select",
        &[
            list(&[receiver]),
            list(&[]),
            list(&[]),
            hydro_float_new(0.0),
        ],
    );
    assert_no_error();
    assert_eq!(decode(hydro_len(hydro_subscript(empty, int(0)))), 0);

    assert_eq!(
        method(sender, "sendto", &[bytes(b"ready"), address(receiver_port)],),
        int(5)
    );
    assert_no_error();

    let ready = module_kw(
        "select",
        "select",
        &[list(&[receiver]), list(&[]), list(&[])],
        &[("timeout", hydro_float_new(5.0))],
    );
    assert_no_error();
    let readable = hydro_subscript(ready, int(0));
    assert_eq!(decode(hydro_len(readable)), 1);
    assert!(eq(hydro_subscript(readable, int(0)), receiver));
    assert_eq!(decode(hydro_len(hydro_subscript(ready, int(1)))), 0);
    // Drain the datagram so the timeout below observes an idle descriptor.
    let drained = method(receiver, "recvfrom", &[int(64)]);
    assert_no_error();
    assert!(eq(hydro_subscript(drained, int(0)), bytes(b"ready")));

    // A finite timeout with no readiness returns empty lists instead of
    // blocking forever.
    let started = Instant::now();
    let timeout = module(
        "select",
        "select",
        &[
            list(&[receiver]),
            list(&[]),
            list(&[]),
            hydro_float_new(0.05),
        ],
    );
    assert_no_error();
    assert_eq!(decode(hydro_len(hydro_subscript(timeout, int(0)))), 0);
    assert!(started.elapsed() < Duration::from_secs(2));

    assert_eq!(method(receiver, "close", &[]), hydro_none());
    assert_eq!(method(sender, "close", &[]), hydro_none());
}

#[test]
fn poll_object_registers_and_reports_events() {
    let _guard = guard();
    hydro_runtime_init();

    assert_eq!(module_get("select", "POLLIN"), int(1));
    assert_eq!(module_get("select", "POLLOUT"), int(4));

    let server = new_stream();
    let port = bind_loopback(server);
    assert_eq!(method(server, "listen", &[int(1)]), hydro_none());
    settimeout(server, 5.0);
    let fd = method(server, "fileno", &[]);
    assert_no_error();

    let poller = module("select", "poll", &[]);
    assert_no_error();
    assert_eq!(
        method(poller, "register", &[fd, module_get("select", "POLLIN")]),
        hydro_none()
    );
    assert_no_error();

    let empty = method(poller, "poll", &[int(0)]);
    assert_no_error();
    assert_eq!(decode(hydro_len(empty)), 0);

    let client = new_stream();
    settimeout(client, 5.0);
    assert_eq!(method(client, "connect", &[address(port)]), hydro_none());
    assert_no_error();

    // Poll before accepting: the pending connection is what makes the
    // listening descriptor readable.
    let ready = method(poller, "poll", &[int(5000)]);
    assert_no_error();
    assert!(decode(hydro_len(ready)) >= 1);
    let entry = hydro_subscript(ready, int(0));
    assert_eq!(decode(hydro_subscript(entry, int(0))), decode(fd));
    assert_ne!(decode(hydro_subscript(entry, int(1))) & 1, 0);

    let accepted = method(server, "accept", &[]);
    assert_no_error();
    let connection = hydro_subscript(accepted, int(0));
    settimeout(connection, 5.0);

    assert_eq!(method(poller, "unregister", &[fd]), hydro_none());
    assert_no_error();
    assert_eq!(method(poller, "close", &[]), hydro_none());
    assert_eq!(method(connection, "close", &[]), hydro_none());
    assert_eq!(method(client, "close", &[]), hydro_none());
    assert_eq!(method(server, "close", &[]), hydro_none());
}

#[test]
fn nonblocking_connect_completes_through_select() {
    let _guard = guard();
    hydro_runtime_init();

    let server = new_stream();
    let port = bind_loopback(server);
    assert_eq!(method(server, "listen", &[int(1)]), hydro_none());
    settimeout(server, 5.0);

    let client = new_stream();
    assert_eq!(
        method(client, "setblocking", &[hydro_false()]),
        hydro_none()
    );
    assert_eq!(method(client, "getblocking", &[]), hydro_false());
    let errno = method(client, "connect_ex", &[address(port)]);
    assert_no_error();
    // Loopback connects either complete immediately or report EINPROGRESS.
    assert!(decode(errno) == 0 || decode(errno) == 115);

    if decode(errno) != 0 {
        let writable = module(
            "select",
            "select",
            &[list(&[]), list(&[client]), list(&[]), hydro_float_new(5.0)],
        );
        assert_no_error();
        assert_eq!(decode(hydro_len(hydro_subscript(writable, int(1)))), 1);
        let completion = method(client, "connect_ex", &[address(port)]);
        assert_no_error();
        assert_eq!(decode(completion), 0);
    }

    let accepted = method(server, "accept", &[]);
    assert_no_error();
    let connection = hydro_subscript(accepted, int(0));
    settimeout(connection, 5.0);

    // A non-blocking read with no data raises BlockingIOError semantics.
    assert_eq!(method(client, "recv", &[int(4)]), hydro_none());
    assert!(
        eq(
            error_message(),
            text("[Errno 11] Resource temporarily unavailable")
        ),
        "unexpected non-blocking error message"
    );
    clear_error();

    assert_eq!(
        method(connection, "sendall", &[bytes(b"done")]),
        hydro_none()
    );
    let readable = module(
        "select",
        "select",
        &[list(&[client]), list(&[]), list(&[]), hydro_float_new(5.0)],
    );
    assert_no_error();
    assert_eq!(decode(hydro_len(hydro_subscript(readable, int(0)))), 1);
    assert!(eq(method(client, "recv", &[int(4)]), bytes(b"done")));

    assert_eq!(method(connection, "close", &[]), hydro_none());
    assert_eq!(method(client, "close", &[]), hydro_none());
    assert_eq!(method(server, "close", &[]), hydro_none());
}

#[test]
fn default_selector_registers_and_selects_readiness() {
    let _guard = guard();
    hydro_runtime_init();

    assert_eq!(module_get("selectors", "EVENT_READ"), int(1));
    assert_eq!(module_get("selectors", "EVENT_WRITE"), int(2));

    let server = new_stream();
    let port = bind_loopback(server);
    assert_eq!(method(server, "listen", &[int(1)]), hydro_none());
    settimeout(server, 5.0);

    let selector = module("selectors", "DefaultSelector", &[]);
    assert_no_error();
    let key = method(
        selector,
        "register",
        &[
            server,
            module_get("selectors", "EVENT_READ"),
            text("listener"),
        ],
    );
    assert_no_error();
    let server_fd = method(server, "fileno", &[]);
    assert_no_error();
    assert_eq!(decode(getattr(key, "fd")), decode(server_fd));
    assert!(eq(getattr(key, "fileobj"), server));
    assert!(eq(getattr(key, "data"), text("listener")));

    // No connection yet: selector.select(0) must report nothing.
    let empty = method(selector, "select", &[hydro_float_new(0.0)]);
    assert_no_error();
    assert_eq!(decode(hydro_len(empty)), 0);

    let client = new_stream();
    settimeout(client, 5.0);
    assert_eq!(method(client, "connect", &[address(port)]), hydro_none());
    assert_no_error();

    let ready = method_kw(
        selector,
        "select",
        &[],
        &[("timeout", hydro_float_new(5.0))],
    );
    assert_no_error();
    assert!(decode(hydro_len(ready)) >= 1);
    let event = hydro_subscript(ready, int(0));
    let ready_key = hydro_subscript(event, int(0));
    assert_ne!(decode(hydro_subscript(event, int(1))) & 1, 0);
    assert!(eq(getattr(ready_key, "fileobj"), server));
    assert!(eq(getattr(ready_key, "data"), text("listener")));

    let map = method(selector, "get_map", &[]);
    assert_no_error();
    assert_eq!(decode(hydro_len(map)), 1);

    let removed = method(selector, "unregister", &[server]);
    assert_no_error();
    assert!(eq(removed, ready_key));
    assert_eq!(method(selector, "close", &[]), hydro_none());

    let accepted = method(server, "accept", &[]);
    assert_no_error();
    let connection = hydro_subscript(accepted, int(0));
    assert_eq!(method(connection, "close", &[]), hydro_none());
    assert_eq!(method(client, "close", &[]), hydro_none());
    assert_eq!(method(server, "close", &[]), hydro_none());
}
