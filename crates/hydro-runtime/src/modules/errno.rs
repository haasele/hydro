//! `errno`: platform error constants, the `errorcode` reverse map and the
//! module attribute dictionary.
//!
//! The constant table is the Linux (asm-generic) numbering used by the
//! compatibility corpus. Duplicate values keep the first name in table order
//! (`EDEADLK` before `EDEADLOCK`, `EAGAIN` before `EWOULDBLOCK`, `ENOTSUP`
//! before `EOPNOTSUPP`), which makes `errorcode` deterministic.

use std::collections::HashSet;

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR};
use crate::value::{self, NONE};

/// `(name, value)` pairs in the order used by `errorcode` and `__dict__`.
const TABLE: &[(&str, i32)] = &[
    ("E2BIG", 7),
    ("EACCES", 13),
    ("EADDRINUSE", 98),
    ("EADDRNOTAVAIL", 99),
    ("EADV", 68),
    ("EAFNOSUPPORT", 97),
    ("EAGAIN", 11),
    ("EALREADY", 114),
    ("EBADE", 52),
    ("EBADF", 9),
    ("EBADFD", 77),
    ("EBADMSG", 74),
    ("EBADR", 53),
    ("EBADRQC", 56),
    ("EBADSLT", 57),
    ("EBFONT", 59),
    ("EBUSY", 16),
    ("ECANCELED", 125),
    ("ECHILD", 10),
    ("ECHRNG", 44),
    ("ECOMM", 70),
    ("ECONNABORTED", 103),
    ("ECONNREFUSED", 111),
    ("ECONNRESET", 104),
    ("EDEADLK", 35),
    ("EDEADLOCK", 35),
    ("EDESTADDRREQ", 89),
    ("EDOM", 33),
    ("EDOTDOT", 73),
    ("EDQUOT", 122),
    ("EEXIST", 17),
    ("EFAULT", 14),
    ("EFBIG", 27),
    ("EFTYPE", 134),
    ("EHOSTDOWN", 112),
    ("EHOSTUNREACH", 113),
    ("EHWPOISON", 133),
    ("EIDRM", 43),
    ("EILSEQ", 84),
    ("EINPROGRESS", 115),
    ("EINTR", 4),
    ("EINVAL", 22),
    ("EIO", 5),
    ("EISCONN", 106),
    ("EISDIR", 21),
    ("EISNAM", 120),
    ("EKEYEXPIRED", 127),
    ("EKEYREJECTED", 129),
    ("EKEYREVOKED", 128),
    ("EL2HLT", 51),
    ("EL2NSYNC", 45),
    ("EL3HLT", 46),
    ("EL3RST", 47),
    ("ELIBACC", 79),
    ("ELIBBAD", 80),
    ("ELIBEXEC", 83),
    ("ELIBMAX", 82),
    ("ELIBSCN", 81),
    ("ELNRNG", 48),
    ("ELOOP", 40),
    ("EMEDIUMTYPE", 124),
    ("EMFILE", 24),
    ("EMLINK", 31),
    ("EMSGSIZE", 90),
    ("EMULTIHOP", 72),
    ("ENAMETOOLONG", 36),
    ("ENAVAIL", 119),
    ("ENETDOWN", 100),
    ("ENETRESET", 102),
    ("ENETUNREACH", 101),
    ("ENFILE", 23),
    ("ENOANO", 55),
    ("ENOBUFS", 105),
    ("ENOCSI", 50),
    ("ENODATA", 61),
    ("ENODEV", 19),
    ("ENOENT", 2),
    ("ENOEXEC", 8),
    ("ENOKEY", 126),
    ("ENOLCK", 37),
    ("ENOLINK", 67),
    ("ENOMEDIUM", 123),
    ("ENOMEM", 12),
    ("ENOMSG", 42),
    ("ENONET", 64),
    ("ENOPKG", 65),
    ("ENOPROTOOPT", 92),
    ("ENOSPC", 28),
    ("ENOSR", 63),
    ("ENOSTR", 60),
    ("ENOSYS", 38),
    ("ENOTBLK", 15),
    ("ENOTCONN", 107),
    ("ENOTDIR", 20),
    ("ENOTEMPTY", 39),
    ("ENOTNAM", 118),
    ("ENOTRECOVERABLE", 131),
    ("ENOTSOCK", 88),
    ("ENOTSUP", 95),
    ("ENOTTY", 25),
    ("ENOTUNIQ", 76),
    ("ENXIO", 6),
    ("EOPNOTSUPP", 95),
    ("EOVERFLOW", 75),
    ("EOWNERDEAD", 130),
    ("EPERM", 1),
    ("EPFNOSUPPORT", 96),
    ("EPIPE", 32),
    ("EPROTO", 71),
    ("EPROTONOSUPPORT", 93),
    ("EPROTOTYPE", 91),
    ("ERANGE", 34),
    ("EREMCHG", 78),
    ("EREMOTE", 66),
    ("EREMOTEIO", 121),
    ("ERESTART", 85),
    ("ERFKILL", 132),
    ("EROFS", 30),
    ("ESHUTDOWN", 108),
    ("ESOCKTNOSUPPORT", 94),
    ("ESPIPE", 29),
    ("ESRCH", 3),
    ("ESRMNT", 69),
    ("ESTALE", 116),
    ("ESTRPIPE", 86),
    ("ETIME", 62),
    ("ETIMEDOUT", 110),
    ("ETOOMANYREFS", 109),
    ("ETXTBSY", 26),
    ("EUCLEAN", 117),
    ("EUNATCH", 49),
    ("EUSERS", 87),
    ("EWOULDBLOCK", 11),
    ("EXDEV", 18),
    ("EXFULL", 54),
];

/// `module 'errno' has no attribute '<name>'`.
fn attribute_error(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'errno' has no attribute '{name}'"),
    )
}

/// Tagged integer for a table value.
fn int_value(raw: i32) -> i64 {
    value::encode_int(i64::from(raw)).unwrap_or(NONE)
}

fn constant(name: &str) -> Option<i32> {
    TABLE
        .iter()
        .find(|(existing, _)| *existing == name)
        .map(|(_, raw)| *raw)
}

/// `errno.errorcode`: value -> first name with that value.
///
/// The dict is filled incrementally: it is the only owner of the strings
/// inserted earlier, so every allocation after the first must see it.
fn errorcode() -> i64 {
    let dict = value::dict_new(Vec::new());
    let mut seen = HashSet::new();
    for &(name, raw) in TABLE {
        if seen.insert(raw) {
            container::dict_set(dict, int_value(raw), value::str_from_bytes(name.as_bytes()));
        }
    }
    dict
}

/// `errno.__dict__`: every constant plus `errorcode`, in table order.
fn module_dict() -> i64 {
    let dict = value::dict_new(Vec::new());
    for &(name, raw) in TABLE {
        container::dict_set(dict, value::str_from_bytes(name.as_bytes()), int_value(raw));
    }
    let codes = errorcode();
    container::dict_set(dict, value::str_from_bytes(b"errorcode"), codes);
    dict
}

/// Resolves `errno.<name>` (also the `getattr(errno, name)` path).
pub(crate) fn lookup(name: &str) -> i64 {
    match name {
        "errorcode" => errorcode(),
        "__dict__" => module_dict(),
        _ => match constant(name) {
            Some(raw) => int_value(raw),
            None => attribute_error(name),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(n: i32) -> i64 {
        value::encode_int(i64::from(n)).expect("tagged int")
    }

    fn pairs(v: i64) -> Vec<(i64, i64)> {
        value::as_dict(v).expect("dict value").to_vec()
    }

    fn name_of(v: i64) -> Option<&'static str> {
        value::as_str(v)
    }

    #[test]
    fn constants_are_linux_numbers() {
        exc::init();
        assert_eq!(lookup("EDOM"), code(33));
        assert_eq!(lookup("ERANGE"), code(34));
        assert_eq!(lookup("EPERM"), code(1));
        assert_eq!(lookup("ENOENT"), code(2));
        assert_eq!(lookup("EWOULDBLOCK"), code(11));
        assert_eq!(lookup("EXFULL"), code(54));
        assert!(!exc::pending());
    }

    #[test]
    fn common_names_used_by_the_stdlib_are_defined() {
        exc::init();
        let expected = [
            ("ENOENT", 2),
            ("EINVAL", 22),
            ("EACCES", 13),
            ("EEXIST", 17),
            ("ENOTDIR", 20),
            ("EISDIR", 21),
            ("ENOSPC", 28),
            ("EPIPE", 32),
            ("ECONNREFUSED", 111),
            ("ETIMEDOUT", 110),
            ("EAGAIN", 11),
            ("EWOULDBLOCK", 11),
            ("EINTR", 4),
            ("ENOMEM", 12),
            ("EBADF", 9),
            ("EIO", 5),
            ("ENOSYS", 38),
            ("EOPNOTSUPP", 95),
            ("ENOTSUP", 95),
            ("EDOM", 33),
            ("ERANGE", 34),
        ];
        for (name, raw) in expected {
            assert_eq!(lookup(name), code(raw), "{name}");
            assert!(!exc::pending(), "{name} raised");
        }
    }

    #[test]
    fn errorcode_covers_every_constant_and_keeps_the_first_name() {
        let codes = pairs(lookup("errorcode"));
        let name_for = |wanted: i64| {
            codes
                .iter()
                .find(|(key, _)| *key == wanted)
                .and_then(|(_, name)| name_of(*name))
        };
        // Duplicate values: the first table entry wins.
        assert_eq!(name_for(code(35)), Some("EDEADLK"));
        assert_eq!(name_for(code(11)), Some("EAGAIN"));
        assert_eq!(name_for(code(95)), Some("ENOTSUP"));
        assert_eq!(name_for(code(33)), Some("EDOM"));
        assert_eq!(name_for(code(34)), Some("ERANGE"));

        for &(name, raw) in TABLE {
            let constant = code(raw);
            assert!(
                codes.iter().any(|(key, _)| *key == constant),
                "{name} missing from errorcode"
            );
            assert_eq!(lookup(name), constant);
            assert!(!exc::pending(), "{name} raised");
        }
    }

    #[test]
    fn module_dict_is_self_consistent() {
        let module = pairs(lookup("__dict__"));
        assert!(!module
            .iter()
            .any(|(key, _)| name_of(*key) == Some("__dict__")));
        let codes = pairs(lookup("errorcode"));
        let mut uppercase = 0usize;
        for (key, stored) in &module {
            let Some(name) = name_of(*key) else {
                panic!("non-string key in errno.__dict__");
            };
            // `str.isupper()`: at least one cased character, none lowercase.
            // Table names may contain digits (`E2BIG`, `EL3HLT`).
            if !name.chars().any(|ch| ch.is_ascii_uppercase())
                || name.chars().any(|ch| ch.is_ascii_lowercase())
            {
                continue;
            }
            uppercase += 1;
            assert_eq!(*stored, lookup(name), "{name}");
            assert!(
                codes.iter().any(|(value_key, _)| *value_key == *stored),
                "{name} value missing from errorcode"
            );
        }
        // Every table constant (including duplicate values) is present.
        assert_eq!(uppercase, TABLE.len());
        // And `errorcode` itself is exposed.
        assert!(module
            .iter()
            .any(|(key, _)| name_of(*key) == Some("errorcode")));
    }

    #[test]
    fn unknown_attributes_raise_attribute_error() {
        exc::init();
        assert_eq!(lookup("NOPE"), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "module 'errno' has no attribute 'NOPE'"
        );
    }
}
