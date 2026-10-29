//! `grp`: the Unix group database.
//!
//! `getgrnam`, `getgrgid` and `getgrall` read the real group database through
//! libc. Entries are namespaces with CPython's `struct_group` fields
//! (`gr_name`, `gr_passwd`, `gr_gid`, `gr_mem`); `gr_mem` is a list of member
//! names. `struct_group(sequence)` builds a record from a 4-item sequence
//! exactly like CPython's struct sequence constructor.
//!
//! `getgrnam`/`getgrgid` use the reentrant `*_r` entry points so concurrent
//! lookups on other runtime threads cannot overwrite the entry being
//! converted; `getgrall` walks the non-reentrant iterator under a
//! process-wide lock and copies every entry immediately.
//!
//! Errors follow CPython: an unknown group is a `KeyError`, a wrong argument
//! type a `TypeError`, an out-of-range gid an `OverflowError` and a lookup
//! failure an `OSError`.

use std::ffi::CStr;
use std::sync::{Mutex, MutexGuard};

use crate::exc::{
    self, ATTRIBUTE_ERROR, KEY_ERROR, OS_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::value;

/// Field names of `grp.struct_group`, in sequence order.
const GROUP_FIELDS: [&str; 4] = ["gr_name", "gr_passwd", "gr_gid", "gr_mem"];

/// Initial `struct group` scratch buffer.
const INITIAL_BUFFER: usize = 1024;

/// Largest group scratch buffer a lookup will allocate (large groups need a
/// lot of room for the member vector).
const MAX_BUFFER: usize = 1 << 20;

/// Defensive cap on the number of member names read from one entry.
const MAX_MEMBERS: usize = 1 << 20;

/// Serialises `getgrent` enumeration, which walks process-global static state.
static ENUM_LOCK: Mutex<()> = Mutex::new(());

fn enum_lock() -> MutexGuard<'static, ()> {
    ENUM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Encodes an integer, promoting past the tagged range.
fn int_value(number: i64) -> i64 {
    value::encode_int(number).unwrap_or_else(|| value::bigint_new(number.into()))
}

/// Copies a NUL-terminated C string into a Python `str`.
fn cstr_value(pointer: *const libc::c_char) -> i64 {
    if pointer.is_null() {
        return value::str_alloc(String::new());
    }
    // SAFETY: the field is either null (checked above) or a valid
    // NUL-terminated string owned by the group entry, read immediately.
    let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
    value::str_from_bytes(bytes)
}

/// Appends one field to a rooted namespace, keeping the value alive across
/// the mutation.
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

/// Builds the `gr_mem` member list from a null-terminated `char **`.
fn members_value(members: *mut *mut libc::c_char) -> i64 {
    let list = value::list_new(Vec::new());
    let roots = heap::root_values(&[list]);
    if !members.is_null() {
        let mut cursor = members;
        let mut count = 0usize;
        loop {
            // SAFETY: `cursor` points inside the null-terminated vector owned
            // by the group entry; the pointer is advanced only after a
            // non-null element was read.
            let member = unsafe { *cursor };
            if member.is_null() || count >= MAX_MEMBERS {
                break;
            }
            push_item(list, cstr_value(member));
            count += 1;
            // SAFETY: `cursor` is a valid element pointer and the vector is
            // terminated by a null pointer no later than MAX_MEMBERS.
            cursor = unsafe { cursor.add(1) };
        }
    }
    heap::release_roots(roots);
    list
}

/// Converts a raw `struct group` into a `struct_group`-shaped namespace.
fn group_namespace(entry: &libc::group) -> i64 {
    let namespace = value::namespace_new(Vec::new());
    let roots = heap::root_values(&[namespace]);
    record_field(namespace, "gr_name", cstr_value(entry.gr_name));
    record_field(namespace, "gr_passwd", cstr_value(entry.gr_passwd));
    record_field(namespace, "gr_gid", int_value(i64::from(entry.gr_gid)));
    let members = members_value(entry.gr_mem);
    record_field(namespace, "gr_mem", members);
    heap::release_roots(roots);
    namespace
}

/// Calls a reentrant group lookup, retrying with a larger buffer on `ERANGE`.
fn lookup_group(
    mut lookup: impl FnMut(
        *mut libc::group,
        *mut libc::c_char,
        usize,
        *mut *mut libc::group,
    ) -> libc::c_int,
) -> Result<Option<i64>, i32> {
    let mut size = INITIAL_BUFFER;
    loop {
        let mut entry: libc::group = unsafe { std::mem::zeroed() };
        let mut buffer = vec![0 as libc::c_char; size];
        let mut result: *mut libc::group = std::ptr::null_mut();
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
        return Ok(Some(group_namespace(&entry)));
    }
}

/// Raises `OSError` for an errno returned by a reentrant lookup.
fn raise_errno(function: &str, code: i32) -> i64 {
    let error = std::io::Error::from_raw_os_error(code);
    let text = error.to_string();
    let detail = text.split(" (os error").next().unwrap_or(text.as_str());
    exc::raise_with_message(OS_ERROR, &format!("{function}: [Errno {code}] {detail}"))
}

/// Raises CPython's `KeyError` for a missing group.
fn raise_missing(message: String) -> i64 {
    exc::raise_with_message(KEY_ERROR, &value::repr(value::str_alloc(message)))
}

/// `grp.getgrnam(name)`: the group called `name`.
fn getgrnam(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "getgrnam() takes exactly 1 argument ({} given)",
            args.len()
        ));
    }
    let Some(&name_value) = args.first() else {
        return type_error("getgrnam() missing required argument 'name' (pos 1)");
    };
    let Some(name) = value::as_str(name_value) else {
        return type_error(&format!(
            "getgrnam() argument must be str, not {}",
            value::type_name(name_value)
        ));
    };
    let name = match std::ffi::CString::new(name) {
        Ok(name) => name,
        Err(_) => return exc::raise_with_message(VALUE_ERROR, "embedded null character"),
    };
    let result = lookup_group(|entry, buffer, len, result| unsafe {
        libc::getgrnam_r(name.as_ptr(), entry, buffer, len, result)
    });
    match result {
        Ok(Some(entry)) => entry,
        Ok(None) => raise_missing(format!(
            "getgrnam(): name not found: {}",
            value::repr(name_value)
        )),
        Err(code) => raise_errno("getgrnam", code),
    }
}

/// `grp.getgrgid(gid)`: the group with numeric gid `gid`.
fn getgrgid(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "getgrgid() takes exactly 1 argument ({} given)",
            args.len()
        ));
    }
    let Some(&gid_value) = args.first() else {
        return type_error("getgrgid() missing required argument 'gid' (pos 1)");
    };
    let Some(gid) = value::as_integer(gid_value) else {
        return type_error(&format!(
            "getgrgid() argument must be int, not {}",
            value::type_name(gid_value)
        ));
    };
    let gid = match libc::gid_t::try_from(gid) {
        Ok(gid) => gid,
        Err(_) => {
            return exc::raise_with_message(OVERFLOW_ERROR, "getgrgid(): gid is out of range")
        }
    };
    let result = lookup_group(|entry, buffer, len, result| unsafe {
        libc::getgrgid_r(gid, entry, buffer, len, result)
    });
    match result {
        Ok(Some(entry)) => entry,
        Ok(None) => raise_missing(format!("getgrgid(): gid not found: {gid}")),
        Err(code) => raise_errno("getgrgid", code),
    }
}

/// `grp.getgrall()`: every group in database order.
fn getgrall(args: &[i64]) -> i64 {
    if !args.is_empty() {
        return type_error(&format!(
            "getgrall() takes no arguments ({} given)",
            args.len()
        ));
    }
    let _guard = enum_lock();
    let list = value::list_new(Vec::new());
    let roots = heap::root_values(&[list]);
    // SAFETY: `setgrent`/`getgrent`/`endgrent` walk libc's static enumeration
    // state; `ENUM_LOCK` keeps concurrent runtime calls from interleaving and
    // every entry is copied before the next call.
    unsafe {
        libc::setgrent();
        loop {
            let entry = libc::getgrent();
            if entry.is_null() {
                break;
            }
            let item = group_namespace(&*entry);
            push_item(list, item);
        }
        libc::endgrent();
    }
    heap::release_roots(roots);
    list
}

/// `grp.struct_group(sequence)`: builds the 4-field record explicitly.
fn struct_group(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "struct_group() takes exactly 1 argument ({} given)",
            args.len()
        ));
    }
    let Some(&sequence) = args.first() else {
        return type_error("struct_group() missing required argument 'sequence' (pos 1)");
    };
    let items: Vec<i64> = if let Some(items) = value::as_tuple(sequence) {
        items.to_vec()
    } else if let Some(items) = value::as_list(sequence) {
        items.to_vec()
    } else {
        return type_error(&format!(
            "struct_group() argument must be a sequence, not {}",
            value::type_name(sequence)
        ));
    };
    if items.len() != GROUP_FIELDS.len() {
        return type_error(&format!(
            "struct_group() takes a 4-sequence ({}-sequence given)",
            items.len()
        ));
    }
    let namespace = value::namespace_new(Vec::new());
    let mut roots_values = items.clone();
    roots_values.push(namespace);
    let roots = heap::root_values(&roots_values);
    for (name, item) in GROUP_FIELDS.iter().zip(items.iter()) {
        record_field(namespace, name, *item);
    }
    heap::release_roots(roots);
    namespace
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "struct_group" => struct_group(args),
        "getgrnam" => getgrnam(args),
        "getgrgid" => getgrgid(args),
        "getgrall" => getgrall(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'grp' has no attribute '{function}'"),
        ),
    }
}

/// `grp` defines no module-level integer constants; `struct_group` is the
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
    fn current_group_resolves_by_gid_and_name() {
        exc::init();
        // SAFETY: `getgid` has no preconditions.
        let gid = unsafe { libc::getgid() };
        let entry = call("getgrgid", &[int(i64::from(gid))]);
        if entry == NONE {
            // The group database is unavailable in this environment.
            exc::init();
            return;
        }
        assert!(!exc::pending(), "getgrgid failed: {}", message());
        assert_ne!(entry, NONE);
        assert_eq!(field(entry, "gr_gid"), int(i64::from(gid)));
        assert!(value::as_list(field(entry, "gr_mem")).is_some());

        let name = value::to_string(field(entry, "gr_name"));
        assert!(!name.is_empty());
        let by_name = call("getgrnam", &[text(&name)]);
        if by_name == NONE {
            exc::init();
            return;
        }
        assert!(!exc::pending(), "getgrnam failed: {}", message());
        assert_eq!(field(by_name, "gr_gid"), int(i64::from(gid)));
        assert!(!exc::pending());
    }

    #[test]
    fn getgrall_returns_entries_with_every_field() {
        exc::init();
        let all = call("getgrall", &[]);
        assert!(!exc::pending(), "getgrall failed: {}", message());
        let entries = value::as_list(all).expect("list of entries");
        if entries.is_empty() {
            // No group database in this environment.
            return;
        }
        for entry in entries {
            assert_ne!(field(*entry, "gr_name"), NONE);
            assert_ne!(field(*entry, "gr_gid"), NONE);
            assert!(value::as_list(field(*entry, "gr_mem")).is_some());
        }
        assert!(!exc::pending());
    }

    #[test]
    fn missing_group_raises_key_error() {
        exc::init();
        assert_eq!(call("getgrnam", &[text("hydro-no-such-group-7f3a")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(KEY_ERROR));
        assert!(
            message().contains("getgrnam(): name not found"),
            "{}",
            message()
        );
        exc::init();

        let entry = call("getgrgid", &[int(4_000_000)]);
        if entry == NONE {
            assert_eq!(exc::pending_tag(), i64::from(KEY_ERROR));
            assert!(
                message().contains("getgrgid(): gid not found"),
                "{}",
                message()
            );
            exc::init();
        }
    }

    #[test]
    fn argument_validation_raises_type_errors() {
        exc::init();
        assert_eq!(call("getgrnam", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("getgrnam", &[int(0)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("getgrnam", &[text("root"), text("root")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("getgrgid", &[text("0")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("getgrgid", &[int(1 << 40)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(OVERFLOW_ERROR));
        exc::init();

        assert_eq!(call("getgrall", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("nope", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));
        exc::init();

        assert_eq!(constant("gr_name"), None);
        assert!(!exc::pending());
    }

    #[test]
    fn struct_group_validates_and_builds_the_record() {
        exc::init();
        let members = value::list_new(vec![text("a"), text("b")]);
        let sequence = value::tuple_new(vec![text("hydro"), text("x"), int(4242), members]);
        let entry = call("struct_group", &[sequence]);
        assert!(!exc::pending(), "struct_group failed: {}", message());
        assert_eq!(value::to_string(field(entry, "gr_name")), "hydro");
        assert_eq!(field(entry, "gr_gid"), int(4242));
        let stored = value::as_list(field(entry, "gr_mem")).expect("member list");
        assert_eq!(stored.len(), 2);
        assert_eq!(value::to_string(stored[0]), "a");

        assert_eq!(
            call("struct_group", &[value::tuple_new(vec![int(1)])]),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("struct_group", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        assert_eq!(call("struct_group", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();
    }
}
