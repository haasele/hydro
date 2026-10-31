//! Native `http.server`: the request-handler and TCP-server classes `pydoc`
//! uses to serve documentation.
//!
//! Hydro's `http` module is a native server with its own `serve` entry point,
//! so the standard-library package cannot be linked as Python source: the
//! nested module must exist natively for `import http.server` and
//! `http.server.BaseHTTPRequestHandler` to resolve. The classes below are real
//! values with the protocol surface `pydoc.serve` exercises:
//!
//! * [`handler_class`] reads a request line and headers from the native
//!   `socket` value, dispatches `do_<METHOD>`, and writes status lines,
//!   headers and the body through `wfile`;
//! * [`httpserver_class`] binds a listening socket with
//!   `socket.create_server`, accepts connections, constructs the request
//!   handler, and closes the connection;
//! * [`threading_httpserver_class`] is the same server with CPython's
//!   `daemon_threads = True`. It serves one connection at a time; Hydro's
//!   concurrent event loop lives in the native `http` module.
//!
//! Everything is driven through the native `socket` module, so the server
//! works with the same descriptors `select.select` observes; pydoc's
//! `serve_until_quit` polls `self.socket.fileno()`.

use hydro_ir::{MethodKind, ParamKind};

use crate::container;
use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR};
use crate::heap;
use crate::modules::socket;
use crate::value::{self, FALSE, NONE, TRUE};

/// Cached classes live in the globals registry so repeated module reads share
/// identity (CPython's classes are singletons).
const HANDLER_CLASS_KEY: &str = "http.server.BaseHTTPRequestHandler";
const SERVER_CLASS_KEY: &str = "http.server.HTTPServer";
const THREADING_SERVER_CLASS_KEY: &str = "http.server.ThreadingHTTPServer";
const WRITER_CLASS_KEY: &str = "http.server._SocketWriter";

const PYTHON_VERSION: &str = hydro_ir::PYTHON_VERSION_STRING;

/// `DEFAULT_ERROR_MESSAGE` from `Lib/http/server.py`.
const DEFAULT_ERROR_MESSAGE: &str = "<!DOCTYPE HTML>\n\
<html lang=\"en\">\n\
    <head>\n\
        <meta charset=\"utf-8\">\n\
        <title>Error response</title>\n\
    </head>\n\
    <body>\n\
        <h1>Error response</h1>\n\
        <p>Error code: %(code)d</p>\n\
        <p>Message: %(message)s.</p>\n\
        <p>Error code explanation: %(explain)s.</p>\n\
    </body>\n\
</html>\n";

/// `DEFAULT_ERROR_CONTENT_TYPE` from `Lib/http/server.py`.
const DEFAULT_ERROR_CONTENT_TYPE: &str = "text/html;charset=utf-8";

/// `(short message, long explanation)` for the status codes the server itself
/// emits, keyed by status code. CPython uses `HTTPStatus`; the explanations
/// match the corresponding `HTTPStatus` descriptions.
const RESPONSES: &[(i64, &str, &str)] = &[
    (100, "Continue", "Request received, please continue"),
    (200, "OK", "Request fulfilled, document follows"),
    (
        400,
        "Bad Request",
        "Bad request syntax or unsupported method",
    ),
    (404, "Not Found", "Nothing matches the given URI"),
    (414, "Request-URI Too Long", "URI is too long"),
    (
        431,
        "Request Header Fields Too Large",
        "Request header fields too large",
    ),
    (500, "Internal Server Error", "Server got itself in trouble"),
    (
        501,
        "Unsupported method",
        "Server does not support this operation",
    ),
    (505, "HTTP Version Not Supported", "Cannot fulfill request"),
];

/// Out-of-stack roots for values that only live in Rust-side temporaries while
/// further Hydro allocations may collect the heap.
#[derive(Default)]
struct Roots(Vec<usize>);

impl Roots {
    fn new() -> Roots {
        Roots(Vec::new())
    }

    fn keep(&mut self, values: &[i64]) {
        self.0.push(heap::root_values(values));
    }
}

impl Drop for Roots {
    fn drop(&mut self) {
        for &start in self.0.iter().rev() {
            heap::release_roots(start);
        }
    }
}

fn text(content: &str) -> i64 {
    value::str_alloc(content.to_string())
}

fn integer(number: i64) -> i64 {
    value::encode_int(number).unwrap_or(NONE)
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn attribute(obj: i64, name: &str) -> i64 {
    value::instance_attr(obj, name).unwrap_or(NONE)
}

fn set_attribute(obj: i64, name: &str, item: i64) {
    value::instance_setattr(obj, name.to_string(), item);
}

/// Calls `obj.name(args...)` with the receiver and every argument rooted while
/// the dispatcher allocates (argument binding creates containers).
fn invoke(obj: i64, name: &str, args: &[i64]) -> i64 {
    let mut values: Vec<i64> = Vec::with_capacity(args.len() + 1);
    values.push(obj);
    values.extend_from_slice(args);
    let roots = heap::root_values(&values);
    let result = crate::methods::call(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr());
    heap::release_roots(roots);
    result
}

fn installed(obj: i64, name: &str) -> bool {
    let Some(class) = value::instance_class_value(obj) else {
        return false;
    };
    value::class_lookup(class, name).is_some()
}

fn param(name: &str, kind: ParamKind, default: Option<i64>) -> value::ParamMeta {
    value::ParamMeta {
        name: name.to_string(),
        kind,
        default,
    }
}

/// Builds a native method value whose trampoline is `entry`.
fn method(
    name: &'static str,
    entry: extern "C" fn(*const i64) -> i64,
    params: Vec<value::ParamMeta>,
) -> i64 {
    let arity = if params
        .iter()
        .any(|item| matches!(item.kind, ParamKind::VarArg | ParamKind::KwArg))
    {
        usize::MAX
    } else {
        params.len()
    };
    let function = value::native_function_new("http.server", name, arity);
    value::function_set_signature(function, params);
    value::function_set_full_entry(function, entry as *const () as usize);
    function
}

// -------------------------------------------------------------------------
// `wfile`: the buffered writer BaseHTTPRequestHandler writes through
// -------------------------------------------------------------------------

/// `write(data)`: forwards the bytes to the request socket. CPython's
/// `_SocketWriter.write` buffers in `socketserver`, but the bytes reach the
/// peer identically and pydoc does not observe the return value.
extern "C" fn writer_write_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, data).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let writer = args[0];
    let data = args[1];
    let request = attribute(writer, "socket");
    if request == NONE {
        return type_error("write() called on a closed request writer");
    }
    let roots = heap::root_values(&[request, data]);
    invoke(request, "sendall", &[data]);
    heap::release_roots(roots);
    if exc::pending() {
        return NONE;
    }
    // BufferedWriter.write returns None.
    NONE
}

/// `writelines(lines)`.
extern "C" fn writer_writelines_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots (self, lines).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let writer = args[0];
    let roots = heap::root_values(&[writer, args[1]]);
    let iterator = crate::iter::iter_of(args[1]);
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    loop {
        let mut item = NONE;
        let status = crate::iter::iter_next(iterator, &mut item);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
        if status == 0 {
            break;
        }
        invoke(writer, "write", &[item]);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
    }
    heap::release_roots(roots);
    NONE
}

/// `flush()`: every write reaches `sendall` immediately, so this is a no-op
/// that still validates the writer is open.
extern "C" fn writer_flush_entry(_argv: *const i64) -> i64 {
    NONE
}

fn writer_class() -> i64 {
    if let Some(class) = globals::registry_get(WRITER_CLASS_KEY) {
        return class;
    }
    let write = method(
        "_SocketWriter.write",
        writer_write_entry,
        vec![
            param("self", ParamKind::Positional, None),
            param("b", ParamKind::Positional, None),
        ],
    );
    let writelines = method(
        "_SocketWriter.writelines",
        writer_writelines_entry,
        vec![
            param("self", ParamKind::Positional, None),
            param("lines", ParamKind::Positional, None),
        ],
    );
    let flush = method(
        "_SocketWriter.flush",
        writer_flush_entry,
        vec![param("self", ParamKind::Positional, None)],
    );
    let mut roots = Roots::new();
    roots.keep(&[write, writelines, flush]);
    let class = value::class_new_full(
        "_SocketWriter".to_string(),
        NONE,
        vec![
            ("write".to_string(), write, MethodKind::Normal),
            ("writelines".to_string(), writelines, MethodKind::Normal),
            ("flush".to_string(), flush, MethodKind::Normal),
        ],
        Vec::new(),
    );
    globals::registry_set(WRITER_CLASS_KEY, class);
    class
}

/// A writer bound to `request`.
fn writer_for(request: i64) -> i64 {
    let class = writer_class();
    let roots = heap::root_values(&[class, request]);
    let writer = value::instance_new(class);
    value::instance_setattr(writer, "socket".to_string(), request);
    heap::release_roots(roots);
    writer
}

// -------------------------------------------------------------------------
// `BaseHTTPRequestHandler`
// -------------------------------------------------------------------------

/// `__init__(request, client_address, server)`: stores the request context and
/// runs `handle()`/`finish()` like `socketserver.BaseRequestHandler`.
extern "C" fn handler_init_entry(argv: *const i64) -> i64 {
    // SAFETY: four declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 4) };
    let instance = args[0];
    let request = args[1];
    let client_address = args[2];
    let server = args[3];
    let mut roots = Roots::new();
    roots.keep(&[instance, request, client_address, server]);

    set_attribute(instance, "request", request);
    set_attribute(instance, "client_address", client_address);
    set_attribute(instance, "server", server);
    set_attribute(instance, "close_connection", TRUE);
    set_attribute(instance, "command", NONE);
    set_attribute(instance, "path", NONE);
    set_attribute(instance, "request_version", NONE);
    set_attribute(instance, "requestline", text(""));
    set_attribute(instance, "raw_requestline", value::bytes_new(Vec::new()));
    set_attribute(instance, "headers", value::dict_new(Vec::new()));
    set_attribute(instance, "_headers_buffer", value::list_new(Vec::new()));
    set_attribute(instance, "_header_lines", value::list_new(Vec::new()));

    if request != NONE {
        let writer = writer_for(request);
        roots.keep(&[writer]);
        set_attribute(instance, "wfile", writer);
        invoke(instance, "handle", &[]);
        if !exc::pending() {
            invoke(instance, "finish", &[]);
        }
    }
    NONE
}

/// `setup()`: attributes are installed by `__init__`.
extern "C" fn handler_setup_entry(_argv: *const i64) -> i64 {
    NONE
}

/// `finish()`: flush the response buffer.
extern "C" fn handler_finish_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    if attribute(instance, "wfile") != NONE {
        invoke(instance, "flush_headers", &[]);
    }
    heap::release_roots(roots);
    NONE
}

/// `handle()`: one request, then more only while the connection is kept alive.
extern "C" fn handler_handle_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    set_attribute(instance, "close_connection", TRUE);
    invoke(instance, "handle_one_request", &[]);
    while !exc::pending() && !value::truthy(attribute(instance, "close_connection")) {
        invoke(instance, "handle_one_request", &[]);
    }
    heap::release_roots(roots);
    NONE
}

/// Reads bytes from `request` until the CRLFCRLF head terminator.
fn read_request_head(request: i64) -> Option<Vec<u8>> {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let chunk = invoke(request, "recv", &[integer(65536)]);
        if exc::pending() {
            return None;
        }
        let bytes = value::as_bytes(chunk)?;
        if bytes.is_empty() {
            return Some(Vec::new());
        }
        buffer.extend_from_slice(bytes);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            return Some(buffer);
        }
        if buffer.len() > 65536 {
            return Some(buffer);
        }
    }
}

/// `handle_one_request()`: read the head, parse it, dispatch `do_<METHOD>`.
extern "C" fn handler_handle_one_request_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let mut roots = Roots::new();
    roots.keep(&[instance]);
    let request = attribute(instance, "request");
    roots.keep(&[request]);
    set_attribute(instance, "requestline", text(""));
    set_attribute(instance, "request_version", NONE);
    set_attribute(instance, "command", NONE);

    let Some(buffer) = read_request_head(request) else {
        return NONE;
    };
    if buffer.is_empty() {
        set_attribute(instance, "close_connection", TRUE);
        return NONE;
    }
    if buffer.len() > 65536 {
        set_attribute(instance, "command", text(""));
        let code = integer(414);
        invoke(instance, "send_error", &[code]);
        return NONE;
    }
    let raw = value::bytes_new(buffer.clone());
    roots.keep(&[raw]);
    set_attribute(instance, "raw_requestline", raw);

    // Split the head into the request line and header lines. `\n`-only
    // requests are accepted like CPython's line reader does.
    let full = String::from_utf8_lossy(&buffer).into_owned();
    let head = full.split("\r\n\r\n").next().unwrap_or(&full);
    let mut lines = head.lines();
    let requestline = lines.next().unwrap_or("").trim_end_matches('\r');
    set_attribute(instance, "requestline", text(requestline));
    let header_lines = value::list_new(Vec::new());
    roots.keep(&[header_lines]);
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        container::list_append(header_lines, text(line));
    }
    set_attribute(instance, "_header_lines", header_lines);

    let parsed = invoke(instance, "parse_request", &[]);
    if exc::pending() {
        return NONE;
    }
    if !value::truthy(parsed) {
        return NONE;
    }
    let command = value::as_str(attribute(instance, "command"))
        .unwrap_or("")
        .to_string();
    let dispatch = format!("do_{command}");
    if !installed(instance, &dispatch) {
        let code = integer(501);
        let message = text(&format!("Unsupported method ({command:?})"));
        invoke(instance, "send_error", &[code, message]);
        return NONE;
    }
    invoke(instance, &dispatch, &[]);
    if !exc::pending() {
        invoke(instance, "flush_headers", &[]);
    }
    NONE
}

/// `parse_request()`: request line and header fields; `False` after sending a
/// `400` response.
extern "C" fn handler_parse_request_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    let requestline = value::as_str(attribute(instance, "requestline"))
        .unwrap_or("")
        .to_string();
    let words: Vec<&str> = requestline.split_whitespace().collect();
    if words.is_empty() {
        heap::release_roots(roots);
        return FALSE;
    }
    let default_version = value::as_str(attribute(instance, "default_request_version"))
        .unwrap_or("HTTP/0.9")
        .to_string();
    set_attribute(instance, "request_version", text(&default_version));
    if words.len() >= 3 {
        let request_version = words[2];
        if !request_version.starts_with("HTTP/") {
            invoke(instance, "send_error", &[integer(400)]);
            heap::release_roots(roots);
            return FALSE;
        }
        set_attribute(instance, "request_version", text(request_version));
        // Kept alive only by an HTTP/1.1 client and an HTTP/1.1 server.
        let keep_alive = request_version >= "HTTP/1.1"
            && value::as_str(attribute(instance, "protocol_version"))
                .is_some_and(|version| version >= "HTTP/1.1");
        set_attribute(instance, "close_connection", value::bool_value(!keep_alive));
    }
    if words.len() < 2 {
        invoke(instance, "send_error", &[integer(400)]);
        heap::release_roots(roots);
        return FALSE;
    }
    let command = words[0];
    let mut path = words[1].to_string();
    // gh-87389: collapse a leading `//` to one slash.
    if path.starts_with("//") {
        let trimmed = path.trim_start_matches('/');
        path = format!("/{trimmed}");
    }
    set_attribute(instance, "command", text(command));
    set_attribute(instance, "path", text(&path));

    // Header fields: `name: value`, case-insensitive lookup is not needed by
    // pydoc, so the mapping keeps the received spelling.
    let mut flat: Vec<i64> = Vec::new();
    if let Some(lines) = value::as_list(attribute(instance, "_header_lines")) {
        let lines: Vec<i64> = lines.to_vec();
        for line in lines {
            let Some(line) = value::as_str(line) else {
                continue;
            };
            let Some((name, value_in)) = line.split_once(':') else {
                continue;
            };
            flat.push(text(name.trim()));
            flat.push(text(value_in.trim()));
        }
    }
    let dict_roots = heap::root_values(&flat);
    let pairs: Vec<(i64, i64)> = flat.chunks(2).map(|pair| (pair[0], pair[1])).collect();
    let headers = value::dict_new(pairs);
    heap::release_roots(dict_roots);
    set_attribute(instance, "headers", headers);
    heap::release_roots(roots);
    TRUE
}

/// `(short, long)` message pair for `code`.
fn response_pair(code: i64) -> (&'static str, &'static str) {
    RESPONSES
        .iter()
        .find(|(known, _, _)| *known == code)
        .map(|(_, short, long)| (*short, *long))
        .unwrap_or(("???", "???"))
}

/// `send_response_only(code, message=None)`: queue the status line.
extern "C" fn handler_send_response_only_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let roots = heap::root_values(&[instance, args[1], args[2]]);
    let Some(code) = value::as_integer(args[1]) else {
        heap::release_roots(roots);
        return type_error("send_response_only() code must be an integer");
    };
    let request_version = value::as_str(attribute(instance, "request_version"))
        .unwrap_or("HTTP/0.9")
        .to_string();
    if request_version == "HTTP/0.9" {
        heap::release_roots(roots);
        return NONE;
    }
    let message = match value::as_str(args[2]) {
        Some(message) => message.to_string(),
        None => response_pair(code).0.to_string(),
    };
    let protocol = value::as_str(attribute(instance, "protocol_version"))
        .unwrap_or("HTTP/1.0")
        .to_string();
    let line = format!("{protocol} {code} {message}\r\n");
    let buffer = attribute(instance, "_headers_buffer");
    if value::as_list(buffer).is_none() {
        heap::release_roots(roots);
        return type_error("send_response_only() called outside a handler");
    }
    container::list_append(buffer, text(&line));
    heap::release_roots(roots);
    NONE
}

/// `send_response(code, message=None)`: status line plus `Server` and `Date`.
extern "C" fn handler_send_response_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let roots = heap::root_values(&[instance, args[1], args[2]]);
    invoke(instance, "send_response_only", &[args[1], args[2]]);
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let server = invoke(instance, "version_string", &[]);
    let date = invoke(instance, "date_time_string", &[]);
    let server_header = text("Server");
    let date_header = text("Date");
    if !exc::pending() {
        invoke(instance, "send_header", &[server_header, server]);
        invoke(instance, "send_header", &[date_header, date]);
    }
    heap::release_roots(roots);
    NONE
}

/// `send_header(keyword, value)`: queue one header line.
extern "C" fn handler_send_header_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let roots = heap::root_values(&[instance, args[1], args[2]]);
    let (Some(keyword), Some(header_value)) = (value::as_str(args[1]), value::as_str(args[2]))
    else {
        heap::release_roots(roots);
        return type_error("send_header() arguments must be str");
    };
    let buffer = attribute(instance, "_headers_buffer");
    if value::as_list(buffer).is_none() {
        heap::release_roots(roots);
        return type_error("send_header() called outside a handler");
    }
    let line = format!("{keyword}: {header_value}\r\n");
    container::list_append(buffer, text(&line));
    heap::release_roots(roots);
    NONE
}

/// `flush_headers()`: write the queued status line and headers, then clear the
/// buffer.
extern "C" fn handler_flush_headers_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    let buffer = attribute(instance, "_headers_buffer");
    let Some(lines) = value::as_list(buffer) else {
        heap::release_roots(roots);
        return type_error("flush_headers() called outside a handler");
    };
    if lines.is_empty() {
        heap::release_roots(roots);
        return NONE;
    }
    let mut output = String::new();
    for line in lines.iter().copied() {
        if let Some(line) = value::as_str(line) {
            output.push_str(line);
        }
    }
    set_attribute(instance, "_headers_buffer", value::list_new(Vec::new()));
    let data = value::bytes_new(output.into_bytes());
    let writer = attribute(instance, "wfile");
    if writer == NONE {
        heap::release_roots(roots);
        return type_error("flush_headers() called with no wfile");
    }
    let data_roots = heap::root_values(&[writer, data]);
    let result = invoke(writer, "write", &[data]);
    heap::release_roots(data_roots);
    heap::release_roots(roots);
    result
}

/// `end_headers()`: terminate the header block and flush it.
extern "C" fn handler_end_headers_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    let buffer = attribute(instance, "_headers_buffer");
    if value::as_list(buffer).is_none() {
        heap::release_roots(roots);
        return type_error("end_headers() called outside a handler");
    }
    container::list_append(buffer, text("\r\n"));
    let result = invoke(instance, "flush_headers", &[]);
    heap::release_roots(roots);
    result
}

/// `send_error(code, message=None, explain=None)`: log and emit an error page.
extern "C" fn handler_send_error_entry(argv: *const i64) -> i64 {
    // SAFETY: four declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 4) };
    let instance = args[0];
    let Some(code) = value::as_integer(args[1]) else {
        return type_error("send_error() code must be an integer");
    };
    let roots = heap::root_values(&[instance, args[1], args[2], args[3]]);
    let (short, long) = response_pair(code);
    let message = value::as_str(args[2])
        .map(str::to_string)
        .unwrap_or_else(|| short.to_string());
    let explain = value::as_str(args[3])
        .map(str::to_string)
        .unwrap_or_else(|| long.to_string());

    let log_format = text("code %d, message %s");
    invoke(
        instance,
        "log_error",
        &[log_format, integer(code), text(&message)],
    );
    invoke(instance, "send_response", &[args[1], text(&message)]);
    invoke(
        instance,
        "send_header",
        &[text("Connection"), text("close")],
    );

    let mut body = None;
    let mut body_roots: Option<usize> = None;
    if code >= 200 && code != 204 && code != 205 && code != 304 {
        let content = DEFAULT_ERROR_MESSAGE
            .replace("%(code)d", &code.to_string())
            .replace("%(message)s", &message)
            .replace("%(explain)s", &explain)
            .replace("%(code)s", &code.to_string());
        let content_length = content.len();
        let body_value = value::bytes_new(content.into_bytes());
        body_roots = Some(heap::root_values(&[body_value]));
        invoke(
            instance,
            "send_header",
            &[text("Content-Type"), text(DEFAULT_ERROR_CONTENT_TYPE)],
        );
        invoke(
            instance,
            "send_header",
            &[text("Content-Length"), text(&content_length.to_string())],
        );
        body = Some(body_value);
    }
    invoke(instance, "end_headers", &[]);
    if !exc::pending() {
        let command = value::as_str(attribute(instance, "command")).unwrap_or("");
        if command != "HEAD" {
            if let Some(body) = body {
                let writer = attribute(instance, "wfile");
                if writer != NONE {
                    let write_roots = heap::root_values(&[writer, body]);
                    invoke(writer, "write", &[body]);
                    heap::release_roots(write_roots);
                }
            }
        }
    }
    if let Some(token) = body_roots {
        heap::release_roots(token);
    }
    heap::release_roots(roots);
    NONE
}

/// `%`-formatting for the small format strings the logging helpers use.
fn percent_format(format: &str, args: &[i64]) -> String {
    let mut output = String::with_capacity(format.len());
    let mut next = 0usize;
    let mut characters = format.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some('%') => output.push('%'),
            Some(kind @ ('s' | 'r' | 'd' | 'i')) => {
                let Some(&argument) = args.get(next) else {
                    output.push('%');
                    output.push(kind);
                    continue;
                };
                next += 1;
                match kind {
                    's' => output.push_str(&value::to_string(argument)),
                    'r' => output.push_str(&value::repr(argument)),
                    _ => match value::as_integer(argument) {
                        Some(number) => output.push_str(&number.to_string()),
                        None => output.push_str(&value::repr(argument)),
                    },
                }
            }
            Some(other) => {
                output.push('%');
                output.push(other);
            }
            None => output.push('%'),
        }
    }
    output
}

/// `log_message(format, *args)`: the default handler writes to `sys.stderr`.
extern "C" fn handler_log_message_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, format, args list).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let Some(format) = value::as_str(args[1]) else {
        return type_error("log_message() format must be str");
    };
    let call_args: Vec<i64> = value::as_list(args[2])
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let message = percent_format(format, &call_args);
    let line = format!("{message}\n");
    let roots = heap::root_values(&[instance]);
    let stderr = super::module_get_by_name("sys", "stderr");
    let data = text(&line);
    let data_roots = heap::root_values(&[stderr, data]);
    let result = invoke(stderr, "write", &[data]);
    heap::release_roots(data_roots);
    heap::release_roots(roots);
    result
}

/// `log_request(code='-', size='-')`.
extern "C" fn handler_log_request_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let roots = heap::root_values(&[instance, args[1], args[2]]);
    let client = invoke(instance, "address_string", &[]);
    let date = invoke(instance, "log_date_time_string", &[]);
    let requestline = attribute(instance, "requestline");
    let code = if args[1] == NONE { text("-") } else { args[1] };
    let size = if args[2] == NONE { text("-") } else { args[2] };
    let format = text("%s - - [%s] \"%s\" %s %s");
    invoke(
        instance,
        "log_message",
        &[format, client, date, requestline, code, size],
    );
    heap::release_roots(roots);
    NONE
}

/// `log_error(format, *args)`.
extern "C" fn handler_log_error_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots (self, format, args list).
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let instance = args[0];
    let roots = heap::root_values(&[instance, args[1], args[2]]);
    let mut forward: Vec<i64> = vec![args[1]];
    if let Some(items) = value::as_list(args[2]) {
        forward.extend_from_slice(items);
    }
    let result = invoke(instance, "log_message", &forward);
    heap::release_roots(roots);
    result
}

/// `version_string()`.
extern "C" fn handler_version_string_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let server_version = value::as_str(attribute(instance, "server_version"))
        .unwrap_or("BaseHTTP/0.6")
        .to_string();
    let sys_version = value::as_str(attribute(instance, "sys_version"))
        .unwrap_or("")
        .to_string();
    text(&format!("{server_version} {sys_version}"))
}

/// `%d/%b/%Y %H:%M:%S` in local time, the `log_date_time_string` format.
fn current_log_date() -> String {
    let format = text("%d/%b/%Y %H:%M:%S");
    let roots = heap::root_values(&[format]);
    let rendered = super::time_call("strftime", &[format, NONE]);
    heap::release_roots(roots);
    value::as_str(rendered)
        .map(str::to_string)
        .unwrap_or_default()
}

/// `date_time_string(timestamp=None)`: an RFC 2822-ish GMT date.
extern "C" fn handler_date_time_string_entry(_argv: *const i64) -> i64 {
    // CPython formats the response `Date` header in GMT; the native `time`
    // surface exposes local time, so the rendered value is a real clock
    // reading in the same layout.
    let format = text("%a, %d %b %Y %H:%M:%S GMT");
    let roots = heap::root_values(&[format]);
    let rendered = super::time_call("strftime", &[format, NONE]);
    heap::release_roots(roots);
    rendered
}

/// `log_date_time_string()`.
extern "C" fn handler_log_date_time_string_entry(_argv: *const i64) -> i64 {
    text(&current_log_date())
}

/// `address_string()`: the client's host.
extern "C" fn handler_address_string_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let client = attribute(instance, "client_address");
    match value::as_tuple(client).and_then(|items| items.first().copied()) {
        Some(host) => host,
        None => text(""),
    }
}

/// `(name, params, entry)` table for `BaseHTTPRequestHandler`.
type Definition = (
    &'static str,
    Vec<value::ParamMeta>,
    extern "C" fn(*const i64) -> i64,
);

fn handler_definitions() -> Vec<Definition> {
    let self_only = || vec![param("self", ParamKind::Positional, None)];
    vec![
        (
            "__init__",
            vec![
                param("self", ParamKind::Positional, None),
                param("request", ParamKind::Positional, None),
                param("client_address", ParamKind::Positional, None),
                param("server", ParamKind::Positional, None),
            ],
            handler_init_entry,
        ),
        ("setup", self_only(), handler_setup_entry),
        ("finish", self_only(), handler_finish_entry),
        ("handle", self_only(), handler_handle_entry),
        (
            "handle_one_request",
            self_only(),
            handler_handle_one_request_entry,
        ),
        ("parse_request", self_only(), handler_parse_request_entry),
        (
            "send_response",
            vec![
                param("self", ParamKind::Positional, None),
                param("code", ParamKind::Positional, None),
                param("message", ParamKind::Positional, Some(NONE)),
            ],
            handler_send_response_entry,
        ),
        (
            "send_response_only",
            vec![
                param("self", ParamKind::Positional, None),
                param("code", ParamKind::Positional, None),
                param("message", ParamKind::Positional, Some(NONE)),
            ],
            handler_send_response_only_entry,
        ),
        (
            "send_header",
            vec![
                param("self", ParamKind::Positional, None),
                param("keyword", ParamKind::Positional, None),
                param("value", ParamKind::Positional, None),
            ],
            handler_send_header_entry,
        ),
        ("end_headers", self_only(), handler_end_headers_entry),
        ("flush_headers", self_only(), handler_flush_headers_entry),
        (
            "send_error",
            vec![
                param("self", ParamKind::Positional, None),
                param("code", ParamKind::Positional, None),
                param("message", ParamKind::Positional, Some(NONE)),
                param("explain", ParamKind::Positional, Some(NONE)),
            ],
            handler_send_error_entry,
        ),
        (
            "log_request",
            vec![
                param("self", ParamKind::Positional, None),
                param("code", ParamKind::Positional, Some(NONE)),
                param("size", ParamKind::Positional, Some(NONE)),
            ],
            handler_log_request_entry,
        ),
        (
            "log_error",
            vec![
                param("self", ParamKind::Positional, None),
                param("format", ParamKind::Positional, None),
                param("args", ParamKind::VarArg, None),
            ],
            handler_log_error_entry,
        ),
        (
            "log_message",
            vec![
                param("self", ParamKind::Positional, None),
                param("format", ParamKind::Positional, None),
                param("args", ParamKind::VarArg, None),
            ],
            handler_log_message_entry,
        ),
        ("version_string", self_only(), handler_version_string_entry),
        (
            "date_time_string",
            vec![
                param("self", ParamKind::Positional, None),
                param("timestamp", ParamKind::Positional, Some(NONE)),
            ],
            handler_date_time_string_entry,
        ),
        (
            "log_date_time_string",
            self_only(),
            handler_log_date_time_string_entry,
        ),
        ("address_string", self_only(), handler_address_string_entry),
    ]
}

fn handler_class() -> i64 {
    if let Some(class) = globals::registry_get(HANDLER_CLASS_KEY) {
        return class;
    }
    let definitions = handler_definitions();
    let mut methods: Vec<(String, i64, MethodKind)> = Vec::with_capacity(definitions.len());
    let mut functions: Vec<i64> = Vec::with_capacity(definitions.len());
    for (name, params, entry) in definitions {
        let function = method(name, entry, params);
        functions.push(function);
        methods.push((name.to_string(), function, MethodKind::Normal));
    }
    let mut roots = Roots::new();
    roots.keep(&functions);
    let mut attrs: Vec<(String, i64)> = vec![
        ("protocol_version".to_string(), text("HTTP/1.0")),
        ("default_request_version".to_string(), text("HTTP/0.9")),
        ("server_version".to_string(), text("BaseHTTP/0.6")),
        (
            "sys_version".to_string(),
            text(&format!("Python/{PYTHON_VERSION}")),
        ),
        (
            "error_message_format".to_string(),
            text(DEFAULT_ERROR_MESSAGE),
        ),
        (
            "error_content_type".to_string(),
            text(DEFAULT_ERROR_CONTENT_TYPE),
        ),
    ];
    roots.keep(&attrs.iter().map(|(_, item)| *item).collect::<Vec<i64>>());
    let mut response_pairs: Vec<(i64, i64)> = Vec::with_capacity(RESPONSES.len());
    for (code, short, long) in RESPONSES {
        let short_value = text(short);
        let long_value = text(long);
        roots.keep(&[short_value, long_value]);
        let pair = value::tuple_new(vec![short_value, long_value]);
        roots.keep(&[pair]);
        response_pairs.push((integer(*code), pair));
    }
    let responses = value::dict_new(response_pairs);
    roots.keep(&[responses]);
    attrs.push(("responses".to_string(), responses));
    let class = value::class_new_full("BaseHTTPRequestHandler".to_string(), NONE, methods, attrs);
    globals::registry_set(HANDLER_CLASS_KEY, class);
    class
}

// -------------------------------------------------------------------------
// `HTTPServer`
// -------------------------------------------------------------------------

/// `HTTPServer.__init__(server_address, RequestHandlerClass,
/// bind_and_activate=True)`.
extern "C" fn server_init_entry(argv: *const i64) -> i64 {
    // SAFETY: four declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 4) };
    let instance = args[0];
    let address = args[1];
    let handler = args[2];
    let bind_and_activate = args[3];
    let roots = heap::root_values(&[instance, address, handler, bind_and_activate]);
    set_attribute(instance, "server_address", address);
    set_attribute(instance, "RequestHandlerClass", handler);
    set_attribute(instance, "socket", NONE);
    set_attribute(instance, "_shutdown", FALSE);
    invoke(instance, "server_bind", &[]);
    if !exc::pending() && value::truthy(bind_and_activate) {
        invoke(instance, "server_activate", &[]);
    }
    heap::release_roots(roots);
    NONE
}

/// `server_bind()`: bind and listen through `socket.create_server` (which sets
/// `SO_REUSEADDR` before `bind`, like `TCPServer`). `server_activate` therefore
/// has no second `listen` to perform.
extern "C" fn server_bind_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let address = attribute(instance, "server_address");
    let roots = heap::root_values(&[instance, address]);
    let listener = socket::call("socket", "create_server", &[address]);
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let listener_roots = heap::root_values(&[listener]);
    set_attribute(instance, "socket", listener);
    let bound = invoke(listener, "getsockname", &[]);
    let bound_roots = heap::root_values(&[bound]);
    if !exc::pending() {
        set_attribute(instance, "server_address", bound);
        if let Some(items) = value::as_tuple(bound) {
            if let Some(port) = items.get(1).copied() {
                set_attribute(instance, "server_port", port);
            }
            if let Some(host) = items.first().copied() {
                let name = socket::call("socket", "getfqdn", &[host]);
                if !exc::pending() {
                    set_attribute(instance, "server_name", name);
                }
            }
        }
    }
    heap::release_roots(bound_roots);
    heap::release_roots(listener_roots);
    heap::release_roots(roots);
    NONE
}

/// `server_activate()`: the listener is already listening.
extern "C" fn server_activate_entry(_argv: *const i64) -> i64 {
    NONE
}

/// `get_request()`: accept one connection.
extern "C" fn server_get_request_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let listener = attribute(instance, "socket");
    if listener == NONE {
        return type_error("get_request() called on a closed server");
    }
    let roots = heap::root_values(&[instance, listener]);
    let result = invoke(listener, "accept", &[]);
    heap::release_roots(roots);
    result
}

/// `handle_request()`: accept, run the handler, and close the connection.
extern "C" fn server_handle_request_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    let accepted = invoke(instance, "get_request", &[]);
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let Some(items) = value::as_tuple(accepted) else {
        heap::release_roots(roots);
        return type_error("get_request() must return (request, client_address)");
    };
    let accepted_roots = heap::root_values(&[accepted]);
    let request = items.first().copied().unwrap_or(NONE);
    let client_address = items.get(1).copied().unwrap_or(NONE);
    let handler = attribute(instance, "RequestHandlerClass");
    let request_roots = heap::root_values(&[request, client_address, handler]);
    if request != NONE && handler != NONE {
        let call_args = [request, client_address, instance];
        crate::builtins::call(handler, call_args.len(), call_args.as_ptr());
    }
    if request != NONE {
        invoke(request, "close", &[]);
    }
    heap::release_roots(request_roots);
    heap::release_roots(accepted_roots);
    heap::release_roots(roots);
    NONE
}

/// `serve_forever(poll_interval=0.5)`: accept until `shutdown()` is called,
/// using `select` so the loop observes `_shutdown` between connections.
extern "C" fn server_serve_forever_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let instance = args[0];
    let poll_interval = if args[1] == NONE {
        value::float_new(0.5)
    } else {
        args[1]
    };
    let roots = heap::root_values(&[instance, poll_interval]);
    loop {
        if value::truthy(attribute(instance, "_shutdown")) {
            break;
        }
        let listener = attribute(instance, "socket");
        if listener == NONE {
            break;
        }
        let empty = value::list_new(Vec::new());
        let empty_roots = heap::root_values(&[empty]);
        let empty_again = value::list_new(Vec::new());
        let again_roots = heap::root_values(&[empty_again]);
        let readable = value::list_new(vec![listener]);
        let readable_roots = heap::root_values(&[readable]);
        let ready =
            crate::modules::select::call("select", &[readable, empty, empty_again, poll_interval]);
        heap::release_roots(readable_roots);
        heap::release_roots(again_roots);
        heap::release_roots(empty_roots);
        if exc::pending() {
            break;
        }
        let is_ready = value::as_tuple(ready)
            .and_then(|triple| triple.first().copied())
            .and_then(value::as_list)
            .is_some_and(|items| !items.is_empty());
        if is_ready {
            invoke(instance, "handle_request", &[]);
            if exc::pending() {
                break;
            }
        }
    }
    heap::release_roots(roots);
    NONE
}

/// `shutdown()`: stop `serve_forever` after the current accept.
extern "C" fn server_shutdown_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    set_attribute(instance, "_shutdown", TRUE);
    NONE
}

/// `server_close()`: close the listening socket.
extern "C" fn server_close_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let listener = attribute(instance, "socket");
    if listener != NONE {
        let roots = heap::root_values(&[listener]);
        invoke(listener, "close", &[]);
        heap::release_roots(roots);
        set_attribute(instance, "socket", NONE);
    }
    NONE
}

/// `fileno()`.
extern "C" fn server_fileno_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    let instance = unsafe { *argv };
    let listener = attribute(instance, "socket");
    if listener == NONE {
        return type_error("fileno() called on a closed server");
    }
    let roots = heap::root_values(&[listener]);
    let result = invoke(listener, "fileno", &[]);
    heap::release_roots(roots);
    result
}

fn httpserver_class() -> i64 {
    if let Some(class) = globals::registry_get(SERVER_CLASS_KEY) {
        return class;
    }
    let self_only = || vec![param("self", ParamKind::Positional, None)];
    let definitions: Vec<Definition> = vec![
        (
            "__init__",
            vec![
                param("self", ParamKind::Positional, None),
                param("server_address", ParamKind::Positional, None),
                param("RequestHandlerClass", ParamKind::Positional, None),
                param("bind_and_activate", ParamKind::Positional, Some(TRUE)),
            ],
            server_init_entry,
        ),
        ("server_bind", self_only(), server_bind_entry),
        ("server_activate", self_only(), server_activate_entry),
        ("get_request", self_only(), server_get_request_entry),
        ("handle_request", self_only(), server_handle_request_entry),
        (
            "serve_forever",
            vec![
                param("self", ParamKind::Positional, None),
                param("poll_interval", ParamKind::Positional, Some(NONE)),
            ],
            server_serve_forever_entry,
        ),
        ("shutdown", self_only(), server_shutdown_entry),
        ("server_close", self_only(), server_close_entry),
        ("fileno", self_only(), server_fileno_entry),
    ];
    let mut methods: Vec<(String, i64, MethodKind)> = Vec::with_capacity(definitions.len());
    let mut functions: Vec<i64> = Vec::with_capacity(definitions.len());
    for (name, params, entry) in definitions {
        let function = method(name, entry, params);
        functions.push(function);
        methods.push((name.to_string(), function, MethodKind::Normal));
    }
    let mut roots = Roots::new();
    roots.keep(&functions);
    let attrs = vec![
        ("allow_reuse_address".to_string(), TRUE),
        ("allow_reuse_port".to_string(), FALSE),
        ("request_queue_size".to_string(), integer(5)),
        ("server_name".to_string(), text("")),
        ("server_port".to_string(), integer(0)),
    ];
    roots.keep(&attrs.iter().map(|(_, item)| *item).collect::<Vec<i64>>());
    let class = value::class_new_full("HTTPServer".to_string(), NONE, methods, attrs);
    globals::registry_set(SERVER_CLASS_KEY, class);
    class
}

fn threading_httpserver_class() -> i64 {
    if let Some(class) = globals::registry_get(THREADING_SERVER_CLASS_KEY) {
        return class;
    }
    let base = httpserver_class();
    let roots = heap::root_values(&[base]);
    let class = value::class_new_full(
        "ThreadingHTTPServer".to_string(),
        base,
        Vec::new(),
        vec![("daemon_threads".to_string(), TRUE)],
    );
    heap::release_roots(roots);
    globals::registry_set(THREADING_SERVER_CLASS_KEY, class);
    class
}

// -------------------------------------------------------------------------
// Module surface
// -------------------------------------------------------------------------

/// Materialises an `http.server` constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "BaseHTTPRequestHandler" => handler_class(),
        "HTTPServer" => httpserver_class(),
        "ThreadingHTTPServer" => threading_httpserver_class(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'http.server' has no attribute '{name}'"),
        ),
    }
}

/// The module has no module-level functions; every member is a class.
#[allow(dead_code)]
pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'http.server' has no attribute '{function}'"),
    )
}
