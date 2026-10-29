//! `pwd`: the Unix user account database.
//!
//! `getpwnam`, `getpwuid` and `getpwall` read the real password database
//! through libc. The returned entries are namespaces with CPython's
//! `struct_passwd` fields (`pw_name`, `pw_passwd`, `pw_uid`, `pw_gid`,
//! `pw_gecos`, `pw_dir`, `pw_shell`); `struct_passwd(sequence)` builds one
//! from a 7-item sequence exactly like CPython's struct sequence constructor.
//!
//! `getpwnam`/`getpwuid` go through the reentrant `*_r` entry points so a
//! concurrent lookup on another runtime thread cannot overwrite the entry
//! being converted. `getpwall` enumerates the non-reentrant iterator under
//! process-wide lock and copies every entry immediately.
//!
//! Errors follow CPython: a missing account is a `KeyError`, a wrong argument
//! type a `TypeError`, an out-of-range uid a `KeyError`-free `OverflowError`
//! and a lookup failure an `OSError`. No user input can panic.

use std::ffi::CStr;
use std::sync::{Mutex, MutexGuard};

use crate::exc::{
    self, ATTRIBUTE_ERROR, KEY_ERROR, OS_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::value;

/// Field names of `pwd.struct_passwd`, in sequence order.
const PASSWD_FIELDS: [&str; 7] = [
    "pw_name",
    "pw_passwd",
    "pw_uid",
    "pw_gid",
    "pw_gecos",
    "pw_dir",
    "pw_shell",
];

/// Initial `struct passwd` scratch buffer. The `*_r` calls report `ERANGE`
/// when it is too small; the lookup then grows it up to [`MAX_BUFFER`].
const INITIAL_BUFFER: usize = 1024;

/// Largest passwd/group scratch buffer a lookup will allocate.
const MAX_BUFFER: usize = 1 << 20;

/// Serialises `getpwent` enumeration, which walks process-global static state.
static ENUM_LOCK: Mutex<()> = Mutex::new(());

fn enum_lock() -> MutexGuard<'static, ()> {
    ENUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Encodes an integer, keeping the value exact even past the tagged range.
fn int_value(number: i64) -> i64 {
    value::encode_int(number).unwrap_or_else(|| value::bigint_new(number.into()))
}

/// Copies a NUL-terminated C string into a Python `str`.
///
/// A null field (possible on some systems for accounts without a shell or
/// directory) becomes the empty string; undecodable bytes are replaced
/// instead of raising.
fn cstr_value(pointer: *const libc::c_char) -> i64 {
    if pointer.is_null() {
        return value::str_alloc(String::new());
    }
    // SAFETY: the field is either null (checked above) or a valid
    // NUL-terminated string owned by the passwd entry, read immediately.
    let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
    value::str_from_bytes(bytes)
}

/// Appends one field to a rooted namespace, keeping the value alive across
/// the mutation so a collection inside the container lock cannot free it.
fn record_field(namespace: i64, name: &str, field_value: i64) {
    let roots = heap::root_values(&[field_value]);
    if let Some(mut state) = value::namespace_pairs_mut(namespace) {
        state.pairs.push((name.to_string(), field_value));
    }
    heap::write_barrier_edge(namespace, field_value);
    heap::release_roots(roots);
}

/// Pushes `item` onto a rooted list, keeping it alive across the mutation.
fn push_item(list: i64, item: i64) {
    let roots = heap::root_values(&[item]);
    if let Some(mut items) = value::list_items_mut(list) {
        items.push(item);
    }
    heap::write_barrier_edge(list, item);
    heap::release_roots(roots);
}

/// Converts a raw `struct passwd` into a `struct_passwd`-shaped namespace.
fn passwd_namespace(entry: &libc::passwd) -> i64 {
    let namespace = value::namespace_new(Vec::new());
    let roots = heap::root_values(&[namespace]);
    record_field(namespace, "pw_name", cstr_value(entry.pw_name));
    record_field(namespace, "pw_passwd", cstr_value(entry.pw_passwd));
    record_field(namespace, "pw_uid", int_value(i64::from(entry.pw_uid)));
    record_field(namespace, "pw_gid", int_value(i64::from(entry.pw_gid)));
    record_field(namespace, "pw_gecos", cstr_value(entry.pw_gecos));
    record_field(namespace, "pw_dir", cstr_value(entry.pw_dir));
    record_field(namespace, "pw_shell", cstr_value(entry.pw_shell));
    heap::release_roots(roots);
    namespace
}

/// Calls a reentrant passwd lookup, retrying with a larger buffer on
/// `ERANGE`.
///
/// `Ok(Some(entry))` is a match, `Ok(None)` a missing account and `Err(errno)`
/// a lookup failure.
fn lookup_passwd(
    mut lookup: impl FnMut(
        *mut libc::passwd,
        *mut libc::c_char,
        usize,
        *mut *mut libc::passwd,
    ) -> libc::c_int,
) -> Result<Option<i64>, i32> {
    let mut size = INITIAL_BUFFER;
    loop {
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buffer = vec![0 as libc::c_char; size];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let code = lookup(&mut entry, buffer.as_mut_ptr(), buffer.len(), &mut result);
        if code == libc::ERANGE && size < MAX_BUFFER {
            size = (size * 4).min(MAX_BUFFER);
            continue;
        }
        if code != 0 {
            return Err(code);
        }
        if result.is_null() {
            return Ok(None);
        }
        return Ok(Some(passwd_namespace(&entry)));
    }
}

/// Raises `OSError` for an errno returned by a reentrant lookup.
fn raise_errno(function: &str, code: i32) -> i64 {
    let error = std::io::Error::from_raw_os_error(code);
    let text = error.to_string();
    let detail = text.split(" (os error").next().unwrap_or(text.as_str());
    exc::raise_with_message(OS_ERROR, &format!("{function}: [Errno {code}] {detail}"))
}

/// Raises CPython's `KeyError` for a missing account, rendering the argument
/// the way `str(KeyError(...))` does.
fn raise_missing(message: String) -> i64 {
    exc::raise_with_message(KEY_ERROR, &value::repr(value::str_alloc(message)))
}

/// `pwd.getpwnam(name)`: the account called `name`.
fn getpwnam(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "getpwnam() takes exactly 1 argument ({} given)",
            args.len()
        ));
    }
    let Some(&name_value) = args.first() else {
        return type_error("getpwnam() missing required argument 'name' (pos 1)");
    };
    let Some(name) = value::as_str(name_value) else {
        return type_error(&format!(
            "getpwnam() argument must be str, not {}",
            value::type_name(name_value)
        ));
    };
    let name = match std::ffi::CString::new(name) {
        Ok(name) => name,
        Err(_) => return exc::raise_with_message(VALUE_ERROR, "embedded null character"),
    };
    let result = lookup_passwd(|entry, buffer, len, result| unsafe {
        libc::getpwnam_r(name.as_ptr(), entry, buffer, len, result)
    });
    match result {
        Ok(Some(entry)) => entry,
        Ok(None) => raise_missing(format!(
            "getpwnam(): name not found: {}",
            value::repr(name_value)
        )),
        Err(code) => raise_errno("getpwnam", code),
    }
}

/// `pwd.getpwuid(uid)`: the account with numeric uid `uid`.
fn getpwuid(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "getpwuid() takes exactly 1 argument ({} given)",
            args.len()
        ));
    }
    let Some(&uid_value) = args.first() else {
        return type_error("getpwuid() missing required argument 'uid' (pos 1)");
    };
    let Some(uid) = value::as_integer(uid_value) else {
        return type_error(&format!(
            "getpwuid() argument must be int, not {}",
            value::type_name(uid_value)
        ));
    };
    let uid = match libc::uid_t::try_from(uid) {
        Ok(uid) => uid,
        Err(_) => {
            return exc::raise_with_message(OVERFLOW_ERROR, "getpwuid(): uid is out of range")
        }
    };
    let result = lookup_passwd(|entry, buffer, len, result| unsafe {
        libc::getpwuid_r(uid, entry, buffer, len, result)
    });
    match result {
        Ok(Some(entry)) => entry,
        Ok(None) => raise_missing(format!("getpwuid(): uid not found: {uid}")),
        Err(code) => raise_errno("getpwuid", code),
    }
}

/// `pwd.getpwall()`: every account in database order.
fn getpwall(args: &[i64]) -> i64 {
    if !args.is_empty() {
        return type_error(&format!(
            "getpwall() takes no arguments ({} given)",
            args.len()
        ));
    }
    let _guard = enum_lock();
    let list = value::list_new(Vec::new());
    let roots = heap::root_values(&[list]);
    // SAFETY: `setpwent`/`getpwent`/`endpwent` walk libc's static enumeration
    // state; `ENUM_LOCK` keeps concurrent runtime calls from interleaving and
    // every entry is copied before the next call.
    unsafe {
        libc::setpwent();
        loop {
            let entry = libc::getpwent();
            if entry.is_null() {
                break;
            }
            let item = passwd_namespace(&*entry);
            push_item(list, item);
        }
        libc::endpwent();
    }
    heap::release_roots(roots);
    list
}

/// `pwd.struct_passwd(sequence)`: builds the 7-field record explicitly.
fn struct_passwd(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "struct_passwd() takes exactly 1 argument ({} given)",
            args.len()
        ));
    }
    let Some(&sequence) = args.first() else {
        return type_error("struct_passwd() missing required argument 'sequence' (pos 1)");
    };
    let items: Vec<i64> = if let Some(items) = value::as_tuple(sequence) {
        items.to_vec()
    } else if let Some(items) = value::as_list(sequence) {
        items.to_vec()
    } else {
        return type_error(&format!(
            "struct_passwd() argument must be a sequence, not {}",
            value::type_name(sequence)
        ));
    };
    if items.len() != PASSWD_FIELDS.len() {
        return type_error(&format!(
            "struct_passwd() takes a 7-sequence ({}-sequence given)",
            items.len()
        ));
    }
    let namespace = value::namespace_new(Vec::new());
    let mut roots_values = items.clone();
    roots_values.push(namespace);
    let roots = heap::root_values(&roots_values);
    for (name, item) in PASSWD_FIELDS.iter().zip(items.iter()) {
        record_field(namespace, name, *item);
    }
    heap::release_roots(roots);
    namespace
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "struct_passwd" => struct_passwd(args),
        "getpwnam" => getpwnam(args),
        "getpwuid" => getpwuid(args),
        "getpwall" => getpwall(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'pwd' has no attribute '{function}'"),
        ),
    }
}

/// `pwd` defines no module-level integer constants; `struct_passwd` is the
/// only module attribute that is not a function.
pub(crate) fn constant(_name: &str) -> Option<i64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{encode_int, str_alloc, NONE};

    fn int(n: i64) -> i64 {
        encode_int(n).expect("small integer")
    }

    fn text(item: &str) -> i64 {
        str_alloc(item.to_string())
    }

    fn field(entry: i64, name: &str) -> i64 {
        value::as_namespace(entry)
            .and_then(|pairs| pairs.iter().find(|(key, _)| key == name))
            .map(|(_, item)| *item)
            .unwrap_or(NONE)
    }

    fn message() -> String {
        value::to_string(exc::pending_value())
    }

    #[test]
    fn getpwuid_and_getpwnam_agree_on_the_current_user() {
        exc::init();
        // SAFETY: `getuid` has no preconditions.
        let uid = unsafe { libc::getuid() };
        let entry = call("getpwuid", &[int(i64::from(uid))]);
        if entry == NONE {
            // The password database is unavailable in this environment.
            exc::init();
            return;
        }
        assert!(!exc::pending(), "getpwuid failed: {}", message());
        assert_ne!(entry, NONE);

        let name = field(entry, "pw_name");
        let name = value::as_str(name).expect("pw_name is a str").to_string();
        assert!(!name.is_empty());
        assert_eq!(field(entry, "pw_uid"), int(i64::from(uid)));

        let by_name = call("getpwnam", &[text(&name)]);
        if by_name == NONE {
            exc::init();
            return;
        }
        assert!(!exc::pending(), "getpwnam failed: {}", message());
        assert_eq!(field(by_name, "pw_uid"), int(i64::from(uid)));
        assert_eq!(
            value::as_str(field(by_name, "pw_dir")).map(str::to_owned),
            value::as_str(field(entry, "pw_dir")).map(str::to_owned)
        );
        assert!(!exc::pending());
    }

    #[test]
    fn getpwall_returns_entries_with_every_field() {
        exc::init();
        let all = call("getpwall", &[]);
        assert!(!exc::pending(), "getpwall failed: {}", message());
        let entries = value::as_list(all).expect("list of entries");
        if entries.is_empty() {
            // No password database in this environment.
            return;
        }
        for entry in entries {
            for name in PASSWD_FIELDS {
                assert_ne!(field(*entry, name), NONE, "missing {name}");
            }
        }
        assert!(!exc::pending());
    }

    #[test]
    fn missing_account_raises_key_error() {
        exc::init();
        let name = "hydro-no-such-user-7f3a";
        assert_eq!(call("getpwnam", &[text(name)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(KEY_ERROR));
        assert!(
            message().contains("getpwnam(): name not found"),
            "{}",
            message()
        );
        exc::init();

        // A uid far above the system range is not in the database; should the
        // account exist after all, the lookup succeeds and there is nothing
        // to assert.
        let entry = call("getpwuid", &[int(4_000_000)]);
        if entry == NONE {
            assert_eq!(exc::pending_tag(), i64::from(KEY_ERROR));
            assert!(
                message().contains("getpwuid(): uid not found"),
                "{}",
                message()
            );
            exc::init();
        }
    }

    #[test]
    fn argument_validation_raises_type_errors() {
        exc::init();
        assert_eq!(call("getpwnam", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("getpwnam", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("getpwnam", &[text("root"), text("root")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("getpwuid", &[text("0")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        // A uid that does not fit `uid_t` is an OverflowError, not a panic.
        assert_eq!(call("getpwuid", &[int(1 << 40)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(OVERFLOW_ERROR));
        exc::init();

        assert_eq!(call("getpwall", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("getpwuid", &[text("nope")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("no_such_function", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));
        exc::init();

        assert_eq!(constant("pw_name"), None);
        assert!(!exc::pending());
    }

    #[test]
    fn struct_passwd_validates_and_builds_the_record() {
        exc::init();
        let sequence = value::tuple_new(vec![
            text("hydro"),
            text("x"),
            int(4242),
            int(4243),
            text("gecos"),
            text("/home/hydro"),
            text("/bin/sh"),
        ]);
        let entry = call("struct_passwd", &[sequence]);
        assert!(!exc::pending(), "struct_passwd failed: {}", message());
        assert_eq!(value::to_string(field(entry, "pw_name")), "hydro");
        assert_eq!(field(entry, "pw_uid"), int(4242));
        assert_eq!(field(entry, "pw_gid"), int(4243));
        assert_eq!(value::to_string(field(entry, "pw_shell")), "/bin/sh");

        assert_eq!(
            call("struct_passwd", &[value::tuple_new(vec![int(1)])]),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("struct_passwd", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("struct_passwd", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        // A list is accepted like a tuple.
        let sequence = value::list_new(vec![
            text("n"),
            text("p"),
            int(1),
            int(2),
            text("g"),
            text("d"),
            text("s"),
        ]);
        let entry = call("struct_passwd", &[sequence]);
        assert!(!exc::pending());
        assert_eq!(value::to_string(field(entry, "pw_name")), "n");
    }
}
