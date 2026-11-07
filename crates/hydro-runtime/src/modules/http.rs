#![allow(dead_code)] // HTTPConnection helpers are staged for the connection class.
//! `http`: a native HTTP/1.1 server with a static-response fast path.
//!
//! `http.serve(port, routes, handler, max_connections)` binds one or more
//! listeners and blocks for the rest of the process. Each listener is owned by
//! exactly one OS thread that also owns every socket it accepted and every read
//! buffer: the thread runs a level-triggered `epoll` loop together with an
//! `eventfd` that its dynamic worker pool uses to wake it. A request whose
//! exact `(method, path)` pair (the query string is stripped) appears in
//! `routes` is answered from the event-loop thread by copying the pre-built
//! response bytes to the socket and never enters Python. Every other request
//! is queued to a bounded pool of native worker threads that call the Python
//! `handler(method, path, body)` and hand the returned `bytes` object back
//! through a completion queue plus the `eventfd`. A connection has at most one
//! dynamic job in flight, so pipelined requests are served in order and no
//! socket is read ahead while a handler is running.
//!
//! ## Shards
//!
//! The `HYDRO_HTTP_LOOPS` environment variable selects the number of event-loop
//! shards (default 4, clamped to `1..=8`):
//!
//! * `HYDRO_HTTP_LOOPS=1` is the compatible single-loop path: one
//!   `TcpListener::bind`, no `SO_REUSEPORT`, one epoll thread and the full
//!   `min(max_connections, 64)` worker budget.
//! * A value above one binds that many sockets to the same `0.0.0.0:port` with
//!   `SO_REUSEPORT` and `SO_REUSEADDR` set before `bind`, so the kernel spreads
//!   incoming connections across shards. Every shard has its own epoll set,
//!   eventfd, connection map, accept loop and worker pool; nothing is shared
//!   between shards except the immutable route table, the rooted handler and
//!   the GC runtime.
//!
//! All shard listeners are bound before [`heap::enable_shared`] and before any
//! worker exists, and every shard's epoll set and eventfd are created there
//! too; a bind or setup failure closes everything created so far and raises
//! `OSError` without spawning a worker. The connection budget is split so the
//! per-shard caps sum to exactly `max_connections`: shard `i` gets
//! `max_connections / loops` plus one while `i < max_connections % loops`. The
//! dynamic worker budget is `max(1, MAX_WORKERS / loops)` per shard using
//! integer division, so the total is at most 64 and rounding down leaves a few
//! idle slots for loop counts that do not divide 64 (for example 5 loops get 12
//! workers each, 60 in total). The single-loop path keeps the historical
//! `min(max_connections, 64)` pool.
//!
//! One shard runs on the calling thread and the other `loops - 1` shards each
//! run on a spawned native thread, so the Python caller never blocks in
//! `join`. If spawning a loop thread fails, every pool created so far is told
//! to stop and `OSError` is raised instead of continuing with fewer loops.
//!
//! ## Event loop
//!
//! * A shard's listener and every connection socket are non-blocking. The
//!   listen backlog is raised to 1024 and one `epoll_wait` batch carries up to
//!   1024 events, so a 1024-connection burst is accepted and serviced in one
//!   pass per shard.
//!   The accept path drains the backlog until `WouldBlock`; once the shard's
//!   connection cap is reached the listener's `EPOLLIN` interest is disarmed
//!   and re-armed when a connection closes, so the loop never spins or blocks.
//!   Every accepted socket gets `TCP_NODELAY` so small responses are not held
//!   back by Nagle's algorithm.
//! * Each connection owns its read buffer, so a partial head or body never
//!   blocks other connections. Responses are written with partial-write
//!   handling: `EPOLLOUT` stays armed while bytes remain queued, and the next
//!   pipelined request is parsed only after the previous response is flushed.
//! * Interest is `EPOLLIN` while a connection is idle, `EPOLLOUT` while output
//!   is pending, and neither while a dynamic job is in flight; `EPOLLRDHUP`,
//!   `EPOLLERR` and `EPOLLHUP` are always part of the mask. Closing removes
//!   the descriptor from the epoll set and from the connection map.
//! * Only `epoll_wait` parks the event-loop thread (through
//!   `heap::blocking_enter`/`heap::blocking_leave`); socket I/O, request
//!   parsing and queue hand-offs never block.
//!
//! ## Workers and GC safety
//!
//! * `serve` calls [`heap::enable_shared`] before the first worker exists, so
//!   every value reachable from the Python heap (including the handler) lives
//!   in the shared heap.
//! * The handler is installed as a process-wide root
//!   ([`heap::add_global_root`]) before any worker starts, so it stays alive
//!   across collections and after the calling thread's registration ends.
//! * Each shard's pool holds `max(1, MAX_WORKERS / loops)` workers. Each
//!   worker follows the `_thread.start_new_thread` protocol: `enter_thread`
//!   before the OS thread exists, then `register_thread` (with no
//!   out-of-stack roots) and a safepoint as its first action, and
//!   `leave_thread` from a `Drop` guard.
//! * Waiting for a job parks the worker with
//!   `blocking_enter`/`blocking_leave` around the queue wait, so a
//!   stop-the-world collection can always complete; no heap lock is ever held
//!   across that wait, and no I/O is performed while a queue lock is held.
//! * The three `bytes` arguments and the handler result are only reachable
//!   from the worker's Rust stack, which the collector does not scan precisely,
//!   so the argument buffers and the result are registered with
//!   `heap::root_values` for the duration of the call and the byte copy.
//! * `routes` is copied into Rust-owned buffers during validation, so no
//!   Python value is touched by the static-response path.
//!
//! ## Protocol subset
//!
//! Request parsing supports HTTP/1.1 (and HTTP/1.0, which defaults to
//! closing) with `Content-Length` bodies, keep-alive, and pipelined requests
//! left in the read buffer. `Transfer-Encoding: chunked` request bodies are
//! rejected by closing the connection, as the contract requires no chunked
//! request bodies. Connection errors, malformed requests and handler failures
//! close only that connection; the listening socket keeps running.
//!
//! ## `http.client` exceptions
//!
//! [`http_exception_class`] and [`remote_disconnected_class`] expose the two
//! exception types CPython defines in `http/client.py` as cached native
//! classes: `HTTPException(Exception)` and
//! `RemoteDisconnected(ConnectionResetError, HTTPException)`. They are real
//! exception payloads, so `str`, `.args`, `isinstance` and `except` all behave
//! like the Python-level declarations; the module surface (`http.client`) is
//! wired by the module registry.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::exc::{
    self, ATTRIBUTE_ERROR, CONNECTION_RESET_ERROR, EXCEPTION, OS_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::modules::socket;
use crate::value::{self, NONE};

/// Largest request head (request line plus headers) we buffer.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Largest `Content-Length` request body we accept.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Read chunk size; a request is assembled from these slices.
const READ_CHUNK: usize = 16 * 1024;
/// Upper bound on dynamic worker threads.
const MAX_WORKERS: usize = 64;
/// Largest batch of events returned by one `epoll_wait`: one per connection
/// of a full 1024-connection burst.
const MAX_EVENTS: usize = 1024;
/// Accept backlog requested after bind, matching [`MAX_EVENTS`].
const ACCEPT_BACKLOG: i32 = 1024;
/// Default number of event-loop shards, used when `HYDRO_HTTP_LOOPS` is unset
/// or unparsable.
const DEFAULT_LOOPS: usize = 4;
/// Upper bound on `HYDRO_HTTP_LOOPS`.
const MAX_LOOPS: usize = 8;

/// Events every connection registration carries: a half-close or a fatal
/// error must always wake the loop.
const BASE_EVENTS: u32 = (libc::EPOLLRDHUP | libc::EPOLLERR | libc::EPOLLHUP) as u32;
/// Registration of a freshly accepted, idle connection.
const CONN_EVENTS: u32 = BASE_EVENTS | libc::EPOLLIN as u32;

/// `(method, path) -> complete response bytes` table. Nested maps let both
/// halves of the key be looked up as borrowed byte slices, so a static request
/// performs no allocation.
type Routes = HashMap<Vec<u8>, HashMap<Vec<u8>, Arc<[u8]>>>;

/// Immutable state shared with the worker pool.
struct Config {
    routes: Routes,
}

impl Config {
    /// Looks up a pre-built response for `method`/`path`, without allocating.
    fn route(&self, method: &[u8], path: &[u8]) -> Option<&Arc<[u8]>> {
        self.routes.get(method).and_then(|paths| paths.get(path))
    }
}

/// One parsed HTTP request. `path` excludes the query string.
struct Request {
    method: Vec<u8>,
    path: Vec<u8>,
    body: Vec<u8>,
    keep_alive: bool,
}

/// A request head before its body has been read.
struct Head {
    method: Vec<u8>,
    path: Vec<u8>,
    body_len: usize,
    keep_alive: bool,
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

/// `OSError` carrying the platform errno, if any.
fn io_error(error: io::Error) -> i64 {
    match error.raw_os_error() {
        Some(code) => exc::raise_with_message(OS_ERROR, &format!("[Errno {code}] {error}")),
        None => exc::raise_with_message(OS_ERROR, &error.to_string()),
    }
}

/// Number of event-loop shards requested through `HYDRO_HTTP_LOOPS`.
///
/// Unset or unparsable values fall back to [`DEFAULT_LOOPS`]; the result is
/// clamped to `1..=MAX_LOOPS`.
fn http_loops() -> usize {
    std::env::var("HYDRO_HTTP_LOOPS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|loops| loops.clamp(1, MAX_LOOPS))
        .unwrap_or(DEFAULT_LOOPS)
}

/// The compatible single-loop listener: the platform bind, no reuse options.
fn bind_single_listener(port: u16) -> io::Result<TcpListener> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    listener.set_nonblocking(true)?;
    // `TcpListener::bind` uses a small platform default backlog; raise it so a
    // 1024-connection burst is not truncated before the accept loop runs.
    // Failure is not fatal: the default backlog still accepts connections.
    unsafe {
        libc::listen(listener.as_raw_fd(), ACCEPT_BACKLOG);
    }
    Ok(listener)
}

/// Binds one `SO_REUSEPORT` listener for a sharded event loop.
///
/// The socket is created by hand so `SO_REUSEADDR` and `SO_REUSEPORT` are set
/// before `bind` and the backlog is exact. Every failure path closes the
/// descriptor; on success the listener owns it.
fn bind_shard_listener(port: u16) -> io::Result<TcpListener> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    if let Err(error) = configure_shard_socket(fd, port) {
        close_fd(fd);
        return Err(error);
    }
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    // Ownership sits in the listener from here on, so a failure just drops it.
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Sets the reuse options on `fd`, then binds and listens on `0.0.0.0:port`.
fn configure_shard_socket(fd: RawFd, port: u16) -> io::Result<()> {
    setsockopt_flag(fd, libc::SO_REUSEADDR)?;
    setsockopt_flag(fd, libc::SO_REUSEPORT)?;
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: libc::INADDR_ANY,
        },
        sin_zero: [0; 8],
    };
    let bound = unsafe {
        libc::bind(
            fd,
            (&address as *const libc::sockaddr_in).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if bound == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::listen(fd, ACCEPT_BACKLOG) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Enables one boolean `SOL_SOCKET` option.
fn setsockopt_flag(fd: RawFd, option: libc::c_int) -> io::Result<()> {
    let enable: libc::c_int = 1;
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&enable as *const libc::c_int).cast::<libc::c_void>(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The epoll set and wake eventfd owned by one shard's event loop.
///
/// Dropping closes both, including when a `Server` is dropped because its loop
/// thread could not be spawned.
struct LoopFds {
    epoll: RawFd,
    wake: RawFd,
}

impl LoopFds {
    /// Creates the epoll set and eventfd and registers the listener and the
    /// eventfd on it. Every failure closes what was created so far.
    fn new(listener_fd: RawFd) -> io::Result<Self> {
        let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll == -1 {
            return Err(io::Error::last_os_error());
        }
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake == -1 {
            let error = io::Error::last_os_error();
            close_fd(epoll);
            return Err(error);
        }
        if let Err(error) = epoll_ctl(
            epoll,
            libc::EPOLL_CTL_ADD,
            listener_fd,
            libc::EPOLLIN as u32,
            listener_fd as u64,
        ) {
            close_fd(wake);
            close_fd(epoll);
            return Err(error);
        }
        if let Err(error) = epoll_ctl(
            epoll,
            libc::EPOLL_CTL_ADD,
            wake,
            libc::EPOLLIN as u32,
            wake as u64,
        ) {
            close_fd(wake);
            close_fd(epoll);
            return Err(error);
        }
        Ok(Self { epoll, wake })
    }
}

impl Drop for LoopFds {
    fn drop(&mut self) {
        close_fd(self.wake);
        close_fd(self.epoll);
    }
}

/// Tells every worker pool created so far to drain and exit.
fn stop_pools(pools: &[Arc<Shared>]) {
    for pool in pools {
        pool.stop();
    }
}

/// Runs one shard's event loop on a dedicated thread.
///
/// The thread performs no Python allocation and is not a registered GC thread;
/// it only parks in `epoll_wait` through `blocking_enter`/`blocking_leave`,
/// which is a no-op for it.
fn spawn_loop(server: Server, index: usize) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("hydro-http-loop-{index}"))
        .spawn(move || server.run())
        .map(|_| ())
}

/// Native module dispatch for `http`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "serve" => serve(args),
        // `http.client.parse_headers(fp)` is a module function, so its value is
        // a native function bound to this module's dispatch.
        "client_parse_headers" => match args {
            [fp] => parse_headers_from_fp(*fp),
            _ => type_error("parse_headers() takes 1 argument"),
        },
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'http' has no attribute '{function}'"),
        ),
    }
}

/// `http.client.HTTPException`: the base class of every `http.client` error.
///
/// CPython declares it as `class HTTPException(Exception)`, so an instance
/// carries a real exception payload (`.args`, `str(e)`), `isinstance(e,
/// Exception)` holds, and `except http.client.HTTPException:` catches it.
/// Cached so every read and subclass shares one class identity.
pub(crate) fn http_exception_class() -> i64 {
    const KEY: &str = "http.client.HTTPException";
    if let Some(cached) = crate::core::globals::registry_get(KEY) {
        return cached;
    }
    let base = exc::exception_class(EXCEPTION);
    // `base` and the class only live on the Rust stack while the class is
    // built, so root them until the registry entry owns the result.
    let roots = heap::root_values(&[base]);
    let class = value::class_new_full("HTTPException".to_string(), base, Vec::new(), Vec::new());
    if class == NONE {
        heap::release_roots(roots);
        return class;
    }
    heap::push_scoped_root(class);
    crate::core::globals::registry_set(KEY, class);
    heap::release_roots(roots);
    class
}

/// `http.client.RemoteDisconnected(ConnectionResetError, HTTPException)`.
///
/// The multiple-bases form is CPython's declaration, so
/// `isinstance(e, ConnectionResetError)`, `isinstance(e, OSError)`,
/// `isinstance(e, HTTPException)` and `isinstance(e, Exception)` all hold. The
/// exception payload takes the first native ancestor's tag
/// (`ConnectionResetError`), which keeps `str(e)` and `except OSError:` exact.
pub(crate) fn remote_disconnected_class() -> i64 {
    const KEY: &str = "http.client.RemoteDisconnected";
    if let Some(cached) = crate::core::globals::registry_get(KEY) {
        return cached;
    }
    let connection_reset = exc::exception_class(CONNECTION_RESET_ERROR);
    let http_exception = http_exception_class();
    let roots = heap::root_values(&[connection_reset, http_exception]);
    let class = value::class_new_multi(
        "RemoteDisconnected".to_string(),
        vec![connection_reset, http_exception],
        NONE,
        Vec::new(),
        Vec::new(),
    );
    if class == NONE {
        heap::release_roots(roots);
        return class;
    }
    heap::push_scoped_root(class);
    crate::core::globals::registry_set(KEY, class);
    heap::release_roots(roots);
    class
}

/// Materialises an `http.client` constant.
///
/// The module-registry wiring for `http.client` dispatches attribute reads
/// here, mirroring [`crate::modules::warnings::constant`].
pub(crate) fn constant(name: &str) -> i64 {
    if let Some(code) = status_constant(name) {
        return client_integer(code);
    }
    match name {
        "HTTPException" | "error" => http_exception_class(),
        "RemoteDisconnected" => remote_disconnected_class(),
        "InvalidURL" => invalid_url_class(),
        "NotConnected" => not_connected_class(),
        "UnknownProtocol" => unknown_protocol_class(),
        "UnknownTransferEncoding" => unknown_transfer_encoding_class(),
        "UnimplementedFileMode" => unimplemented_file_mode_class(),
        "ImproperConnectionState" => improper_connection_state_class(),
        "CannotSendRequest" => cannot_send_request_class(),
        "CannotSendHeader" => cannot_send_header_class(),
        "ResponseNotReady" => response_not_ready_class(),
        "BadStatusLine" => bad_status_line_class(),
        "LineTooLong" => line_too_long_class(),
        "IncompleteRead" => incomplete_read_class(),
        "HTTPMessage" => message_class(),
        "HTTPResponse" => response_class(),
        "HTTPConnection" => connection_class(),
        "HTTPSConnection" => https_connection_class(),
        "parse_headers" => {
            // The function value carries its trampoline directly, so it works
            // even though `http.client` has no module-call dispatch arm.
            let function = value::native_function_new("http", "client_parse_headers", 1);
            value::function_set_signature(function, vec![client_positional("fp")]);
            value::function_set_full_entry(
                function,
                client_parse_headers_entry as *const () as usize,
            );
            function
        }
        "responses" => responses_dict(),
        "__all__" => client_all(),
        "HTTP_PORT" => client_integer(80),
        "HTTPS_PORT" => client_integer(443),
        "_MAXHEADERS" => client_integer(CLIENT_MAXHEADERS as i64),
        "_MAXLINE" => client_integer(CLIENT_MAXLINE),
        "_MAXINTERIMRESPONSES" => client_integer(CLIENT_MAXINTERIM as i64),
        "_UNKNOWN" => client_text("UNKNOWN"),
        "_CS_IDLE" => client_text(CS_IDLE),
        "_CS_REQ_STARTED" => client_text(CS_REQ_STARTED),
        "_CS_REQ_SENT" => client_text(CS_REQ_SENT),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'http.client' has no attribute '{name}'"),
        ),
    }
}

/// `http.serve(port, routes, handler, max_connections) -> None`.
fn serve(args: &[i64]) -> i64 {
    if args.len() != 4 {
        return type_error("http.serve() takes 4 arguments");
    }
    let Some(port) = value::as_integer(args[0]) else {
        return type_error("port must be an integer");
    };
    if !(0..=65_535).contains(&port) {
        return value_error("port must be in the range 0..65535");
    }
    let Some(pairs) = value::as_dict(args[1]) else {
        return type_error("routes must be a dict");
    };
    let mut routes: Routes = HashMap::new();
    for (key, response) in pairs {
        let Some(key) = value::as_str(*key) else {
            return type_error("route keys must be strings");
        };
        let Some(response) = value::as_bytes(*response) else {
            return type_error("route responses must be bytes");
        };
        // Keys are exactly `"METHOD /path"`; the first space separates them so
        // a path may itself contain spaces (they are not valid in practice but
        // need no special error).
        let Some((method, path)) = key.split_once(' ') else {
            return value_error("route keys must have the form 'METHOD /path'");
        };
        if method.is_empty() || !path.starts_with('/') {
            return value_error("route keys must have the form 'METHOD /path'");
        }
        routes
            .entry(method.as_bytes().to_vec())
            .or_default()
            .insert(
                path.as_bytes().to_vec(),
                Arc::<[u8]>::from(response.to_vec()),
            );
    }

    let handler = args[2];
    if !is_callable(handler) {
        return type_error("handler must be callable");
    }
    let Some(max_connections) = value::as_integer(args[3]) else {
        return type_error("max_connections must be an integer");
    };
    if max_connections < 1 {
        return value_error("max_connections must be positive");
    }
    let max_connections = usize::try_from(max_connections).unwrap_or(usize::MAX);

    // `HYDRO_HTTP_LOOPS=1` keeps the compatible single-listener path, while a
    // larger value binds that many `SO_REUSEPORT` listeners on the same port.
    let loops = http_loops();

    // Bind every shard listener before switching the runtime to the shared
    // heap: a bind failure is a plain Python exception with no side effects,
    // and every listener created so far is closed on the error return.
    let listeners = if loops == 1 {
        match bind_single_listener(port as u16) {
            Ok(listener) => vec![listener],
            Err(error) => return io_error(error),
        }
    } else {
        let mut listeners = Vec::with_capacity(loops);
        for _ in 0..loops {
            match bind_shard_listener(port as u16) {
                Ok(listener) => listeners.push(listener),
                Err(error) => return io_error(error),
            }
        }
        listeners
    };

    // Each shard's epoll set and eventfd also exist before the first worker or
    // loop thread, so a setup failure still leaves the heap untouched.
    let mut loop_fds = Vec::with_capacity(loops);
    for listener in &listeners {
        match LoopFds::new(listener.as_raw_fd()) {
            Ok(fds) => loop_fds.push(fds),
            Err(error) => return io_error(error),
        }
    }

    // Every object created so far (including the handler) moves into the
    // shared heap before the first worker can observe it, and the handler is
    // rooted for the rest of the run: workers are detached and the route table
    // retains no Python value.
    heap::enable_shared();
    heap::add_global_root(handler);

    let config = Arc::new(Config { routes });
    // Integer division rounds each shard's pool down; `max(1, ..)` keeps every
    // shard able to run a handler and bounds the total by `MAX_WORKERS`. The
    // compatible single-loop path keeps its historical pool size.
    let workers = if loops == 1 {
        max_connections.clamp(1, MAX_WORKERS)
    } else {
        (MAX_WORKERS / loops).max(1)
    };
    let mut shards = Vec::with_capacity(loops);
    let mut pools = Vec::with_capacity(loops);
    for (index, (listener, fds)) in listeners.into_iter().zip(loop_fds).enumerate() {
        // The caps sum to exactly `max_connections`: shard `index` gets one
        // extra connection while it is below the remainder.
        let capacity = max_connections / loops
            + (if index < max_connections % loops {
                1
            } else {
                0
            });
        let server = Server::build(listener, fds, Arc::clone(&config), capacity);
        if let Err(error) = spawn_workers(&server.shared, handler, workers) {
            // `spawn_workers` already stopped this pool; stop the rest so no
            // worker outlives the failed `serve` call.
            stop_pools(&pools);
            return io_error(error);
        }
        pools.push(Arc::clone(&server.shared));
        shards.push(server);
    }

    // One shard runs on the calling thread and the others on their own threads,
    // so the Python caller never blocks in `join`.
    let mut shards = shards.into_iter();
    let Some(main_shard) = shards.next() else {
        stop_pools(&pools);
        return io_error(io::Error::other("no event loop shard to run"));
    };
    for (offset, shard) in shards.enumerate() {
        if let Err(error) = spawn_loop(shard, offset + 1) {
            // Shards already running on their own threads cannot be reclaimed
            // from here, but no worker pool is left running.
            stop_pools(&pools);
            return io_error(error);
        }
    }
    main_shard.run();
    NONE
}

/// A queued unit of dynamic work: exactly one request on one connection.
struct Job {
    /// Raw descriptor of the connection the job belongs to.
    fd: RawFd,
    /// Monotonic connection id; descriptors are reused, ids are not.
    id: u64,
    method: Vec<u8>,
    path: Vec<u8>,
    body: Vec<u8>,
    keep_alive: bool,
}

/// The worker's answer for one job. `response` is `None` when the handler
/// raised or returned a non-bytes value; the connection is then closed.
struct Completion {
    fd: RawFd,
    id: u64,
    response: Option<Vec<u8>>,
    keep_alive: bool,
}

/// Job hand-off between the event loop and the dynamic worker pool.
struct Shared {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    stopped: AtomicBool,
    done: Mutex<VecDeque<Completion>>,
    wake_fd: RawFd,
}

impl Shared {
    fn new(wake_fd: RawFd) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            stopped: AtomicBool::new(false),
            done: Mutex::new(VecDeque::new()),
            wake_fd,
        }
    }

    /// Hands a job to the pool. Called from the event-loop thread; never
    /// blocks on I/O and holds the queue lock only long enough to push.
    fn dispatch(&self, job: Job) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.push_back(job);
        drop(queue);
        self.ready.notify_one();
    }

    /// Waits for the next job, parking the worker for the collector.
    ///
    /// The thread is marked blocked before the queue lock is taken, so it is
    /// never required at a safepoint while it waits; no lock is held once the
    /// wait itself begins.
    fn next_job(&self) -> Option<Job> {
        heap::blocking_enter();
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let job = loop {
            if let Some(job) = queue.pop_front() {
                break Some(job);
            }
            if self.stopped.load(Ordering::SeqCst) {
                break None;
            }
            queue = self
                .ready
                .wait(queue)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        };
        drop(queue);
        heap::blocking_leave();
        job
    }

    /// Publishes a finished job and wakes the event loop.
    fn complete(&self, completion: Completion) {
        let mut done = self
            .done
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        done.push_back(completion);
        drop(done);
        self.wake();
    }

    /// Signals the pool to drain and exit (used when worker setup fails).
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ready.notify_all();
    }

    /// Wakes the event loop through the non-blocking eventfd.
    fn wake(&self) {
        let value: u64 = 1;
        let _ = unsafe {
            libc::write(
                self.wake_fd,
                &value as *const u64 as *const libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
    }
}

/// Marks the worker as exited; dropping runs once the thread's work is over,
/// including on unwind.
struct WorkerExit;

impl Drop for WorkerExit {
    fn drop(&mut self) {
        heap::leave_thread();
    }
}

/// Spawns `count` dynamic workers, each already counted as a live GC thread.
fn spawn_workers(shared: &Arc<Shared>, handler: i64, count: usize) -> io::Result<()> {
    for _ in 0..count {
        let worker_shared = Arc::clone(shared);
        let id = heap::new_thread_id();
        // Count the thread before it starts so the collector sees it.
        heap::enter_thread();
        let spawned = thread::Builder::new()
            .name(format!("hydro-http-{id}"))
            .spawn(move || {
                heap::register_thread(id, Vec::new());
                let _exit = WorkerExit;
                heap::safepoint();
                worker_loop(&worker_shared, handler);
            });
        if let Err(error) = spawned {
            // The closure was dropped before running; only the pre-counted
            // live thread needs rolling back. Running workers are told to stop.
            heap::leave_unstarted_thread();
            shared.stop();
            return Err(error);
        }
    }
    Ok(())
}

/// Runs jobs until the pool is stopped. `serve` never returns, so this loop
/// normally lives for the rest of the process.
fn worker_loop(shared: &Shared, handler: i64) {
    while let Some(job) = shared.next_job() {
        // A raised exception or a non-bytes result yields `None` and closes
        // only the connection that produced the job.
        let response = call_handler(handler, &job.method, &job.path, &job.body);
        shared.complete(Completion {
            fd: job.fd,
            id: job.id,
            response,
            keep_alive: job.keep_alive,
        });
    }
}

/// One accepted connection and its buffered bytes. All state lives on the
/// event-loop thread; the worker only sees the copied request.
struct Conn {
    stream: TcpStream,
    /// Monotonic identity, stable across descriptor reuse.
    id: u64,
    /// Received bytes not yet consumed by a complete request.
    buffer: Vec<u8>,
    /// Complete response bytes waiting to be written.
    out: Vec<u8>,
    /// Number of bytes of `out` already handed to the socket.
    out_pos: usize,
    /// A dynamic job for this connection is with the pool.
    pending: bool,
    /// The request currently being served asked for the connection to close.
    close_after_write: bool,
    /// The peer half-closed (EOF/RDHUP/HUP).
    peer_closed: bool,
    /// The connection must be dropped.
    dead: bool,
    /// Epoll interest currently registered for this descriptor.
    events: u32,
}

impl Conn {
    fn new(stream: TcpStream, id: u64) -> Self {
        Self {
            stream,
            id,
            buffer: Vec::new(),
            out: Vec::new(),
            out_pos: 0,
            pending: false,
            close_after_write: false,
            peer_closed: false,
            dead: false,
            events: CONN_EVENTS,
        }
    }

    /// Whether the connection may take more bytes from the socket.
    fn wants_read(&self) -> bool {
        !self.dead
            && !self.pending
            && !self.peer_closed
            && !self.close_after_write
            && self.out.is_empty()
    }

    /// The epoll mask matching the current state.
    fn interest(&self) -> u32 {
        if self.dead {
            return 0;
        }
        if self.pending {
            // No reads while a handler owns the request; a half-close is still
            // reported so the connection can close once the answer is written.
            return BASE_EVENTS;
        }
        if self.out_pos < self.out.len() {
            return BASE_EVENTS | libc::EPOLLOUT as u32;
        }
        if self.peer_closed || self.close_after_write {
            return BASE_EVENTS;
        }
        BASE_EVENTS | libc::EPOLLIN as u32
    }
}

/// A packed mirror of `struct epoll_event`, which the Linux ABI defines
/// without padding (`u32` then `u64` at offset 4).
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

/// `epoll_ctl` wrapper; `data` is the descriptor echoed back by
/// `epoll_wait`.
fn epoll_ctl(epoll: RawFd, op: i32, fd: RawFd, events: u32, data: u64) -> io::Result<()> {
    let mut event = EpollEvent { events, data };
    let result = unsafe {
        libc::epoll_ctl(
            epoll,
            op,
            fd,
            &mut event as *mut EpollEvent as *mut libc::epoll_event,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

/// One shard: its listener, epoll set, wake eventfd, and every live connection
/// accepted by this shard.
struct Server {
    listener: TcpListener,
    listener_fd: RawFd,
    /// This shard's epoll set and eventfd; dropping closes both.
    fds: LoopFds,
    config: Arc<Config>,
    shared: Arc<Shared>,
    conns: HashMap<RawFd, Conn>,
    max_connections: usize,
    /// Next connection id; never reused, unlike descriptors.
    next_id: u64,
    /// Whether EPOLLIN is currently registered for the listener.
    listener_armed: bool,
}

impl Server {
    /// Assembles one shard around an already-bound listener and its loop fds.
    fn build(
        listener: TcpListener,
        fds: LoopFds,
        config: Arc<Config>,
        max_connections: usize,
    ) -> Self {
        let listener_fd = listener.as_raw_fd();
        Self {
            listener,
            listener_fd,
            shared: Arc::new(Shared::new(fds.wake)),
            fds,
            config,
            conns: HashMap::new(),
            max_connections,
            next_id: 1,
            listener_armed: true,
        }
    }

    /// Runs until `epoll_wait` fails for a reason other than a signal.
    fn run(mut self) {
        let mut events = [EpollEvent { events: 0, data: 0 }; MAX_EVENTS];
        loop {
            heap::blocking_enter();
            let ready = unsafe {
                libc::epoll_wait(
                    self.fds.epoll,
                    events.as_mut_ptr() as *mut libc::epoll_event,
                    MAX_EVENTS as i32,
                    -1,
                )
            };
            let failure = if ready < 0 {
                Some(io::Error::last_os_error())
            } else {
                None
            };
            heap::blocking_leave();
            match failure {
                Some(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Some(_) => return,
                None => {}
            }
            for event in events.iter().take(ready as usize) {
                // Copy the packed fields out before comparing them.
                let flags = event.events;
                let data = event.data;
                if data == self.fds.wake as u64 {
                    self.drain_wake();
                    self.drain_completions();
                } else if data == self.listener_fd as u64 {
                    self.accept_pending();
                } else {
                    self.on_event(data as RawFd, flags);
                }
            }
        }
    }

    /// Reads the eventfd until its counter is drained.
    fn drain_wake(&mut self) {
        let mut value: u64 = 0;
        loop {
            let read = unsafe {
                libc::read(
                    self.fds.wake,
                    &mut value as *mut u64 as *mut libc::c_void,
                    std::mem::size_of::<u64>(),
                )
            };
            if read != std::mem::size_of::<u64>() as isize {
                return;
            }
        }
    }

    /// Drains the completion queue and moves each answer to its connection.
    fn drain_completions(&mut self) {
        let completions: Vec<Completion> = {
            let mut done = self
                .shared
                .done
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            done.drain(..).collect()
        };
        let mut touched = Vec::with_capacity(completions.len());
        for completion in completions {
            let Some(conn) = self.conns.get_mut(&completion.fd) else {
                // The connection was closed while its job ran; drop the answer.
                continue;
            };
            if conn.id != completion.id {
                // The descriptor was reused by a newer connection; this answer
                // belongs to the closed one.
                continue;
            }
            touched.push(completion.fd);
            conn.pending = false;
            match completion.response {
                Some(response) => {
                    conn.out = response;
                    conn.out_pos = 0;
                    if !completion.keep_alive {
                        conn.close_after_write = true;
                    }
                }
                None => conn.dead = true,
            }
        }
        for fd in touched {
            self.pump(fd);
        }
    }

    /// Accepts every pending connection until the backlog is empty or the
    /// connection cap is reached.
    fn accept_pending(&mut self) {
        loop {
            if self.conns.len() >= self.max_connections {
                break;
            }
            match self.listener.accept() {
                Ok((stream, _peer)) => {
                    // Responses are small and latency-sensitive; Nagle would
                    // delay them waiting for more data.
                    let _ = stream.set_nodelay(true);
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let fd = stream.as_raw_fd();
                    if epoll_ctl(
                        self.fds.epoll,
                        libc::EPOLL_CTL_ADD,
                        fd,
                        CONN_EVENTS,
                        fd as u64,
                    )
                    .is_err()
                    {
                        // Dropping the stream closes the descriptor.
                        continue;
                    }
                    let id = self.next_id;
                    self.next_id = self.next_id.wrapping_add(1);
                    self.conns.insert(fd, Conn::new(stream, id));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        self.sync_listener();
    }

    /// Arms the listener only while there is a free connection slot.
    fn sync_listener(&mut self) {
        let armed = self.conns.len() < self.max_connections;
        if armed == self.listener_armed {
            return;
        }
        let events = if armed { libc::EPOLLIN as u32 } else { 0 };
        if epoll_ctl(
            self.fds.epoll,
            libc::EPOLL_CTL_MOD,
            self.listener_fd,
            events,
            self.listener_fd as u64,
        )
        .is_ok()
        {
            self.listener_armed = armed;
        }
    }

    /// Handles a readiness event for one connection.
    fn on_event(&mut self, fd: RawFd, flags: u32) {
        let dead = match self.conns.get_mut(&fd) {
            Some(conn) => handle_event(conn, flags),
            None => return,
        };
        if dead {
            self.close(fd);
            return;
        }
        self.pump(fd);
    }

    /// Drives one connection until it is blocked, waiting, or closed.
    fn pump(&mut self, fd: RawFd) {
        let dead = match self.conns.get_mut(&fd) {
            Some(conn) => pump_conn(conn, self.config.as_ref(), self.shared.as_ref(), fd),
            None => return,
        };
        if dead {
            self.close(fd);
        } else {
            self.refresh(fd);
        }
    }

    /// Updates the epoll interest of a connection if its state changed.
    fn refresh(&mut self, fd: RawFd) {
        let changed = match self.conns.get_mut(&fd) {
            Some(conn) => {
                let interest = conn.interest();
                if interest == conn.events {
                    return;
                }
                conn.events = interest;
                interest
            }
            None => return,
        };
        if epoll_ctl(self.fds.epoll, libc::EPOLL_CTL_MOD, fd, changed, fd as u64).is_err() {
            // A descriptor that cannot be modified is unusable.
            self.close(fd);
        }
    }

    /// Removes a connection from the epoll set and closes it.
    fn close(&mut self, fd: RawFd) {
        if !self.conns.contains_key(&fd) {
            return;
        }
        let _ = epoll_ctl(self.fds.epoll, libc::EPOLL_CTL_DEL, fd, 0, fd as u64);
        self.conns.remove(&fd);
        self.sync_listener();
    }
}

/// Applies one readiness event to a connection; returns `true` when it died.
fn handle_event(conn: &mut Conn, flags: u32) -> bool {
    if flags & libc::EPOLLERR as u32 != 0 {
        return true;
    }
    // Read first: a client may send its request and half-close in one go, in
    // which case EPOLLIN and EPOLLRDHUP are reported together and the data is
    // still buffered in the kernel.
    if flags & libc::EPOLLIN as u32 != 0 && conn.wants_read() {
        match fill(conn) {
            Ok(true) => {}
            Ok(false) => conn.peer_closed = true,
            Err(_) => return true,
        }
    }
    if flags & (libc::EPOLLRDHUP as u32 | libc::EPOLLHUP as u32) != 0 {
        conn.peer_closed = true;
    }
    false
}

/// Reads everything currently available into the connection buffer.
///
/// Returns `Ok(false)` on orderly EOF. A `WouldBlock` simply means the socket
/// has no more bytes right now and is not an error.
fn fill(conn: &mut Conn) -> io::Result<bool> {
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        match conn.stream.read(&mut chunk) {
            Ok(0) => return Ok(false),
            Ok(read) => {
                conn.buffer.extend_from_slice(&chunk[..read]);
                if conn.buffer.len() > MAX_HEADER_BYTES + MAX_BODY_BYTES {
                    return Err(bad_request("request is too large"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Writes as much of the queued response as the socket accepts.
///
/// Returns `Ok(true)` when the whole response was flushed, `Ok(false)` when
/// the socket would block with bytes still queued.
fn flush_out(conn: &mut Conn) -> io::Result<bool> {
    while conn.out_pos < conn.out.len() {
        match conn.stream.write(&conn.out[conn.out_pos..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "write returned zero",
                ))
            }
            Ok(written) => conn.out_pos += written,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    conn.out.clear();
    conn.out_pos = 0;
    Ok(true)
}

/// Advances one connection as far as it can go without blocking.
///
/// Static responses are queued here, a dynamic request is dispatched, and
/// queued output is flushed. The loop stops when the socket would block, when
/// a job is in flight, or when the connection is done.
fn pump_conn(conn: &mut Conn, config: &Config, shared: &Shared, fd: RawFd) -> bool {
    loop {
        if conn.dead || conn.pending {
            break;
        }
        if !conn.out.is_empty() {
            match flush_out(conn) {
                Ok(true) => {}
                Ok(false) => break,
                Err(_) => {
                    conn.dead = true;
                    break;
                }
            }
            if conn.close_after_write {
                conn.dead = true;
                break;
            }
            // Fully flushed: pipelined requests may now be served.
            continue;
        }
        if conn.close_after_write {
            conn.dead = true;
            break;
        }
        // No queued output: serve exactly one buffered request, if complete.
        let consumed = serve_one(conn, config, shared, fd);
        if conn.dead || conn.pending || !conn.out.is_empty() {
            continue;
        }
        if consumed {
            // The response was empty, so there is no output to flush before
            // the next pipelined request can be served.
            continue;
        }
        // The buffer holds only a partial request; a peer that stopped sending
        // can never complete it, so the connection is closed.
        if conn.peer_closed {
            conn.dead = true;
        }
        break;
    }
    conn.dead
}

/// Serves at most one request from the connection buffer.
///
/// A static route queues its pre-built response; anything else becomes a
/// dynamic job and marks the connection pending so no further bytes are read
/// until the answer arrives. Returns `true` when a request was consumed, even
/// if its response happened to be empty.
fn serve_one(conn: &mut Conn, config: &Config, shared: &Shared, fd: RawFd) -> bool {
    let request = match take_request(&mut conn.buffer) {
        Ok(Some(request)) => request,
        Ok(None) => return false,
        Err(_) => {
            conn.dead = true;
            return false;
        }
    };
    if let Some(response) = config.route(&request.method, &request.path) {
        // No Python call on the static path; the response bytes are already
        // the complete HTTP message.
        conn.out.extend_from_slice(response);
        conn.out_pos = 0;
        if !request.keep_alive {
            conn.close_after_write = true;
        }
        return true;
    }
    shared.dispatch(Job {
        fd,
        id: conn.id,
        method: request.method,
        path: request.path,
        body: request.body,
        keep_alive: request.keep_alive,
    });
    conn.pending = true;
    true
}

/// Calls `handler(method, path, body)` and returns the response bytes.
///
/// The arguments and the result are rooted for the duration of the call and
/// the copy because they live in Rust-side buffers the collector cannot scan.
/// A pending Python exception is consumed here: the worker has no Python
/// caller to propagate it to, and leaving it set would poison later requests
/// on the same thread.
fn call_handler(handler: i64, method: &[u8], path: &[u8], body: &[u8]) -> Option<Vec<u8>> {
    // One marker captures the root-list length; every value created below is
    // pushed onto that list before the next allocation can run a collection.
    // These values live in Rust locals the collector need not treat as precise
    // frames.
    let mark = heap::root_values(&[]);
    let method_value = value::bytes_new(method.to_vec());
    heap::push_scoped_root(method_value);
    let path_value = value::bytes_new(path.to_vec());
    heap::push_scoped_root(path_value);
    let body_value = value::bytes_new(body.to_vec());
    heap::push_scoped_root(body_value);

    let argv = [method_value, path_value, body_value];
    let result = crate::builtins::call(handler, argv.len(), argv.as_ptr());
    // Same rule for the result: root it before reading its payload.
    heap::push_scoped_root(result);
    let response = if exc::pending() {
        let _ = exc::clear();
        None
    } else {
        value::as_bytes(result).map(|bytes| bytes.to_vec())
    };
    heap::release_roots(mark);
    response
}

/// Extracts one complete request from `buffer`, if one is present.
///
/// The consumed request bytes are removed from the buffer; leftover bytes
/// belong to the next pipelined request. Returns `Ok(None)` when the buffer
/// only holds a partial head or body, and an error for a malformed request or
/// an over-long head.
fn take_request(buffer: &mut Vec<u8>) -> io::Result<Option<Request>> {
    let Some(header_end) = find_header_end(buffer) else {
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(bad_request("request head is too large"));
        }
        return Ok(None);
    };
    if header_end > MAX_HEADER_BYTES {
        return Err(bad_request("request head is too large"));
    }
    let head = parse_head(&buffer[..header_end])?;
    let body_start = header_end + 4;
    let total = body_start + head.body_len;
    if buffer.len() < total {
        return Ok(None);
    }
    let body = buffer[body_start..total].to_vec();
    buffer.drain(..total);
    Ok(Some(Request {
        method: head.method,
        path: head.path,
        body,
        keep_alive: head.keep_alive,
    }))
}

/// Position of the `\r\n\r\n` that terminates the request head.
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn bad_request(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Parses the request line and the headers we need.
fn parse_head(head: &[u8]) -> io::Result<Head> {
    let mut lines = head.split(|&byte| byte == b'\n');
    let request_line = trim_end_cr(lines.next().unwrap_or(&[]));
    let mut parts = request_line.split(|&byte| byte == b' ');
    let method = parts.next().filter(|part| !part.is_empty());
    let target = parts.next();
    let version = parts.next();
    let (Some(method), Some(target), Some(version)) = (method, target, version) else {
        return Err(bad_request("malformed request line"));
    };
    if parts.next().is_some() {
        return Err(bad_request("malformed request line"));
    }
    if !version.starts_with(b"HTTP/1.") {
        return Err(bad_request("unsupported HTTP version"));
    }

    // HTTP/1.1 is keep-alive by default; HTTP/1.0 closes unless the client
    // asks otherwise.
    let mut keep_alive = version != (b"HTTP/1.0" as &[u8]);
    let mut body_len = 0usize;
    for raw in lines {
        let line = trim_end_cr(raw);
        if line.is_empty() {
            continue;
        }
        let Some(colon) = line.iter().position(|&byte| byte == b':') else {
            return Err(bad_request("malformed header line"));
        };
        let name = trim_ascii(&line[..colon]);
        let value = trim_ascii(&line[colon + 1..]);
        if name.eq_ignore_ascii_case(b"content-length") {
            let text =
                std::str::from_utf8(value).map_err(|_| bad_request("invalid Content-Length"))?;
            body_len = text
                .trim()
                .parse::<usize>()
                .map_err(|_| bad_request("invalid Content-Length"))?;
            if body_len > MAX_BODY_BYTES {
                return Err(bad_request("request body is too large"));
            }
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            if contains_token(value, b"chunked") {
                return Err(bad_request("chunked request bodies are not supported"));
            }
        } else if name.eq_ignore_ascii_case(b"connection") {
            if contains_token(value, b"close") {
                keep_alive = false;
            } else if contains_token(value, b"keep-alive") {
                keep_alive = true;
            }
        }
    }

    let path = match target.iter().position(|&byte| byte == b'?') {
        Some(index) => &target[..index],
        None => target,
    };

    Ok(Head {
        method: method.to_vec(),
        path: path.to_vec(),
        body_len,
        keep_alive,
    })
}

/// Whether `value` is a comma-separated list containing `token` (case
/// insensitive, surrounding whitespace ignored).
fn contains_token(value: &[u8], token: &[u8]) -> bool {
    value
        .split(|&byte| byte == b',')
        .any(|part| trim_ascii(part).eq_ignore_ascii_case(token))
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

fn trim_end_cr(bytes: &[u8]) -> &[u8] {
    match bytes.split_last() {
        Some((&b'\r', rest)) => rest,
        _ => bytes,
    }
}

/// Mirrors the callable shapes accepted by [`crate::builtins::call`].
fn is_callable(candidate: i64) -> bool {
    value::as_function(candidate).is_some()
        || value::as_class(candidate).is_some()
        || value::as_ext_function(candidate).is_some()
        || value::as_ext_object(candidate).is_some()
        || value::as_instance(candidate)
            .is_some_and(|instance| value::class_lookup(instance.class, "__call__").is_some())
}

// -------------------------------------------------------------------------
// `http.client`
// -------------------------------------------------------------------------
//
// The client surface mirrors `Lib/http/client.py`. Hydro's native `socket`
// exposes `recv`/`sendall` but no `makefile`, so `HTTPResponse` reads through
// a real file object when the socket provides one (the test suite's fakes and
// `io.BytesIO`) and buffers `recv` bytes itself otherwise. Every class is a
// cached native class value, so `isinstance`, subclassing and identity all
// behave like the Python declarations.

/// `_MAXLINE`: the longest status/header/chunk line accepted.
const CLIENT_MAXLINE: i64 = 65536;
/// `_MAXHEADERS`: response headers (and trailers) accepted.
const CLIENT_MAXHEADERS: usize = 100;
/// `_MAXINTERIMRESPONSES`: 1xx responses skipped before the final one.
const CLIENT_MAXINTERIM: usize = 100;

/// Connection states, spelled like `_CS_IDLE` and friends.
const CS_IDLE: &str = "Idle";
const CS_REQ_STARTED: &str = "Request-started";
const CS_REQ_SENT: &str = "Request-sent";

/// `(CONSTANT, code, phrase)` for every `HTTPStatus` member.
const STATUS_CODES: &[(&str, i64, &str)] = &[
    ("CONTINUE", 100, "Continue"),
    ("SWITCHING_PROTOCOLS", 101, "Switching Protocols"),
    ("PROCESSING", 102, "Processing"),
    ("EARLY_HINTS", 103, "Early Hints"),
    ("OK", 200, "OK"),
    ("CREATED", 201, "Created"),
    ("ACCEPTED", 202, "Accepted"),
    (
        "NON_AUTHORITATIVE_INFORMATION",
        203,
        "Non Authoritative Information",
    ),
    ("NO_CONTENT", 204, "No Content"),
    ("RESET_CONTENT", 205, "Reset Content"),
    ("PARTIAL_CONTENT", 206, "Partial Content"),
    ("MULTI_STATUS", 207, "Multi-Status"),
    ("ALREADY_REPORTED", 208, "Already Reported"),
    ("IM_USED", 226, "IM Used"),
    ("MULTIPLE_CHOICES", 300, "Multiple Choices"),
    ("MOVED_PERMANENTLY", 301, "Moved Permanently"),
    ("FOUND", 302, "Found"),
    ("SEE_OTHER", 303, "See Other"),
    ("NOT_MODIFIED", 304, "Not Modified"),
    ("USE_PROXY", 305, "Use Proxy"),
    ("TEMPORARY_REDIRECT", 307, "Temporary Redirect"),
    ("PERMANENT_REDIRECT", 308, "Permanent Redirect"),
    ("BAD_REQUEST", 400, "Bad Request"),
    ("UNAUTHORIZED", 401, "Unauthorized"),
    ("PAYMENT_REQUIRED", 402, "Payment Required"),
    ("FORBIDDEN", 403, "Forbidden"),
    ("NOT_FOUND", 404, "Not Found"),
    ("METHOD_NOT_ALLOWED", 405, "Method Not Allowed"),
    ("NOT_ACCEPTABLE", 406, "Not Acceptable"),
    (
        "PROXY_AUTHENTICATION_REQUIRED",
        407,
        "Proxy Authentication Required",
    ),
    ("REQUEST_TIMEOUT", 408, "Request Timeout"),
    ("CONFLICT", 409, "Conflict"),
    ("GONE", 410, "Gone"),
    ("LENGTH_REQUIRED", 411, "Length Required"),
    ("PRECONDITION_FAILED", 412, "Precondition Failed"),
    ("REQUEST_ENTITY_TOO_LARGE", 413, "Request Entity Too Large"),
    ("REQUEST_URI_TOO_LONG", 414, "Request-URI Too Long"),
    ("UNSUPPORTED_MEDIA_TYPE", 415, "Unsupported Media Type"),
    (
        "REQUESTED_RANGE_NOT_SATISFIABLE",
        416,
        "Requested Range Not Satisfiable",
    ),
    ("EXPECTATION_FAILED", 417, "Expectation Failed"),
    ("IM_A_TEAPOT", 418, "I'm a Teapot"),
    ("MISDIRECTED_REQUEST", 421, "Misdirected Request"),
    ("UNPROCESSABLE_ENTITY", 422, "Unprocessable Entity"),
    ("LOCKED", 423, "Locked"),
    ("FAILED_DEPENDENCY", 424, "Failed Dependency"),
    ("TOO_EARLY", 425, "Too Early"),
    ("UPGRADE_REQUIRED", 426, "Upgrade Required"),
    ("PRECONDITION_REQUIRED", 428, "Precondition Required"),
    ("TOO_MANY_REQUESTS", 429, "Too Many Requests"),
    (
        "REQUEST_HEADER_FIELDS_TOO_LARGE",
        431,
        "Request Header Fields Too Large",
    ),
    (
        "UNAVAILABLE_FOR_LEGAL_REASONS",
        451,
        "Unavailable For Legal Reasons",
    ),
    ("INTERNAL_SERVER_ERROR", 500, "Internal Server Error"),
    ("NOT_IMPLEMENTED", 501, "Not Implemented"),
    ("BAD_GATEWAY", 502, "Bad Gateway"),
    ("SERVICE_UNAVAILABLE", 503, "Service Unavailable"),
    ("GATEWAY_TIMEOUT", 504, "Gateway Timeout"),
    (
        "HTTP_VERSION_NOT_SUPPORTED",
        505,
        "HTTP Version Not Supported",
    ),
    ("VARIANT_ALSO_NEGOTIATES", 506, "Variant Also Negotiates"),
    ("INSUFFICIENT_STORAGE", 507, "Insufficient Storage"),
    ("LOOP_DETECTED", 508, "Loop Detected"),
    ("NOT_EXTENDED", 510, "Not Extended"),
    (
        "NETWORK_AUTHENTICATION_REQUIRED",
        511,
        "Network Authentication Required",
    ),
];

fn client_text(content: &str) -> i64 {
    value::str_alloc(content.to_string())
}

fn client_integer(number: i64) -> i64 {
    value::encode_int(number).unwrap_or(NONE)
}

fn client_bool(flag: bool) -> i64 {
    value::bool_value(flag)
}

fn client_type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Instance attribute, then the class attribute the instance inherits.
fn client_attribute(instance: i64, name: &str) -> i64 {
    if let Some(item) = value::instance_attr(instance, name) {
        return item;
    }
    let Some(class) = value::instance_class_value(instance) else {
        return NONE;
    };
    match value::class_lookup(class, name) {
        Some(value::ClassEntry::Attr(item)) => item,
        _ => NONE,
    }
}

fn client_set(instance: i64, name: &str, item: i64) {
    value::instance_setattr(instance, name.to_string(), item);
}

/// Calls `obj.name(args...)` with the receiver and every argument rooted.
fn client_invoke(obj: i64, name: &str, args: &[i64]) -> i64 {
    let mut values: Vec<i64> = Vec::with_capacity(args.len() + 1);
    values.push(obj);
    values.extend_from_slice(args);
    let roots = heap::root_values(&values);
    let result = crate::methods::call(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr());
    heap::release_roots(roots);
    result
}

/// Whether `obj` exposes a callable attribute `name`.
fn client_has(obj: i64, name: &str) -> bool {
    if value::instance_attr(obj, name).is_some() {
        return true;
    }
    if let Some(class) = value::instance_class_value(obj) {
        if value::class_lookup(class, name).is_some() {
            return true;
        }
    }
    // Native streams (`io.BytesIO`, open files) are not instances: their
    // surface is the fixed `io` method set.
    if value::as_file(obj).is_some() {
        return matches!(
            name,
            "read"
                | "readline"
                | "readlines"
                | "readinto"
                | "read1"
                | "peek"
                | "close"
                | "flush"
                | "fileno"
                | "seek"
                | "tell"
                | "write"
        );
    }
    false
}

fn client_param(name: &str, kind: hydro_ir::ParamKind, default: Option<i64>) -> value::ParamMeta {
    value::ParamMeta {
        name: name.to_string(),
        kind,
        default,
    }
}

fn client_positional(name: &str) -> value::ParamMeta {
    client_param(name, hydro_ir::ParamKind::Positional, None)
}

fn client_optional(name: &str, default: i64) -> value::ParamMeta {
    client_param(name, hydro_ir::ParamKind::Positional, Some(default))
}

/// Builds a native method value whose trampoline is `entry`.
fn client_method(
    name: &'static str,
    entry: extern "C" fn(*const i64) -> i64,
    params: Vec<value::ParamMeta>,
) -> i64 {
    let arity = if params.iter().any(|item| {
        matches!(
            item.kind,
            hydro_ir::ParamKind::VarArg | hydro_ir::ParamKind::KwArg
        )
    }) {
        usize::MAX
    } else {
        params.len()
    };
    let function = value::native_function_new("http", name, arity);
    value::function_set_signature(function, params);
    value::function_set_full_entry(function, entry as *const () as usize);
    function
}

type ClientDefinition = (
    &'static str,
    Vec<value::ParamMeta>,
    extern "C" fn(*const i64) -> i64,
);

/// Builds (or returns) a cached class with `__module__` set to `http.client`.
fn client_class(
    key: &str,
    name: &str,
    base: i64,
    definitions: Vec<ClientDefinition>,
    attrs: Vec<(&str, i64)>,
) -> i64 {
    if let Some(cached) = crate::core::globals::registry_get(key) {
        return cached;
    }
    let mut methods: Vec<(String, i64, hydro_ir::MethodKind)> =
        Vec::with_capacity(definitions.len());
    let mut functions: Vec<i64> = Vec::with_capacity(definitions.len());
    for (method_name, params, entry) in definitions {
        let function = client_method(method_name, entry, params);
        functions.push(function);
        methods.push((
            method_name.to_string(),
            function,
            hydro_ir::MethodKind::Normal,
        ));
    }
    let module = client_text("http.client");
    let mut class_attrs: Vec<(String, i64)> = attrs
        .iter()
        .map(|(attr_name, item)| (attr_name.to_string(), *item))
        .collect();
    class_attrs.push(("__module__".to_string(), module));
    let mut roots: Vec<i64> = functions.clone();
    roots.push(module);
    roots.extend(attrs.iter().map(|(_, item)| *item));
    let root = heap::root_values(&roots);
    let class = value::class_new_full(name.to_string(), base, methods, class_attrs);
    if class != NONE {
        crate::core::globals::registry_set(key, class);
    }
    heap::release_roots(root);
    class
}

fn client_exception_subclass(key: &str, name: &str, base: i64) -> i64 {
    if let Some(cached) = crate::core::globals::registry_get(key) {
        return cached;
    }
    let module = client_text("http.client");
    let root = heap::root_values(&[base, module]);
    let class = value::class_new_full(
        name.to_string(),
        base,
        Vec::new(),
        vec![("__module__".to_string(), module)],
    );
    if class != NONE {
        crate::core::globals::registry_set(key, class);
    }
    heap::release_roots(root);
    class
}

fn invalid_url_class() -> i64 {
    client_exception_subclass(
        "http.client.InvalidURL",
        "InvalidURL",
        http_exception_class(),
    )
}

fn not_connected_class() -> i64 {
    client_exception_subclass(
        "http.client.NotConnected",
        "NotConnected",
        http_exception_class(),
    )
}

fn unknown_protocol_class() -> i64 {
    client_exception_subclass(
        "http.client.UnknownProtocol",
        "UnknownProtocol",
        http_exception_class(),
    )
}

fn unknown_transfer_encoding_class() -> i64 {
    client_exception_subclass(
        "http.client.UnknownTransferEncoding",
        "UnknownTransferEncoding",
        http_exception_class(),
    )
}

fn unimplemented_file_mode_class() -> i64 {
    client_exception_subclass(
        "http.client.UnimplementedFileMode",
        "UnimplementedFileMode",
        http_exception_class(),
    )
}

fn improper_connection_state_class() -> i64 {
    client_exception_subclass(
        "http.client.ImproperConnectionState",
        "ImproperConnectionState",
        http_exception_class(),
    )
}

fn cannot_send_request_class() -> i64 {
    client_exception_subclass(
        "http.client.CannotSendRequest",
        "CannotSendRequest",
        improper_connection_state_class(),
    )
}

fn cannot_send_header_class() -> i64 {
    client_exception_subclass(
        "http.client.CannotSendHeader",
        "CannotSendHeader",
        improper_connection_state_class(),
    )
}

fn response_not_ready_class() -> i64 {
    client_exception_subclass(
        "http.client.ResponseNotReady",
        "ResponseNotReady",
        improper_connection_state_class(),
    )
}

fn bad_status_line_class() -> i64 {
    client_exception_subclass(
        "http.client.BadStatusLine",
        "BadStatusLine",
        http_exception_class(),
    )
}

fn line_too_long_class() -> i64 {
    client_exception_subclass(
        "http.client.LineTooLong",
        "LineTooLong",
        http_exception_class(),
    )
}

fn incomplete_read_class() -> i64 {
    client_exception_subclass(
        "http.client.IncompleteRead",
        "IncompleteRead",
        http_exception_class(),
    )
}

/// Builds an exception instance of `class` carrying the pre-rendered
/// `repr`/`str` text CPython would produce.
///
/// `values` keeps the original tagged arguments for `.args`.
fn client_exception_instance(class: i64, tag: u32, display: &str, values: Vec<i64>) -> i64 {
    // The tagged arguments and the class only live on the Rust stack while
    // the instance is allocated, so they are registered as roots first.
    let mut roots: Vec<i64> = values.clone();
    roots.push(class);
    let root = heap::root_values(&roots);
    let data = crate::exc::ExcData {
        tag,
        name: crate::exc::tag_name(tag),
        msg: display.to_string(),
        file: String::new(),
        line: 0,
        frame: String::new(),
        cause: None,
        context: None,
        suppress_context: false,
        exit_code: None,
        args: vec![display.to_string()],
        arg_values: values,
        arg_reprs: vec![display.to_string()],
        keywords: Vec::new(),
        object: None,
    };
    let instance = value::exc_instance_new(class, data);
    heap::release_roots(root);
    instance
}

fn client_raise(class: i64, display: &str) -> i64 {
    let instance = client_exception_instance(class, EXCEPTION, display, Vec::new());
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

/// Raises `class(message)` with `repr`/`str` of the message quoted once.
fn client_raise_message(class: i64, message: &str) -> i64 {
    let argument = client_text(message);
    let root = heap::root_values(std::slice::from_ref(&argument));
    let display = value::repr(argument);
    let instance = client_exception_instance(class, EXCEPTION, &display, vec![argument]);
    heap::release_roots(root);
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

fn raise_bad_status_line(line: &str) -> i64 {
    let argument = client_text(line);
    let root = heap::root_values(std::slice::from_ref(&argument));
    let line_text = if line.is_empty() {
        client_text(&value::repr(argument))
    } else {
        argument
    };
    let line_root = heap::root_values(std::slice::from_ref(&line_text));
    let display = value::repr(line_text);
    let instance = client_exception_instance(
        bad_status_line_class(),
        EXCEPTION,
        &display,
        vec![line_text],
    );
    heap::release_roots(line_root);
    heap::release_roots(root);
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

fn raise_line_too_long(line_type: &str) -> i64 {
    client_raise_message(
        line_too_long_class(),
        &format!("got more than {CLIENT_MAXLINE} bytes when reading {line_type}"),
    )
}

fn raise_incomplete_read(partial: &[u8], expected: Option<usize>) -> i64 {
    let display = match expected {
        Some(missing) => format!(
            "IncompleteRead({} bytes read, {} more expected)",
            partial.len(),
            missing
        ),
        None => format!("IncompleteRead({} bytes read)", partial.len()),
    };
    let partial_value = value::bytes_new(partial.to_vec());
    let instance = client_exception_instance(
        incomplete_read_class(),
        EXCEPTION,
        &display,
        vec![partial_value],
    );
    if exc::pending() {
        return NONE;
    }
    let expected_value = match expected {
        Some(missing) => client_integer(missing as i64),
        None => NONE,
    };
    client_set(instance, "partial", partial_value);
    client_set(instance, "expected", expected_value);
    exc::raise_value(instance)
}

fn raise_response_not_ready(state: i64) -> i64 {
    let class = response_not_ready_class();
    let display = value::repr(state);
    let instance = client_exception_instance(class, EXCEPTION, &display, vec![state]);
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

fn raise_cannot_send_request(state: i64) -> i64 {
    let class = cannot_send_request_class();
    let display = value::repr(state);
    let instance = client_exception_instance(class, EXCEPTION, &display, vec![state]);
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

fn raise_cannot_send_header() -> i64 {
    client_raise(cannot_send_header_class(), "CannotSendHeader()")
}

fn raise_not_connected() -> i64 {
    client_raise(not_connected_class(), "NotConnected()")
}

fn raise_remote_disconnected(message: &str) -> i64 {
    let class = remote_disconnected_class();
    let argument = client_text(message);
    let display = value::repr(argument);
    let instance =
        client_exception_instance(class, CONNECTION_RESET_ERROR, &display, vec![argument]);
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

fn status_constant(name: &str) -> Option<i64> {
    STATUS_CODES
        .iter()
        .find(|(constant, _, _)| *constant == name)
        .map(|(_, code, _)| *code)
}

fn responses_dict() -> i64 {
    const KEY: &str = "http.client.responses";
    if let Some(cached) = crate::core::globals::registry_get(KEY) {
        return cached;
    }
    let mut roots: Vec<i64> = Vec::with_capacity(STATUS_CODES.len());
    let mut pairs: Vec<(i64, i64)> = Vec::with_capacity(STATUS_CODES.len());
    for (_, code, phrase) in STATUS_CODES {
        let phrase_value = client_text(phrase);
        roots.push(phrase_value);
        pairs.push((client_integer(*code), phrase_value));
    }
    let root = heap::root_values(&roots);
    let mapping = value::dict_new(pairs);
    crate::core::globals::registry_set(KEY, mapping);
    heap::release_roots(root);
    mapping
}

fn client_all() -> i64 {
    const NAMES: &[&str] = &[
        "HTTPResponse",
        "HTTPConnection",
        "HTTPException",
        "NotConnected",
        "UnknownProtocol",
        "UnknownTransferEncoding",
        "UnimplementedFileMode",
        "IncompleteRead",
        "InvalidURL",
        "ImproperConnectionState",
        "CannotSendRequest",
        "CannotSendHeader",
        "ResponseNotReady",
        "BadStatusLine",
        "LineTooLong",
        "RemoteDisconnected",
        "error",
        "responses",
        "HTTPSConnection",
    ];
    let mut values = Vec::with_capacity(NAMES.len());
    for name in NAMES {
        values.push(client_text(name));
    }
    let root = heap::root_values(&values);
    let list = value::list_new(values);
    heap::release_roots(root);
    list
}

/// latin-1 decoding, the encoding of HTTP status lines and headers.
fn latin1_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| *byte as char).collect()
}

/// latin-1 encoding; a code point above 0xFF raises `UnicodeEncodeError`.
fn latin1_bytes(content: &str, name: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(content.len());
    for character in content.chars() {
        let code = character as u32;
        if code > 0xFF {
            let message = format!(
                "'latin-1' codec can't encode character in position: {name} is not valid Latin-1"
            );
            exc::raise_with_message(exc::UNICODE_ENCODE_ERROR, &message);
            return None;
        }
        out.push(code as u8);
    }
    Some(out)
}

fn split_whitespace_max(content: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = content.trim_start();
    while !rest.is_empty() && out.len() + 1 < limit {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        out.push(rest[..end].to_string());
        rest = rest[end..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

// -------------------------------------------------------------------------
// `HTTPMessage`: the case-insensitive header container
// -------------------------------------------------------------------------

/// Instance header store: `_names`/`_values` are parallel lists.
extern "C" fn message_init_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    let names = value::list_new(Vec::new());
    let values = value::list_new(Vec::new());
    let inner = heap::root_values(&[names, values]);
    client_set(instance, "_names", names);
    client_set(instance, "_values", values);
    heap::release_roots(inner);
    heap::release_roots(root);
    NONE
}

/// Header names are compared case-insensitively; `bytes` names decode as
/// latin-1, matching `email.message.Message`.
fn header_key(item: i64) -> Option<String> {
    match value::as_str(item) {
        Some(name) => Some(name.to_string()),
        None => value::as_bytes(item).map(latin1_string),
    }
}

fn message_matches(name: i64, wanted: &str) -> bool {
    header_key(name).is_some_and(|name| name.eq_ignore_ascii_case(wanted))
}

fn message_pair(msg: i64, index: usize) -> (i64, i64) {
    let names = client_attribute(msg, "_names");
    let values = client_attribute(msg, "_values");
    let name = value::as_list(names)
        .and_then(|items| items.get(index).copied())
        .unwrap_or(NONE);
    let value = value::as_list(values)
        .and_then(|items| items.get(index).copied())
        .unwrap_or(NONE);
    (name, value)
}

fn message_len(msg: i64) -> usize {
    let names = client_attribute(msg, "_names");
    value::as_list(names).map(|items| items.len()).unwrap_or(0)
}

fn message_find(msg: i64, key: &str) -> Option<usize> {
    for index in 0..message_len(msg) {
        let (name, _) = message_pair(msg, index);
        if message_matches(name, key) {
            return Some(index);
        }
    }
    None
}

fn message_get_first(msg: i64, key: &str) -> Option<i64> {
    message_find(msg, key).map(|index| message_pair(msg, index).1)
}

fn message_get_all_values(msg: i64, key: &str) -> Option<Vec<i64>> {
    let mut out = Vec::new();
    for index in 0..message_len(msg) {
        let (name, item) = message_pair(msg, index);
        if message_matches(name, key) {
            out.push(item);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn message_get_text(msg: i64, key: &str) -> Option<String> {
    message_get_first(msg, key).and_then(|item| {
        value::as_str(item)
            .map(str::to_string)
            .or_else(|| value::as_bytes(item).map(latin1_string))
    })
}

fn message_push(msg: i64, name: i64, item: i64) {
    let names = client_attribute(msg, "_names");
    let values = client_attribute(msg, "_values");
    let root = heap::root_values(&[names, values, name, item]);
    crate::container::list_append(names, name);
    crate::container::list_append(values, item);
    heap::release_roots(root);
}

extern "C" fn message_getitem_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, name).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let instance = args[0];
    let root = heap::root_values(&[instance, args[1]]);
    let Some(key) = header_key(args[1]) else {
        heap::release_roots(root);
        return client_type_error("header name must be str");
    };
    let result = message_get_first(instance, &key).unwrap_or(NONE);
    if result == NONE {
        let class = exc::exception_class(exc::KEY_ERROR);
        let display = value::repr(args[1]);
        let raised = client_exception_instance(class, exc::KEY_ERROR, &display, vec![args[1]]);
        heap::release_roots(root);
        return exc::raise_value(raised);
    }
    heap::release_roots(root);
    result
}

extern "C" fn message_get_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, name, failobj).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let Some(key) = header_key(args[1]) else {
        return client_type_error("header name must be str");
    };
    message_get_first(args[0], &key).unwrap_or(args[2])
}

/// Shared implementation of `get`/`getheader`: first value or `failobj`.
extern "C" fn message_getheader_entry(argv: *const i64) -> i64 {
    message_get_entry(argv)
}

extern "C" fn message_get_all_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, name, failobj).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let root = heap::root_values(&[instance, args[1], args[2]]);
    let Some(key) = header_key(args[1]) else {
        heap::release_roots(root);
        return client_type_error("header name must be str");
    };
    let Some(items) = message_get_all_values(instance, &key) else {
        heap::release_roots(root);
        return args[2];
    };
    let list = value::list_new(items);
    heap::release_roots(root);
    list
}

/// Rewrites one `_values` entry (the native list surface has no
/// `__setitem__` dispatch, so the list is rebuilt in place).
fn message_store_value(msg: i64, index: usize, item: i64) {
    let values = client_attribute(msg, "_values");
    let Some(snapshot) = value::as_list(values).map(|items| items.to_vec()) else {
        return;
    };
    let mut roots: Vec<i64> = snapshot.clone();
    roots.push(item);
    roots.push(values);
    let root = heap::root_values(&roots);
    client_invoke(values, "clear", &[]);
    for (position, existing) in snapshot.iter().enumerate() {
        let selected = if position == index { item } else { *existing };
        client_invoke(values, "append", &[selected]);
    }
    heap::release_roots(root);
}

/// Removes the given indexes from `_names`/`_values`.
fn message_remove_indexes(msg: i64, indexes: &[usize]) {
    for target in [
        client_attribute(msg, "_names"),
        client_attribute(msg, "_values"),
    ] {
        let Some(snapshot) = value::as_list(target).map(|items| items.to_vec()) else {
            continue;
        };
        let mut roots: Vec<i64> = snapshot.clone();
        roots.push(target);
        let root = heap::root_values(&roots);
        client_invoke(target, "clear", &[]);
        for (position, existing) in snapshot.iter().enumerate() {
            if !indexes.contains(&position) {
                client_invoke(target, "append", &[*existing]);
            }
        }
        heap::release_roots(root);
    }
}

extern "C" fn message_setitem_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, name, value).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let root = heap::root_values(&[instance, args[1], args[2]]);
    let Some(key) = header_key(args[1]) else {
        heap::release_roots(root);
        return client_type_error("header name must be str");
    };
    match message_find(instance, &key) {
        Some(index) => message_store_value(instance, index, args[2]),
        None => message_push(instance, args[1], args[2]),
    }
    heap::release_roots(root);
    NONE
}

extern "C" fn message_delitem_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, name).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let instance = args[0];
    let root = heap::root_values(&[instance, args[1]]);
    let Some(key) = header_key(args[1]) else {
        heap::release_roots(root);
        return client_type_error("header name must be str");
    };
    let mut indexes: Vec<usize> = Vec::new();
    for index in 0..message_len(instance) {
        let (name, _) = message_pair(instance, index);
        if message_matches(name, &key) {
            indexes.push(index);
        }
    }
    if indexes.is_empty() {
        let class = exc::exception_class(exc::KEY_ERROR);
        let display = value::repr(args[1]);
        let raised = client_exception_instance(class, exc::KEY_ERROR, &display, vec![args[1]]);
        heap::release_roots(root);
        return exc::raise_value(raised);
    }
    message_remove_indexes(instance, &indexes);
    heap::release_roots(root);
    NONE
}

extern "C" fn message_contains_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, name).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let Some(key) = header_key(args[1]) else {
        return client_bool(false);
    };
    client_bool(message_find(args[0], &key).is_some())
}

extern "C" fn message_len_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    client_integer(message_len(instance) as i64)
}

extern "C" fn message_keys_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    let mut items = Vec::with_capacity(message_len(instance));
    for index in 0..message_len(instance) {
        items.push(message_pair(instance, index).0);
    }
    let list = value::list_new(items);
    heap::release_roots(root);
    list
}

extern "C" fn message_values_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    let mut items = Vec::with_capacity(message_len(instance));
    for index in 0..message_len(instance) {
        items.push(message_pair(instance, index).1);
    }
    let list = value::list_new(items);
    heap::release_roots(root);
    list
}

extern "C" fn message_items_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    let count = message_len(instance);
    let mut tuples = Vec::with_capacity(count);
    for index in 0..count {
        let (name, item) = message_pair(instance, index);
        let inner = heap::root_values(&[name, item]);
        tuples.push(value::tuple_new(vec![name, item]));
        heap::release_roots(inner);
    }
    let list = value::list_new(tuples);
    heap::release_roots(root);
    list
}

extern "C" fn message_iter_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    let names = client_attribute(instance, "_names");
    let copy = value::as_list(names)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let list = value::list_new(copy);
    let list_root = heap::root_values(std::slice::from_ref(&list));
    let iterator = crate::iter::iter_of(list);
    heap::release_roots(list_root);
    heap::release_roots(root);
    iterator
}

extern "C" fn message_add_header_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, name, value).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let root = heap::root_values(&[args[0], args[1], args[2]]);
    message_push(args[0], args[1], args[2]);
    heap::release_roots(root);
    NONE
}

extern "C" fn message_replace_header_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, name, value).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let root = heap::root_values(&[instance, args[1], args[2]]);
    let Some(key) = header_key(args[1]) else {
        heap::release_roots(root);
        return client_type_error("header name must be str");
    };
    match message_find(instance, &key) {
        Some(index) => {
            message_store_value(instance, index, args[2]);
            heap::release_roots(root);
            NONE
        }
        None => {
            let class = exc::exception_class(exc::KEY_ERROR);
            let display = value::repr(args[1]);
            let raised = client_exception_instance(class, exc::KEY_ERROR, &display, vec![args[1]]);
            heap::release_roots(root);
            exc::raise_value(raised)
        }
    }
}

extern "C" fn message_str_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    let mut out = String::new();
    for index in 0..message_len(instance) {
        let (name, item) = message_pair(instance, index);
        out.push_str(&value::to_string(name));
        out.push_str(": ");
        out.push_str(&value::to_string(item));
        out.push('\n');
    }
    let rendered = client_text(&out);
    heap::release_roots(root);
    rendered
}

extern "C" fn message_repr_entry(_argv: *const i64) -> i64 {
    client_text("<http.client.HTTPMessage object>")
}

fn message_class() -> i64 {
    let self_only = || vec![client_positional("self")];
    client_class(
        "http.client.HTTPMessage",
        "HTTPMessage",
        NONE,
        vec![
            ("__init__", self_only(), message_init_entry),
            (
                "__getitem__",
                vec![client_positional("self"), client_positional("name")],
                message_getitem_entry,
            ),
            (
                "__setitem__",
                vec![
                    client_positional("self"),
                    client_positional("name"),
                    client_positional("value"),
                ],
                message_setitem_entry,
            ),
            (
                "__delitem__",
                vec![client_positional("self"), client_positional("name")],
                message_delitem_entry,
            ),
            (
                "__contains__",
                vec![client_positional("self"), client_positional("name")],
                message_contains_entry,
            ),
            ("__len__", self_only(), message_len_entry),
            ("__iter__", self_only(), message_iter_entry),
            ("__str__", self_only(), message_str_entry),
            ("__repr__", self_only(), message_repr_entry),
            (
                "get",
                vec![
                    client_positional("self"),
                    client_positional("name"),
                    client_optional("failobj", NONE),
                ],
                message_get_entry,
            ),
            (
                "getheader",
                vec![
                    client_positional("self"),
                    client_positional("name"),
                    client_optional("failobj", NONE),
                ],
                message_getheader_entry,
            ),
            (
                "get_all",
                vec![
                    client_positional("self"),
                    client_positional("name"),
                    client_optional("failobj", NONE),
                ],
                message_get_all_entry,
            ),
            ("keys", self_only(), message_keys_entry),
            ("values", self_only(), message_values_entry),
            ("items", self_only(), message_items_entry),
            (
                "add_header",
                vec![
                    client_positional("self"),
                    client_positional("name"),
                    client_positional("value"),
                ],
                message_add_header_entry,
            ),
            (
                "replace_header",
                vec![
                    client_positional("self"),
                    client_positional("name"),
                    client_positional("value"),
                ],
                message_replace_header_entry,
            ),
        ],
        Vec::new(),
    )
}

/// Parses header lines into `(name, value)` pairs, unfolding continuation
/// lines with `\n` the way `email.parser` keeps them.
fn parse_header_lines(lines: &[Vec<u8>]) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for line in lines {
        let text_line = latin1_string(trim_line_end(line));
        if text_line.is_empty() {
            continue;
        }
        let first = text_line.as_bytes().first().copied().unwrap_or(0);
        if first == b' ' || first == b'\t' {
            if let Some(last) = pairs.last_mut() {
                last.1.push('\n');
                last.1.push_str(&text_line);
            }
            continue;
        }
        let Some((name, value_in)) = text_line.split_once(':') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        pairs.push((name.to_string(), value_in.trim().to_string()));
    }
    pairs
}

fn message_from_pairs(pairs: &[(String, String)]) -> i64 {
    let class = message_class();
    let root = heap::root_values(std::slice::from_ref(&class));
    let message = crate::builtins::call(class, 0, std::ptr::null());
    if exc::pending() || message == NONE {
        heap::release_roots(root);
        return message;
    }
    let message_root = heap::root_values(std::slice::from_ref(&message));
    let names = client_attribute(message, "_names");
    let values = client_attribute(message, "_values");
    let list_root = heap::root_values(&[names, values]);
    for (name, value) in pairs {
        let name_value = client_text(name);
        let value_value = client_text(value);
        let inner = heap::root_values(&[name_value, value_value]);
        crate::container::list_append(names, name_value);
        crate::container::list_append(values, value_value);
        heap::release_roots(inner);
    }
    heap::release_roots(list_root);
    heap::release_roots(message_root);
    heap::release_roots(root);
    message
}

fn trim_line_end(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}

/// Reads header lines from any file object, bounded by `_MAXLINE`/`_MAXHEADERS`.
fn collect_header_lines<F>(mut next_line: F) -> Option<Vec<Vec<u8>>>
where
    F: FnMut() -> Option<Vec<u8>>,
{
    let mut lines: Vec<Vec<u8>> = Vec::new();
    loop {
        let line = next_line()?;
        if line.len() as i64 > CLIENT_MAXLINE {
            raise_line_too_long("header line");
            return None;
        }
        let terminator = line.is_empty() || line == b"\r\n" || line == b"\n";
        lines.push(line);
        if terminator {
            break;
        }
        if lines.len() > CLIENT_MAXHEADERS {
            client_raise_message(
                http_exception_class(),
                &format!("got more than {CLIENT_MAXHEADERS} headers"),
            );
            return None;
        }
    }
    Some(lines)
}

/// `http.client.parse_headers(fp)`: parse the head at `fp`'s cursor.
fn parse_headers_from_fp(fp: i64) -> i64 {
    let lines = collect_header_lines(|| {
        let result = client_invoke(fp, "readline", &[client_integer(CLIENT_MAXLINE + 1)]);
        if exc::pending() {
            return None;
        }
        Some(
            value::as_bytes(result)
                .map(|bytes| bytes.to_vec())
                .unwrap_or_default(),
        )
    });
    let Some(lines) = lines else {
        return NONE;
    };
    let pairs = parse_header_lines(&lines);
    message_from_pairs(&pairs)
}

/// Native entry for the module-level `parse_headers(fp, _class=HTTPMessage)`.
extern "C" fn client_parse_headers_entry(argv: *const i64) -> i64 {
    // SAFETY: one argument slot (the file object).
    let fp = unsafe { *argv };
    parse_headers_from_fp(fp)
}

// -------------------------------------------------------------------------
// `HTTPResponse`
// -------------------------------------------------------------------------

/// Reads up to `limit` bytes through the response's file object or socket.
fn response_read_some(instance: i64, amount: usize) -> Option<Vec<u8>> {
    if value::truthy(client_attribute(instance, "raw")) {
        let mut out = response_pending_take(instance);
        if out.len() >= amount {
            let rest = out.split_off(amount);
            response_pending_store(instance, &rest);
            return Some(out);
        }
        let need = amount - out.len();
        let sock = client_attribute(instance, "sock");
        if sock == NONE {
            return Some(out);
        }
        let result = client_invoke(sock, "recv", &[client_integer(need as i64)]);
        if exc::pending() {
            return None;
        }
        out.extend_from_slice(value::as_bytes(result).unwrap_or_default());
        Some(out)
    } else {
        let fp = client_attribute(instance, "fp");
        if fp == NONE {
            return Some(Vec::new());
        }
        let result = client_invoke(fp, "read", &[client_integer(amount as i64)]);
        if exc::pending() {
            return None;
        }
        Some(
            value::as_bytes(result)
                .map(|bytes| bytes.to_vec())
                .unwrap_or_default(),
        )
    }
}

fn response_read_all(instance: i64) -> Option<Vec<u8>> {
    if value::truthy(client_attribute(instance, "raw")) {
        let mut out = response_pending_take(instance);
        loop {
            let sock = client_attribute(instance, "sock");
            if sock == NONE {
                break;
            }
            let result = client_invoke(sock, "recv", &[client_integer(65536)]);
            if exc::pending() {
                return None;
            }
            let chunk = value::as_bytes(result)
                .map(|bytes| bytes.to_vec())
                .unwrap_or_default();
            if chunk.is_empty() {
                break;
            }
            out.extend_from_slice(&chunk);
        }
        Some(out)
    } else {
        let fp = client_attribute(instance, "fp");
        if fp == NONE {
            return Some(Vec::new());
        }
        let result = client_invoke(fp, "read", &[]);
        if exc::pending() {
            return None;
        }
        Some(
            value::as_bytes(result)
                .map(|bytes| bytes.to_vec())
                .unwrap_or_default(),
        )
    }
}

/// Reads exactly `amount` bytes or raises `IncompleteRead`.
fn response_safe_read(instance: i64, amount: usize) -> Option<Vec<u8>> {
    let mut data: Vec<u8> = Vec::with_capacity(amount.min(1 << 20));
    while data.len() < amount {
        let want = (amount - data.len()).min(1 << 20);
        let chunk = response_read_some(instance, want)?;
        if chunk.is_empty() {
            break;
        }
        data.extend_from_slice(&chunk);
    }
    if data.len() < amount {
        let missing = amount - data.len();
        raise_incomplete_read(&data, Some(missing));
        return None;
    }
    Some(data)
}

fn response_pending_view(instance: i64) -> Vec<u8> {
    let pending = client_attribute(instance, "_pending");
    value::as_bytes(pending)
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default()
}

fn response_pending_take(instance: i64) -> Vec<u8> {
    let bytes = response_pending_view(instance);
    if !bytes.is_empty() {
        client_set(instance, "_pending", value::bytes_new(Vec::new()));
    }
    bytes
}

fn response_pending_store(instance: i64, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    client_set(instance, "_pending", value::bytes_new(bytes.to_vec()));
}

/// `readline` against the raw socket or the file object.
fn response_readline_raw(instance: i64, limit: i64) -> Option<Vec<u8>> {
    let mut out = response_pending_take(instance);
    loop {
        if let Some(index) = out.iter().position(|byte| *byte == b'\n') {
            if limit < 0 || (index as i64) < limit {
                let rest = out.split_off(index + 1);
                response_pending_store(instance, &rest);
                return Some(out);
            }
            break;
        }
        if limit >= 0 && out.len() as i64 >= limit {
            break;
        }
        let want = if limit >= 0 {
            (limit - out.len() as i64).clamp(1, 65536)
        } else {
            65536
        };
        let sock = client_attribute(instance, "sock");
        if sock == NONE {
            break;
        }
        let result = client_invoke(sock, "recv", &[client_integer(want)]);
        if exc::pending() {
            return None;
        }
        let chunk = value::as_bytes(result)
            .map(|bytes| bytes.to_vec())
            .unwrap_or_default();
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    if limit >= 0 && out.len() as i64 > limit {
        let rest = out.split_off(limit as usize);
        response_pending_store(instance, &rest);
    }
    Some(out)
}

fn response_readline_any(instance: i64, limit: i64) -> Option<Vec<u8>> {
    if value::truthy(client_attribute(instance, "raw")) {
        return response_readline_raw(instance, limit);
    }
    let fp = client_attribute(instance, "fp");
    if fp == NONE {
        return Some(Vec::new());
    }
    let result = client_invoke(fp, "readline", &[client_integer(limit)]);
    if exc::pending() {
        return None;
    }
    Some(
        value::as_bytes(result)
            .map(|bytes| bytes.to_vec())
            .unwrap_or_default(),
    )
}

/// Fills the pending buffer up to `limit` bytes without consuming them.
fn response_peek_raw(instance: i64, limit: usize) -> Option<Vec<u8>> {
    let mut data = response_pending_view(instance);
    while data.len() < limit {
        let sock = client_attribute(instance, "sock");
        if sock == NONE {
            break;
        }
        let want = (limit - data.len()).min(65536);
        let result = client_invoke(sock, "recv", &[client_integer(want as i64)]);
        if exc::pending() {
            return None;
        }
        let chunk = value::as_bytes(result)
            .map(|bytes| bytes.to_vec())
            .unwrap_or_default();
        if chunk.is_empty() {
            break;
        }
        data.extend_from_slice(&chunk);
    }
    response_pending_store(instance, &data);
    Some(data)
}

fn response_conn_closed(instance: i64) -> bool {
    value::truthy(client_attribute(instance, "_conn_closed"))
}

fn response_close_conn(instance: i64) -> i64 {
    if response_conn_closed(instance) {
        return NONE;
    }
    client_set(instance, "_conn_closed", client_bool(true));
    let fp = client_attribute(instance, "fp");
    if fp != NONE {
        client_set(instance, "fp", NONE);
        client_invoke(fp, "close", &[]);
    }
    // Raw mode has no file object to close: when the connection handed the
    // socket over (close-delimited response), the response owns it now.
    if value::truthy(client_attribute(instance, "_owns_sock")) {
        let sock = client_attribute(instance, "sock");
        if sock != NONE {
            client_set(instance, "sock", NONE);
            let root = heap::root_values(std::slice::from_ref(&sock));
            client_invoke(sock, "close", &[]);
            heap::release_roots(root);
        }
    }
    NONE
}

/// Writes one debug line to `sys.stdout` (honouring `redirect_stdout`).
fn client_debug_line(line: &str) {
    let stdout = super::module_get_by_name("sys", "stdout");
    if stdout == NONE {
        return;
    }
    let payload = client_text(line);
    let root = heap::root_values(&[stdout, payload]);
    client_invoke(stdout, "write", &[payload]);
    heap::release_roots(root);
    if exc::pending() {
        exc::clear();
    }
}

/// `_read_status()` -> `(version, status, reason)`, raising on a bad line.
fn response_read_status(instance: i64) -> Option<(String, i64, String)> {
    let line_bytes = response_readline_any(instance, CLIENT_MAXLINE + 1)?;
    if line_bytes.len() as i64 > CLIENT_MAXLINE {
        raise_line_too_long("status line");
        return None;
    }
    let line = latin1_string(&line_bytes);
    if value::as_integer(client_attribute(instance, "debuglevel")).unwrap_or(0) > 0 {
        client_debug_line(&format!("reply: {}\n", value::repr(client_text(&line))));
    }
    if line.is_empty() {
        raise_remote_disconnected("Remote end closed connection without response");
        return None;
    }
    let trimmed = latin1_string(trim_line_end(&line_bytes));
    let words = split_whitespace_max(&trimmed, 3);
    let version = words.first().cloned().unwrap_or_default();
    let status_text = words.get(1).cloned().unwrap_or_default();
    let reason = words.get(2).cloned().unwrap_or_default();
    let valid_status = status_text.parse::<i64>().ok();
    if !version.starts_with("HTTP/")
        || !valid_status.is_some_and(|code| (100..=999).contains(&code))
    {
        response_close_conn(instance);
        raise_bad_status_line(&trimmed);
        return None;
    }
    Some((version, valid_status.unwrap_or(0), reason))
}

fn response_check_close(instance: i64, headers: i64) -> bool {
    let connection = message_get_text(headers, "connection").map(|text| text.to_lowercase());
    let version = value::as_integer(client_attribute(instance, "version")).unwrap_or(0);
    if version == 11 {
        return connection.is_some_and(|text| text.contains("close"));
    }
    if message_get_first(headers, "keep-alive").is_some() {
        return false;
    }
    if connection.is_some_and(|text| text.contains("keep-alive")) {
        return false;
    }
    if message_get_text(headers, "proxy-connection")
        .is_some_and(|text| text.to_lowercase().contains("keep-alive"))
    {
        return false;
    }
    true
}

extern "C" fn response_init_entry(argv: *const i64) -> i64 {
    // SAFETY: five declared parameter slots (self, sock, debuglevel, method, url).
    let args = unsafe { std::slice::from_raw_parts(argv, 5) };
    let instance = args[0];
    let sock = args[1];
    let root = heap::root_values(&[instance, sock, args[2], args[3], args[4]]);
    let has_makefile = client_has(sock, "makefile");
    let (fp, raw) = if has_makefile {
        let fp = client_invoke(sock, "makefile", &[client_text("rb")]);
        if exc::pending() {
            heap::release_roots(root);
            return NONE;
        }
        (fp, false)
    } else {
        (NONE, true)
    };
    let inner = heap::root_values(&[fp]);
    client_set(instance, "sock", sock);
    client_set(instance, "fp", fp);
    client_set(instance, "raw", client_bool(raw));
    client_set(instance, "_pending", value::bytes_new(Vec::new()));
    client_set(instance, "_line_pending", value::bytes_new(Vec::new()));
    client_set(instance, "debuglevel", args[2]);
    client_set(instance, "_method", args[3]);
    client_set(instance, "url", args[4]);
    client_set(instance, "headers", NONE);
    client_set(instance, "msg", NONE);
    client_set(instance, "version", client_text("UNKNOWN"));
    client_set(instance, "status", client_text("UNKNOWN"));
    client_set(instance, "reason", client_text("UNKNOWN"));
    client_set(instance, "chunked", client_bool(false));
    client_set(instance, "chunk_left", NONE);
    client_set(instance, "length", NONE);
    client_set(instance, "will_close", client_bool(false));
    client_set(instance, "_conn_closed", client_bool(false));
    client_set(instance, "closed", client_bool(false));
    // A raw-mode response shares the connection's socket; the connection
    // transfers ownership when the response is close-delimited.
    client_set(instance, "_owns_sock", client_bool(false));
    heap::release_roots(inner);
    heap::release_roots(root);
    NONE
}

extern "C" fn response_begin_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    if client_attribute(instance, "headers") != NONE {
        heap::release_roots(root);
        return NONE;
    }
    let mut final_status: Option<(String, i64, String)> = None;
    let mut interim = 0usize;
    while interim < CLIENT_MAXINTERIM {
        let Some(status) = response_read_status(instance) else {
            heap::release_roots(root);
            return NONE;
        };
        if status.1 != 100 {
            final_status = Some(status);
            break;
        }
        if collect_header_lines(|| response_readline_any(instance, CLIENT_MAXLINE + 1)).is_none() {
            heap::release_roots(root);
            return NONE;
        }
        interim += 1;
    }
    let Some((version, status, reason)) = final_status else {
        client_raise_message(
            http_exception_class(),
            &format!("got more than {CLIENT_MAXINTERIM} interim responses"),
        );
        heap::release_roots(root);
        return NONE;
    };
    let status_value = client_integer(status);
    client_set(instance, "code", status_value);
    client_set(instance, "status", status_value);
    client_set(instance, "reason", client_text(reason.trim()));
    let version_number = if version == "HTTP/1.0" || version == "HTTP/0.9" {
        10
    } else if version.starts_with("HTTP/1.") {
        11
    } else {
        client_raise_message(unknown_protocol_class(), &version);
        heap::release_roots(root);
        return NONE;
    };
    client_set(instance, "version", client_integer(version_number));

    let Some(lines) = collect_header_lines(|| response_readline_any(instance, CLIENT_MAXLINE + 1))
    else {
        heap::release_roots(root);
        return NONE;
    };
    let pairs = parse_header_lines(&lines);
    let headers = message_from_pairs(&pairs);
    if exc::pending() {
        heap::release_roots(root);
        return NONE;
    }
    let headers_root = heap::root_values(std::slice::from_ref(&headers));
    client_set(instance, "headers", headers);
    client_set(instance, "msg", headers);
    if value::as_integer(client_attribute(instance, "debuglevel")).unwrap_or(0) > 0 {
        for (name, value) in &pairs {
            client_debug_line(&format!("header: {name}: {value}\n"));
        }
    }

    let transfer_encoding = message_get_text(headers, "transfer-encoding");
    let chunked = transfer_encoding
        .map(|text| text.to_lowercase().trim() == "chunked")
        .unwrap_or(false);
    client_set(instance, "chunked", client_bool(chunked));
    client_set(instance, "chunk_left", NONE);
    let will_close = response_check_close(instance, headers);
    client_set(instance, "will_close", client_bool(will_close));

    let mut length: Option<i64> = None;
    if !chunked {
        if let Some(text) = message_get_text(headers, "content-length") {
            if let Ok(parsed) = text.trim().parse::<i64>() {
                if parsed >= 0 {
                    length = Some(parsed);
                }
            }
        }
    }
    let method = client_attribute(instance, "_method");
    let is_head = value::as_str(method) == Some("HEAD");
    if status == 204 || status == 304 || (100..200).contains(&status) || is_head {
        length = Some(0);
    }
    client_set(
        instance,
        "length",
        match length {
            Some(number) => client_integer(number),
            None => NONE,
        },
    );
    if !will_close && !chunked && length.is_none() {
        client_set(instance, "will_close", client_bool(true));
    }
    heap::release_roots(headers_root);
    heap::release_roots(root);
    NONE
}

extern "C" fn response_read_status_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    let Some((version, status, reason)) = response_read_status(instance) else {
        heap::release_roots(root);
        return NONE;
    };
    let tuple = value::tuple_new(vec![
        client_text(&version),
        client_integer(status),
        client_text(&reason),
    ]);
    heap::release_roots(root);
    tuple
}

extern "C" fn response_close_conn_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    response_close_conn(instance)
}

extern "C" fn response_isclosed_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    client_bool(response_conn_closed(instance))
}

extern "C" fn response_close_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    client_set(instance, "closed", client_bool(true));
    response_close_conn(instance);
    heap::release_roots(root);
    NONE
}

extern "C" fn response_enter_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).

    unsafe { *argv }
}

extern "C" fn response_exit_entry(_argv: *const i64) -> i64 {
    NONE
}

extern "C" fn response_readable_entry(_argv: *const i64) -> i64 {
    client_bool(true)
}

extern "C" fn response_flush_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let fp = client_attribute(instance, "fp");
    if fp != NONE {
        let root = heap::root_values(&[fp]);
        client_invoke(fp, "flush", &[]);
        heap::release_roots(root);
    }
    NONE
}

extern "C" fn response_fileno_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let target = if value::truthy(client_attribute(instance, "raw")) {
        client_attribute(instance, "sock")
    } else {
        client_attribute(instance, "fp")
    };
    if target == NONE {
        return client_type_error("fileno() called on a closed response");
    }
    let root = heap::root_values(&[target]);
    let result = client_invoke(target, "fileno", &[]);
    heap::release_roots(root);
    result
}

/// Reads `amt` bytes (`None` = unbounded) honouring the response framing.
fn response_read_body(instance: i64, amt: i64) -> i64 {
    if response_conn_closed(instance) {
        return value::bytes_new(Vec::new());
    }
    let method = client_attribute(instance, "_method");
    if value::as_str(method) == Some("HEAD") {
        response_close_conn(instance);
        return value::bytes_new(Vec::new());
    }
    let chunked = value::truthy(client_attribute(instance, "chunked"));
    let length = client_attribute(instance, "length");
    let numeric = if amt == NONE {
        None
    } else {
        value::as_integer(amt)
    };
    match numeric {
        Some(amount) if amount >= 0 => {
            let data = if chunked {
                response_read_chunked(instance, Some(amount as usize))
            } else {
                response_bounded_read(instance, amount as usize)
            };
            match data {
                Some(data) => value::bytes_new(data),
                None => NONE,
            }
        }
        _ => {
            if chunked {
                return match response_read_chunked(instance, None) {
                    Some(data) => value::bytes_new(data),
                    None => NONE,
                };
            }
            if let Some(limit) = value::as_integer(length) {
                if limit == 0 {
                    response_close_conn(instance);
                    return value::bytes_new(Vec::new());
                }
                let data = match response_safe_read(instance, limit as usize) {
                    Some(data) => data,
                    None => {
                        response_close_conn(instance);
                        return NONE;
                    }
                };
                client_set(instance, "length", client_integer(0));
                response_close_conn(instance);
                return value::bytes_new(data);
            }
            let data = match response_read_all(instance) {
                Some(data) => data,
                None => return NONE,
            };
            response_close_conn(instance);
            value::bytes_new(data)
        }
    }
}

/// Bounded read that clips to `Content-Length` and closes at the end.
fn response_bounded_read(instance: i64, want: usize) -> Option<Vec<u8>> {
    let length = client_attribute(instance, "length");
    let mut amount = want;
    if let Some(limit) = value::as_integer(length) {
        if amount as i64 > limit {
            amount = limit.max(0) as usize;
        }
    }
    let data = response_read_some(instance, amount)?;
    if data.is_empty() && amount > 0 {
        response_close_conn(instance);
    } else if let Some(limit) = value::as_integer(length) {
        let remaining = (limit - data.len() as i64).max(0);
        client_set(instance, "length", client_integer(remaining));
        if remaining == 0 {
            response_close_conn(instance);
        }
    }
    Some(data)
}

extern "C" fn response_read_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, amt).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let root = heap::root_values(&[args[0]]);
    let result = response_read_body(args[0], args[1]);
    heap::release_roots(root);
    result
}

/// Writable length of a `bytearray` or `memoryview` buffer.
fn response_writable_len(buffer: i64) -> Option<usize> {
    if let Some(bytes) = value::as_bytearray(buffer) {
        return Some(bytes.len());
    }
    if let Some(state) = value::as_memoryview(buffer) {
        if state.released || state.readonly {
            return None;
        }
        return Some(state.len());
    }
    None
}

/// Copies `data` into a writable buffer, returning the byte count.
fn response_write_buffer(buffer: i64, data: &[u8]) -> usize {
    if let Some(bytes) = value::bytearray_bytes_mut(buffer) {
        let count = data.len().min(bytes.len());
        bytes[..count].copy_from_slice(&data[..count]);
        return count;
    }
    let Some(state) = value::as_memoryview(buffer) else {
        return 0;
    };
    if state.released || state.readonly {
        return 0;
    }
    let count = state.len().min(data.len());
    let offset = state.offset as isize;
    let stride = state.stride;
    let exporter = state.buffer;
    let Some(bytes) = value::bytearray_bytes_mut(exporter) else {
        return 0;
    };
    let mut written = 0;
    #[allow(clippy::needless_range_loop)]
    for index in 0..count {
        let at = offset + stride * index as isize;
        if at < 0 {
            break;
        }
        let at = at as usize;
        if at >= bytes.len() {
            break;
        }
        bytes[at] = data[index];
        written += 1;
    }
    written
}

extern "C" fn response_readinto_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, buffer).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let instance = args[0];
    let buffer = args[1];
    let root = heap::root_values(&[instance, buffer]);
    if response_conn_closed(instance) {
        heap::release_roots(root);
        return client_integer(0);
    }
    if value::as_str(client_attribute(instance, "_method")) == Some("HEAD") {
        response_close_conn(instance);
        heap::release_roots(root);
        return client_integer(0);
    }
    let Some(capacity) = response_writable_len(buffer) else {
        heap::release_roots(root);
        return client_type_error(&format!(
            "readinto() argument must be a writable bytes-like object, not '{}'",
            value::type_name(buffer)
        ));
    };
    if capacity == 0 {
        heap::release_roots(root);
        return client_integer(0);
    }
    let data = if value::truthy(client_attribute(instance, "chunked")) {
        response_read_chunked(instance, Some(capacity))
    } else {
        response_bounded_read(instance, capacity)
    };
    let Some(data) = data else {
        heap::release_roots(root);
        return NONE;
    };
    let written = response_write_buffer(buffer, &data);
    heap::release_roots(root);
    client_integer(written as i64)
}

/// `readline(limit)`: length-clipped, `\n`-terminated, closes at the end.
fn response_readline_body(instance: i64, limit: i64) -> i64 {
    if response_conn_closed(instance) {
        return value::bytes_new(Vec::new());
    }
    if value::as_str(client_attribute(instance, "_method")) == Some("HEAD") {
        return value::bytes_new(Vec::new());
    }
    if value::truthy(client_attribute(instance, "chunked")) {
        return response_readline_chunked(instance, limit);
    }
    let length = client_attribute(instance, "length");
    let mut effective = limit;
    if let Some(limit_value) = value::as_integer(length) {
        if effective < 0 || effective > limit_value {
            effective = limit_value;
        }
    }
    let Some(data) = response_readline_any(instance, effective) else {
        return NONE;
    };
    if data.is_empty() && effective != 0 {
        response_close_conn(instance);
    } else if let Some(limit_value) = value::as_integer(length) {
        let remaining = (limit_value - data.len() as i64).max(0);
        client_set(instance, "length", client_integer(remaining));
        if remaining == 0 {
            response_close_conn(instance);
        }
    }
    value::bytes_new(data)
}

extern "C" fn response_readline_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, limit).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let root = heap::root_values(&[args[0]]);
    let limit = value::as_integer(args[1]).unwrap_or(-1);
    let result = response_readline_body(args[0], limit);
    heap::release_roots(root);
    result
}

extern "C" fn response_readlines_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, hint).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let instance = args[0];
    let hint = value::as_integer(args[1]).unwrap_or(-1);
    let root = heap::root_values(std::slice::from_ref(&instance));
    let mut items: Vec<i64> = Vec::new();
    let mut total = 0i64;
    loop {
        let line = response_readline_body(instance, -1);
        if exc::pending() {
            heap::release_roots(root);
            return NONE;
        }
        let Some(bytes) = value::as_bytes(line) else {
            break;
        };
        if bytes.is_empty() {
            break;
        }
        total += bytes.len() as i64;
        items.push(line);
        if hint > 0 && total >= hint {
            break;
        }
    }
    let list = value::list_new(items);
    heap::release_roots(root);
    list
}

/// `read1(n)`: at most one underlying read.
fn response_read1_body(instance: i64, n: i64) -> i64 {
    if response_conn_closed(instance) {
        return value::bytes_new(Vec::new());
    }
    if value::as_str(client_attribute(instance, "_method")) == Some("HEAD") {
        return value::bytes_new(Vec::new());
    }
    let chunked = value::truthy(client_attribute(instance, "chunked"));
    if chunked {
        return response_read1_chunked(instance, n);
    }
    let mut amount: Option<usize> = if n < 0 { None } else { Some(n as usize) };
    let length = client_attribute(instance, "length");
    if let Some(limit) = value::as_integer(length) {
        match amount {
            Some(value) if (value as i64) > limit => amount = Some(limit.max(0) as usize),
            None => amount = Some(limit.max(0) as usize),
            _ => {}
        }
    }
    let data = match amount {
        Some(want) => response_read_some(instance, want),
        None => response_read_all(instance),
    };
    let Some(data) = data else {
        return NONE;
    };
    let requested = n != 0;
    if data.is_empty() && requested {
        response_close_conn(instance);
    } else if let Some(limit) = value::as_integer(length) {
        let remaining = (limit - data.len() as i64).max(0);
        client_set(instance, "length", client_integer(remaining));
        if remaining == 0 {
            response_close_conn(instance);
        }
    }
    value::bytes_new(data)
}

extern "C" fn response_read1_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, n).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let root = heap::root_values(&[args[0]]);
    let n = value::as_integer(args[1]).unwrap_or(-1);
    let result = response_read1_body(args[0], n);
    heap::release_roots(root);
    result
}

extern "C" fn response_peek_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, n).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let instance = args[0];
    if response_conn_closed(instance) {
        return value::bytes_new(Vec::new());
    }
    if value::as_str(client_attribute(instance, "_method")) == Some("HEAD") {
        return value::bytes_new(Vec::new());
    }
    let n = value::as_integer(args[1]).unwrap_or(-1);
    let root = heap::root_values(std::slice::from_ref(&instance));
    let fp = client_attribute(instance, "fp");
    let raw = value::truthy(client_attribute(instance, "raw"));
    if value::truthy(client_attribute(instance, "chunked")) {
        let result = match chunked_next_size(instance) {
            Some(left) if left > 0 => {
                let limit = if n < 0 {
                    65536
                } else {
                    (n as usize).min(left as usize)
                };
                let data = if raw || fp == NONE {
                    match response_peek_raw(instance, limit) {
                        Some(data) => data,
                        None => {
                            heap::release_roots(root);
                            return NONE;
                        }
                    }
                } else if client_has(fp, "peek") {
                    let peeked = client_invoke(fp, "peek", &[client_integer(left)]);
                    if exc::pending() {
                        heap::release_roots(root);
                        return NONE;
                    }
                    let mut data = value::as_bytes(peeked)
                        .map(|bytes| bytes.to_vec())
                        .unwrap_or_default();
                    data.truncate(limit);
                    data
                } else {
                    Vec::new()
                };
                let shown = if n < 0 {
                    data
                } else {
                    data.into_iter().take(n as usize).collect()
                };
                value::bytes_new(shown)
            }
            _ => value::bytes_new(Vec::new()),
        };
        heap::release_roots(root);
        return result;
    }
    let data = if value::truthy(client_attribute(instance, "raw")) || fp == NONE {
        let limit = if n < 0 { 65536 } else { n as usize };
        match response_peek_raw(instance, limit) {
            Some(data) => data,
            None => {
                heap::release_roots(root);
                return NONE;
            }
        }
    } else if client_has(fp, "peek") {
        let result = client_invoke(fp, "peek", &[client_integer(n)]);
        if exc::pending() {
            heap::release_roots(root);
            return NONE;
        }
        value::as_bytes(result)
            .map(|bytes| bytes.to_vec())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let shown = if n < 0 {
        data
    } else {
        data.into_iter().take(n as usize).collect()
    };
    let result = value::bytes_new(shown);
    heap::release_roots(root);
    result
}

extern "C" fn response_getheader_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, name, default).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let root = heap::root_values(&[instance, args[1], args[2]]);
    let headers = client_attribute(instance, "headers");
    if headers == NONE {
        heap::release_roots(root);
        return client_raise(response_not_ready_class(), "ResponseNotReady()");
    }
    let Some(key) = header_key(args[1]) else {
        heap::release_roots(root);
        return client_type_error("header name must be str");
    };
    let Some(values) = message_get_all_values(headers, &key) else {
        heap::release_roots(root);
        return args[2];
    };
    let mut parts: Vec<String> = Vec::with_capacity(values.len());
    for item in values {
        if let Some(text) = value::as_str(item) {
            parts.push(text.to_string());
        } else if let Some(bytes) = value::as_bytes(item) {
            parts.push(latin1_string(bytes));
        }
    }
    let joined = client_text(&parts.join(", "));
    heap::release_roots(root);
    joined
}

extern "C" fn response_getheaders_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    let headers = client_attribute(instance, "headers");
    if headers == NONE {
        heap::release_roots(root);
        return client_raise(response_not_ready_class(), "ResponseNotReady()");
    }
    let result = client_invoke(headers, "items", &[]);
    heap::release_roots(root);
    result
}

extern "C" fn response_info_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    client_attribute(instance, "headers")
}

extern "C" fn response_geturl_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    client_attribute(instance, "url")
}

extern "C" fn response_getcode_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    client_attribute(instance, "status")
}

/// Next positive chunk size; `-1` marks the end of the body.
fn chunked_next_size(instance: i64) -> Option<i64> {
    let current = value::as_integer(client_attribute(instance, "chunk_left")).unwrap_or(-1);
    if current > 0 {
        return Some(current);
    }
    if current == 0 {
        let _ = response_safe_read(instance, 2)?;
    }
    let line = response_readline_any(instance, CLIENT_MAXLINE + 1)?;
    if line.len() as i64 > CLIENT_MAXLINE {
        raise_line_too_long("chunk size");
        return None;
    }
    let raw = latin1_string(trim_line_end(&line));
    let size_text = raw.split(';').next().unwrap_or("").trim();
    let size = match i64::from_str_radix(size_text, 16) {
        Ok(size) => size,
        Err(_) => {
            response_close_conn(instance);
            raise_incomplete_read(&[], None);
            return None;
        }
    };
    if size == 0 {
        discard_trailer(instance)?;
        response_close_conn(instance);
        client_set(instance, "chunk_left", NONE);
        return Some(-1);
    }
    client_set(instance, "chunk_left", client_integer(size));
    Some(size)
}

fn discard_trailer(instance: i64) -> Option<()> {
    let mut count = 0usize;
    loop {
        let line = response_readline_any(instance, CLIENT_MAXLINE + 1)?;
        if line.len() as i64 > CLIENT_MAXLINE {
            raise_line_too_long("trailer line");
            return None;
        }
        if line.is_empty() || line == b"\r\n" || line == b"\n" {
            return Some(());
        }
        count += 1;
        if count > CLIENT_MAXHEADERS {
            client_raise_message(
                http_exception_class(),
                &format!("got more than {CLIENT_MAXHEADERS} trailers"),
            );
            return None;
        }
    }
}

fn response_read_chunked(instance: i64, amt: Option<usize>) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut remaining = amt;
    loop {
        let left = chunked_next_size(instance)?;
        if left < 0 {
            break;
        }
        match remaining {
            Some(0) => break,
            Some(want) if (want as i64) <= left => {
                let data = response_safe_read(instance, want)?;
                out.extend_from_slice(&data);
                client_set(instance, "chunk_left", client_integer(left - want as i64));
                remaining = Some(0);
            }
            Some(want) => {
                let data = response_safe_read(instance, left as usize)?;
                out.extend_from_slice(&data);
                client_set(instance, "chunk_left", client_integer(0));
                remaining = Some(want - left as usize);
            }
            None => {
                let data = response_safe_read(instance, left as usize)?;
                out.extend_from_slice(&data);
                client_set(instance, "chunk_left", client_integer(0));
            }
        }
    }
    Some(out)
}

fn response_read1_chunked(instance: i64, n: i64) -> i64 {
    let Some(left) = chunked_next_size(instance) else {
        return NONE;
    };
    if left < 0 || n == 0 {
        return value::bytes_new(Vec::new());
    }
    let want = if n < 0 || n > left { left } else { n };
    let Some(data) = response_read_some(instance, want as usize) else {
        return NONE;
    };
    if data.is_empty() {
        raise_incomplete_read(&[], None);
        return NONE;
    }
    client_set(
        instance,
        "chunk_left",
        client_integer(left - data.len() as i64),
    );
    value::bytes_new(data)
}

/// Chunked `readline`: decodes chunks until a `\n`, buffering the remainder.
fn response_readline_chunked(instance: i64, limit: i64) -> i64 {
    let pending_value = client_attribute(instance, "_line_pending");
    let mut pending = value::as_bytes(pending_value)
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default();
    let root = heap::root_values(std::slice::from_ref(&instance));
    loop {
        if let Some(index) = pending.iter().position(|byte| *byte == b'\n') {
            if limit < 0 || (index as i64) < limit {
                let rest = pending.split_off(index + 1);
                client_set(instance, "_line_pending", value::bytes_new(rest));
                let line = value::bytes_new(pending);
                heap::release_roots(root);
                return line;
            }
            break;
        }
        if limit >= 0 && pending.len() as i64 >= limit {
            break;
        }
        let Some(chunk) = response_read_chunked(instance, Some(8192)) else {
            heap::release_roots(root);
            return NONE;
        };
        if chunk.is_empty() {
            break;
        }
        pending.extend_from_slice(&chunk);
    }
    if limit >= 0 && pending.len() as i64 > limit {
        let rest = pending.split_off(limit as usize);
        client_set(instance, "_line_pending", value::bytes_new(rest));
    } else {
        client_set(instance, "_line_pending", value::bytes_new(Vec::new()));
    }
    let line = value::bytes_new(pending);
    heap::release_roots(root);
    line
}

fn response_class() -> i64 {
    let self_only = || vec![client_positional("self")];
    client_class(
        "http.client.HTTPResponse",
        "HTTPResponse",
        NONE,
        vec![
            (
                "__init__",
                vec![
                    client_positional("self"),
                    client_positional("sock"),
                    client_optional("debuglevel", client_integer(0)),
                    client_optional("method", NONE),
                    client_optional("url", NONE),
                ],
                response_init_entry,
            ),
            ("begin", self_only(), response_begin_entry),
            (
                "read",
                vec![client_positional("self"), client_optional("amt", NONE)],
                response_read_entry,
            ),
            (
                "read1",
                vec![
                    client_positional("self"),
                    client_optional("n", client_integer(-1)),
                ],
                response_read1_entry,
            ),
            (
                "readinto",
                vec![client_positional("self"), client_positional("b")],
                response_readinto_entry,
            ),
            (
                "readline",
                vec![
                    client_positional("self"),
                    client_optional("limit", client_integer(-1)),
                ],
                response_readline_entry,
            ),
            (
                "readlines",
                vec![
                    client_positional("self"),
                    client_optional("hint", client_integer(-1)),
                ],
                response_readlines_entry,
            ),
            (
                "peek",
                vec![
                    client_positional("self"),
                    client_optional("n", client_integer(-1)),
                ],
                response_peek_entry,
            ),
            (
                "getheader",
                vec![
                    client_positional("self"),
                    client_positional("name"),
                    client_optional("default", NONE),
                ],
                response_getheader_entry,
            ),
            ("getheaders", self_only(), response_getheaders_entry),
            ("info", self_only(), response_info_entry),
            ("geturl", self_only(), response_geturl_entry),
            ("getcode", self_only(), response_getcode_entry),
            ("isclosed", self_only(), response_isclosed_entry),
            ("close", self_only(), response_close_entry),
            ("fileno", self_only(), response_fileno_entry),
            ("flush", self_only(), response_flush_entry),
            ("readable", self_only(), response_readable_entry),
            ("_read_status", self_only(), response_read_status_entry),
            ("_close_conn", self_only(), response_close_conn_entry),
            ("__enter__", self_only(), response_enter_entry),
            (
                "__exit__",
                vec![
                    client_positional("self"),
                    client_optional("exc_type", NONE),
                    client_optional("exc_value", NONE),
                    client_optional("traceback", NONE),
                ],
                response_exit_entry,
            ),
        ],
        Vec::new(),
    )
}

// -------------------------------------------------------------------------
// `HTTPConnection`
// -------------------------------------------------------------------------

/// Parses `host[:port]` exactly like `_get_hostport`, raising `InvalidURL`
/// on a non-numeric port.
fn connection_host_port(host: i64, port: i64, default_port: i64) -> Option<(String, i64)> {
    let Some(mut host_text) = value::as_str(host).map(str::to_string) else {
        exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!(
                "'{}' object has no attribute 'rfind'",
                value::type_name(host)
            ),
        );
        return None;
    };
    let port_number = if port == NONE {
        let colon = host_text.rfind(':').map(|index| index as i64).unwrap_or(-1);
        let bracket = host_text.rfind(']').map(|index| index as i64).unwrap_or(-1);
        if colon > bracket {
            let tail = host_text[(colon + 1) as usize..].to_string();
            let parsed = tail.parse::<i64>();
            match parsed {
                Ok(number) => {
                    host_text.truncate(colon as usize);
                    number
                }
                Err(_) => {
                    if tail.is_empty() {
                        host_text.truncate(colon as usize);
                        default_port
                    } else {
                        client_raise_message(
                            invalid_url_class(),
                            &format!("nonnumeric port: '{tail}'"),
                        );
                        return None;
                    }
                }
            }
        } else {
            default_port
        }
    } else {
        match value::as_integer(port) {
            Some(number) => number,
            None => {
                client_type_error("port must be an integer");
                return None;
            }
        }
    };
    if host_text.starts_with('[') && host_text.ends_with(']') && host_text.len() >= 2 {
        host_text = host_text[1..host_text.len() - 1].to_string();
    }
    if !(0..=65535).contains(&port_number) {
        client_raise_message(
            invalid_url_class(),
            &format!("nonnumeric port: '{port_number}'"),
        );
        return None;
    }
    Some((host_text, port_number))
}

/// Rejects control characters in a host, like `_validate_host`.
fn connection_validate_host_text(host: &str) -> Option<()> {
    if let Some(character) = host
        .chars()
        .find(|character| (*character as u32) <= 0x20 || (*character as u32) == 0x7f)
    {
        client_raise_message(
            invalid_url_class(),
            &format!(
                "URL can't contain control characters. {host:?} (found at least {character:?})"
            ),
        );
        return None;
    }
    Some(())
}

fn connection_init_impl(
    instance: i64,
    host: i64,
    port: i64,
    timeout: i64,
    source_address: i64,
    blocksize: i64,
) -> i64 {
    let root = heap::root_values(&[instance, host, port, timeout, source_address, blocksize]);
    client_set(instance, "timeout", timeout);
    client_set(instance, "source_address", source_address);
    client_set(instance, "blocksize", blocksize);
    client_set(instance, "sock", NONE);
    let buffer = value::list_new(Vec::new());
    let inner = heap::root_values(&[buffer]);
    client_set(instance, "_buffer", buffer);
    client_set(instance, "_response", NONE);
    let state = client_text(CS_IDLE);
    client_set(instance, "_state", state);
    client_set(instance, "_method", NONE);
    client_set(instance, "_tunnel_host", NONE);
    client_set(instance, "_tunnel_port", NONE);
    let tunnel_headers = value::dict_new(Vec::new());
    client_set(instance, "_tunnel_headers", tunnel_headers);
    client_set(instance, "_raw_proxy_headers", NONE);
    let create = value::native_function_new("socket", "create_connection", 2);
    client_set(instance, "_create_connection", create);
    heap::release_roots(inner);
    let default_port = value::as_integer(client_attribute(instance, "default_port")).unwrap_or(80);
    let Some((host_text, port_number)) = connection_host_port(host, port, default_port) else {
        heap::release_roots(root);
        return NONE;
    };
    if connection_validate_host_text(&host_text).is_none() {
        heap::release_roots(root);
        return NONE;
    }
    let host_value = client_text(&host_text);
    client_set(instance, "host", host_value);
    client_set(instance, "port", client_integer(port_number));
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_init_entry(argv: *const i64) -> i64 {
    // SAFETY: six declared parameter slots (self, host, port, timeout,
    // source_address, blocksize).
    let args = unsafe { std::slice::from_raw_parts(argv, 6) };
    connection_init_impl(args[0], args[1], args[2], args[3], args[4], args[5])
}

extern "C" fn https_init_entry(argv: *const i64) -> i64 {
    // SAFETY: seven declared parameter slots (self, host, port, timeout,
    // source_address, context, blocksize).
    let args = unsafe { std::slice::from_raw_parts(argv, 7) };
    let instance = args[0];
    connection_init_impl(instance, args[1], args[2], args[3], args[4], args[6]);
    if exc::pending() {
        return NONE;
    }
    let context = if args[5] == NONE {
        let created = socket::ssl_call("_create_default_https_context", &[]);
        if exc::pending() {
            return NONE;
        }
        created
    } else {
        args[5]
    };
    let root = heap::root_values(&[instance, context]);
    client_set(instance, "_context", context);
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_get_hostport_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, host, port).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let default_port = value::as_integer(client_attribute(instance, "default_port")).unwrap_or(80);
    let root = heap::root_values(&[instance]);
    let Some((host_text, port_number)) = connection_host_port(args[1], args[2], default_port)
    else {
        heap::release_roots(root);
        return NONE;
    };
    let tuple = value::tuple_new(vec![client_text(&host_text), client_integer(port_number)]);
    heap::release_roots(root);
    tuple
}

extern "C" fn connection_validate_host_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, host).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let Some(host) = value::as_str(args[1]) else {
        return client_type_error("host must be str");
    };
    match connection_validate_host_text(host) {
        Some(()) => NONE,
        None => NONE,
    }
}

/// Best-effort `TCP_NODELAY` on the connected socket.
fn connection_set_nodelay(sock: i64) {
    if sock == NONE {
        return;
    }
    let level = client_integer(libc::IPPROTO_TCP as i64);
    let option = client_integer(libc::TCP_NODELAY as i64);
    let value_one = client_integer(1);
    let root = heap::root_values(&[sock, level, option, value_one]);
    client_invoke(sock, "setsockopt", &[level, option, value_one]);
    heap::release_roots(root);
    if exc::pending() {
        // Platforms without the option raise `ENOPROTOOPT`; CPython tolerates
        // it, and the native socket surface reports it as an `OSError`.
        exc::clear();
    }
}

/// `connect()`: open the socket and run the CONNECT tunnel, if configured.
fn connection_connect_impl(instance: i64) -> i64 {
    let root = heap::root_values(std::slice::from_ref(&instance));
    let host = client_attribute(instance, "host");
    let port = client_attribute(instance, "port");
    let timeout = client_attribute(instance, "timeout");
    let source = client_attribute(instance, "source_address");
    let create = client_attribute(instance, "_create_connection");
    let address = value::tuple_new(vec![host, port]);
    let inner = heap::root_values(&[address, create, timeout, source]);
    let mut call_args = vec![address, timeout];
    if source != NONE {
        call_args.push(source);
    }
    let sock = crate::builtins::call(create, call_args.len(), call_args.as_ptr());
    if exc::pending() {
        heap::release_roots(inner);
        heap::release_roots(root);
        return NONE;
    }
    let sock_root = heap::root_values(std::slice::from_ref(&sock));
    client_set(instance, "sock", sock);
    connection_set_nodelay(sock);
    let tunnel = client_attribute(instance, "_tunnel_host");
    if tunnel != NONE {
        connection_tunnel(instance);
    }
    heap::release_roots(sock_root);
    heap::release_roots(inner);
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_connect_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    connection_connect_impl(instance)
}

extern "C" fn https_connect_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    connection_connect_impl(instance);
    if exc::pending() {
        heap::release_roots(root);
        return NONE;
    }
    let context = client_attribute(instance, "_context");
    let sock = client_attribute(instance, "sock");
    let tunnel = client_attribute(instance, "_tunnel_host");
    let server_hostname = if tunnel != NONE {
        tunnel
    } else {
        client_attribute(instance, "host")
    };
    let inner = heap::root_values(&[context, sock, server_hostname]);
    let wrapped = socket::ssl_call(
        "_ssl_context_wrap_socket",
        &[context, sock, server_hostname],
    );
    if exc::pending() {
        heap::release_roots(inner);
        heap::release_roots(root);
        return NONE;
    }
    client_set(instance, "sock", wrapped);
    heap::release_roots(inner);
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_close_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    let idle = client_text(CS_IDLE);
    client_set(instance, "_state", idle);
    let sock = client_attribute(instance, "sock");
    if sock != NONE {
        client_set(instance, "sock", NONE);
        client_invoke(sock, "close", &[]);
    }
    let response = client_attribute(instance, "_response");
    if response != NONE {
        client_set(instance, "_response", NONE);
        client_invoke(response, "close", &[]);
    }
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_set_debuglevel_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, level).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    client_set(args[0], "debuglevel", args[1]);
    NONE
}

extern "C" fn connection_enter_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).

    unsafe { *argv }
}

extern "C" fn connection_exit_entry(argv: *const i64) -> i64 {
    // SAFETY: four declared parameter slots (self, exc_type, exc_value,
    // traceback).
    let args = unsafe { std::slice::from_raw_parts(argv, 4) };
    let instance = args[0];
    let root = heap::root_values(std::slice::from_ref(&instance));
    connection_close_entry(std::slice::from_ref(&instance).as_ptr());
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_set_tunnel_entry(argv: *const i64) -> i64 {
    // SAFETY: four declared parameter slots (self, host, port, headers).
    let args = unsafe { std::slice::from_raw_parts(argv, 4) };
    let instance = args[0];
    let root = heap::root_values(&[instance, args[1], args[2], args[3]]);
    let sock = client_attribute(instance, "sock");
    if sock != NONE {
        exc::raise_with_message(
            exc::RUNTIME_ERROR,
            "Can't set up tunnel for established connection",
        );
        heap::release_roots(root);
        return NONE;
    }
    let default_port = value::as_integer(client_attribute(instance, "default_port")).unwrap_or(80);
    let Some((host_text, port_number)) = connection_host_port(args[1], args[2], default_port)
    else {
        heap::release_roots(root);
        return NONE;
    };
    let host_value = client_text(&host_text);
    let inner = heap::root_values(&[host_value]);
    client_set(instance, "_tunnel_host", host_value);
    client_set(instance, "_tunnel_port", client_integer(port_number));
    let mut pairs: Vec<(i64, i64)> = Vec::new();
    if args[3] != NONE {
        if let Some(items) = value::as_dict(args[3]) {
            pairs = items.to_vec();
        }
    }
    let mut has_host = false;
    for (name, _) in &pairs {
        if header_key(*name).is_some_and(|text| text.eq_ignore_ascii_case("host")) {
            has_host = true;
        }
    }
    if !has_host {
        let host_name = client_text("Host");
        let host_value = client_text(&format!("{host_text}:{port_number}"));
        let pair_root = heap::root_values(&[host_name, host_value]);
        pairs.push((host_name, host_value));
        heap::release_roots(pair_root);
    }
    let tunnel_headers = value::dict_new(pairs);
    client_set(instance, "_tunnel_headers", tunnel_headers);
    heap::release_roots(inner);
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_get_proxy_headers_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    if client_attribute(instance, "_raw_proxy_headers") == NONE {
        return NONE;
    }
    client_attribute(instance, "_proxy_headers_message")
}

/// Bytes of a header name/value, latin-1 encoded.
fn header_bytes(item: i64) -> Option<Vec<u8>> {
    if let Some(text) = value::as_str(item) {
        return latin1_bytes(text, "header");
    }
    value::as_bytes(item).map(|bytes| bytes.to_vec())
}

/// `[^:\s][^:\r\n]*` — the legal header-name shape.
fn legal_header_name(name: &[u8]) -> bool {
    let Some(first) = name.first().copied() else {
        return false;
    };
    if first == b':' || first.is_ascii_whitespace() {
        return false;
    }
    !name
        .iter()
        .any(|byte| *byte == b':' || *byte == b'\r' || *byte == b'\n')
}

/// `\n(?![ \t])|\r(?![ \t\n])` — the illegal header-value shape.
fn legal_header_value(value: &[u8]) -> bool {
    for (index, byte) in value.iter().enumerate() {
        let next = value.get(index + 1).copied();
        if *byte == b'\n' && !matches!(next, Some(b' ') | Some(b'\t')) {
            return false;
        }
        if *byte == b'\r' && !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\n')) {
            return false;
        }
    }
    true
}

/// `_validate_method`: no control characters in the method.
fn connection_validate_method(method: i64) -> Option<()> {
    let Some(text) = value::as_str(method) else {
        client_type_error("method must be str");
        return None;
    };
    if let Some(character) = text.chars().find(|character| (*character as u32) <= 0x1f) {
        exc::raise_with_message(
            VALUE_ERROR,
            &format!(
                "method can't contain control characters. {text:?} (found at least {character:?})"
            ),
        );
        return None;
    }
    Some(())
}

/// `_validate_path`: no control characters or spaces in the request target.
fn connection_validate_path(url: i64) -> Option<()> {
    let Some(text) = value::as_str(url) else {
        client_invoke(url, "startswith", &[]);
        return None;
    };
    if let Some(character) = text
        .chars()
        .find(|character| (*character as u32) <= 0x20 || (*character as u32) == 0x7f)
    {
        client_raise_message(
            invalid_url_class(),
            &format!(
                "URL can't contain control characters. {text:?} (found at least {character:?})"
            ),
        );
        return None;
    }
    Some(())
}

fn connection_output(instance: i64, line: i64) -> i64 {
    let buffer = client_attribute(instance, "_buffer");
    if buffer == NONE {
        return client_type_error("connection has no output buffer");
    }
    let root = heap::root_values(&[instance, buffer, line]);
    let result = client_invoke(buffer, "append", &[line]);
    heap::release_roots(root);
    result
}

fn connection_send(instance: i64, data: i64) -> i64 {
    let root = heap::root_values(&[instance, data]);
    let mut sock = client_attribute(instance, "sock");
    if sock == NONE {
        if value::truthy(client_attribute(instance, "auto_open")) {
            connection_connect_impl(instance);
            if exc::pending() {
                heap::release_roots(root);
                return NONE;
            }
            sock = client_attribute(instance, "sock");
        } else {
            heap::release_roots(root);
            return raise_not_connected();
        }
    }
    if sock == NONE {
        heap::release_roots(root);
        return NONE;
    }
    let sock_root = heap::root_values(std::slice::from_ref(&sock));
    if client_has(data, "read") {
        let blocksize = client_attribute(instance, "blocksize");
        let limit = if value::as_integer(blocksize).is_some_and(|size| size > 0) {
            blocksize
        } else {
            client_integer(8192)
        };
        loop {
            let chunk = client_invoke(data, "read", &[limit]);
            if exc::pending() {
                heap::release_roots(sock_root);
                heap::release_roots(root);
                return NONE;
            }
            let bytes = value::as_bytes(chunk)
                .map(|bytes| bytes.to_vec())
                .unwrap_or_default();
            if bytes.is_empty() {
                break;
            }
            let piece = value::bytes_new(bytes);
            let inner = heap::root_values(std::slice::from_ref(&piece));
            client_invoke(sock, "sendall", &[piece]);
            heap::release_roots(inner);
            if exc::pending() {
                heap::release_roots(sock_root);
                heap::release_roots(root);
                return NONE;
            }
        }
    } else {
        client_invoke(sock, "sendall", &[data]);
    }
    heap::release_roots(sock_root);
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_send_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, data).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    connection_send(args[0], args[1])
}

extern "C" fn connection_output_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, line).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    connection_output(args[0], args[1])
}

/// Body chunks: file-like objects stream, buffer objects are one chunk, and
/// any other iterable is drained chunk by chunk.
fn connection_body_chunks(instance: i64, body: i64) -> Option<Vec<i64>> {
    if client_has(body, "read") {
        let blocksize = client_attribute(instance, "blocksize");
        let limit = if value::as_integer(blocksize).is_some_and(|size| size > 0) {
            blocksize
        } else {
            client_integer(8192)
        };
        let mut chunks = Vec::new();
        loop {
            let chunk = client_invoke(body, "read", &[limit]);
            if exc::pending() {
                return None;
            }
            let bytes = value::as_bytes(chunk)
                .map(|bytes| bytes.to_vec())
                .unwrap_or_default();
            if bytes.is_empty() {
                break;
            }
            chunks.push(chunk);
        }
        return Some(chunks);
    }
    if value::as_bytes(body).is_some() {
        return Some(vec![body]);
    }
    let iterator = crate::iter::iter_of(body);
    if exc::pending() {
        return None;
    }
    let root = heap::root_values(std::slice::from_ref(&iterator));
    let mut chunks = Vec::new();
    loop {
        let mut item = NONE;
        let status = crate::iter::iter_next(iterator, &mut item);
        if exc::pending() {
            heap::release_roots(root);
            return None;
        }
        if status == 0 {
            break;
        }
        chunks.push(item);
    }
    heap::release_roots(root);
    Some(chunks)
}

fn connection_send_output(instance: i64, message_body: i64, encode_chunked: bool) -> i64 {
    let root = heap::root_values(&[instance, message_body]);
    let buffer = client_attribute(instance, "_buffer");
    if buffer == NONE {
        heap::release_roots(root);
        return client_type_error("connection has no output buffer");
    }
    let buffer_root = heap::root_values(&[buffer]);
    let empty = value::bytes_new(Vec::new());
    let tail = value::list_new(vec![empty, empty]);
    let tail_root = heap::root_values(&[tail]);
    client_invoke(buffer, "extend", &[tail]);
    heap::release_roots(tail_root);
    if exc::pending() {
        heap::release_roots(buffer_root);
        heap::release_roots(root);
        return NONE;
    }
    let separator = value::bytes_new(b"\r\n".to_vec());
    let separator_root = heap::root_values(&[separator]);
    let message = client_invoke(separator, "join", &[buffer]);
    heap::release_roots(separator_root);
    let message_root = heap::root_values(std::slice::from_ref(&message));
    client_invoke(buffer, "clear", &[]);
    heap::release_roots(buffer_root);
    if exc::pending() {
        heap::release_roots(message_root);
        heap::release_roots(root);
        return NONE;
    }
    connection_send(instance, message);
    heap::release_roots(message_root);
    if exc::pending() {
        heap::release_roots(root);
        return NONE;
    }
    if message_body != NONE {
        let Some(chunks) = connection_body_chunks(instance, message_body) else {
            heap::release_roots(root);
            return NONE;
        };
        let vsn = value::as_integer(client_attribute(instance, "_http_vsn")).unwrap_or(11);
        for chunk in chunks {
            let Some(chunk_len) = value::as_bytes(chunk).map(|bytes| bytes.len()) else {
                heap::release_roots(root);
                return client_type_error(
                    "message_body should be a bytes-like object or an iterable",
                );
            };
            if chunk_len == 0 {
                continue;
            }
            let chunk_root = heap::root_values(std::slice::from_ref(&chunk));
            let outgoing = if encode_chunked && vsn == 11 {
                let prefix = value::bytes_new(format!("{chunk_len:X}\r\n").into_bytes());
                let suffix = value::bytes_new(b"\r\n".to_vec());
                let parts = value::list_new(vec![prefix, chunk, suffix]);
                let parts_root = heap::root_values(&[parts]);
                let empty_separator = value::bytes_new(Vec::new());
                let joined = client_invoke(empty_separator, "join", &[parts]);
                heap::release_roots(parts_root);
                joined
            } else {
                chunk
            };
            let outgoing_root = heap::root_values(std::slice::from_ref(&outgoing));
            connection_send(instance, outgoing);
            heap::release_roots(outgoing_root);
            heap::release_roots(chunk_root);
            if exc::pending() {
                heap::release_roots(root);
                return NONE;
            }
        }
        if encode_chunked && vsn == 11 {
            let ending = value::bytes_new(b"0\r\n\r\n".to_vec());
            let ending_root = heap::root_values(std::slice::from_ref(&ending));
            connection_send(instance, ending);
            heap::release_roots(ending_root);
        }
    }
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_send_output_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, message_body,
    // encode_chunked).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    connection_send_output(args[0], args[1], value::truthy(args[2]))
}

fn connection_putheader_impl(instance: i64, header: i64, values: &[i64]) -> i64 {
    let state = client_attribute(instance, "_state");
    if value::as_str(state) != Some(CS_REQ_STARTED) {
        return raise_cannot_send_header();
    }
    let Some(name_bytes) = header_bytes(header) else {
        return client_type_error("header name must be str or bytes");
    };
    if !legal_header_name(&name_bytes) {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("Invalid header name {}", value::repr(header)),
        );
    }
    let root = heap::root_values(values);
    let mut encoded: Vec<Vec<u8>> = Vec::with_capacity(values.len());
    for &one in values {
        let bytes = if let Some(text) = value::as_str(one) {
            match latin1_bytes(text, "header") {
                Some(bytes) => bytes,
                None => {
                    heap::release_roots(root);
                    return NONE;
                }
            }
        } else if let Some(number) = value::as_integer(one) {
            number.to_string().into_bytes()
        } else if let Some(bytes) = value::as_bytes(one) {
            bytes.to_vec()
        } else {
            heap::release_roots(root);
            return client_type_error("header value must be str, int or bytes");
        };
        if !legal_header_value(&bytes) {
            heap::release_roots(root);
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("Invalid header value {}", value::repr(one)),
            );
        }
        encoded.push(bytes);
    }
    let mut line = name_bytes;
    line.extend_from_slice(b": ");
    for (index, part) in encoded.iter().enumerate() {
        if index > 0 {
            line.extend_from_slice(b"\r\n\t");
        }
        line.extend_from_slice(part);
    }
    let line_value = value::bytes_new(line);
    let result = connection_output(instance, line_value);
    heap::release_roots(root);
    result
}

extern "C" fn connection_putheader_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, header, values list).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let values: Vec<i64> = value::as_list(args[2])
        .map(|items| items.to_vec())
        .unwrap_or_default();
    connection_putheader_impl(args[0], args[1], &values)
}

/// The `Host:` header value for `putrequest`.
fn connection_host_header(instance: i64, url: &str) -> i64 {
    let mut netloc = String::new();
    if url.starts_with("http") {
        if let Some((_, rest)) = url.split_once("://") {
            netloc = rest.split(['/', '?', '#']).next().unwrap_or("").to_string();
        }
    }
    if !netloc.is_empty() {
        return client_text(&netloc);
    }
    let tunnel = client_attribute(instance, "_tunnel_host");
    let use_tunnel = tunnel != NONE && value::as_str(tunnel).is_some();
    let host = if use_tunnel {
        tunnel
    } else {
        client_attribute(instance, "host")
    };
    let port = if use_tunnel {
        client_attribute(instance, "_tunnel_port")
    } else {
        client_attribute(instance, "port")
    };
    let Some(host_text) = value::as_str(host) else {
        return NONE;
    };
    let mut host_enc = host_text.to_string();
    if host_enc.contains(':') && !host_enc.starts_with('[') {
        host_enc = format!("[{host_enc}]");
    }
    if host_text.contains(':') {
        if let Some(percent) = host_enc.find('%') {
            let prefix = host_enc[..percent].to_string();
            host_enc = format!("{prefix}]");
        }
    }
    let port_number = value::as_integer(port).unwrap_or(80);
    let default_port = value::as_integer(client_attribute(instance, "default_port")).unwrap_or(80);
    if port_number == default_port {
        client_text(&host_enc)
    } else {
        client_text(&format!("{host_enc}:{port_number}"))
    }
}

fn connection_putrequest_impl(
    instance: i64,
    method: i64,
    url: i64,
    skip_host: bool,
    skip_accept_encoding: bool,
) -> i64 {
    let root = heap::root_values(&[instance, method, url]);
    let response = client_attribute(instance, "_response");
    if response != NONE {
        let closed = client_invoke(response, "isclosed", &[]);
        if !exc::pending() && value::truthy(closed) {
            client_set(instance, "_response", NONE);
        }
    }
    let state = client_attribute(instance, "_state");
    if value::as_str(state) == Some(CS_IDLE) {
        let started = client_text(CS_REQ_STARTED);
        client_set(instance, "_state", started);
    } else {
        heap::release_roots(root);
        return raise_cannot_send_request(state);
    }
    if connection_validate_method(method).is_none() {
        heap::release_roots(root);
        return NONE;
    }
    client_set(instance, "_method", method);
    let url = if value::truthy(url) {
        url
    } else {
        client_text("/")
    };
    if connection_validate_path(url).is_none() {
        heap::release_roots(root);
        return NONE;
    }
    let (Some(method_text), Some(url_text)) = (value::as_str(method), value::as_str(url)) else {
        heap::release_roots(root);
        return client_type_error("method and url must be str");
    };
    let vsn_str = value::as_str(client_attribute(instance, "_http_vsn_str"))
        .unwrap_or("HTTP/1.1")
        .to_string();
    let request = format!("{method_text} {url_text} {vsn_str}");
    let request_value = value::bytes_new(request.into_bytes());
    let request_root = heap::root_values(std::slice::from_ref(&request_value));
    connection_output(instance, request_value);
    heap::release_roots(request_root);
    if exc::pending() {
        heap::release_roots(root);
        return NONE;
    }
    let vsn = value::as_integer(client_attribute(instance, "_http_vsn")).unwrap_or(11);
    if vsn == 11 {
        if !skip_host {
            let host_value = connection_host_header(instance, url_text);
            if host_value == NONE {
                heap::release_roots(root);
                return NONE;
            }
            let host_name = client_text("Host");
            let inner = heap::root_values(&[host_value, host_name]);
            connection_putheader_impl(instance, host_name, &[host_value]);
            heap::release_roots(inner);
            if exc::pending() {
                heap::release_roots(root);
                return NONE;
            }
        }
        if !skip_accept_encoding {
            let name = client_text("Accept-Encoding");
            let value_part = client_text("identity");
            let inner = heap::root_values(&[name, value_part]);
            connection_putheader_impl(instance, name, &[value_part]);
            heap::release_roots(inner);
            if exc::pending() {
                heap::release_roots(root);
                return NONE;
            }
        }
    }
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_putrequest_entry(argv: *const i64) -> i64 {
    // SAFETY: five declared parameter slots (self, method, url, skip_host,
    // skip_accept_encoding).
    let args = unsafe { std::slice::from_raw_parts(argv, 5) };
    connection_putrequest_impl(
        args[0],
        args[1],
        args[2],
        value::truthy(args[3]),
        value::truthy(args[4]),
    )
}

fn connection_endheaders_impl(instance: i64, message_body: i64, encode_chunked: bool) -> i64 {
    let state = client_attribute(instance, "_state");
    if value::as_str(state) == Some(CS_REQ_STARTED) {
        let sent = client_text(CS_REQ_SENT);
        client_set(instance, "_state", sent);
    } else {
        return raise_cannot_send_header();
    }
    connection_send_output(instance, message_body, encode_chunked)
}

extern "C" fn connection_endheaders_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, message_body,
    // encode_chunked).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    connection_endheaders_impl(args[0], args[1], value::truthy(args[2]))
}

fn connection_expects_body(method: i64) -> bool {
    value::as_str(method)
        .map(|text| matches!(text.to_uppercase().as_str(), "PATCH" | "POST" | "PUT"))
        .unwrap_or(false)
}

/// `_get_content_length(body, method)`.
fn connection_content_length(body: i64, method: i64) -> Option<i64> {
    if body == NONE {
        return if connection_expects_body(method) {
            Some(0)
        } else {
            None
        };
    }
    if client_has(body, "read") {
        return None;
    }
    if let Some(bytes) = value::as_bytes(body) {
        return Some(bytes.len() as i64);
    }
    if let Some(text) = value::as_str(body) {
        return Some(text.chars().count() as i64);
    }
    None
}

/// Headers as `(name, value)` pairs from a mapping or an iterable of pairs.
fn connection_header_pairs(headers: i64) -> Option<Vec<(i64, i64)>> {
    if headers == NONE {
        return Some(Vec::new());
    }
    if let Some(items) = value::as_dict(headers) {
        return Some(items.to_vec());
    }
    let iterator = crate::iter::iter_of(headers);
    if exc::pending() {
        return None;
    }
    let root = heap::root_values(std::slice::from_ref(&iterator));
    let mut pairs = Vec::new();
    loop {
        let mut item = NONE;
        let status = crate::iter::iter_next(iterator, &mut item);
        if exc::pending() {
            heap::release_roots(root);
            return None;
        }
        if status == 0 {
            break;
        }
        let Some(tuple) = value::as_tuple(item) else {
            heap::release_roots(root);
            client_type_error("headers must be a mapping or an iterable of 2-tuples");
            return None;
        };
        let items = tuple.to_vec();
        if items.len() != 2 {
            heap::release_roots(root);
            client_type_error("header tuple must have exactly 2 items");
            return None;
        }
        pairs.push((items[0], items[1]));
    }
    heap::release_roots(root);
    Some(pairs)
}

fn connection_send_request(
    instance: i64,
    method: i64,
    url: i64,
    body: i64,
    headers: i64,
    encode_chunked: bool,
) -> i64 {
    let root = heap::root_values(&[instance, method, url, body, headers]);
    let Some(pairs) = connection_header_pairs(headers) else {
        heap::release_roots(root);
        return NONE;
    };
    let pairs_root = heap::root_values(
        &pairs
            .iter()
            .flat_map(|(name, value)| [*name, *value])
            .collect::<Vec<i64>>(),
    );
    let mut lower_names: Vec<String> = Vec::with_capacity(pairs.len());
    for (name, _) in &pairs {
        if let Some(text) = header_key(*name) {
            lower_names.push(text.to_lowercase());
        }
    }
    let skip_host = lower_names.iter().any(|name| name == "host");
    let skip_accept = lower_names.iter().any(|name| name == "accept-encoding");
    connection_putrequest_impl(instance, method, url, skip_host, skip_accept);
    if exc::pending() {
        heap::release_roots(pairs_root);
        heap::release_roots(root);
        return NONE;
    }
    let mut encode = encode_chunked;
    if !lower_names.iter().any(|name| name == "content-length") {
        if !lower_names.iter().any(|name| name == "transfer-encoding") {
            encode = false;
            match connection_content_length(body, method) {
                None => {
                    if body != NONE {
                        encode = true;
                        let name = client_text("Transfer-Encoding");
                        let part = client_text("chunked");
                        let inner = heap::root_values(&[name, part]);
                        connection_putheader_impl(instance, name, &[part]);
                        heap::release_roots(inner);
                    }
                }
                Some(length) => {
                    let name = client_text("Content-Length");
                    let part = client_text(&length.to_string());
                    let inner = heap::root_values(&[name, part]);
                    connection_putheader_impl(instance, name, &[part]);
                    heap::release_roots(inner);
                }
            }
        }
    } else {
        encode = false;
    }
    if exc::pending() {
        heap::release_roots(pairs_root);
        heap::release_roots(root);
        return NONE;
    }
    for (name, value) in &pairs {
        connection_putheader_impl(instance, *name, &[*value]);
        if exc::pending() {
            heap::release_roots(pairs_root);
            heap::release_roots(root);
            return NONE;
        }
    }
    let body = if let Some(text) = value::as_str(body) {
        match latin1_bytes(text, "body") {
            Some(bytes) => value::bytes_new(bytes),
            None => {
                heap::release_roots(pairs_root);
                heap::release_roots(root);
                return NONE;
            }
        }
    } else {
        body
    };
    connection_endheaders_impl(instance, body, encode);
    heap::release_roots(pairs_root);
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_request_entry(argv: *const i64) -> i64 {
    // SAFETY: six declared parameter slots (self, method, url, body, headers,
    // encode_chunked).
    let args = unsafe { std::slice::from_raw_parts(argv, 6) };
    connection_send_request(
        args[0],
        args[1],
        args[2],
        args[3],
        args[4],
        value::truthy(args[5]),
    )
}

extern "C" fn connection_send_request_entry(argv: *const i64) -> i64 {
    connection_request_entry(argv)
}

extern "C" fn connection_getresponse_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    let root = heap::root_values(std::slice::from_ref(&instance));
    let response = client_attribute(instance, "_response");
    if response != NONE {
        let closed = client_invoke(response, "isclosed", &[]);
        if !exc::pending() && value::truthy(closed) {
            client_set(instance, "_response", NONE);
        }
    }
    let response = client_attribute(instance, "_response");
    let state = client_attribute(instance, "_state");
    if value::as_str(state) != Some(CS_REQ_SENT) || response != NONE {
        heap::release_roots(root);
        return raise_response_not_ready(state);
    }
    let mut class = client_attribute(instance, "response_class");
    if class == NONE {
        class = response_class();
    }
    let sock = client_attribute(instance, "sock");
    let debuglevel = client_attribute(instance, "debuglevel");
    let method = client_attribute(instance, "_method");
    let class_root = heap::root_values(&[class, sock, debuglevel, method]);
    let call_args = [sock, debuglevel];
    let keyword_name = client_text("method");
    let keywords = [keyword_name, method];
    let created = crate::builtins::call_kw(class, 2, call_args.as_ptr(), 1, keywords.as_ptr());
    if exc::pending() {
        heap::release_roots(class_root);
        heap::release_roots(root);
        return NONE;
    }
    let response_root = heap::root_values(std::slice::from_ref(&created));
    client_invoke(created, "begin", &[]);
    if exc::pending() {
        client_invoke(created, "close", &[]);
        heap::release_roots(response_root);
        heap::release_roots(class_root);
        heap::release_roots(root);
        return NONE;
    }
    let idle = client_text(CS_IDLE);
    client_set(instance, "_state", idle);
    let will_close = value::truthy(client_attribute(created, "will_close"));
    if will_close {
        // CPython closes the connection and lets the response keep its
        // `makefile` handle; a raw-mode response reads the same socket, so the
        // socket is handed over instead of closed here.
        let raw = value::truthy(client_attribute(created, "raw"));
        let sock = client_attribute(instance, "sock");
        client_set(instance, "sock", NONE);
        client_set(instance, "_response", NONE);
        if raw {
            client_set(created, "_owns_sock", client_bool(true));
        } else if sock != NONE {
            let sock_root = heap::root_values(std::slice::from_ref(&sock));
            client_invoke(sock, "close", &[]);
            heap::release_roots(sock_root);
        }
    } else {
        client_set(instance, "_response", created);
    }
    heap::release_roots(response_root);
    heap::release_roots(class_root);
    heap::release_roots(root);
    if exc::pending() {
        NONE
    } else {
        created
    }
}

/// `_tunnel()`: CONNECT through the proxy and validate the reply.
fn connection_tunnel(instance: i64) -> i64 {
    let root = heap::root_values(std::slice::from_ref(&instance));
    let tunnel_host = client_attribute(instance, "_tunnel_host");
    let Some(tunnel_text) = value::as_str(tunnel_host) else {
        heap::release_roots(root);
        return client_type_error("tunnel host must be str");
    };
    if let Some(character) = tunnel_text
        .chars()
        .find(|character| (*character as u32) <= 0x20 || (*character as u32) == 0x7f)
    {
        heap::release_roots(root);
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!(
                "Tunnel host can't contain control characters {tunnel_text:?} (found at least {character:?})"
            ),
        );
    }
    let port = value::as_integer(client_attribute(instance, "_tunnel_port")).unwrap_or(443);
    let vsn_str = value::as_str(client_attribute(instance, "_http_vsn_str"))
        .unwrap_or("HTTP/1.1")
        .to_string();
    let mut connect_line = format!("CONNECT {tunnel_text}:{port} {vsn_str}\r\n").into_bytes();
    let headers = client_attribute(instance, "_tunnel_headers");
    let pairs: Vec<(i64, i64)> = value::as_dict(headers)
        .map(|pairs| pairs.to_vec())
        .unwrap_or_default();
    for (name, item) in pairs {
        let Some(name_bytes) = header_bytes(name) else {
            heap::release_roots(root);
            return client_type_error("tunnel header name must be str");
        };
        if !legal_header_name(&name_bytes) {
            heap::release_roots(root);
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("Invalid header name {}", value::repr(name)),
            );
        }
        let item_text = value::to_string(item);
        let Some(item_bytes) = latin1_bytes(&item_text, "header") else {
            heap::release_roots(root);
            return NONE;
        };
        if !legal_header_value(&item_bytes) {
            heap::release_roots(root);
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("Invalid header value {}", value::repr(item)),
            );
        }
        connect_line.extend_from_slice(&name_bytes);
        connect_line.extend_from_slice(b": ");
        connect_line.extend_from_slice(&item_bytes);
        connect_line.extend_from_slice(b"\r\n");
    }
    connect_line.extend_from_slice(b"\r\n");
    let payload = value::bytes_new(connect_line);
    let payload_root = heap::root_values(std::slice::from_ref(&payload));
    connection_send(instance, payload);
    heap::release_roots(payload_root);
    if exc::pending() {
        heap::release_roots(root);
        return NONE;
    }
    let mut class = client_attribute(instance, "response_class");
    if class == NONE {
        class = response_class();
    }
    let sock = client_attribute(instance, "sock");
    let method = client_attribute(instance, "_method");
    let class_root = heap::root_values(&[class, sock, method]);
    let call_args = [sock, client_integer(0)];
    let keyword_name = client_text("method");
    let keywords = [keyword_name, method];
    let response = crate::builtins::call_kw(class, 2, call_args.as_ptr(), 1, keywords.as_ptr());
    if exc::pending() {
        heap::release_roots(class_root);
        heap::release_roots(root);
        return NONE;
    }
    let response_root = heap::root_values(std::slice::from_ref(&response));
    client_invoke(response, "begin", &[]);
    if exc::pending() {
        client_invoke(response, "close", &[]);
        heap::release_roots(response_root);
        heap::release_roots(class_root);
        heap::release_roots(root);
        return NONE;
    }
    let status = client_attribute(response, "status");
    let headers_message = client_attribute(response, "headers");
    client_set(instance, "_raw_proxy_headers", client_bool(true));
    client_set(instance, "_proxy_headers_message", headers_message);
    if value::as_integer(status) != Some(200) {
        let reason = value::as_str(client_attribute(response, "reason"))
            .unwrap_or("")
            .trim()
            .to_string();
        let code = value::as_integer(status).unwrap_or(0);
        client_invoke(response, "close", &[]);
        connection_close_entry(std::slice::from_ref(&instance).as_ptr());
        heap::release_roots(response_root);
        heap::release_roots(class_root);
        heap::release_roots(root);
        return exc::raise_with_message(
            OS_ERROR,
            &format!("Tunnel connection failed: {code} {reason}"),
        );
    }
    client_invoke(response, "close", &[]);
    heap::release_roots(response_root);
    heap::release_roots(class_root);
    heap::release_roots(root);
    NONE
}

extern "C" fn connection_tunnel_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter slot (self).
    let instance = unsafe { *argv };
    connection_tunnel(instance)
}

fn connection_class() -> i64 {
    let self_only = || vec![client_positional("self")];
    let encode_chunked = || {
        client_param(
            "encode_chunked",
            hydro_ir::ParamKind::KwOnly,
            Some(client_bool(false)),
        )
    };
    client_class(
        "http.client.HTTPConnection",
        "HTTPConnection",
        NONE,
        vec![
            (
                "__init__",
                vec![
                    client_positional("self"),
                    client_positional("host"),
                    client_optional("port", NONE),
                    client_optional("timeout", NONE),
                    client_optional("source_address", NONE),
                    client_param(
                        "blocksize",
                        hydro_ir::ParamKind::KwOnly,
                        Some(client_integer(8192)),
                    ),
                ],
                connection_init_entry,
            ),
            (
                "_get_hostport",
                vec![
                    client_positional("self"),
                    client_positional("host"),
                    client_positional("port"),
                ],
                connection_get_hostport_entry,
            ),
            (
                "_validate_host",
                vec![client_positional("self"), client_positional("host")],
                connection_validate_host_entry,
            ),
            (
                "set_tunnel",
                vec![
                    client_positional("self"),
                    client_positional("host"),
                    client_optional("port", NONE),
                    client_optional("headers", NONE),
                ],
                connection_set_tunnel_entry,
            ),
            (
                "get_proxy_response_headers",
                self_only(),
                connection_get_proxy_headers_entry,
            ),
            (
                "set_debuglevel",
                vec![client_positional("self"), client_positional("level")],
                connection_set_debuglevel_entry,
            ),
            ("connect", self_only(), connection_connect_entry),
            ("close", self_only(), connection_close_entry),
            (
                "send",
                vec![client_positional("self"), client_positional("data")],
                connection_send_entry,
            ),
            (
                "_output",
                vec![client_positional("self"), client_positional("line")],
                connection_output_entry,
            ),
            (
                "_send_output",
                vec![
                    client_positional("self"),
                    client_optional("message_body", NONE),
                    encode_chunked(),
                ],
                connection_send_output_entry,
            ),
            (
                "putrequest",
                vec![
                    client_positional("self"),
                    client_positional("method"),
                    client_positional("url"),
                    client_optional("skip_host", client_bool(false)),
                    client_optional("skip_accept_encoding", client_bool(false)),
                ],
                connection_putrequest_entry,
            ),
            (
                "putheader",
                vec![
                    client_positional("self"),
                    client_positional("header"),
                    client_param("values", hydro_ir::ParamKind::VarArg, None),
                ],
                connection_putheader_entry,
            ),
            (
                "endheaders",
                vec![
                    client_positional("self"),
                    client_optional("message_body", NONE),
                    encode_chunked(),
                ],
                connection_endheaders_entry,
            ),
            (
                "request",
                vec![
                    client_positional("self"),
                    client_positional("method"),
                    client_positional("url"),
                    client_optional("body", NONE),
                    client_optional("headers", NONE),
                    encode_chunked(),
                ],
                connection_request_entry,
            ),
            (
                "_send_request",
                vec![
                    client_positional("self"),
                    client_positional("method"),
                    client_positional("url"),
                    client_positional("body"),
                    client_positional("headers"),
                    client_positional("encode_chunked"),
                ],
                connection_send_request_entry,
            ),
            ("getresponse", self_only(), connection_getresponse_entry),
            ("_tunnel", self_only(), connection_tunnel_entry),
            ("__enter__", self_only(), connection_enter_entry),
            (
                "__exit__",
                vec![
                    client_positional("self"),
                    client_optional("exc_type", NONE),
                    client_optional("exc_value", NONE),
                    client_optional("traceback", NONE),
                ],
                connection_exit_entry,
            ),
        ],
        vec![
            ("_http_vsn", client_integer(11)),
            ("_http_vsn_str", client_text("HTTP/1.1")),
            ("default_port", client_integer(80)),
            ("auto_open", client_integer(1)),
            ("debuglevel", client_integer(0)),
            ("response_class", response_class()),
        ],
    )
}

fn https_connection_class() -> i64 {
    let self_only = || vec![client_positional("self")];
    client_class(
        "http.client.HTTPSConnection",
        "HTTPSConnection",
        connection_class(),
        vec![
            (
                "__init__",
                vec![
                    client_positional("self"),
                    client_positional("host"),
                    client_optional("port", NONE),
                    client_optional("timeout", NONE),
                    client_optional("source_address", NONE),
                    client_param("context", hydro_ir::ParamKind::KwOnly, Some(NONE)),
                    client_param(
                        "blocksize",
                        hydro_ir::ParamKind::KwOnly,
                        Some(client_integer(8192)),
                    ),
                ],
                https_init_entry,
            ),
            ("connect", self_only(), https_connect_entry),
        ],
        vec![("default_port", client_integer(443))],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_line_headers_and_query() {
        let head = parse_head(
            b"GET /api/devices?x=1 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close",
        )
        .expect("valid request");
        assert_eq!(head.method, b"GET");
        assert_eq!(head.path, b"/api/devices");
        assert_eq!(head.body_len, 0);
        assert!(!head.keep_alive);
    }

    #[test]
    fn rejects_malformed_and_chunked_requests() {
        assert!(parse_head(b"GET / HTTP/1.1 extra").is_err());
        assert!(parse_head(b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked").is_err());
        assert!(parse_head(b"GET / HTTP/1.1\r\nContent-Length: nope").is_err());
    }

    #[test]
    fn header_end_and_token_matching() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\n"), Some(14));
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
        assert!(contains_token(b"keep-alive, Close", b"close"));
        assert!(!contains_token(b"keep-alive", b"close"));
    }

    #[test]
    fn extracts_buffered_requests_and_waits_for_partial_body() {
        let mut buffer = b"POST /x HTTP/1.1\r\nContent-Length: 5\r\n\r\nhel".to_vec();
        let pending_len = buffer.len();
        assert!(take_request(&mut buffer)
            .expect("a partial body is not an error")
            .is_none());
        // Nothing is consumed until the body is complete.
        assert_eq!(buffer.len(), pending_len);

        buffer.extend_from_slice(b"loGET /y HTTP/1.1\r\n\r\n");
        let first = take_request(&mut buffer)
            .expect("valid request")
            .expect("complete request");
        assert_eq!(first.method, b"POST");
        assert_eq!(first.path, b"/x");
        assert_eq!(first.body, b"hello");
        assert!(first.keep_alive);

        let second = take_request(&mut buffer)
            .expect("valid request")
            .expect("pipelined request");
        assert_eq!(second.method, b"GET");
        assert_eq!(second.path, b"/y");
        assert!(second.body.is_empty());
        assert!(buffer.is_empty());
        assert!(take_request(&mut buffer).expect("empty buffer").is_none());
    }

    #[test]
    fn leaves_partial_head_buffered_and_rejects_overlong_heads() {
        let mut partial = b"GET / HTTP/1.1\r\nHost: x".to_vec();
        let partial_len = partial.len();
        assert!(take_request(&mut partial).expect("partial head").is_none());
        assert_eq!(partial.len(), partial_len);

        let mut oversized = vec![b'a'; MAX_HEADER_BYTES + 1];
        assert!(take_request(&mut oversized).is_err());

        // A complete head with a body still missing stays buffered too.
        let mut body = b"PUT /z HTTP/1.1\r\nContent-Length: 3\r\n\r\n".to_vec();
        let body_len = body.len();
        assert!(take_request(&mut body).expect("partial body").is_none());
        assert_eq!(body.len(), body_len);
    }

    #[test]
    fn client_headers_unfold_and_skip_malformed_lines() {
        let lines: Vec<Vec<u8>> = vec![
            b"First: val\r\n".to_vec(),
            b": nval\r\n".to_vec(),
            b"obs-fold: text\r\n".to_vec(),
            b" folded with space\r\n".to_vec(),
            b"\tfolded with tab\r\n".to_vec(),
            b"Content-Length: 0\r\n".to_vec(),
            b"\r\n".to_vec(),
        ];
        let pairs = parse_header_lines(&lines);
        assert_eq!(
            pairs,
            vec![
                ("First".to_string(), "val".to_string()),
                (
                    "obs-fold".to_string(),
                    "text\n folded with space\n\tfolded with tab".to_string()
                ),
                ("Content-Length".to_string(), "0".to_string()),
            ]
        );
    }

    #[test]
    fn client_header_name_and_value_rules_match_cpython() {
        assert!(legal_header_name(b"ValidName"));
        assert!(legal_header_name(b"Key Space"));
        assert!(legal_header_name(b"KeySpace "));
        assert!(!legal_header_name(b"Invalid:Name"));
        assert!(!legal_header_name(b":InvalidName"));
        assert!(!legal_header_name(b" InvalidName"));
        assert!(!legal_header_name(b"Invalid\r\nName"));
        assert!(!legal_header_name(b""));

        assert!(legal_header_value(b"is\r\n allowed"));
        assert!(legal_header_value(b"is\r\n\tallowed"));
        assert!(!legal_header_value(b"Invalid\r\nValue"));
        assert!(!legal_header_value(b"InvalidValue\r\n"));
        assert!(!legal_header_value(b"next\nline"));
    }

    #[test]
    fn client_host_port_parsing_matches_cpython() {
        exc::init();
        let cases: &[(&str, Option<&str>, i64)] = &[
            ("www.python.org", Some("www.python.org"), 80),
            ("www.python.org:80", Some("www.python.org"), 80),
            ("www.python.org:", Some("www.python.org"), 80),
            ("[fe80::207:e9ff:fe9b]", Some("fe80::207:e9ff:fe9b"), 80),
            ("[fe80::207:e9ff:fe9b]:", Some("fe80::207:e9ff:fe9b"), 80),
            (
                "[fe80::207:e9ff:fe9b]:8000",
                Some("fe80::207:e9ff:fe9b"),
                8000,
            ),
        ];
        for (input, expected_host, expected_port) in cases {
            let host = client_text(input);
            let parsed = connection_host_port(host, NONE, 80);
            assert!(!exc::pending(), "{input}");
            let (name, port) = parsed.expect("parsed");
            assert_eq!(name.as_str(), expected_host.unwrap());
            assert_eq!(port, *expected_port);
        }
        for bad in ["www.python.org:abc", "user:password@www.python.org"] {
            let host = client_text(bad);
            assert!(connection_host_port(host, NONE, 80).is_none(), "{bad}");
            assert!(exc::pending(), "{bad}");
            exc::clear();
        }
    }

    #[test]
    fn client_message_lookup_is_case_insensitive() {
        exc::init();
        let pairs = vec![
            ("Content-Length".to_string(), "0".to_string()),
            ("Set-Cookie".to_string(), "a=1".to_string()),
            ("Set-Cookie".to_string(), "b=2".to_string()),
        ];
        let message = message_from_pairs(&pairs);
        assert!(!exc::pending());
        assert_eq!(
            message_get_text(message, "content-length").as_deref(),
            Some("0")
        );
        assert_eq!(
            message_get_text(message, "CONTENT-LENGTH").as_deref(),
            Some("0")
        );
        let cookies = message_get_all_values(message, "set-cookie").expect("cookies");
        assert_eq!(cookies.len(), 2);
        assert_eq!(message_len(message), 3);
        assert!(message_get_text(message, "missing").is_none());
    }

    #[test]
    fn client_classes_are_cached_and_wired() {
        exc::init();
        assert_eq!(constant("HTTPMessage"), message_class());
        assert_eq!(constant("HTTPResponse"), response_class());
        assert_eq!(constant("HTTPConnection"), connection_class());
        assert_eq!(constant("HTTPSConnection"), https_connection_class());
        assert_eq!(
            value::class_display_name(constant("HTTPConnection")),
            Some("HTTPConnection")
        );
        assert_eq!(
            value::class_display_name(constant("HTTPSConnection")),
            Some("HTTPSConnection")
        );
        assert_eq!(constant("HTTP_PORT"), client_integer(80));
        assert_eq!(constant("responses"), responses_dict());
        assert_eq!(constant("error"), http_exception_class());
        assert!(!exc::pending());
    }
}
