//! `http`: a native HTTP/1.1 server with a static-response fast path.
//!
//! `http.serve(port, routes, handler, max_connections)` binds one listener and
//! blocks for the rest of the process. For every accepted connection a native
//! worker thread parses requests; a request whose exact `(method, path)` pair
//! (the query string is stripped) appears in `routes` is answered by copying
//! the pre-built response bytes to the socket and never enters Python. Every
//! other request calls the Python `handler(method, path, body)` from the
//! worker thread and writes the returned `bytes` object verbatim.
//!
//! ## Thread and GC safety
//!
//! * `serve` calls [`heap::enable_shared`] before the first worker exists, so
//!   every value reachable from the Python heap (including the handler) lives
//!   in the shared heap.
//! * The handler is installed as a process-wide root
//!   ([`heap::add_global_root`]) before any worker starts; it therefore stays
//!   alive across collections and after the calling thread's registration
//!   ends.
//! * Each worker follows the `_thread.start_new_thread` protocol:
//!   `enter_thread` before the OS thread exists, then `register_thread` (with
//!   no out-of-stack roots) and a safepoint as its first action, and
//!   `leave_thread` from a `Drop` guard. The guard also releases the
//!   connection permit.
//! * The three `bytes` arguments and the handler result are only reachable
//!   from the worker's Rust stack, which the collector may not scan precisely,
//!   so both the argument buffer and the result are registered with
//!   `heap::root_values` for the duration of the call and the byte copy.
//! * Blocking socket operations (accept, read, write) park the thread with
//!   `blocking_enter`/`blocking_leave`; no runtime lock is held across a
//!   block. Waiting for a free connection slot polls with the same parking,
//!   so a stop-the-world collection can always complete.
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

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

/// Largest request head (request line plus headers) we buffer.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Largest `Content-Length` request body we accept.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Read chunk size; a request head is assembled from these slices.
const READ_CHUNK: usize = 16 * 1024;
/// Poll interval while the connection limit is exhausted.
const SLOT_POLL: Duration = Duration::from_millis(2);

/// `(method, path) -> complete response bytes` table. Nested maps let both
/// halves of the key be looked up as borrowed byte slices, so a static request
/// performs no allocation.
type Routes = HashMap<Vec<u8>, HashMap<Vec<u8>, Arc<[u8]>>>;

/// Immutable state shared by every connection worker.
struct Config {
    routes: Routes,
    handler: i64,
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

    // Bind before switching the runtime to the shared heap: a bind failure is
    // a plain Python exception with no side effects.
    let listener = match TcpListener::bind(("0.0.0.0", port as u16)) {
        Ok(listener) => listener,
        Err(error) => return io_error(error),
    };

    // Every object created so far (including the handler) moves into the
    // shared heap before the first worker can observe it, and the handler is
    // rooted for the rest of the run: workers are detached and the route table
    // retains no Python value.
    heap::enable_shared();
    heap::add_global_root(handler);

    let config = Arc::new(Config { routes, handler });
    accept_loop(listener, config, max_connections);
    NONE
}

/// Accepts connections until the process exits.
fn accept_loop(listener: TcpListener, config: Arc<Config>, max_connections: usize) {
    let limit = Arc::new(Limit {
        active: Mutex::new(0),
        max: max_connections,
    });
    loop {
        heap::blocking_enter();
        let accepted = listener.accept();
        heap::blocking_leave();
        let stream = match accepted {
            Ok((stream, _peer)) => stream,
            Err(error) => {
                // A failed accept (descriptor exhaustion, ...) is transient;
                // the listener stays open. Back off while parked so an
                // unreadable error cannot spin this thread.
                if error.kind() != io::ErrorKind::Interrupted {
                    heap::blocking_enter();
                    thread::sleep(SLOT_POLL);
                    heap::blocking_leave();
                }
                continue;
            }
        };

        // The permit is acquired before the thread id exists, so a connection
        // waiting for a slot is not counted as a live thread.
        let permit = acquire(&limit);
        let config = Arc::clone(&config);
        let id = heap::new_thread_id();
        // Count the thread before it starts so the collector sees it.
        heap::enter_thread();
        let spawned = thread::Builder::new()
            .name(format!("hydro-http-{id}"))
            .spawn(move || {
                heap::register_thread(id, Vec::new());
                // The guard releases the slot and the GC registration even if
                // the connection handler unwinds.
                let _exit = WorkerExit { _permit: permit };
                heap::safepoint();
                handle_connection(stream, config);
            });
        if spawned.is_err() {
            // The closure (and its permit) was dropped; only the pre-counted
            // live thread needs rolling back.
            heap::leave_unstarted_thread();
        }
    }
}

/// Caps the number of concurrently running connection threads.
struct Limit {
    active: Mutex<usize>,
    max: usize,
}

/// Owns one active connection slot; releasing is `Drop`-based so a spawn
/// failure or panic cannot leak capacity.
struct Permit {
    limit: Arc<Limit>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut active = self
            .limit
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
    }
}

/// Blocks until a slot is free, parking for the collector while it waits.
fn acquire(limit: &Arc<Limit>) -> Permit {
    loop {
        let mut active = limit
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *active < limit.max {
            *active += 1;
            drop(active);
            return Permit {
                limit: Arc::clone(limit),
            };
        }
        drop(active);
        // No runtime lock is held; the collector may stop the world while this
        // thread waits.
        heap::blocking_enter();
        thread::sleep(SLOT_POLL);
        heap::blocking_leave();
    }
}

/// Marks the worker as exited. Field order drops the permit after the GC
/// registration is gone.
struct WorkerExit {
    _permit: Permit,
}

impl Drop for WorkerExit {
    fn drop(&mut self) {
        heap::leave_thread();
    }
}

/// Serves every request on one connection until it closes.
fn handle_connection(mut stream: TcpStream, config: Arc<Config>) {
    let mut buffer: Vec<u8> = Vec::new();
    // EOF, I/O error or malformed request ends only this connection.
    while let Ok(Some(request)) = next_request(&mut stream, &mut buffer) {
        let static_response = config
            .routes
            .get(request.method.as_slice())
            .and_then(|paths| paths.get(request.path.as_slice()));
        if let Some(response) = static_response {
            // No Python call on the static path; the response bytes are
            // already the complete HTTP message.
            if write_all(&mut stream, &response[..]).is_err() {
                break;
            }
        } else {
            // The handler owns the complete response. A raised exception or a
            // non-bytes result closes the connection.
            let Some(response) = call_handler(
                config.handler,
                &request.method,
                &request.path,
                &request.body,
            ) else {
                break;
            };
            if write_all(&mut stream, &response).is_err() {
                break;
            }
        }

        if !request.keep_alive {
            break;
        }
    }
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
    heap::add_thread_root(method_value);
    let path_value = value::bytes_new(path.to_vec());
    heap::add_thread_root(path_value);
    let body_value = value::bytes_new(body.to_vec());
    heap::add_thread_root(body_value);

    let argv = [method_value, path_value, body_value];
    let result = crate::builtins::call(handler, argv.len(), argv.as_ptr());
    // Same rule for the result: root it before reading its payload.
    heap::add_thread_root(result);
    let response = if exc::pending() {
        let _ = exc::clear();
        None
    } else {
        value::as_bytes(result).map(|bytes| bytes.to_vec())
    };
    heap::release_roots(mark);
    response
}

/// Reads (or re-parses from the buffer) the next request.
///
/// Leftover bytes after the body belong to the next pipelined request and stay
/// in `buffer`.
fn next_request(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> io::Result<Option<Request>> {
    loop {
        if let Some(header_end) = find_header_end(buffer) {
            if header_end > MAX_HEADER_BYTES {
                return Err(bad_request("request head is too large"));
            }
            let head = parse_head(&buffer[..header_end])?;
            let body_start = header_end + 4;
            let total = body_start + head.body_len;
            if buffer.len() >= total {
                let body = buffer[body_start..total].to_vec();
                buffer.drain(..total);
                return Ok(Some(Request {
                    method: head.method,
                    path: head.path,
                    body,
                    keep_alive: head.keep_alive,
                }));
            }
        } else if buffer.len() > MAX_HEADER_BYTES {
            return Err(bad_request("request head is too large"));
        }
        if !read_more(stream, buffer)? {
            return Ok(None);
        }
    }
}

/// Reads one chunk from the socket, parking for the collector while blocked.
fn read_more(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> io::Result<bool> {
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        heap::blocking_enter();
        let read = stream.read(&mut chunk);
        heap::blocking_leave();
        match read {
            Ok(0) => return Ok(false),
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                return Ok(true);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// `write_all` with collector parking around the blocking call.
fn write_all(stream: &mut TcpStream, bytes: &[u8]) -> io::Result<()> {
    heap::blocking_enter();
    let result = stream.write_all(bytes);
    heap::blocking_leave();
    result
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
}
