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

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::ops;
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
    // CPython's `_locale.getencoding()` is `_Py_GetLocaleEncodingObject()`:
    // `nl_langinfo(CODESET)` of the current locale, or "utf-8" when the
    // platform reports an empty codeset.
    #[cfg(unix)]
    {
        // SAFETY: `nl_langinfo` returns a NUL-terminated string owned by libc.
        let pointer = unsafe { libc::nl_langinfo(libc::CODESET) };
        if !pointer.is_null() {
            // SAFETY: the pointer is a NUL-terminated C string.
            let text = unsafe { CStr::from_ptr(pointer) }
                .to_string_lossy()
                .into_owned();
            if !text.is_empty() {
                return value::str_alloc(text);
            }
        }
        value::str_alloc("utf-8".to_string())
    }
    #[cfg(not(unix))]
    {
        preferred_encoding(true)
    }
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
    value::str_alloc(normalize_name(text))
}

/// The locale-name normalisation `normalize` and `_parse_localename` share:
/// a port of `Lib/locale.py:normalize`, including its four alias-table
/// lookups.
fn normalize_name(text: &str) -> String {
    let lowered = text.to_lowercase().replace(':', ".");
    let (code, modifier) = match lowered.split_once('@') {
        Some((code, modifier)) => (code, modifier),
        None => (lowered.as_str(), ""),
    };
    let (langname, encoding) = match code.split_once('.') {
        Some((langname, encoding)) => (langname, encoding),
        None => (code, ""),
    };
    // The first lookup treats `-` and `_` inside the encoding as equivalent.
    let mut lang_enc = langname.to_string();
    if !encoding.is_empty() {
        let normalized: String = encoding
            .chars()
            .filter(|ch| *ch != '-' && *ch != '_')
            .collect();
        lang_enc.push('.');
        lang_enc.push_str(&normalized);
    }
    let mut lookup = lang_enc.clone();
    if !modifier.is_empty() {
        lookup.push('@');
        lookup.push_str(modifier);
    }
    if let Some(found) = locale_alias_lookup(&lookup) {
        return found.to_string();
    }
    if !modifier.is_empty() {
        if let Some(found) = locale_alias_lookup(&lang_enc) {
            if !found.contains('@') {
                return append_modifier(found, modifier);
            }
            if found
                .split_once('@')
                .is_some_and(|(_, existing)| existing.eq_ignore_ascii_case(modifier))
            {
                return found.to_string();
            }
        }
    }
    if !encoding.is_empty() {
        let mut lookup = langname.to_string();
        if !modifier.is_empty() {
            lookup.push('@');
            lookup.push_str(modifier);
        }
        if let Some(found) = locale_alias_lookup(&lookup) {
            if !found.contains('@') {
                return replace_encoding(found, encoding);
            }
            if let Some((base, existing)) = found.split_once('@') {
                return format!("{}@{}", replace_encoding(base, encoding), existing);
            }
        }
        if !modifier.is_empty() {
            if let Some(found) = locale_alias_lookup(langname) {
                if !found.contains('@') {
                    return append_modifier(&replace_encoding(found, encoding), modifier);
                }
                if let Some((base, existing)) = found.split_once('@') {
                    if existing.eq_ignore_ascii_case(modifier) {
                        return format!("{}@{}", replace_encoding(base, encoding), existing);
                    }
                }
            }
        }
    }
    text.to_string()
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
        "format_string" => format_string(args),
        "nl_langinfo" => nl_langinfo(args),
        "atof" => atof(args),
        "atoi" => atoi(args),
        "str" => locale_str(args),
        "delocalize" => delocalize(args),
        "localize" => localize(args),
        "currency" => currency(args),
        "getdefaultlocale" | "_getdefaultlocale" => getdefaultlocale(args),
        "_parse_localename" => parse_localename_call(args),
        "_build_localename" => build_localename(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'locale' has no attribute '{function}'"),
        ),
    }
}

/// `localeconv()`: the C `struct lconv` fields as a dict, with the
/// `locale._override_localeconv` test hook applied.
///
/// `Lib/locale.py` wraps the C function this way, and `test_locale`'s cooked
/// locale cases rely on the override reaching both `localeconv()` and the
/// formatting helpers (which read their separators from this dict).
fn localeconv() -> i64 {
    let dict = base_localeconv();
    let Some(overrides) =
        crate::core::globals::attribute_override("locale", "_override_localeconv")
    else {
        return dict;
    };
    let Some(entries) = value::as_dict(overrides) else {
        return dict;
    };
    let pairs: Vec<(i64, i64)> = entries.to_vec();
    for (key, item) in pairs {
        container::dict_set(dict, key, item);
    }
    dict
}

/// `localeconv()` without the `_override_localeconv` hook.
fn base_localeconv() -> i64 {
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

/// Reads one field of a `localeconv()` result dict.
fn dict_field(dict: i64, key: &str) -> Option<i64> {
    value::as_dict(dict)?
        .iter()
        .find(|(name, _)| value::as_str(*name) == Some(key))
        .map(|(_, item)| *item)
}

fn dict_string(dict: i64, key: &str) -> String {
    dict_field(dict, key)
        .and_then(value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn dict_int(dict: i64, key: &str) -> i64 {
    dict_field(dict, key)
        .and_then(value::as_integer)
        .unwrap_or(127)
}

fn dict_grouping(dict: i64, key: &str) -> Vec<i64> {
    dict_field(dict, key)
        .and_then(value::as_list)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| value::as_integer(*item))
                .collect()
        })
        .unwrap_or_default()
}

/// The `struct lconv` pieces the number-formatting helpers need. Reading them
/// through [`localeconv`] applies `_override_localeconv`, like CPython.
struct ConvFields {
    decimal_point: String,
    thousands_sep: String,
    mon_decimal_point: String,
    mon_thousands_sep: String,
    grouping: Vec<i64>,
    mon_grouping: Vec<i64>,
}

fn conv_fields() -> ConvFields {
    let dict = localeconv();
    ConvFields {
        decimal_point: dict_string(dict, "decimal_point"),
        thousands_sep: dict_string(dict, "thousands_sep"),
        mon_decimal_point: dict_string(dict, "mon_decimal_point"),
        mon_thousands_sep: dict_string(dict, "mon_thousands_sep"),
        grouping: dict_grouping(dict, "grouping"),
        mon_grouping: dict_grouping(dict, "mon_grouping"),
    }
}

/// Expands `grouping` like `Lib/locale.py:_grouping_intervals`: `CHAR_MAX`
/// ends the sequence, a `0` repeats the previous interval forever (reported as
/// the trailing `true`).
fn grouping_intervals(grouping: &[i64]) -> Result<(Vec<i64>, bool), i64> {
    let mut intervals = Vec::new();
    for &interval in grouping {
        if interval == 127 {
            return Ok((intervals, false));
        }
        if interval == 0 {
            if intervals.is_empty() {
                return Err(exc::raise_with_message(VALUE_ERROR, "invalid grouping"));
            }
            return Ok((intervals, true));
        }
        intervals.push(interval);
    }
    Ok((intervals, false))
}

/// `_group`: inserts `thousands_sep` from the right using `grouping`, returning
/// the text and how many separator characters were inserted.
fn group_digits(value: &str, fields: &ConvFields, monetary: bool) -> Result<(String, usize), i64> {
    let separator = if monetary {
        &fields.mon_thousands_sep
    } else {
        &fields.thousands_sep
    };
    let grouping = if monetary {
        &fields.mon_grouping
    } else {
        &fields.grouping
    };
    if grouping.is_empty() {
        return Ok((value.to_string(), 0));
    }
    let (intervals, repeat) = grouping_intervals(grouping)?;
    let mut chars: Vec<char> = value.chars().collect();
    let mut right_spaces = String::new();
    while chars.last() == Some(&' ') {
        chars.pop();
        right_spaces.push(' ');
    }
    let mut left_spaces = String::new();
    let mut groups: Vec<String> = Vec::new();
    let mut index = 0usize;
    loop {
        if !chars.last().is_some_and(|ch| ch.is_ascii_digit()) {
            left_spaces = chars.iter().collect();
            chars.clear();
            break;
        }
        let interval = match intervals.get(index) {
            Some(&interval) => interval,
            None if repeat => *intervals.last().expect("repeat implies an interval"),
            None => break,
        };
        if interval <= 0 {
            break;
        }
        let take = (interval as usize).min(chars.len());
        let split = chars.len() - take;
        groups.push(chars[split..].iter().collect());
        chars.truncate(split);
        index += 1;
    }
    if !chars.is_empty() {
        groups.push(chars.iter().collect());
    }
    groups.reverse();
    let joined = groups.join(separator);
    let separators = separator.chars().count() * groups.len().saturating_sub(1);
    Ok((format!("{left_spaces}{joined}{right_spaces}"), separators))
}

/// `_strip_padding`: removes at most `amount` spaces from either end.
fn strip_padding(text: &str, mut amount: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut left = 0usize;
    while amount > 0 && left < chars.len() && chars[left] == ' ' {
        left += 1;
        amount -= 1;
    }
    let mut right = chars.len();
    while amount > 0 && right > left && chars[right - 1] == ' ' {
        right -= 1;
        amount -= 1;
    }
    chars[left..right].iter().collect()
}

/// `_localize`: applies the locale separator and decimal point to a formatted
/// number.
fn localize_number(
    formatted: &str,
    grouping: bool,
    monetary: bool,
    fields: &ConvFields,
) -> Result<String, i64> {
    if let Some((integer, fraction)) = formatted.split_once('.') {
        let (grouped, separators) = if grouping {
            group_digits(integer, fields, monetary)?
        } else {
            (integer.to_string(), 0)
        };
        let point = if monetary {
            &fields.mon_decimal_point
        } else {
            &fields.decimal_point
        };
        let text = format!("{grouped}{point}{fraction}");
        Ok(if separators > 0 {
            strip_padding(&text, separators)
        } else {
            text
        })
    } else {
        let (grouped, separators) = if grouping {
            group_digits(formatted, fields, monetary)?
        } else {
            (formatted.to_string(), 0)
        };
        Ok(if separators > 0 {
            strip_padding(&grouped, separators)
        } else {
            grouped
        })
    }
}

/// Whether a `%` conversion localizes its result (`_format`'s conversion set).
fn localizing_conversion(conversion: char) -> bool {
    matches!(
        conversion,
        'e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'd' | 'i' | 'u'
    )
}

/// Splits `format` into the template whose conversions become `%s` and the
/// matched conversion specs, mirroring the `_percent_re` pattern.
fn percent_conversions(format: &str) -> (String, Vec<(String, char)>) {
    let chars: Vec<char> = format.chars().collect();
    let mut template = String::new();
    let mut conversions = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] != '%' {
            template.push(chars[index]);
            index += 1;
            continue;
        }
        let start = index;
        let mut cursor = index + 1;
        if cursor < chars.len() && chars[cursor] == '(' {
            let mut close = cursor + 1;
            while close < chars.len() && chars[close] != ')' {
                close += 1;
            }
            if close < chars.len() {
                cursor = close + 1;
            }
        }
        while cursor < chars.len() && "-+#0 123456789.hlL*".contains(chars[cursor]) {
            cursor += 1;
        }
        if cursor < chars.len() && "eEfFgGdiouxXcrs%".contains(chars[cursor]) {
            conversions.push((chars[start..=cursor].iter().collect(), chars[cursor]));
            template.push_str("%s");
            index = cursor + 1;
        } else {
            template.push('%');
            index += 1;
        }
    }
    (template, conversions)
}

fn localize_formatted(formatted: i64, grouping: bool, monetary: bool, fields: &ConvFields) -> i64 {
    match value::as_str(formatted) {
        Some(text) => match localize_number(text, grouping, monetary, fields) {
            Ok(text) => value::str_alloc(text),
            Err(error) => error,
        },
        None => formatted,
    }
}

/// `format_string(f, val, grouping=False, monetary=False)`.
fn format_string(args: &[i64]) -> i64 {
    let Some(format) = value::as_str(args[0]) else {
        return exc::raise_with_message(TYPE_ERROR, "format_string() argument must be str");
    };
    let value_arg = args.get(1).copied().unwrap_or(NONE);
    let grouping = args.get(2).is_some_and(|item| value::truthy(*item));
    let monetary = args.get(3).is_some_and(|item| value::truthy(*item));
    if exc::pending() {
        return NONE;
    }
    let fields = conv_fields();
    let (template, conversions) = percent_conversions(format);
    let results = value::list_new(Vec::new());
    let template_value = value::str_alloc(template);
    let roots = crate::heap::root_values(&[results, template_value]);
    let push = |item: i64| {
        if let Some(mut items) = value::list_items_mut(results) {
            items.push(item);
            crate::heap::write_barrier_edge(results, item);
        }
    };
    if value::as_dict(value_arg).is_some() {
        for (spec, conversion) in &conversions {
            if *conversion == '%' {
                push(value::str_from_bytes(b"%"));
                continue;
            }
            let formatted = ops::binop(ops::OP_MOD, value::str_alloc(spec.clone()), value_arg);
            if exc::pending() {
                crate::heap::release_roots(roots);
                return NONE;
            }
            let formatted = if localizing_conversion(*conversion) {
                localize_formatted(formatted, grouping, monetary, &fields)
            } else {
                formatted
            };
            if exc::pending() {
                crate::heap::release_roots(roots);
                return NONE;
            }
            push(formatted);
        }
    } else {
        let items: Vec<i64> = match value::as_tuple(value_arg) {
            Some(items) => items.to_vec(),
            None => vec![value_arg],
        };
        let mut index = 0usize;
        for (spec, conversion) in &conversions {
            if *conversion == '%' {
                push(value::str_from_bytes(b"%"));
                continue;
            }
            let stars = spec.matches('*').count();
            if index + 1 + stars > items.len() {
                crate::heap::release_roots(roots);
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "not enough arguments for format string",
                );
            }
            let argument = if stars == 0 {
                items[index]
            } else {
                value::tuple_new(items[index..index + 1 + stars].to_vec())
            };
            let formatted = ops::binop(ops::OP_MOD, value::str_alloc(spec.clone()), argument);
            if exc::pending() {
                crate::heap::release_roots(roots);
                return NONE;
            }
            let formatted = if localizing_conversion(*conversion) {
                localize_formatted(formatted, grouping, monetary, &fields)
            } else {
                formatted
            };
            if exc::pending() {
                crate::heap::release_roots(roots);
                return NONE;
            }
            push(formatted);
            index += 1 + stars;
        }
    }
    let values: Vec<i64> = value::as_list(results)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let tuple = value::tuple_new(values);
    let result = ops::binop(ops::OP_MOD, template_value, tuple);
    crate::heap::release_roots(roots);
    result
}

/// `nl_langinfo(item)`.
fn nl_langinfo(args: &[i64]) -> i64 {
    let Some(item) = args.first().copied().and_then(value::as_integer) else {
        return exc::raise_with_message(TYPE_ERROR, "nl_langinfo() argument must be int");
    };
    #[cfg(unix)]
    {
        // SAFETY: `item` is an `nl_item` value; the returned string is owned
        // by libc.
        let pointer = unsafe { libc::nl_langinfo(item as libc::nl_item) };
        if pointer.is_null() {
            return value::str_from_bytes(b"");
        }
        // SAFETY: `nl_langinfo` returns a NUL-terminated string.
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
        value::str_alloc(text)
    }
    #[cfg(not(unix))]
    {
        let _ = item;
        NONE
    }
}

/// `delocalize(string)`: removes the thousands separators and maps the decimal
/// point back to `.`.
fn delocalize(args: &[i64]) -> i64 {
    let Some(text) = value::as_str(args[0]) else {
        return exc::raise_with_message(TYPE_ERROR, "delocalize() argument must be str");
    };
    let fields = conv_fields();
    let mut text = text.to_string();
    if !fields.thousands_sep.is_empty() {
        text = text.replace(&fields.thousands_sep, "");
    }
    if !fields.decimal_point.is_empty() {
        text = text.replace(&fields.decimal_point, ".");
    }
    value::str_alloc(text)
}

/// `localize(string, grouping=False, monetary=False)`.
fn localize(args: &[i64]) -> i64 {
    let Some(text) = value::as_str(args[0]) else {
        return exc::raise_with_message(TYPE_ERROR, "localize() argument must be str");
    };
    let grouping = args.get(1).is_some_and(|item| value::truthy(*item));
    let monetary = args.get(2).is_some_and(|item| value::truthy(*item));
    if exc::pending() {
        return NONE;
    }
    let fields = conv_fields();
    match localize_number(text, grouping, monetary, &fields) {
        Ok(text) => value::str_alloc(text),
        Err(error) => error,
    }
}

/// `atof(string, func=float)`: parses a locale-formatted number.
fn atof(args: &[i64]) -> i64 {
    let Some(_text) = value::as_str(args[0]) else {
        return exc::raise_with_message(TYPE_ERROR, "atof() argument must be str");
    };
    let delocalized = delocalize(&[args[0]]);
    if exc::pending() {
        return NONE;
    }
    if let Some(func) = args.get(1).copied().filter(|item| *item != NONE) {
        return crate::builtins::call(func, 1, &delocalized);
    }
    let Some(text) = value::as_str(delocalized).map(str::to_string) else {
        return NONE;
    };
    let text = text.trim();
    match text.parse::<f64>() {
        Ok(number) => value::float_new(number),
        Err(_) => exc::raise_with_message(
            VALUE_ERROR,
            &format!("could not convert string to float: {text:?}"),
        ),
    }
}

/// `atoi(string)`: parses a locale-formatted integer.
fn atoi(args: &[i64]) -> i64 {
    let Some(_text) = value::as_str(args[0]) else {
        return exc::raise_with_message(TYPE_ERROR, "atoi() argument must be str");
    };
    let delocalized = delocalize(&[args[0]]);
    if exc::pending() {
        return NONE;
    }
    let Some(text) = value::as_str(delocalized) else {
        return NONE;
    };
    match value::parse_decimal_int(text.trim()) {
        Some(number) => value::bigint_new(number),
        None => exc::raise_with_message(
            VALUE_ERROR,
            &format!("invalid literal for int() with base 10: {text:?}"),
        ),
    }
}

/// `str(val)`: `_format("%.12g", val)`.
fn locale_str(args: &[i64]) -> i64 {
    let formatted = ops::binop(ops::OP_MOD, value::str_from_bytes(b"%.12g"), args[0]);
    if exc::pending() {
        return NONE;
    }
    let fields = conv_fields();
    localize_formatted(formatted, false, false, &fields)
}

/// `currency(val, symbol=True, grouping=False, international=False)`.
fn currency(args: &[i64]) -> i64 {
    let dict = localeconv();
    let international = args.get(3).is_some_and(|item| value::truthy(*item));
    let symbol = args.get(1).is_none_or(|item| value::truthy(*item));
    let grouping = args.get(2).is_some_and(|item| value::truthy(*item));
    if exc::pending() {
        return NONE;
    }
    let digits = dict_int(
        dict,
        if international {
            "int_frac_digits"
        } else {
            "frac_digits"
        },
    );
    if digits == 127 {
        return exc::raise_with_message(
            VALUE_ERROR,
            "Currency formatting is not possible using the 'C' locale.",
        );
    }
    let number = match value::as_float(args[0]) {
        Some(number) => number,
        None => match value::as_integer(args[0]) {
            Some(integer) => integer as f64,
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "currency() argument must be a number, not {}",
                        value::type_name(args[0])
                    ),
                )
            }
        },
    };
    let negative = number.is_sign_negative();
    let fields = conv_fields();
    let formatted = format!("{:.*}", digits.max(0) as usize, number.abs());
    let Ok(mut text) = localize_number(&formatted, grouping, true, &fields) else {
        return NONE;
    };
    text = format!("<{text}>");
    if symbol {
        let symbol_text = dict_string(
            dict,
            if international {
                "int_curr_symbol"
            } else {
                "currency_symbol"
            },
        );
        let precedes = dict_int(
            dict,
            if negative {
                "n_cs_precedes"
            } else {
                "p_cs_precedes"
            },
        ) != 0;
        let separated = dict_int(
            dict,
            if negative {
                "n_sep_by_space"
            } else {
                "p_sep_by_space"
            },
        ) != 0;
        if precedes {
            let space = if separated { " " } else { "" };
            text = format!("{symbol_text}{space}{text}");
        } else {
            let mut symbol_text = symbol_text;
            if international && symbol_text.ends_with(' ') {
                symbol_text.pop();
            }
            let space = if separated { " " } else { "" };
            text = format!("{text}{space}{symbol_text}");
        }
    }
    let sign_position = dict_int(
        dict,
        if negative {
            "n_sign_posn"
        } else {
            "p_sign_posn"
        },
    );
    let sign = dict_string(
        dict,
        if negative {
            "negative_sign"
        } else {
            "positive_sign"
        },
    );
    text = match sign_position {
        0 => format!("({text})"),
        1 => format!("{sign}{text}"),
        2 => format!("{text}{sign}"),
        3 => text.replace('<', &sign),
        4 => text.replace('>', &sign),
        _ => format!("{sign}{text}"),
    };
    value::str_alloc(text.replace(['<', '>'], ""))
}

/// The POSIX env-var search behind `getdefaultlocale`.
fn default_locale(envvars: &[String]) -> i64 {
    let mut localename = String::from("C");
    for variable in envvars {
        if let Ok(text) = std::env::var(variable) {
            if !text.is_empty() {
                localename = if variable == "LANGUAGE" {
                    text.split(':').next().unwrap_or("").to_string()
                } else {
                    text
                };
                break;
            }
        }
    }
    match parse_localename(&localename) {
        Ok((language, encoding)) => value::tuple_new(vec![language, encoding]),
        Err(error) => error,
    }
}

fn env_var_names(value_arg: Option<i64>) -> Vec<String> {
    let defaults = ["LC_ALL", "LC_CTYPE", "LANG", "LANGUAGE"];
    match value_arg {
        Some(items) if items != NONE => {
            let list = value::as_tuple(items)
                .map(|items| items.to_vec())
                .or_else(|| value::as_list(items).map(|items| items.to_vec()));
            match list {
                Some(items) => items
                    .iter()
                    .filter_map(|item| value::as_str(*item).map(str::to_string))
                    .collect(),
                None => defaults.iter().map(|name| name.to_string()).collect(),
            }
        }
        _ => defaults.iter().map(|name| name.to_string()).collect(),
    }
}

/// `getdefaultlocale([envvars])` / `_getdefaultlocale([envvars])`.
fn getdefaultlocale(args: &[i64]) -> i64 {
    let envvars = env_var_names(args.first().copied());
    default_locale(&envvars)
}

/// `_parse_localename(localename)`: `(language, encoding)`, raising `ValueError`
/// for a name the alias engine cannot resolve.
fn parse_localename(name: &str) -> Result<(i64, i64), i64> {
    let code = normalize_name(name);
    let (code, modifier) = match code.split_once('@') {
        Some((code, modifier)) => (code.to_string(), Some(modifier.to_string())),
        None => (code, None),
    };
    if modifier.as_deref() == Some("euro") && !code.contains('.') {
        return Ok((
            value::str_alloc(code),
            value::str_alloc("iso-8859-15".to_string()),
        ));
    }
    if let Some((language, encoding)) = code.split_once('.') {
        return Ok((
            value::str_alloc(language.to_string()),
            value::str_alloc(encoding.to_string()),
        ));
    }
    if code == "C" {
        return Ok((NONE, NONE));
    }
    if code == "UTF-8" {
        return Ok((NONE, value::str_alloc("UTF-8".to_string())));
    }
    Err(exc::raise_with_message(
        VALUE_ERROR,
        &format!("unknown locale: {name}"),
    ))
}

fn parse_localename_call(args: &[i64]) -> i64 {
    let Some(name) = value::as_str(args[0]) else {
        return exc::raise_with_message(TYPE_ERROR, "_parse_localename() argument must be str");
    };
    match parse_localename(name) {
        Ok((language, encoding)) => value::tuple_new(vec![language, encoding]),
        Err(error) => error,
    }
}

/// `_build_localename(localetuple)`: `'language.encoding'` (or `'language'`).
fn build_localename(args: &[i64]) -> i64 {
    let Some(items) = value::as_tuple(args[0])
        .map(|items| items.to_vec())
        .or_else(|| value::as_list(args[0]).map(|items| items.to_vec()))
    else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "Locale must be None, a string, or an iterable of two strings -- \
             language code, encoding.",
        );
    };
    if items.len() != 2 {
        return exc::raise_with_message(
            TYPE_ERROR,
            "Locale must be None, a string, or an iterable of two strings -- \
             language code, encoding.",
        );
    }
    let language = if items[0] == NONE {
        "C".to_string()
    } else {
        match value::as_str(items[0]) {
            Some(text) => text.to_string(),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "Locale must be None, a string, or an iterable of two strings -- \
                     language code, encoding.",
                )
            }
        }
    };
    if items[1] == NONE {
        return value::str_alloc(language);
    }
    match value::as_str(items[1]) {
        Some(encoding) => value::str_alloc(format!("{language}.{encoding}")),
        None => exc::raise_with_message(
            TYPE_ERROR,
            "Locale must be None, a string, or an iterable of two strings -- \
             language code, encoding.",
        ),
    }
}

/// The `nl_item` constants `_locale` exports on glibc (the corpus platform);
/// `locale` re-exports them through `from _locale import *`.
const LANGINFO_CONSTANTS: &[(&str, i64)] = &[
    ("ABDAY_1", 0x20000),
    ("ABDAY_2", 0x20001),
    ("ABDAY_3", 0x20002),
    ("ABDAY_4", 0x20003),
    ("ABDAY_5", 0x20004),
    ("ABDAY_6", 0x20005),
    ("ABDAY_7", 0x20006),
    ("DAY_1", 0x20007),
    ("DAY_2", 0x20008),
    ("DAY_3", 0x20009),
    ("DAY_4", 0x2000A),
    ("DAY_5", 0x2000B),
    ("DAY_6", 0x2000C),
    ("DAY_7", 0x2000D),
    ("ABMON_1", 0x2000E),
    ("ABMON_2", 0x2000F),
    ("ABMON_3", 0x20010),
    ("ABMON_4", 0x20011),
    ("ABMON_5", 0x20012),
    ("ABMON_6", 0x20013),
    ("ABMON_7", 0x20014),
    ("ABMON_8", 0x20015),
    ("ABMON_9", 0x20016),
    ("ABMON_10", 0x20017),
    ("ABMON_11", 0x20018),
    ("ABMON_12", 0x20019),
    ("MON_1", 0x2001A),
    ("MON_2", 0x2001B),
    ("MON_3", 0x2001C),
    ("MON_4", 0x2001D),
    ("MON_5", 0x2001E),
    ("MON_6", 0x2001F),
    ("MON_7", 0x20020),
    ("MON_8", 0x20021),
    ("MON_9", 0x20022),
    ("MON_10", 0x20023),
    ("MON_11", 0x20024),
    ("MON_12", 0x20025),
    ("AM_STR", 0x20026),
    ("PM_STR", 0x20027),
    ("D_T_FMT", 0x20028),
    ("D_FMT", 0x20029),
    ("T_FMT", 0x2002A),
    ("T_FMT_AMPM", 0x2002B),
    ("ERA", 0x2002C),
    ("_DATE_FMT", 0x2002D),
    ("ERA_D_FMT", 0x2002E),
    ("ALT_DIGITS", 0x2002F),
    ("ERA_D_T_FMT", 0x20030),
    ("ERA_T_FMT", 0x20031),
    ("CODESET", 14),
    ("CRNCYSTR", 0x4000F),
    ("RADIXCHAR", 0x10000),
    ("THOUSEP", 0x10001),
    ("YESEXPR", 0x50000),
    ("NOEXPR", 0x50001),
    ("YESSTR", 0x50002),
    ("NOSTR", 0x50003),
];

/// CPython 3.14.9's `Lib/locale.py:locale_alias` table, verbatim.
const LOCALE_ALIASES: &[(&str, &str)] = &[
    ("a3", "az_AZ.KOI8-C"),
    ("a3_az", "az_AZ.KOI8-C"),
    ("a3_az.koic", "az_AZ.KOI8-C"),
    ("aa_dj", "aa_DJ.ISO8859-1"),
    ("aa_er", "aa_ER.UTF-8"),
    ("aa_et", "aa_ET.UTF-8"),
    ("af", "af_ZA.ISO8859-1"),
    ("af_za", "af_ZA.ISO8859-1"),
    ("agr_pe", "agr_PE.UTF-8"),
    ("ak_gh", "ak_GH.UTF-8"),
    ("am", "am_ET.UTF-8"),
    ("am_et", "am_ET.UTF-8"),
    ("american", "en_US.ISO8859-1"),
    ("an_es", "an_ES.ISO8859-15"),
    ("anp_in", "anp_IN.UTF-8"),
    ("ar", "ar_AA.ISO8859-6"),
    ("ar_aa", "ar_AA.ISO8859-6"),
    ("ar_ae", "ar_AE.ISO8859-6"),
    ("ar_bh", "ar_BH.ISO8859-6"),
    ("ar_dz", "ar_DZ.ISO8859-6"),
    ("ar_eg", "ar_EG.ISO8859-6"),
    ("ar_in", "ar_IN.UTF-8"),
    ("ar_iq", "ar_IQ.ISO8859-6"),
    ("ar_jo", "ar_JO.ISO8859-6"),
    ("ar_kw", "ar_KW.ISO8859-6"),
    ("ar_lb", "ar_LB.ISO8859-6"),
    ("ar_ly", "ar_LY.ISO8859-6"),
    ("ar_ma", "ar_MA.ISO8859-6"),
    ("ar_om", "ar_OM.ISO8859-6"),
    ("ar_qa", "ar_QA.ISO8859-6"),
    ("ar_sa", "ar_SA.ISO8859-6"),
    ("ar_sd", "ar_SD.ISO8859-6"),
    ("ar_ss", "ar_SS.UTF-8"),
    ("ar_sy", "ar_SY.ISO8859-6"),
    ("ar_tn", "ar_TN.ISO8859-6"),
    ("ar_ye", "ar_YE.ISO8859-6"),
    ("arabic", "ar_AA.ISO8859-6"),
    ("as", "as_IN.UTF-8"),
    ("as_in", "as_IN.UTF-8"),
    ("ast_es", "ast_ES.ISO8859-15"),
    ("ayc_pe", "ayc_PE.UTF-8"),
    ("az", "az_AZ.ISO8859-9E"),
    ("az_az", "az_AZ.ISO8859-9E"),
    ("az_az.iso88599e", "az_AZ.ISO8859-9E"),
    ("az_ir", "az_IR.UTF-8"),
    ("be", "be_BY.CP1251"),
    ("be@latin", "be_BY.UTF-8@latin"),
    ("be_bg.utf8", "bg_BG.UTF-8"),
    ("be_by", "be_BY.CP1251"),
    ("be_by@latin", "be_BY.UTF-8@latin"),
    ("bem_zm", "bem_ZM.UTF-8"),
    ("ber_dz", "ber_DZ.UTF-8"),
    ("ber_ma", "ber_MA.UTF-8"),
    ("bg", "bg_BG.CP1251"),
    ("bg_bg", "bg_BG.CP1251"),
    ("bhb_in.utf8", "bhb_IN.UTF-8"),
    ("bho_in", "bho_IN.UTF-8"),
    ("bho_np", "bho_NP.UTF-8"),
    ("bi_vu", "bi_VU.UTF-8"),
    ("bn_bd", "bn_BD.UTF-8"),
    ("bn_in", "bn_IN.UTF-8"),
    ("bo_cn", "bo_CN.UTF-8"),
    ("bo_in", "bo_IN.UTF-8"),
    ("bokmal", "nb_NO.ISO8859-1"),
    ("bokmål", "nb_NO.ISO8859-1"),
    ("br", "br_FR.ISO8859-1"),
    ("br_fr", "br_FR.ISO8859-1"),
    ("brx_in", "brx_IN.UTF-8"),
    ("bs", "bs_BA.ISO8859-2"),
    ("bs_ba", "bs_BA.ISO8859-2"),
    ("bulgarian", "bg_BG.CP1251"),
    ("byn_er", "byn_ER.UTF-8"),
    ("c", "C"),
    ("c-french", "fr_CA.ISO8859-1"),
    ("c.ascii", "C"),
    ("c.en", "C"),
    ("c.iso88591", "en_US.ISO8859-1"),
    ("c_c", "C"),
    ("c_c.c", "C"),
    ("ca", "ca_ES.ISO8859-1"),
    ("ca_ad", "ca_AD.ISO8859-1"),
    ("ca_es", "ca_ES.ISO8859-1"),
    ("ca_es@valencia", "ca_ES.UTF-8@valencia"),
    ("ca_fr", "ca_FR.ISO8859-1"),
    ("ca_it", "ca_IT.ISO8859-1"),
    ("catalan", "ca_ES.ISO8859-1"),
    ("ce_ru", "ce_RU.UTF-8"),
    ("cextend", "en_US.ISO8859-1"),
    ("chinese-s", "zh_CN.eucCN"),
    ("chinese-t", "zh_TW.eucTW"),
    ("chr_us", "chr_US.UTF-8"),
    ("ckb_iq", "ckb_IQ.UTF-8"),
    ("cmn_tw", "cmn_TW.UTF-8"),
    ("crh_ru", "crh_RU.UTF-8"),
    ("crh_ua", "crh_UA.UTF-8"),
    ("croatian", "hr_HR.ISO8859-2"),
    ("cs", "cs_CZ.ISO8859-2"),
    ("cs_cs", "cs_CZ.ISO8859-2"),
    ("cs_cz", "cs_CZ.ISO8859-2"),
    ("csb_pl", "csb_PL.UTF-8"),
    ("cv_ru", "cv_RU.UTF-8"),
    ("cy", "cy_GB.ISO8859-1"),
    ("cy_gb", "cy_GB.ISO8859-1"),
    ("cz", "cs_CZ.ISO8859-2"),
    ("cz_cz", "cs_CZ.ISO8859-2"),
    ("czech", "cs_CZ.ISO8859-2"),
    ("da", "da_DK.ISO8859-1"),
    ("da_dk", "da_DK.ISO8859-1"),
    ("danish", "da_DK.ISO8859-1"),
    ("dansk", "da_DK.ISO8859-1"),
    ("de", "de_DE.ISO8859-1"),
    ("de_at", "de_AT.ISO8859-1"),
    ("de_be", "de_BE.ISO8859-1"),
    ("de_ch", "de_CH.ISO8859-1"),
    ("de_de", "de_DE.ISO8859-1"),
    ("de_it", "de_IT.UTF-8"),
    ("de_li", "de_LI.ISO8859-1"),
    ("de_lu", "de_LU.ISO8859-1"),
    ("deutsch", "de_DE.ISO8859-1"),
    ("doi_in", "doi_IN.UTF-8"),
    ("dsb_de", "dsb_DE.UTF-8"),
    ("dutch", "nl_NL.ISO8859-1"),
    ("dutch.iso88591", "nl_BE.ISO8859-1"),
    ("dv_mv", "dv_MV.UTF-8"),
    ("dz_bt", "dz_BT.UTF-8"),
    ("ee", "ee_EE.ISO8859-4"),
    ("ee_ee", "ee_EE.ISO8859-4"),
    ("eesti", "et_EE.ISO8859-1"),
    ("el", "el_GR.ISO8859-7"),
    ("el_cy", "el_CY.ISO8859-7"),
    ("el_gr", "el_GR.ISO8859-7"),
    ("el_gr@euro", "el_GR.ISO8859-15"),
    ("en", "en_US.ISO8859-1"),
    ("en_ag", "en_AG.UTF-8"),
    ("en_au", "en_AU.ISO8859-1"),
    ("en_be", "en_BE.ISO8859-1"),
    ("en_bw", "en_BW.ISO8859-1"),
    ("en_ca", "en_CA.ISO8859-1"),
    ("en_dk", "en_DK.ISO8859-1"),
    ("en_dl.utf8", "en_DL.UTF-8"),
    ("en_gb", "en_GB.ISO8859-1"),
    ("en_hk", "en_HK.ISO8859-1"),
    ("en_ie", "en_IE.ISO8859-1"),
    ("en_il", "en_IL.ISO8859-1"),
    ("en_in", "en_IN.ISO8859-1"),
    ("en_ng", "en_NG.UTF-8"),
    ("en_nz", "en_NZ.ISO8859-1"),
    ("en_ph", "en_PH.ISO8859-1"),
    ("en_sc.utf8", "en_SC.UTF-8"),
    ("en_sg", "en_SG.ISO8859-1"),
    ("en_uk", "en_GB.ISO8859-1"),
    ("en_us", "en_US.ISO8859-1"),
    ("en_us@euro@euro", "en_US.ISO8859-15"),
    ("en_za", "en_ZA.ISO8859-1"),
    ("en_zm", "en_ZM.UTF-8"),
    ("en_zw", "en_ZW.ISO8859-1"),
    ("en_zw.utf8", "en_ZS.UTF-8"),
    ("eng_gb", "en_GB.ISO8859-1"),
    ("english", "en_EN.ISO8859-1"),
    ("english_uk", "en_GB.ISO8859-1"),
    ("english_united-states", "en_US.ISO8859-1"),
    ("english_united-states.437", "C"),
    ("english_us", "en_US.ISO8859-1"),
    ("eo", "eo_XX.ISO8859-3"),
    ("eo.utf8", "eo.UTF-8"),
    ("eo_eo", "eo_EO.ISO8859-3"),
    ("eo_us.utf8", "eo_US.UTF-8"),
    ("eo_xx", "eo_XX.ISO8859-3"),
    ("es", "es_ES.ISO8859-1"),
    ("es_ar", "es_AR.ISO8859-1"),
    ("es_bo", "es_BO.ISO8859-1"),
    ("es_cl", "es_CL.ISO8859-1"),
    ("es_co", "es_CO.ISO8859-1"),
    ("es_cr", "es_CR.ISO8859-1"),
    ("es_cu", "es_CU.ISO8859-1"),
    ("es_do", "es_DO.ISO8859-1"),
    ("es_ec", "es_EC.ISO8859-1"),
    ("es_es", "es_ES.ISO8859-1"),
    ("es_gt", "es_GT.ISO8859-1"),
    ("es_hn", "es_HN.ISO8859-1"),
    ("es_mx", "es_MX.ISO8859-1"),
    ("es_ni", "es_NI.ISO8859-1"),
    ("es_pa", "es_PA.ISO8859-1"),
    ("es_pe", "es_PE.ISO8859-1"),
    ("es_pr", "es_PR.ISO8859-1"),
    ("es_py", "es_PY.ISO8859-1"),
    ("es_sv", "es_SV.ISO8859-1"),
    ("es_us", "es_US.ISO8859-1"),
    ("es_uy", "es_UY.ISO8859-1"),
    ("es_ve", "es_VE.ISO8859-1"),
    ("estonian", "et_EE.ISO8859-1"),
    ("et", "et_EE.ISO8859-15"),
    ("et_ee", "et_EE.ISO8859-15"),
    ("eu", "eu_ES.ISO8859-1"),
    ("eu_es", "eu_ES.ISO8859-1"),
    ("eu_fr", "eu_FR.ISO8859-1"),
    ("fa", "fa_IR.UTF-8"),
    ("fa_ir", "fa_IR.UTF-8"),
    ("fa_ir.isiri3342", "fa_IR.ISIRI-3342"),
    ("ff_sn", "ff_SN.UTF-8"),
    ("fi", "fi_FI.ISO8859-15"),
    ("fi_fi", "fi_FI.ISO8859-15"),
    ("fil_ph", "fil_PH.UTF-8"),
    ("finnish", "fi_FI.ISO8859-1"),
    ("fo", "fo_FO.ISO8859-1"),
    ("fo_fo", "fo_FO.ISO8859-1"),
    ("fr", "fr_FR.ISO8859-1"),
    ("fr_be", "fr_BE.ISO8859-1"),
    ("fr_ca", "fr_CA.ISO8859-1"),
    ("fr_ch", "fr_CH.ISO8859-1"),
    ("fr_fr", "fr_FR.ISO8859-1"),
    ("fr_lu", "fr_LU.ISO8859-1"),
    ("français", "fr_FR.ISO8859-1"),
    ("fre_fr", "fr_FR.ISO8859-1"),
    ("french", "fr_FR.ISO8859-1"),
    ("french.iso88591", "fr_CH.ISO8859-1"),
    ("french_france", "fr_FR.ISO8859-1"),
    ("fur_it", "fur_IT.UTF-8"),
    ("fy_de", "fy_DE.UTF-8"),
    ("fy_nl", "fy_NL.UTF-8"),
    ("ga", "ga_IE.ISO8859-1"),
    ("ga_ie", "ga_IE.ISO8859-1"),
    ("galego", "gl_ES.ISO8859-1"),
    ("galician", "gl_ES.ISO8859-1"),
    ("gbm_in", "gbm_IN.UTF-8"),
    ("gd", "gd_GB.ISO8859-1"),
    ("gd_gb", "gd_GB.ISO8859-1"),
    ("ger_de", "de_DE.ISO8859-1"),
    ("german", "de_DE.ISO8859-1"),
    ("german.iso88591", "de_CH.ISO8859-1"),
    ("german_germany", "de_DE.ISO8859-1"),
    ("gez_er", "gez_ER.UTF-8"),
    ("gez_et", "gez_ET.UTF-8"),
    ("gl", "gl_ES.ISO8859-1"),
    ("gl_es", "gl_ES.ISO8859-1"),
    ("greek", "el_GR.ISO8859-7"),
    ("gu_in", "gu_IN.UTF-8"),
    ("gv", "gv_GB.ISO8859-1"),
    ("gv_gb", "gv_GB.ISO8859-1"),
    ("ha_ng", "ha_NG.UTF-8"),
    ("hak_tw", "hak_TW.UTF-8"),
    ("he", "he_IL.ISO8859-8"),
    ("he_il", "he_IL.ISO8859-8"),
    ("hebrew", "he_IL.ISO8859-8"),
    ("hi", "hi_IN.ISCII-DEV"),
    ("hi_in", "hi_IN.ISCII-DEV"),
    ("hi_in.isciidev", "hi_IN.ISCII-DEV"),
    ("hif_fj", "hif_FJ.UTF-8"),
    ("hne", "hne_IN.UTF-8"),
    ("hne_in", "hne_IN.UTF-8"),
    ("hr", "hr_HR.ISO8859-2"),
    ("hr_hr", "hr_HR.ISO8859-2"),
    ("hrvatski", "hr_HR.ISO8859-2"),
    ("hsb_de", "hsb_DE.ISO8859-2"),
    ("ht_ht", "ht_HT.UTF-8"),
    ("hu", "hu_HU.ISO8859-2"),
    ("hu_hu", "hu_HU.ISO8859-2"),
    ("hungarian", "hu_HU.ISO8859-2"),
    ("hy_am", "hy_AM.UTF-8"),
    ("hy_am.armscii8", "hy_AM.ARMSCII_8"),
    ("ia", "ia.UTF-8"),
    ("ia_fr", "ia_FR.UTF-8"),
    ("icelandic", "is_IS.ISO8859-1"),
    ("id", "id_ID.ISO8859-1"),
    ("id_id", "id_ID.ISO8859-1"),
    ("ie", "ie.UTF-8"),
    ("ig_ng", "ig_NG.UTF-8"),
    ("ik_ca", "ik_CA.UTF-8"),
    ("in", "id_ID.ISO8859-1"),
    ("in_id", "id_ID.ISO8859-1"),
    ("is", "is_IS.ISO8859-1"),
    ("is_is", "is_IS.ISO8859-1"),
    ("iso-8859-1", "en_US.ISO8859-1"),
    ("iso-8859-15", "en_US.ISO8859-15"),
    ("iso8859-1", "en_US.ISO8859-1"),
    ("iso8859-15", "en_US.ISO8859-15"),
    ("iso_8859_1", "en_US.ISO8859-1"),
    ("iso_8859_15", "en_US.ISO8859-15"),
    ("it", "it_IT.ISO8859-1"),
    ("it_ch", "it_CH.ISO8859-1"),
    ("it_it", "it_IT.ISO8859-1"),
    ("italian", "it_IT.ISO8859-1"),
    ("iu", "iu_CA.NUNACOM-8"),
    ("iu_ca", "iu_CA.NUNACOM-8"),
    ("iu_ca.nunacom8", "iu_CA.NUNACOM-8"),
    ("iw", "he_IL.ISO8859-8"),
    ("iw_il", "he_IL.ISO8859-8"),
    ("iw_il.utf8", "iw_IL.UTF-8"),
    ("ja", "ja_JP.eucJP"),
    ("ja_jp", "ja_JP.eucJP"),
    ("ja_jp.euc", "ja_JP.eucJP"),
    ("ja_jp.mscode", "ja_JP.SJIS"),
    ("ja_jp.pck", "ja_JP.SJIS"),
    ("japan", "ja_JP.eucJP"),
    ("japanese", "ja_JP.eucJP"),
    ("japanese-euc", "ja_JP.eucJP"),
    ("japanese.euc", "ja_JP.eucJP"),
    ("jp_jp", "ja_JP.eucJP"),
    ("ka", "ka_GE.GEORGIAN-ACADEMY"),
    ("ka_ge", "ka_GE.GEORGIAN-ACADEMY"),
    ("ka_ge.georgianacademy", "ka_GE.GEORGIAN-ACADEMY"),
    ("ka_ge.georgianps", "ka_GE.GEORGIAN-PS"),
    ("ka_ge.georgianrs", "ka_GE.GEORGIAN-ACADEMY"),
    ("kab_dz", "kab_DZ.UTF-8"),
    ("kk_kz", "kk_KZ.ptcp154"),
    ("kl", "kl_GL.ISO8859-1"),
    ("kl_gl", "kl_GL.ISO8859-1"),
    ("km_kh", "km_KH.UTF-8"),
    ("kn", "kn_IN.UTF-8"),
    ("kn_in", "kn_IN.UTF-8"),
    ("ko", "ko_KR.eucKR"),
    ("ko_kr", "ko_KR.eucKR"),
    ("ko_kr.euc", "ko_KR.eucKR"),
    ("kok_in", "kok_IN.UTF-8"),
    ("korean", "ko_KR.eucKR"),
    ("korean.euc", "ko_KR.eucKR"),
    ("ks", "ks_IN.UTF-8"),
    ("ks_in", "ks_IN.UTF-8"),
    ("ks_in@devanagari.utf8", "ks_IN.UTF-8@devanagari"),
    ("ku_tr", "ku_TR.ISO8859-9"),
    ("kv_ru", "kv_RU.UTF-8"),
    ("kw", "kw_GB.ISO8859-1"),
    ("kw_gb", "kw_GB.ISO8859-1"),
    ("ky", "ky_KG.UTF-8"),
    ("ky_kg", "ky_KG.UTF-8"),
    ("lb_lu", "lb_LU.UTF-8"),
    ("lg_ug", "lg_UG.ISO8859-10"),
    ("li_be", "li_BE.UTF-8"),
    ("li_nl", "li_NL.UTF-8"),
    ("lij_it", "lij_IT.UTF-8"),
    ("lithuanian", "lt_LT.ISO8859-13"),
    ("ln_cd", "ln_CD.UTF-8"),
    ("lo", "lo_LA.MULELAO-1"),
    ("lo_la", "lo_LA.MULELAO-1"),
    ("lo_la.cp1133", "lo_LA.IBM-CP1133"),
    ("lo_la.ibmcp1133", "lo_LA.IBM-CP1133"),
    ("lo_la.mulelao1", "lo_LA.MULELAO-1"),
    ("lt", "lt_LT.ISO8859-13"),
    ("lt_lt", "lt_LT.ISO8859-13"),
    ("ltg_lv.utf8", "ltg_LV.UTF-8"),
    ("lv", "lv_LV.ISO8859-13"),
    ("lv_lv", "lv_LV.ISO8859-13"),
    ("lzh_tw", "lzh_TW.UTF-8"),
    ("mag_in", "mag_IN.UTF-8"),
    ("mai", "mai_IN.UTF-8"),
    ("mai_in", "mai_IN.UTF-8"),
    ("mai_np", "mai_NP.UTF-8"),
    ("mdf_ru", "mdf_RU.UTF-8"),
    ("mfe_mu", "mfe_MU.UTF-8"),
    ("mg_mg", "mg_MG.ISO8859-15"),
    ("mhr_ru", "mhr_RU.UTF-8"),
    ("mi", "mi_NZ.ISO8859-1"),
    ("mi_nz", "mi_NZ.ISO8859-1"),
    ("miq_ni", "miq_NI.UTF-8"),
    ("mjw_in", "mjw_IN.UTF-8"),
    ("mk", "mk_MK.ISO8859-5"),
    ("mk_mk", "mk_MK.ISO8859-5"),
    ("ml", "ml_IN.UTF-8"),
    ("ml_in", "ml_IN.UTF-8"),
    ("mn_mn", "mn_MN.UTF-8"),
    ("mni_in", "mni_IN.UTF-8"),
    ("mnw_mm", "mnw_MM.UTF-8"),
    ("mr", "mr_IN.UTF-8"),
    ("mr_in", "mr_IN.UTF-8"),
    ("ms", "ms_MY.ISO8859-1"),
    ("ms_my", "ms_MY.ISO8859-1"),
    ("mt", "mt_MT.ISO8859-3"),
    ("mt_mt", "mt_MT.ISO8859-3"),
    ("my_mm", "my_MM.UTF-8"),
    ("nan_tw", "nan_TW.UTF-8"),
    ("nb", "nb_NO.ISO8859-1"),
    ("nb_no", "nb_NO.ISO8859-1"),
    ("nds_de", "nds_DE.UTF-8"),
    ("nds_nl", "nds_NL.UTF-8"),
    ("ne_np", "ne_NP.UTF-8"),
    ("nhn_mx", "nhn_MX.UTF-8"),
    ("niu_nu", "niu_NU.UTF-8"),
    ("niu_nz", "niu_NZ.UTF-8"),
    ("nl", "nl_NL.ISO8859-1"),
    ("nl_aw", "nl_AW.UTF-8"),
    ("nl_be", "nl_BE.ISO8859-1"),
    ("nl_nl", "nl_NL.ISO8859-1"),
    ("nn", "nn_NO.ISO8859-1"),
    ("nn_no", "nn_NO.ISO8859-1"),
    ("no", "no_NO.ISO8859-1"),
    ("no@nynorsk", "ny_NO.ISO8859-1"),
    ("no_no", "no_NO.ISO8859-1"),
    ("no_no.iso88591@bokmal", "no_NO.ISO8859-1"),
    ("no_no.iso88591@nynorsk", "no_NO.ISO8859-1"),
    ("norwegian", "no_NO.ISO8859-1"),
    ("nr", "nr_ZA.ISO8859-1"),
    ("nr_za", "nr_ZA.ISO8859-1"),
    ("nso", "nso_ZA.ISO8859-15"),
    ("nso_za", "nso_ZA.ISO8859-15"),
    ("ny", "ny_NO.ISO8859-1"),
    ("ny_no", "ny_NO.ISO8859-1"),
    ("nynorsk", "nn_NO.ISO8859-1"),
    ("oc", "oc_FR.ISO8859-1"),
    ("oc_fr", "oc_FR.ISO8859-1"),
    ("om_et", "om_ET.UTF-8"),
    ("om_ke", "om_KE.ISO8859-1"),
    ("or", "or_IN.UTF-8"),
    ("or_in", "or_IN.UTF-8"),
    ("os_ru", "os_RU.UTF-8"),
    ("pa", "pa_IN.UTF-8"),
    ("pa_in", "pa_IN.UTF-8"),
    ("pa_pk", "pa_PK.UTF-8"),
    ("pap_an", "pap_AN.UTF-8"),
    ("pap_aw", "pap_AW.UTF-8"),
    ("pap_cw", "pap_CW.UTF-8"),
    ("pd", "pd_US.ISO8859-1"),
    ("pd_de", "pd_DE.ISO8859-1"),
    ("pd_us", "pd_US.ISO8859-1"),
    ("ph", "ph_PH.ISO8859-1"),
    ("ph_ph", "ph_PH.ISO8859-1"),
    ("pl", "pl_PL.ISO8859-2"),
    ("pl_pl", "pl_PL.ISO8859-2"),
    ("polish", "pl_PL.ISO8859-2"),
    ("portuguese", "pt_PT.ISO8859-1"),
    ("portuguese_brazil", "pt_BR.ISO8859-1"),
    ("posix", "C"),
    ("posix-utf2", "C"),
    ("pp", "pp_AN.ISO8859-1"),
    ("pp_an", "pp_AN.ISO8859-1"),
    ("ps_af", "ps_AF.UTF-8"),
    ("pt", "pt_PT.ISO8859-1"),
    ("pt_br", "pt_BR.ISO8859-1"),
    ("pt_pt", "pt_PT.ISO8859-1"),
    ("quz_pe", "quz_PE.UTF-8"),
    ("raj_in", "raj_IN.UTF-8"),
    ("rif_ma", "rif_MA.UTF-8"),
    ("ro", "ro_RO.ISO8859-2"),
    ("ro_ro", "ro_RO.ISO8859-2"),
    ("romanian", "ro_RO.ISO8859-2"),
    ("ru", "ru_RU.UTF-8"),
    ("ru_ru", "ru_RU.UTF-8"),
    ("ru_ua", "ru_UA.KOI8-U"),
    ("rumanian", "ro_RO.ISO8859-2"),
    ("russian", "ru_RU.ISO8859-5"),
    ("rw", "rw_RW.ISO8859-1"),
    ("rw_rw", "rw_RW.ISO8859-1"),
    ("sa_in", "sa_IN.UTF-8"),
    ("sah_ru", "sah_RU.UTF-8"),
    ("sat_in", "sat_IN.UTF-8"),
    ("sc_it", "sc_IT.UTF-8"),
    ("scn_it", "scn_IT.UTF-8"),
    ("sd", "sd_IN.UTF-8"),
    ("sd_in", "sd_IN.UTF-8"),
    ("sd_in@devanagari.utf8", "sd_IN.UTF-8@devanagari"),
    ("sd_pk", "sd_PK.UTF-8"),
    ("se_no", "se_NO.UTF-8"),
    ("serbocroatian", "sr_RS.UTF-8@latin"),
    ("sgs_lt", "sgs_LT.UTF-8"),
    ("sh", "sr_RS.UTF-8@latin"),
    ("sh_ba.iso88592@bosnia", "sr_CS.ISO8859-2"),
    ("sh_hr", "sh_HR.ISO8859-2"),
    ("sh_hr.iso88592", "hr_HR.ISO8859-2"),
    ("sh_sp", "sr_CS.ISO8859-2"),
    ("sh_yu", "sr_RS.UTF-8@latin"),
    ("shn_mm", "shn_MM.UTF-8"),
    ("shs_ca", "shs_CA.UTF-8"),
    ("si", "si_LK.UTF-8"),
    ("si_lk", "si_LK.UTF-8"),
    ("sid_et", "sid_ET.UTF-8"),
    ("sinhala", "si_LK.UTF-8"),
    ("sk", "sk_SK.ISO8859-2"),
    ("sk_sk", "sk_SK.ISO8859-2"),
    ("sl", "sl_SI.ISO8859-2"),
    ("sl_cs", "sl_CS.ISO8859-2"),
    ("sl_si", "sl_SI.ISO8859-2"),
    ("slovak", "sk_SK.ISO8859-2"),
    ("slovene", "sl_SI.ISO8859-2"),
    ("slovenian", "sl_SI.ISO8859-2"),
    ("sm_ws", "sm_WS.UTF-8"),
    ("so_dj", "so_DJ.ISO8859-1"),
    ("so_et", "so_ET.UTF-8"),
    ("so_ke", "so_KE.ISO8859-1"),
    ("so_so", "so_SO.ISO8859-1"),
    ("sp", "sr_CS.ISO8859-5"),
    ("sp_yu", "sr_CS.ISO8859-5"),
    ("spanish", "es_ES.ISO8859-1"),
    ("spanish_spain", "es_ES.ISO8859-1"),
    ("sq", "sq_AL.ISO8859-2"),
    ("sq_al", "sq_AL.ISO8859-2"),
    ("sq_mk", "sq_MK.UTF-8"),
    ("sr", "sr_RS.UTF-8"),
    ("sr@cyrillic", "sr_RS.UTF-8"),
    ("sr@latn", "sr_RS.UTF-8@latin"),
    ("sr_cs", "sr_CS.UTF-8"),
    ("sr_cs.iso88592@latn", "sr_CS.ISO8859-2"),
    ("sr_cs@latn", "sr_CS.UTF-8@latin"),
    ("sr_me", "sr_ME.UTF-8"),
    ("sr_rs", "sr_RS.UTF-8"),
    ("sr_rs@latn", "sr_RS.UTF-8@latin"),
    ("sr_sp", "sr_CS.ISO8859-2"),
    ("sr_yu", "sr_RS.UTF-8@latin"),
    ("sr_yu.cp1251@cyrillic", "sr_CS.CP1251"),
    ("sr_yu.iso88592", "sr_CS.ISO8859-2"),
    ("sr_yu.iso88595", "sr_CS.ISO8859-5"),
    ("sr_yu.iso88595@cyrillic", "sr_CS.ISO8859-5"),
    ("sr_yu.microsoftcp1251@cyrillic", "sr_CS.CP1251"),
    ("sr_yu.utf8", "sr_RS.UTF-8"),
    ("sr_yu.utf8@cyrillic", "sr_RS.UTF-8"),
    ("sr_yu@cyrillic", "sr_RS.UTF-8"),
    ("ss", "ss_ZA.ISO8859-1"),
    ("ss_za", "ss_ZA.ISO8859-1"),
    ("ssy_er", "ssy_ER.UTF-8"),
    ("st", "st_ZA.ISO8859-1"),
    ("st_za", "st_ZA.ISO8859-1"),
    ("su_id", "su_ID.UTF-8"),
    ("sv", "sv_SE.ISO8859-1"),
    ("sv_fi", "sv_FI.ISO8859-1"),
    ("sv_se", "sv_SE.ISO8859-1"),
    ("sw_ke", "sw_KE.UTF-8"),
    ("sw_tz", "sw_TZ.UTF-8"),
    ("swedish", "sv_SE.ISO8859-1"),
    ("syr", "syr.UTF-8"),
    ("szl_pl", "szl_PL.UTF-8"),
    ("ta", "ta_IN.TSCII-0"),
    ("ta_in", "ta_IN.TSCII-0"),
    ("ta_in.tscii", "ta_IN.TSCII-0"),
    ("ta_in.tscii0", "ta_IN.TSCII-0"),
    ("ta_lk", "ta_LK.UTF-8"),
    ("tcy_in.utf8", "tcy_IN.UTF-8"),
    ("te", "te_IN.UTF-8"),
    ("te_in", "te_IN.UTF-8"),
    ("tg", "tg_TJ.KOI8-C"),
    ("tg_tj", "tg_TJ.KOI8-C"),
    ("th", "th_TH.ISO8859-11"),
    ("th_th", "th_TH.ISO8859-11"),
    ("th_th.tactis", "th_TH.TIS620"),
    ("th_th.tis620", "th_TH.TIS620"),
    ("thai", "th_TH.ISO8859-11"),
    ("the_np", "the_NP.UTF-8"),
    ("ti_er", "ti_ER.UTF-8"),
    ("ti_et", "ti_ET.UTF-8"),
    ("tig_er", "tig_ER.UTF-8"),
    ("tk_tm", "tk_TM.UTF-8"),
    ("tl", "tl_PH.ISO8859-1"),
    ("tl_ph", "tl_PH.ISO8859-1"),
    ("tn", "tn_ZA.ISO8859-15"),
    ("tn_za", "tn_ZA.ISO8859-15"),
    ("to_to", "to_TO.UTF-8"),
    ("tok", "tok.UTF-8"),
    ("tpi_pg", "tpi_PG.UTF-8"),
    ("tr", "tr_TR.ISO8859-9"),
    ("tr_cy", "tr_CY.ISO8859-9"),
    ("tr_tr", "tr_TR.ISO8859-9"),
    ("ts", "ts_ZA.ISO8859-1"),
    ("ts_za", "ts_ZA.ISO8859-1"),
    ("tt", "tt_RU.TATAR-CYR"),
    ("tt_ru", "tt_RU.TATAR-CYR"),
    ("tt_ru.tatarcyr", "tt_RU.TATAR-CYR"),
    ("tt_ru@iqtelif", "tt_RU.UTF-8@iqtelif"),
    ("turkish", "tr_TR.ISO8859-9"),
    ("ug_cn", "ug_CN.UTF-8"),
    ("uk", "uk_UA.KOI8-U"),
    ("uk_ua", "uk_UA.KOI8-U"),
    ("univ.utf8", "en_US.UTF-8"),
    ("universal.utf8@ucs4", "en_US.UTF-8"),
    ("unm_us", "unm_US.UTF-8"),
    ("ur", "ur_PK.CP1256"),
    ("ur_in", "ur_IN.UTF-8"),
    ("ur_pk", "ur_PK.CP1256"),
    ("uz", "uz_UZ.UTF-8"),
    ("uz_uz", "uz_UZ.UTF-8"),
    ("uz_uz@cyrillic", "uz_UZ.UTF-8"),
    ("ve", "ve_ZA.UTF-8"),
    ("ve_za", "ve_ZA.UTF-8"),
    ("vi", "vi_VN.TCVN"),
    ("vi_vn", "vi_VN.TCVN"),
    ("vi_vn.tcvn", "vi_VN.TCVN"),
    ("vi_vn.tcvn5712", "vi_VN.TCVN"),
    ("vi_vn.viscii", "vi_VN.VISCII"),
    ("vi_vn.viscii111", "vi_VN.VISCII"),
    ("wa", "wa_BE.ISO8859-1"),
    ("wa_be", "wa_BE.ISO8859-1"),
    ("wae_ch", "wae_CH.UTF-8"),
    ("wal_et", "wal_ET.UTF-8"),
    ("wo_sn", "wo_SN.UTF-8"),
    ("xh", "xh_ZA.ISO8859-1"),
    ("xh_za", "xh_ZA.ISO8859-1"),
    ("yi", "yi_US.CP1255"),
    ("yi_us", "yi_US.CP1255"),
    ("yo_ng", "yo_NG.UTF-8"),
    ("yue_hk", "yue_HK.UTF-8"),
    ("yuw_pg", "yuw_PG.UTF-8"),
    ("zgh_ma", "zgh_MA.UTF-8"),
    ("zh", "zh_CN.eucCN"),
    ("zh_cn", "zh_CN.gb2312"),
    ("zh_cn.big5", "zh_TW.big5"),
    ("zh_cn.euc", "zh_CN.eucCN"),
    ("zh_hk", "zh_HK.big5hkscs"),
    ("zh_hk.big5hk", "zh_HK.big5hkscs"),
    ("zh_sg", "zh_SG.GB2312"),
    ("zh_sg.gbk", "zh_SG.GBK"),
    ("zh_tw", "zh_TW.big5"),
    ("zh_tw.euc", "zh_TW.eucTW"),
    ("zh_tw.euctw", "zh_TW.eucTW"),
    ("zu", "zu_ZA.ISO8859-1"),
    ("zu_za", "zu_ZA.ISO8859-1"),
];

/// CPython 3.14.9's `Lib/locale.py:locale_encoding_alias` table, verbatim.
const LOCALE_ENCODING_ALIASES: &[(&str, &str)] = &[
    ("437", "C"),
    ("88591", "ISO8859-1"),
    ("885915", "ISO8859-15"),
    ("88592", "ISO8859-2"),
    ("88595", "ISO8859-5"),
    ("ajec", "eucJP"),
    ("ascii", "ISO8859-1"),
    ("c", "C"),
    ("cp1251", "CP1251"),
    ("cp1255", "CP1255"),
    ("cp1256", "CP1256"),
    ("en", "ISO8859-1"),
    ("euc_jp", "eucJP"),
    ("euc_kr", "eucKR"),
    ("eucjp", "eucJP"),
    ("euckr", "eucKR"),
    ("iso2022_jp", "JIS7"),
    ("iso2022jp", "JIS7"),
    ("iso88591", "ISO8859-1"),
    ("iso885910", "ISO8859-10"),
    ("iso885911", "ISO8859-11"),
    ("iso885913", "ISO8859-13"),
    ("iso885914", "ISO8859-14"),
    ("iso885915", "ISO8859-15"),
    ("iso885916", "ISO8859-16"),
    ("iso88592", "ISO8859-2"),
    ("iso88593", "ISO8859-3"),
    ("iso88594", "ISO8859-4"),
    ("iso88595", "ISO8859-5"),
    ("iso88596", "ISO8859-6"),
    ("iso88597", "ISO8859-7"),
    ("iso88598", "ISO8859-8"),
    ("iso88599", "ISO8859-9"),
    ("iso8859_1", "ISO8859-1"),
    ("iso8859_10", "ISO8859-10"),
    ("iso8859_11", "ISO8859-11"),
    ("iso8859_13", "ISO8859-13"),
    ("iso8859_14", "ISO8859-14"),
    ("iso8859_15", "ISO8859-15"),
    ("iso8859_16", "ISO8859-16"),
    ("iso8859_2", "ISO8859-2"),
    ("iso8859_3", "ISO8859-3"),
    ("iso8859_4", "ISO8859-4"),
    ("iso8859_5", "ISO8859-5"),
    ("iso8859_6", "ISO8859-6"),
    ("iso8859_7", "ISO8859-7"),
    ("iso8859_8", "ISO8859-8"),
    ("iso8859_9", "ISO8859-9"),
    ("jis", "JIS7"),
    ("jis7", "JIS7"),
    ("koi8_r", "KOI8-R"),
    ("koi8_t", "KOI8-T"),
    ("koi8_u", "KOI8-U"),
    ("koi8c", "KOI8-C"),
    ("koi8r", "KOI8-R"),
    ("koi8t", "KOI8-T"),
    ("koi8u", "KOI8-U"),
    ("kz1048", "RK1048"),
    ("latin1", "ISO8859-1"),
    ("latin_1", "ISO8859-1"),
    ("microsoftcp1251", "CP1251"),
    ("microsoftcp1255", "CP1255"),
    ("microsoftcp1256", "CP1256"),
    ("shift_jis", "SJIS"),
    ("shiftjis", "SJIS"),
    ("tactis", "TACTIS"),
    ("utf8", "UTF-8"),
    ("utf_8", "UTF-8"),
];

/// `locale_alias`: lowercase alias -> C library locale name.
fn locale_alias() -> i64 {
    let pairs = LOCALE_ALIASES
        .iter()
        .map(|(alias, name)| {
            (
                value::str_alloc((*alias).to_string()),
                value::str_alloc((*name).to_string()),
            )
        })
        .collect();
    value::dict_new(pairs)
}

/// The `locale_alias` entry for `name`, if the alias engine knows it.
fn locale_alias_lookup(name: &str) -> Option<&'static str> {
    LOCALE_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, locale)| *locale)
}

/// `_append_modifier`: attaches `@modifier` unless the code already has one.
fn append_modifier(code: &str, modifier: &str) -> String {
    if code.contains('@') {
        code.to_string()
    } else {
        format!("{code}@{modifier}")
    }
}

/// `encodings.normalize_encoding` + `locale_encoding_alias`: the C library's
/// spelling for `encoding`.
fn c_encoding(encoding: &str) -> String {
    let mut normalized = String::new();
    let mut separator = false;
    for ch in encoding.chars() {
        if ch.is_ascii_alphanumeric() {
            if separator && !normalized.is_empty() {
                normalized.push('_');
            }
            separator = false;
            normalized.push(ch.to_ascii_lowercase());
        } else if ch != '.' {
            separator = true;
        }
    }
    if let Some((_, mapped)) = LOCALE_ENCODING_ALIASES
        .iter()
        .find(|(alias, _)| *alias == normalized.as_str())
    {
        return (*mapped).to_string();
    }
    normalized
}

/// `_replace_encoding`: keeps the language part and swaps in `encoding`.
fn replace_encoding(code: &str, encoding: &str) -> String {
    let langname = code.split('.').next().unwrap_or(code);
    format!("{langname}.{}", c_encoding(encoding))
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
        "locale_alias" => locale_alias(),
        _ => match LANGINFO_CONSTANTS
            .iter()
            .find(|(constant, _)| *constant == name)
        {
            Some((_, raw)) => encode(*raw),
            None => exc::raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("module 'locale' has no attribute '{name}'"),
            ),
        },
    }
}
