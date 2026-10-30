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

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
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
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'http' has no attribute '{function}'"),
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
}
