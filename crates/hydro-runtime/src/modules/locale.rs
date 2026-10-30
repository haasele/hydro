//! `locale`: process locale settings backed by the C library.
//!
//! `setlocale()` calls the platform `setlocale(3)` directly, so the setting is
//! the real process locale rather than a Python-level mirror. `getlocale()`
//! and `getpreferredencoding()` read it back through `setlocale(category,
//! NULL)`. `strcoll`/`strxfrm` use the C library too.
//!
//! The message-catalog functions (`gettext`, `bindtextdomain`, ...) keep the
//! requested domain state and return messages unchanged: Hydro ships no
//! compiled catalogs, which matches CPython for an uninstalled domain.
//!
//! Failures raise `locale.Error`, implemented as a `ValueError` subclass so
//! `except ValueError` also catches it.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

const LC_CTYPE: i64 = 0;
const LC_NUMERIC: i64 = 1;
const LC_TIME: i64 = 2;
const LC_COLLATE: i64 = 3;
const LC_MONETARY: i64 = 4;
const LC_MESSAGES: i64 = 5;
const LC_ALL: i64 = 6;

/// `setlocale`/`strcoll` touch process-global C state.
static LOCALE_LOCK: Mutex<()> = Mutex::new(());

static DOMAINS: LazyLock<Mutex<Domains>> = LazyLock::new(|| Mutex::new(Domains::default()));

#[derive(Default)]
struct Domains {
    /// `bindtextdomain(domain, dir)` paths.
    paths: HashMap<String, String>,
    /// Current `textdomain(domain)` value.
    current: Option<String>,
}

fn domains() -> MutexGuard<'static, Domains> {
    DOMAINS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn encode(n: i64) -> i64 {
    value::encode_int(n).unwrap_or_else(|| exc::raise_with_message(exc::OVERFLOW_ERROR, "overflow"))
}

fn raise_locale(message: &str) -> i64 {
    // `locale.Error` is a ValueError subclass; constructing an instance keeps
    // the class identity for `except locale.Error`.
    let class = error_class();
    let args = [value::str_alloc(message.to_string())];
    let instance = crate::builtins::call(class, args.len(), args.as_ptr());
    if exc::pending() {
        return instance;
    }
    exc::raise_value(instance)
}

/// `locale.Error`.
fn error_class() -> i64 {
    if let Some(class) = globals_get("locale.Error") {
        return class;
    }
    let base = exc::exception_class(VALUE_ERROR);
    let class = value::class_new("Error".to_string(), base, Vec::new());
    crate::core::globals::registry_set("locale.Error", class);
    class
}

fn globals_get(key: &str) -> Option<i64> {
    crate::core::globals::registry_get(key)
}

fn category_arg(v: i64) -> Result<i64, i64> {
    match value::as_integer(v) {
        Some(category) if (LC_CTYPE..=LC_ALL).contains(&category) => Ok(category),
        Some(_) => Err(exc::raise_with_message(
            VALUE_ERROR,
            "unknown locale category",
        )),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("category must be an integer, not {}", value::type_name(v)),
        )),
    }
}

/// Decodes the C string returned by `setlocale` into a Python value.
fn decode_locale(ptr: *mut libc::c_char) -> i64 {
    if ptr.is_null() {
        return NONE;
    }
    // SAFETY: `setlocale` returns a NUL-terminated string owned by libc.
    let text = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    value::str_alloc(text)
}

/// `setlocale(category, locale=None)`.
fn setlocale(args: &[i64]) -> i64 {
    let category = match category_arg(args[0]) {
        Ok(category) => category,
        Err(error) => return error,
    };
    let locale = match args.get(1).copied() {
        None | Some(NONE) => None,
        Some(raw) => match value::as_str(raw) {
            Some(text) => Some(text.to_string()),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("locale must be str or None, not {}", value::type_name(raw)),
                )
            }
        },
    };
    let _guard = LOCALE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let result = match locale {
        None => {
            // SAFETY: querying the current locale is side-effect free.
            let current = unsafe { libc::setlocale(category as libc::c_int, std::ptr::null()) };
            decode_locale(current)
        }
        Some(text) => {
            let Ok(c_locale) = CString::new(text) else {
                return exc::raise_with_message(VALUE_ERROR, "locale contains a NUL byte");
            };
            // SAFETY: `c_locale` is a valid NUL-terminated string; libc copies
            // it into its own storage before returning.
            let result = unsafe { libc::setlocale(category as libc::c_int, c_locale.as_ptr()) };
            if result.is_null() {
                return raise_locale("unsupported locale setting");
            }
            decode_locale(result)
        }
    };
    if result == NONE {
        raise_locale("unsupported locale setting")
    } else {
        result
    }
}

/// Splits a POSIX locale name into `(language, encoding)`.
fn parse_locale(text: &str) -> (Option<String>, Option<String>) {
    let text = text.trim();
    if text.is_empty() || text == "C" || text == "POSIX" {
        return (None, None);
    }
    let (rest, encoding) = match text.split_once('.') {
        Some((rest, encoding)) => {
            let encoding = encoding.split('@').next().unwrap_or(encoding);
            (rest, Some(encoding.to_string()))
        }
        None => (text, None),
    };
    let language = rest.split('@').next().unwrap_or(rest).to_string();
    (Some(language), encoding)
}

/// `getlocale(category=LC_CTYPE)`.
fn getlocale(args: &[i64]) -> i64 {
    let category = match args.first().copied() {
        None | Some(NONE) => LC_CTYPE,
        Some(raw) => match category_arg(raw) {
            Ok(category) => category,
            Err(error) => return error,
        },
    };
    let guard = LOCALE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: querying the current locale is side-effect free.
    let current = unsafe { libc::setlocale(category as libc::c_int, std::ptr::null()) };
    let text = if current.is_null() {
        String::new()
    } else {
        // SAFETY: `setlocale` returns a NUL-terminated string owned by libc.
        unsafe { CStr::from_ptr(current) }
            .to_string_lossy()
            .into_owned()
    };
    drop(guard);
    let (language, encoding) = parse_locale(&text);
    let language = language.map_or(NONE, value::str_alloc);
    let encoding = encoding.map_or(NONE, value::str_alloc);
    value::tuple_new(vec![language, encoding])
}

/// Environment fallback for `getpreferredencoding`.
fn environment_locale() -> String {
    for variable in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(text) = std::env::var(variable) {
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

/// Encoding aliases Python normalises in `getpreferredencoding`.
fn normalize_encoding(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "UTF-8".to_string();
    }
    let lowered = trimmed.to_ascii_lowercase().replace('_', "-");
    match lowered.as_str() {
        "utf8" | "utf-8" => "UTF-8".to_string(),
        "iso8859-1" | "iso-8859-1" | "latin1" | "latin-1" => "ISO-8859-1".to_string(),
        "ansi-x3.4-1968" | "us-ascii" | "ascii" => "ascii".to_string(),
        _ => trimmed.to_string(),
    }
}

fn preferred_encoding(do_setlocale: bool) -> i64 {
    let source = if do_setlocale {
        let guard = LOCALE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: querying LC_CTYPE is side-effect free.
        let current = unsafe { libc::setlocale(LC_CTYPE as libc::c_int, std::ptr::null()) };
        let text = if current.is_null() {
            String::new()
        } else {
            // SAFETY: `setlocale` returns a NUL-terminated string owned by libc.
            unsafe { CStr::from_ptr(current) }
                .to_string_lossy()
                .into_owned()
        };
        drop(guard);
        text
    } else {
        environment_locale()
    };
    let (_, encoding) = parse_locale(&source);
    let encoding = encoding.unwrap_or_else(|| {
        let fallback = environment_locale();
        let (_, encoding) = parse_locale(&fallback);
        encoding.unwrap_or_default()
    });
    value::str_alloc(normalize_encoding(&encoding))
}

fn getpreferredencoding(args: &[i64]) -> i64 {
    let do_setlocale = match args.first().copied() {
        None | Some(NONE) => true,
        Some(raw) => value::truthy(raw),
    };
    preferred_encoding(do_setlocale)
}

fn getencoding(_args: &[i64]) -> i64 {
    preferred_encoding(true)
}

/// `normalize(localename)`.
fn normalize(args: &[i64]) -> i64 {
    let Some(text) = value::as_str(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "normalize() argument must be str, not {}",
                value::type_name(args[0])
            ),
        );
    };
    let text = text.trim();
    if text.is_empty() {
        return value::str_alloc(String::new());
    }
    let (body, modifier) = match text.split_once('@') {
        Some((body, modifier)) => (body, Some(modifier)),
        None => (text, None),
    };
    let (base, encoding) = match body.split_once('.') {
        Some((base, encoding)) => (base.to_string(), Some(encoding.to_string())),
        None => (body.to_string(), None),
    };
    let mut normalized = match base.split_once('_') {
        Some((language, territory)) => format!(
            "{}_{}",
            language.to_ascii_lowercase(),
            territory.to_ascii_uppercase()
        ),
        None => base.to_ascii_lowercase(),
    };
    if let Some(encoding) = encoding {
        normalized.push('.');
        normalized.push_str(&normalize_encoding(&encoding).to_ascii_uppercase());
    }
    if let Some(modifier) = modifier {
        normalized.push('@');
        normalized.push_str(modifier);
    }
    value::str_alloc(normalized)
}

/// `strcoll(a, b)`.
fn strcoll(args: &[i64]) -> i64 {
    let left = match string_argument(args[0], "strcoll") {
        Ok(text) => text,
        Err(error) => return error,
    };
    let right = match string_argument(args[1], "strcoll") {
        Ok(text) => text,
        Err(error) => return error,
    };
    let guard = LOCALE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    let order = unsafe { libc::strcoll(left.as_ptr(), right.as_ptr()) };
    drop(guard);
    encode(i64::from(order))
}

/// `strxfrm(s)`.
fn strxfrm(args: &[i64]) -> i64 {
    let text = match string_argument(args[0], "strxfrm") {
        Ok(text) => text,
        Err(error) => return error,
    };
    let guard = LOCALE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: the size query passes a NULL destination and zero length.
    let needed = unsafe { libc::strxfrm(std::ptr::null_mut(), text.as_ptr(), 0) };
    if needed == 0 {
        drop(guard);
        return value::str_alloc(String::new());
    }
    let mut buffer = vec![0u8; needed.saturating_add(1)];
    // SAFETY: `buffer` has room for `needed + 1` bytes.
    let written =
        unsafe { libc::strxfrm(buffer.as_mut_ptr() as *mut _, text.as_ptr(), buffer.len()) };
    drop(guard);
    if written >= buffer.len() {
        return exc::raise_with_message(VALUE_ERROR, "strxfrm() result truncated");
    }
    buffer.truncate(written);
    value::str_alloc(String::from_utf8_lossy(&buffer).into_owned())
}

fn string_argument(v: i64, function: &str) -> Result<CString, i64> {
    let Some(text) = value::as_str(v) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{function}() argument must be str, not {}",
                value::type_name(v)
            ),
        ));
    };
    CString::new(text).map_err(|_| {
        exc::raise_with_message(
            VALUE_ERROR,
            &format!("{function}() argument contains a NUL byte"),
        )
    })
}

/// `bindtextdomain(domain, dir=None)`.
fn bindtextdomain(args: &[i64]) -> i64 {
    let domain = match string_argument(args[0], "bindtextdomain") {
        Ok(domain) => domain.to_string_lossy().into_owned(),
        Err(error) => return error,
    };
    let path = match args.get(1).copied() {
        None | Some(NONE) => None,
        Some(raw) => match value::as_str(raw) {
            Some(text) => Some(text.to_string()),
            None => return exc::raise_with_message(TYPE_ERROR, "directory must be str or None"),
        },
    };
    let mut domains = domains();
    match path {
        Some(path) => domains.paths.insert(domain, path.clone()),
        None => domains.paths.get(&domain).cloned(),
    }
    .map_or(NONE, value::str_alloc)
}

/// `textdomain(domain=None)`.
fn textdomain(args: &[i64]) -> i64 {
    let mut domains = domains();
    match args.first().copied() {
        None | Some(NONE) => {}
        Some(raw) => match value::as_str(raw) {
            Some(text) => domains.current = Some(text.to_string()),
            None => return exc::raise_with_message(TYPE_ERROR, "domain must be str"),
        },
    }
    domains.current.clone().map_or(NONE, value::str_alloc)
}

fn gettext(args: &[i64]) -> i64 {
    message_argument(args[0])
}

fn dgettext(args: &[i64]) -> i64 {
    message_argument(args[1])
}

fn dcgettext(args: &[i64]) -> i64 {
    message_argument(args[1])
}

fn message_argument(v: i64) -> i64 {
    match value::as_str(v) {
        Some(text) => value::str_alloc(text.to_string()),
        None => exc::raise_with_message(
            TYPE_ERROR,
            &format!("message must be str, not {}", value::type_name(v)),
        ),
    }
}

/// Dispatches a native `locale` function. Arity is enforced by the central
/// spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "setlocale" => setlocale(args),
        "getlocale" => getlocale(args),
        "getpreferredencoding" => getpreferredencoding(args),
        "localeconv" => localeconv(),
        "getencoding" => getencoding(args),
        "normalize" => normalize(args),
        "strcoll" => strcoll(args),
        "strxfrm" => strxfrm(args),
        "bindtextdomain" => bindtextdomain(args),
        "textdomain" => textdomain(args),
        "gettext" | "_gettext" => gettext(args),
        "dgettext" | "_dgettext" => dgettext(args),
        "dcgettext" | "_dcgettext" => dcgettext(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'locale' has no attribute '{function}'"),
        ),
    }
}

/// `localeconv()`: the C `struct lconv` fields as a dict.
fn localeconv() -> i64 {
    #[cfg(unix)]
    {
        // SAFETY: `localeconv` returns a process-global struct owned by libc;
        // every pointer is read before another locale call can mutate it.
        let pointer = unsafe { libc::localeconv() };
        if pointer.is_null() {
            return value::dict_new(Vec::new());
        }
        // SAFETY: libc owns the struct for the process lifetime; no locale
        // call runs while its strings are copied below.
        let conv = unsafe { &*pointer };
        let c_string = |pointer: *const libc::c_char| -> String {
            if pointer.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(pointer) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        let grouping = |pointer: *const libc::c_char| -> Vec<i64> {
            let mut out = Vec::new();
            if pointer.is_null() {
                return out;
            }
            let mut index = 0isize;
            loop {
                let byte = unsafe { *pointer.offset(index) };
                if byte == 0 {
                    break;
                }
                out.push(encode(byte as i64));
                index += 1;
            }
            out
        };
        let keys: [(&str, i64); 23] = [
            (
                "decimal_point",
                value::str_from_bytes(c_string(conv.decimal_point).as_bytes()),
            ),
            (
                "thousands_sep",
                value::str_from_bytes(c_string(conv.thousands_sep).as_bytes()),
            ),
            ("grouping", value::list_new(grouping(conv.grouping))),
            (
                "int_curr_symbol",
                value::str_from_bytes(c_string(conv.int_curr_symbol).as_bytes()),
            ),
            (
                "currency_symbol",
                value::str_from_bytes(c_string(conv.currency_symbol).as_bytes()),
            ),
            (
                "mon_decimal_point",
                value::str_from_bytes(c_string(conv.mon_decimal_point).as_bytes()),
            ),
            (
                "mon_thousands_sep",
                value::str_from_bytes(c_string(conv.mon_thousands_sep).as_bytes()),
            ),
            ("mon_grouping", value::list_new(grouping(conv.mon_grouping))),
            (
                "positive_sign",
                value::str_from_bytes(c_string(conv.positive_sign).as_bytes()),
            ),
            (
                "negative_sign",
                value::str_from_bytes(c_string(conv.negative_sign).as_bytes()),
            ),
            ("int_frac_digits", encode(conv.int_frac_digits as i64)),
            ("frac_digits", encode(conv.frac_digits as i64)),
            ("p_cs_precedes", encode(conv.p_cs_precedes as i64)),
            ("p_sep_by_space", encode(conv.p_sep_by_space as i64)),
            ("n_cs_precedes", encode(conv.n_cs_precedes as i64)),
            ("n_sep_by_space", encode(conv.n_sep_by_space as i64)),
            ("p_sign_posn", encode(conv.p_sign_posn as i64)),
            ("n_sign_posn", encode(conv.n_sign_posn as i64)),
            ("int_p_cs_precedes", encode(conv.int_p_cs_precedes as i64)),
            ("int_p_sep_by_space", encode(conv.int_p_sep_by_space as i64)),
            ("int_n_cs_precedes", encode(conv.int_n_cs_precedes as i64)),
            ("int_n_sep_by_space", encode(conv.int_n_sep_by_space as i64)),
            ("int_p_sign_posn", encode(conv.int_p_sign_posn as i64)),
        ];
        let roots = crate::heap::root_values(&keys.map(|(_, value)| value));
        let pairs = keys
            .iter()
            .map(|(key, value)| (value::str_alloc((*key).to_string()), *value))
            .collect();
        let result = value::dict_new(pairs);
        crate::heap::release_roots(roots);
        result
    }
    #[cfg(not(unix))]
    {
        value::dict_new(Vec::new())
    }
}

/// Materialises a `locale` module constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "LC_CTYPE" => encode(LC_CTYPE),
        "LC_NUMERIC" => encode(LC_NUMERIC),
        "LC_TIME" => encode(LC_TIME),
        "LC_COLLATE" => encode(LC_COLLATE),
        "LC_MONETARY" => encode(LC_MONETARY),
        "LC_MESSAGES" => encode(LC_MESSAGES),
        "LC_ALL" => encode(LC_ALL),
        "Error" => error_class(),
        "CHAR_MAX" => encode(127),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'locale' has no attribute '{name}'"),
        ),
    }
}
