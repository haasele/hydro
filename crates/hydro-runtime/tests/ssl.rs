//! Integration tests for the native `ssl` module (client-side TLS).
//!
//! A local rustls server is started on a loopback port for each test; the
//! CA plus leaf certificate and PKCS#8 key below are a bounded fixture
//! (`CN=localhost`, SAN `DNS:localhost, IP:127.0.0.1`, valid until 2036). No
//! test contacts an external host. The private key is test data only.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use hydro_runtime::abi::*;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

const CA_PEM: &str = "\
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

const LEAF_PEM: &str = "\
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

const KEY_PEM: &str = "\
-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIErV9AbQP8JYpvIVWvfAlYMHOI2fQi5HC+82OxiZ5QeEoAoGCCqGSM49
AwEHoUQDQgAEHd7u80YZO/gQEgG8Su1sCxMQpuQ6fu9Y/LYoi6fiPZmNPQGUPWtM
zSdbNFum/ybTkvljM5WMQ5ucdG7xsUKm0g==
-----END EC PRIVATE KEY-----
";

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn int(n: i64) -> i64 {
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn bytes(value: &[u8]) -> i64 {
    hydro_bytes_new(value.as_ptr(), value.len())
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

/// Keyword method call, exercising `ssl.SSLContext`'s named parameters.
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

fn eq(a: i64, b: i64) -> bool {
    hydro_binop(7, a, b) == hydro_true()
}

fn assert_no_error() {
    if hydro_exc_pending() != 0 {
        hydro_uncaught();
    }
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn clear_error() {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    hydro_exc_clear();
}

fn server_config() -> Arc<rustls::ServerConfig> {
    let certificates: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(LEAF_PEM.as_bytes())
            .chain(CertificateDer::pem_slice_iter(CA_PEM.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .expect("fixture certificate must parse");
    let key = PrivateKeyDer::from_pem_slice(KEY_PEM.as_bytes()).expect("fixture key must parse");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("default protocol versions")
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .expect("server certificate must load");
    Arc::new(config)
}

/// Starts a one-connection echo server: reads exactly four bytes, writes
/// `pong`, and closes the TLS session.
fn start_echo_server() -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let port = listener.local_addr().expect("local address").port();
    let config = server_config();
    let handle = std::thread::spawn(move || {
        let Ok((tcp, _)) = listener.accept() else {
            return;
        };
        let connection = match rustls::ServerConnection::new(config) {
            Ok(connection) => connection,
            Err(_) => return,
        };
        let mut stream = rustls::StreamOwned::new(connection, tcp);
        let mut request = [0u8; 4];
        if stream.read_exact(&mut request).is_err() {
            return;
        }
        let _ = stream.write_all(b"pong");
        let _ = stream.flush();
    });
    (port, handle)
}

fn address(port: i64) -> i64 {
    tuple(&[text("127.0.0.1"), int(port)])
}

#[test]
fn constants_and_context_exports() {
    let _guard = guard();
    hydro_runtime_init();

    assert_eq!(module_get("ssl", "CERT_NONE"), int(0));
    assert_eq!(module_get("ssl", "CERT_REQUIRED"), int(2));
    assert_eq!(module_get("ssl", "PROTOCOL_TLS_CLIENT"), int(16));
    assert_eq!(module_get("ssl", "HAS_SNI"), hydro_true());
    assert_no_error();
    assert!(eq(
        module_get("ssl", "OPENSSL_VERSION"),
        text("rustls 0.23")
    ));

    let context = module(
        "ssl",
        "SSLContext",
        &[module_get("ssl", "PROTOCOL_TLS_CLIENT")],
    );
    assert_no_error();
    // `verify_mode` is an ordinary instance attribute (CERT_REQUIRED default)
    // and can be replaced, exactly like CPython's SSLContext.
    assert_eq!(hydro_getattr(context, b"verify_mode".as_ptr(), 11), int(2));
    assert_eq!(
        hydro_setattr(context, b"verify_mode".as_ptr(), 11, int(0)),
        hydro_none()
    );
    assert_eq!(hydro_getattr(context, b"verify_mode".as_ptr(), 11), int(0));

    // The context is also reachable through `from ssl import SSLContext`.
    let imported = module_get("ssl", "SSLContext");
    assert_eq!(hydro_exc_pending(), 0);
    assert_ne!(imported, hydro_none());
}

#[test]
fn verified_client_talks_to_local_rustls_server() {
    let _guard = guard();
    let (port, server) = start_echo_server();
    hydro_runtime_init();

    let context = module(
        "ssl",
        "SSLContext",
        &[module_get("ssl", "PROTOCOL_TLS_CLIENT")],
    );
    assert_no_error();
    assert_eq!(
        method_kw(
            context,
            "load_verify_locations",
            &[],
            &[("cadata", text(CA_PEM))],
        ),
        hydro_none()
    );
    assert_no_error();

    let socket = module(
        "socket",
        "create_connection",
        &[address(port as i64), hydro_float_new(5.0)],
    );
    assert_no_error();

    let wrapped = method_kw(
        context,
        "wrap_socket",
        &[socket],
        &[("server_hostname", text("localhost"))],
    );
    assert_no_error();
    assert!(eq(wrapped, socket));
    assert_eq!(method(wrapped, "sendall", &[bytes(b"ping")]), hydro_none());
    assert_no_error();
    let reply = method(wrapped, "recv", &[int(4)]);
    assert_no_error();
    assert!(eq(reply, bytes(b"pong")));
    assert_eq!(method(wrapped, "close", &[]), hydro_none());
    assert_no_error();

    server.join().expect("server thread");
}

#[test]
fn certificate_verification_failure_is_reported() {
    let _guard = guard();
    let (port, server) = start_echo_server();
    hydro_runtime_init();

    // The default context trusts the Mozilla roots, not the fixture's
    // self-signed certificate: the handshake must fail verification.
    let context = module("ssl", "create_default_context", &[]);
    assert_no_error();
    let socket = module(
        "socket",
        "create_connection",
        &[address(port as i64), hydro_float_new(5.0)],
    );
    assert_no_error();
    let result = method(context, "wrap_socket", &[socket, text("localhost")]);
    assert_eq!(result, hydro_none());
    assert_eq!(hydro_exc_tag(), 10, "TLS failures use the OSError tag");
    let message = hydro_to_str(hydro_exc_value());
    assert_ne!(hydro_len(message), int(0));
    clear_error();
    assert_eq!(method(socket, "close", &[]), hydro_none());

    // The server observed a failed handshake and exits.
    server.join().expect("server thread");
}

#[test]
fn legacy_wrap_socket_skips_verification() {
    let _guard = guard();
    let (port, server) = start_echo_server();
    hydro_runtime_init();

    let socket = module(
        "socket",
        "create_connection",
        &[address(port as i64), hydro_float_new(5.0)],
    );
    assert_no_error();
    let wrapped = module("ssl", "wrap_socket", &[socket, text("localhost")]);
    assert_no_error();
    assert!(eq(wrapped, socket));
    assert_eq!(method(wrapped, "sendall", &[bytes(b"ping")]), hydro_none());
    assert_no_error();
    let reply = method(wrapped, "recv", &[int(4)]);
    assert_no_error();
    assert!(eq(reply, bytes(b"pong")));
    assert_eq!(method(wrapped, "close", &[]), hydro_none());
    assert_no_error();

    server.join().expect("server thread");
}

#[test]
fn server_side_context_reports_unsupported() {
    let _guard = guard();
    hydro_runtime_init();

    let context = module(
        "ssl",
        "SSLContext",
        &[module_get("ssl", "PROTOCOL_TLS_SERVER")],
    );
    assert_no_error();
    let socket = module("socket", "socket", &[]);
    assert_no_error();
    assert_eq!(
        method(context, "wrap_socket", &[socket, hydro_none()]),
        hydro_none()
    );
    assert_eq!(hydro_exc_tag(), 10);
    clear_error();
    assert_eq!(method(socket, "close", &[]), hydro_none());
}
