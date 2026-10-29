//! `postgres`: native PostgreSQL v3 (simple query) client.
//!
//! A connection is a capsule backing a [`PgClient`]. The client owns the TCP
//! stream, the startup credentials, and a lazy reconnect slot. Every socket
//! operation parks the calling thread for the collector through
//! [`heap::blocking_enter`]/[`heap::blocking_leave`]; argument values are
//! copied into ordinary Rust buffers before any I/O, so no Hydro object is
//! allocated while parked.
//!
//! `connect` performs the TCP connect (2 s timeout) and the startup handshake
//! without authentication (the server runs with `POSTGRES_HOST_AUTH_METHOD=trust`);
//! any authentication request other than `AuthenticationOk` is reported as an
//! `OSError`. `insert` builds one parameter-free simple-query `INSERT`
//! statement, reads messages until `ReadyForQuery` (`Z`), and returns the
//! `id` of the first `DataRow` (`D`) from `RETURNING id`.
//!
//! Table names are restricted to ASCII `[A-Za-z_][A-Za-z0-9_]*` and string
//! values double `'` and reject control characters, so callers never splice
//! SQL. All protocol reads are length-checked and bounded. An I/O or protocol
//! failure closes the stream, so the next `insert` reconnects.
//!
//! Capsules are never freed: the leaked client lives for the process lifetime,
//! matching the fixed connection pools the benchmark app creates.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::ops;
use crate::value;

/// PostgreSQL protocol version 3.0.
const PROTOCOL_VERSION_3: i32 = 196_608;

/// Upper bound for a single backend message; prevents unbounded allocation
/// from a malformed or hostile server.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// TCP connect timeout used by `connect` and lazy reconnects.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Capsule name that tags a `postgres` connection handle.
const CAPSULE_NAME: &str = "hydro.postgres";

/// A capsule-backed PostgreSQL connection.
struct PgClient {
    host: String,
    port: u16,
    user: String,
    database: String,
    /// `None` after a protocol/I/O failure; the next call reconnects.
    stream: Mutex<Option<TcpStream>>,
}

/// Locks the connection slot, recovering from a poisoned mutex: a panic never
/// leaves the stream in a half-written state because it is only replaced
/// between complete operations.
fn lock_stream(client: &PgClient) -> MutexGuard<'_, Option<TcpStream>> {
    client
        .stream
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Borrows the client behind a validated capsule handle.
fn client_argument(argument: i64) -> Option<&'static PgClient> {
    match value::as_capsule(argument) {
        Some((pointer, Some(name))) if name == CAPSULE_NAME && pointer != 0 => {
            // SAFETY: only `connect` creates this capsule, and it stores the
            // pointer of an intentionally leaked `PgClient`.
            Some(unsafe { &*(pointer as *const PgClient) })
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

/// Rejects a NUL byte in a startup parameter, which would truncate the
/// protocol string.
fn check_startup_parameter(value: &str, name: &str) -> Result<(), String> {
    if value.bytes().any(|byte| byte == 0) {
        return Err(format!("{name} must not contain a NUL byte"));
    }
    Ok(())
}

/// Resolves `host`/`port` to a single socket address, parking for the collector
/// while the resolver runs.
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
    result.map_err(|error| format!("postgres connection error: {error}"))
}

/// `write_all` on the stream, parking the thread for the collector.
fn write_all(stream: &mut TcpStream, buffer: &[u8]) -> Result<(), String> {
    heap::blocking_enter();
    let result = stream.write_all(buffer);
    heap::blocking_leave();
    result.map_err(|error| format!("postgres connection error: {error}"))
}

/// Builds the startup packet: protocol version, `user`/`database` parameters,
/// and the terminating empty parameter.
fn startup_message(user: &str, database: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&PROTOCOL_VERSION_3.to_be_bytes());
    for (key, value) in [("user", user), ("database", database)] {
        body.extend_from_slice(key.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);

    let mut message = Vec::with_capacity(body.len() + 4);
    message.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(&body);
    message
}

/// Encodes a simple-query message wrapping `sql`.
fn query_message(sql: &str) -> Vec<u8> {
    let length = sql.len() + 1 + 4;
    let mut message = Vec::with_capacity(1 + length);
    message.push(b'Q');
    message.extend_from_slice(&(length as i32).to_be_bytes());
    message.extend_from_slice(sql.as_bytes());
    message.push(0);
    message
}

/// Reads one backend message: the type tag and its payload (without the length
/// field). The length is validated against [`MAX_MESSAGE`].
fn read_message(stream: &mut TcpStream) -> Result<(u8, Vec<u8>), String> {
    let mut header = [0u8; 5];
    read_exact(stream, &mut header)?;
    let tag = header[0];
    let length = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    if length < 4 || length as usize > MAX_MESSAGE {
        return Err(format!("invalid postgres message length {length}"));
    }
    let size = length as usize - 4;
    let mut payload = vec![0u8; size];
    if size > 0 {
        read_exact(stream, &mut payload)?;
    }
    Ok((tag, payload))
}

/// Extracts the human-readable message from an `ErrorResponse` payload.
fn error_response(payload: &[u8]) -> String {
    let mut message = None;
    let mut index = 0;
    while index < payload.len() {
        let field = payload[index];
        index += 1;
        if field == 0 {
            break;
        }
        let Some(offset) = payload[index..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let end = index + offset;
        if field == b'M' {
            message = Some(String::from_utf8_lossy(&payload[index..end]).into_owned());
        }
        index = end + 1;
    }
    match message {
        Some(message) => format!("postgres error: {message}"),
        None => "postgres error".to_string(),
    }
}

/// Parses the first column of a `DataRow` payload as an `i64`.
///
/// The payload starts at the column count (the message type and length field
/// are already stripped). A NULL or malformed value is a protocol error.
fn parse_data_row_i64(payload: &[u8]) -> Result<i64, String> {
    if payload.len() < 2 {
        return Err("truncated postgres DataRow".to_string());
    }
    let columns = i16::from_be_bytes([payload[0], payload[1]]);
    if columns < 1 {
        return Err("postgres DataRow has no columns".to_string());
    }
    if payload.len() < 6 {
        return Err("truncated postgres DataRow".to_string());
    }
    let length = i32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]);
    if length < 0 {
        return Err("postgres INSERT RETURNING id was NULL".to_string());
    }
    let length = length as usize;
    if payload.len() < 6 + length {
        return Err("truncated postgres DataRow value".to_string());
    }
    let text = std::str::from_utf8(&payload[6..6 + length])
        .map_err(|_| "postgres id is not valid UTF-8".to_string())?;
    text.trim()
        .parse::<i64>()
        .map_err(|_| format!("invalid postgres id value {text:?}"))
}

/// Performs the startup handshake. With trust authentication the backend sends
/// `AuthenticationOk` (`R` code 0), optional parameter/backend-key messages and
/// `ReadyForQuery` (`Z`).
fn startup(stream: &mut TcpStream, user: &str, database: &str) -> Result<(), String> {
    write_all(stream, &startup_message(user, database))?;
    loop {
        let (tag, payload) = read_message(stream)?;
        match tag {
            b'E' => return Err(error_response(&payload)),
            b'R' => {
                if payload.len() < 4 {
                    return Err("truncated postgres authentication message".to_string());
                }
                let method = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                if method != 0 {
                    return Err(format!(
                        "unsupported postgres authentication method {method}"
                    ));
                }
            }
            b'Z' => return Ok(()),
            // ParameterStatus (S), BackendKeyData (K) and NoticeResponse (N)
            // carry no state this client needs.
            _ => {}
        }
    }
}

/// Connects, then runs the startup handshake. The caller owns the stream.
fn open_connection(host: &str, port: u16, user: &str, database: &str) -> Result<TcpStream, String> {
    let address = resolve(host, port)?;
    heap::blocking_enter();
    let result = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT);
    heap::blocking_leave();
    let mut stream =
        result.map_err(|error| format!("postgres connect to {host}:{port} failed: {error}"))?;
    startup(&mut stream, user, database)?;
    Ok(stream)
}

/// Sends one simple-query `INSERT` and reads until `ReadyForQuery`, returning
/// the first `DataRow` id.
fn run_insert(stream: &mut TcpStream, sql: &str) -> Result<i64, String> {
    write_all(stream, &query_message(sql))?;
    let mut id = None;
    loop {
        let (tag, payload) = read_message(stream)?;
        match tag {
            b'E' => return Err(error_response(&payload)),
            b'D' if id.is_none() => id = Some(parse_data_row_i64(&payload)?),
            b'Z' => break,
            _ => {}
        }
    }
    id.ok_or_else(|| "postgres INSERT returned no id".to_string())
}

/// Validates a SQL identifier: ASCII `[A-Za-z_][A-Za-z0-9_]*`.
fn validate_identifier(name: &str) -> Result<(), String> {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
        _ => return Err("table name must match [A-Za-z_][A-Za-z0-9_]*".to_string()),
    }
    if bytes.any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_')) {
        return Err("table name must match [A-Za-z_][A-Za-z0-9_]*".to_string());
    }
    Ok(())
}

/// Escapes a string literal for a SQL statement: `'` is doubled and control
/// characters (including NUL) are rejected.
fn escape_sql_literal(value: &str) -> Result<String, String> {
    if value
        .chars()
        .any(|character| character == '\0' || character.is_control())
    {
        return Err("SQL value contains a control character".to_string());
    }
    Ok(value.replace('\'', "''"))
}

/// Builds the `INSERT ... RETURNING id` statement for one row.
fn build_insert_sql(table: &str, uuid: &str, mac: &str, firmware: &str) -> Result<String, String> {
    validate_identifier(table)?;
    let uuid = escape_sql_literal(uuid)?;
    let mac = escape_sql_literal(mac)?;
    let firmware = escape_sql_literal(firmware)?;
    Ok(format!(
        "INSERT INTO {table} (uuid, mac, firmware, created_at, updated_at) \
         VALUES ('{uuid}','{mac}','{firmware}', now(), now()) RETURNING id"
    ))
}

/// `postgres.connect(host, port, user, database)`.
fn connect(args: &[i64]) -> i64 {
    let host = match text_argument(args, 0, "connect", "host") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let port = match port_argument(args, 1, "connect") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let user = match text_argument(args, 2, "connect", "user") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let database = match text_argument(args, 3, "connect", "database") {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(message) = check_startup_parameter(&user, "user") {
        return exc::raise_with_message(VALUE_ERROR, &message);
    }
    if let Err(message) = check_startup_parameter(&database, "database") {
        return exc::raise_with_message(VALUE_ERROR, &message);
    }

    match open_connection(&host, port, &user, &database) {
        Ok(stream) => {
            let client = PgClient {
                host,
                port,
                user,
                database,
                stream: Mutex::new(Some(stream)),
            };
            let pointer = Box::into_raw(Box::new(client)) as usize;
            value::capsule_new(pointer, Some(CAPSULE_NAME.to_string()))
        }
        Err(message) => exc::raise_with_message(OS_ERROR, &message),
    }
}

/// `postgres.insert(handle, table, uuid, mac, firmware) -> int`.
fn insert(args: &[i64]) -> i64 {
    let Some(client) = client_argument(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "insert() argument 'handle' must be a postgres connection capsule",
        );
    };
    let table = match text_argument(args, 1, "insert", "table") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let uuid = match text_argument(args, 2, "insert", "uuid") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let mac = match text_argument(args, 3, "insert", "mac") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let firmware = match text_argument(args, 4, "insert", "firmware") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let sql = match build_insert_sql(&table, &uuid, &mac, &firmware) {
        Ok(sql) => sql,
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
        None => match open_connection(&client.host, client.port, &client.user, &client.database) {
            Ok(stream) => stream,
            Err(message) => return exc::raise_with_message(OS_ERROR, &message),
        },
    };

    match run_insert(&mut stream, &sql) {
        Ok(id) => {
            *lock_stream(client) = Some(stream);
            ops::int_result(id)
        }
        // Dropping the stream closes it; the next call reconnects.
        Err(message) => exc::raise_with_message(OS_ERROR, &message),
    }
}

/// Dispatches `postgres.*` calls.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "connect" => connect(args),
        "insert" => insert(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'postgres' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_row(value: Option<&str>) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1i16.to_be_bytes());
        match value {
            Some(text) => {
                payload.extend_from_slice(&(text.len() as i32).to_be_bytes());
                payload.extend_from_slice(text.as_bytes());
            }
            None => payload.extend_from_slice(&(-1i32).to_be_bytes()),
        }
        payload
    }

    #[test]
    fn identifiers_are_restricted() {
        assert!(validate_identifier("devices").is_ok());
        assert!(validate_identifier("_devices_2").is_ok());
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("1devices").is_err());
        assert!(validate_identifier("devices;drop").is_err());
        assert!(validate_identifier("devices\n").is_err());
        assert!(validate_identifier("dev ices").is_err());
        assert!(validate_identifier("dévices").is_err());
    }

    #[test]
    fn literals_are_escaped() {
        assert_eq!(escape_sql_literal("O'Brien").unwrap(), "O''Brien");
        assert_eq!(escape_sql_literal("plain").unwrap(), "plain");
        assert!(escape_sql_literal("line\nbreak").is_err());
        assert!(escape_sql_literal("nul\0byte").is_err());
    }

    #[test]
    fn insert_statement_matches_contract() {
        let sql = build_insert_sql("devices", "a'b", "aa:bb", "v1").unwrap();
        assert_eq!(
            sql,
            "INSERT INTO devices (uuid, mac, firmware, created_at, updated_at) \
             VALUES ('a''b','aa:bb','v1', now(), now()) RETURNING id"
        );
        assert!(build_insert_sql("bad table", "u", "m", "f").is_err());
    }

    #[test]
    fn data_row_parses_first_column() {
        assert_eq!(parse_data_row_i64(&data_row(Some("42"))).unwrap(), 42);
        assert_eq!(parse_data_row_i64(&data_row(Some("-7"))).unwrap(), -7);
        assert!(parse_data_row_i64(&data_row(None)).is_err());
        assert!(parse_data_row_i64(&data_row(Some("nope"))).is_err());
        assert!(parse_data_row_i64(&[]).is_err());
        // Truncated value: declared length exceeds the payload.
        let mut truncated = data_row(Some("12"));
        truncated.truncate(truncated.len() - 1);
        assert!(parse_data_row_i64(&truncated).is_err());
    }

    #[test]
    fn error_response_reads_message_field() {
        let payload = b"SERROR\0Mrelation \"x\" does not exist\0\0";
        assert_eq!(
            error_response(payload),
            "postgres error: relation \"x\" does not exist"
        );
        assert_eq!(error_response(b"\0"), "postgres error");
    }

    #[test]
    fn startup_packet_frames_parameters() {
        let message = startup_message("user", "db");
        let length = i32::from_be_bytes([message[0], message[1], message[2], message[3]]);
        assert_eq!(length as usize, message.len());
        assert_eq!(&message[4..8], &PROTOCOL_VERSION_3.to_be_bytes());
        assert!(message.ends_with(b"user\0user\0database\0db\0\0"));
    }
}
