//! `memcached`: native ASCII-protocol client.
//!
//! A connection is a capsule backing a [`MemcachedClient`]. The client owns the
//! TCP stream and lazily reconnects after an I/O or protocol failure. Every
//! socket operation parks the calling thread for the collector through
//! [`heap::blocking_enter`]/[`heap::blocking_leave`]; command buffers are
//! built from copied Rust data, so no Hydro object is allocated while parked.
//!
//! `set` sends one `set <key> 0 <expiration> <bytes>\r\n<value>\r\n` command
//! and reads a single CRLF-terminated response line. `STORED` returns `True`;
//! anything else (`ERROR`, `CLIENT_ERROR`, `SERVER_ERROR`, ...) raises
//! `OSError` naming the reply and resets the stream. Keys are restricted to
//! 1–250 bytes without whitespace or control bytes, and response lines are
//! capped, so no read is unbounded.
//!
//! Capsules are never freed: the leaked client lives for the process lifetime,
//! matching the fixed connection pool the benchmark app creates.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::value::{self, TRUE};

/// TCP connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum bytes accepted in a key (the protocol limit).
const MAX_KEY_LENGTH: usize = 250;

/// Maximum bytes read for one response line, excluding the terminator.
const MAX_LINE: usize = 512;

/// Capsule name that tags a `memcached` connection handle.
const CAPSULE_NAME: &str = "hydro.memcached";

/// A capsule-backed memcached connection.
struct MemcachedClient {
    host: String,
    port: u16,
    /// `None` after an I/O or protocol failure; the next call reconnects.
    stream: Mutex<Option<TcpStream>>,
}

/// Locks the connection slot, recovering from a poisoned mutex.
fn lock_stream(client: &MemcachedClient) -> MutexGuard<'_, Option<TcpStream>> {
    client
        .stream
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Borrows the client behind a validated capsule handle.
fn client_argument(argument: i64) -> Option<&'static MemcachedClient> {
    match value::as_capsule(argument) {
        Some((pointer, Some(name))) if name == CAPSULE_NAME && pointer != 0 => {
            // SAFETY: only `connect` creates this capsule, and it stores the
            // pointer of an intentionally leaked `MemcachedClient`.
            Some(unsafe { &*(pointer as *const MemcachedClient) })
        }
        _ => None,
    }
}

/// Reads a `str` argument into an owned buffer, raising `TypeError` otherwise.
fn text_argument(args: &[i64], index: usize, function: &str, name: &str) -> Result<String, i64> {
    match value::as_str(args[index]) {
        Some(text) => Ok(text.to_string()),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("{function}() argument '{name}' must be str"),
        )),
    }
}

/// Validates a TCP port argument.
fn port_argument(args: &[i64], index: usize, function: &str) -> Result<u16, i64> {
    let Some(raw) = value::as_integer(args[index]) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("{function}() argument 'port' must be int"),
        ));
    };
    match u16::try_from(raw) {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(exc::raise_with_message(
            VALUE_ERROR,
            &format!("{function}() port must be in range 1..65535"),
        )),
    }
}

/// Reads the `value` operand: `str` is encoded with UTF-8, bytes-like values
/// are copied verbatim.
fn value_argument(argument: i64) -> Option<Vec<u8>> {
    if let Some(text) = value::as_str(argument) {
        return Some(text.as_bytes().to_vec());
    }
    value::as_bytes(argument).map(|bytes| bytes.to_vec())
}

/// Validates the `expiration` operand (a non-negative 32-bit second count).
fn expiration_argument(args: &[i64], index: usize) -> Result<u32, i64> {
    let Some(raw) = value::as_integer(args[index]) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            "set() argument 'expiration' must be int",
        ));
    };
    u32::try_from(raw).map_err(|_| {
        exc::raise_with_message(
            VALUE_ERROR,
            "set() expiration must be a non-negative 32-bit integer",
        )
    })
}

/// Resolves `host`/`port`, parking for the collector while the resolver runs.
fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    heap::blocking_enter();
    let result = (host, port).to_socket_addrs();
    heap::blocking_leave();
    let mut addresses =
        result.map_err(|error| format!("could not resolve {host}:{port}: {error}"))?;
    addresses
        .next()
        .ok_or_else(|| format!("could not resolve {host}:{port}: no addresses"))
}

/// `read_exact` on the stream, parking the thread for the collector.
fn read_exact(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<(), String> {
    heap::blocking_enter();
    let result = stream.read_exact(buffer);
    heap::blocking_leave();
    result.map_err(|error| format!("memcached connection error: {error}"))
}

/// `write_all` on the stream, parking the thread for the collector.
fn write_all(stream: &mut TcpStream, buffer: &[u8]) -> Result<(), String> {
    heap::blocking_enter();
    let result = stream.write_all(buffer);
    heap::blocking_leave();
    result.map_err(|error| format!("memcached connection error: {error}"))
}

/// Opens a connection, parking for the collector while connecting.
fn open_connection(host: &str, port: u16) -> Result<TcpStream, String> {
    let address = resolve(host, port)?;
    heap::blocking_enter();
    let result = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT);
    heap::blocking_leave();
    result.map_err(|error| format!("memcached connect to {host}:{port} failed: {error}"))
}

/// Validates a key: non-empty, at most 250 bytes, no whitespace or control
/// bytes.
fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("memcached key must not be empty".to_string());
    }
    if key.len() > MAX_KEY_LENGTH {
        return Err(format!(
            "memcached key is longer than {MAX_KEY_LENGTH} bytes"
        ));
    }
    if key.bytes().any(|byte| byte <= b' ' || byte == 0x7f) {
        return Err("memcached key contains whitespace or control bytes".to_string());
    }
    Ok(())
}

/// Encodes one `set` command, including the terminating CRLF of the value.
fn build_set_command(key: &str, value: &[u8], expiration: u32) -> Result<Vec<u8>, String> {
    validate_key(key)?;
    let mut command = Vec::with_capacity(key.len() + value.len() + 32);
    command.extend_from_slice(b"set ");
    command.extend_from_slice(key.as_bytes());
    command.extend_from_slice(b" 0 ");
    command.extend_from_slice(expiration.to_string().as_bytes());
    command.push(b' ');
    command.extend_from_slice(value.len().to_string().as_bytes());
    command.extend_from_slice(b"\r\n");
    command.extend_from_slice(value);
    command.extend_from_slice(b"\r\n");
    Ok(command)
}

/// Reads one CRLF-terminated response line, capped at [`MAX_LINE`] bytes.
fn read_line(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    loop {
        if line.len() >= MAX_LINE {
            return Err("memcached response line is too long".to_string());
        }
        let mut byte = [0u8; 1];
        read_exact(stream, &mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(line)
}

/// Sends one command and interprets the single response line.
fn exchange(stream: &mut TcpStream, command: &[u8]) -> Result<(), String> {
    write_all(stream, command)?;
    let line = read_line(stream)?;
    if matches!(line.as_slice(), b"STORED") {
        Ok(())
    } else {
        Err(format!(
            "memcached set failed: {}",
            String::from_utf8_lossy(&line)
        ))
    }
}

/// `memcached.connect(host, port)`.
fn connect(args: &[i64]) -> i64 {
    let host = match text_argument(args, 0, "connect", "host") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let port = match port_argument(args, 1, "connect") {
        Ok(value) => value,
        Err(error) => return error,
    };

    match open_connection(&host, port) {
        Ok(stream) => {
            let client = MemcachedClient {
                host,
                port,
                stream: Mutex::new(Some(stream)),
            };
            let pointer = Box::into_raw(Box::new(client)) as usize;
            value::capsule_new(pointer, Some(CAPSULE_NAME.to_string()))
        }
        Err(message) => exc::raise_with_message(OS_ERROR, &message),
    }
}

/// `memcached.set(handle, key, value, expiration) -> bool`.
fn set(args: &[i64]) -> i64 {
    let Some(client) = client_argument(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "set() argument 'handle' must be a memcached connection capsule",
        );
    };
    let key = match text_argument(args, 1, "set", "key") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let Some(value) = value_argument(args[2]) else {
        return exc::raise_with_message(TYPE_ERROR, "set() argument 'value' must be bytes or str");
    };
    let expiration = match expiration_argument(args, 3) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let command = match build_set_command(&key, &value, expiration) {
        Ok(command) => command,
        Err(message) => return exc::raise_with_message(VALUE_ERROR, &message),
    };

    // Take the connection out of the shared slot so the mutex is not held
    // while the socket blocks the thread.
    let mut stream = {
        let mut slot = lock_stream(client);
        slot.take()
    };
    let mut stream = match stream.take() {
        Some(stream) => stream,
        None => match open_connection(&client.host, client.port) {
            Ok(stream) => stream,
            Err(message) => return exc::raise_with_message(OS_ERROR, &message),
        },
    };

    match exchange(&mut stream, &command) {
        Ok(()) => {
            *lock_stream(client) = Some(stream);
            TRUE
        }
        // Dropping the stream closes it; the next call reconnects.
        Err(message) => exc::raise_with_message(OS_ERROR, &message),
    }
}

/// Dispatches `memcached.*` calls.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "connect" => connect(args),
        "set" => set(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'memcached' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_validated() {
        assert!(validate_key("devices:1").is_ok());
        assert!(validate_key("a").is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key("a b").is_err());
        assert!(validate_key("a\tb").is_err());
        assert!(validate_key("a\rb").is_err());
        assert!(validate_key("\u{7f}").is_err());
        assert!(validate_key(&"k".repeat(MAX_KEY_LENGTH + 1)).is_err());
    }

    #[test]
    fn set_command_is_encoded_exactly() {
        let command = build_set_command("k1", b"hello", 60).unwrap();
        assert_eq!(command, b"set k1 0 60 5\r\nhello\r\n".to_vec());

        let empty = build_set_command("k", b"", 0).unwrap();
        assert_eq!(empty, b"set k 0 0 0\r\n\r\n".to_vec());

        assert!(build_set_command("bad key", b"v", 0).is_err());
    }

    #[test]
    fn values_accept_str_and_bytes() {
        exc::init();
        assert_eq!(
            value_argument(value::str_alloc("text".to_string())),
            Some(b"text".to_vec())
        );
        assert_eq!(
            value_argument(value::bytes_new(vec![0, 1, 2])),
            Some(vec![0, 1, 2])
        );
        assert_eq!(value_argument(TRUE), None);
        assert_eq!(value_argument(value::FALSE), None);
    }
}
