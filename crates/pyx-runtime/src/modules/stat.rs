//! `stat` / `_stat`: interpreting `os.stat()` mode words.
//!
//! The constants and helpers mirror CPython's `Lib/stat.py` on Linux
//! (CPython 3.12). `_stat` is the C accelerator and exposes the same public
//! surface, so both modules share this implementation. Values that CPython
//! only defines as portability fallbacks (`S_IFDOOR`, `S_IFPORT`, `S_IFWHT`)
//! are present with their Linux value `0`, and the corresponding predicates
//! always return `False`, exactly as CPython does on this platform.

use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR};
use crate::value::{self, FALSE, NONE, TRUE};

/// `S_IFMT`: the file-type bits of a mode word.
const TYPE_MASK: i64 = 0o170000;
/// `S_IMODE`: the bits `os.chmod()` can change.
const PERMISSION_MASK: i64 = 0o7777;

/// `(name, value)` pairs exposed by `stat` and `_stat`. Ordered by name so
/// the table doubles as a stable iteration source for module dictionaries.
const TABLE: &[(&str, i64)] = &[
    ("FILE_ATTRIBUTE_ARCHIVE", 32),
    ("FILE_ATTRIBUTE_COMPRESSED", 2048),
    ("FILE_ATTRIBUTE_DEVICE", 64),
    ("FILE_ATTRIBUTE_DIRECTORY", 16),
    ("FILE_ATTRIBUTE_ENCRYPTED", 16384),
    ("FILE_ATTRIBUTE_HIDDEN", 2),
    ("FILE_ATTRIBUTE_INTEGRITY_STREAM", 32768),
    ("FILE_ATTRIBUTE_NORMAL", 128),
    ("FILE_ATTRIBUTE_NOT_CONTENT_INDEXED", 8192),
    ("FILE_ATTRIBUTE_NO_SCRUB_DATA", 131072),
    ("FILE_ATTRIBUTE_OFFLINE", 4096),
    ("FILE_ATTRIBUTE_READONLY", 1),
    ("FILE_ATTRIBUTE_REPARSE_POINT", 1024),
    ("FILE_ATTRIBUTE_SPARSE_FILE", 512),
    ("FILE_ATTRIBUTE_SYSTEM", 4),
    ("FILE_ATTRIBUTE_TEMPORARY", 256),
    ("FILE_ATTRIBUTE_VIRTUAL", 65536),
    ("SF_APPEND", 262144),
    ("SF_ARCHIVED", 65536),
    ("SF_DATALESS", 1073741824),
    ("SF_FIRMLINK", 8388608),
    ("SF_IMMUTABLE", 131072),
    ("SF_NOUNLINK", 1048576),
    ("SF_RESTRICTED", 524288),
    ("SF_SETTABLE", 4294901760),
    ("SF_SNAPSHOT", 2097152),
    ("ST_ATIME", 7),
    ("ST_CTIME", 9),
    ("ST_DEV", 2),
    ("ST_GID", 5),
    ("ST_INO", 1),
    ("ST_MODE", 0),
    ("ST_MTIME", 8),
    ("ST_NLINK", 3),
    ("ST_SIZE", 6),
    ("ST_UID", 4),
    ("S_ENFMT", 1024),
    ("S_IEXEC", 64),
    ("S_IFBLK", 24576),
    ("S_IFCHR", 8192),
    ("S_IFDIR", 16384),
    ("S_IFDOOR", 0),
    ("S_IFIFO", 4096),
    ("S_IFLNK", 40960),
    ("S_IFPORT", 0),
    ("S_IFREG", 32768),
    ("S_IFSOCK", 49152),
    ("S_IFWHT", 0),
    ("S_IREAD", 256),
    ("S_IRGRP", 32),
    ("S_IROTH", 4),
    ("S_IRUSR", 256),
    ("S_IRWXG", 56),
    ("S_IRWXO", 7),
    ("S_IRWXU", 448),
    ("S_ISGID", 1024),
    ("S_ISUID", 2048),
    ("S_ISVTX", 512),
    ("S_IWGRP", 16),
    ("S_IWOTH", 2),
    ("S_IWRITE", 128),
    ("S_IWUSR", 128),
    ("S_IXGRP", 8),
    ("S_IXOTH", 1),
    ("S_IXUSR", 64),
    ("UF_APPEND", 4),
    ("UF_COMPRESSED", 32),
    ("UF_DATAVAULT", 128),
    ("UF_HIDDEN", 32768),
    ("UF_IMMUTABLE", 2),
    ("UF_NODUMP", 1),
    ("UF_NOUNLINK", 16),
    ("UF_OPAQUE", 8),
    ("UF_SETTABLE", 65535),
    ("UF_TRACKED", 64),
];

/// Module-level function dispatch shared by `stat` and `_stat`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    let Some(&raw) = args.first() else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{function}() takes exactly one argument (0 given)"),
        );
    };
    let Some(mode) = value::as_integer(raw) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{function}() argument must be an integer, not {}",
                value::type_name(raw)
            ),
        );
    };
    match function {
        "S_IMODE" => int_value(mode & PERMISSION_MASK),
        "S_IFMT" => int_value(mode & TYPE_MASK),
        "S_ISDIR" => bool_value(is_type(mode, 0o040000)),
        "S_ISCHR" => bool_value(is_type(mode, 0o020000)),
        "S_ISBLK" => bool_value(is_type(mode, 0o060000)),
        "S_ISREG" => bool_value(is_type(mode, 0o100000)),
        "S_ISFIFO" => bool_value(is_type(mode, 0o010000)),
        "S_ISLNK" => bool_value(is_type(mode, 0o120000)),
        "S_ISSOCK" => bool_value(is_type(mode, 0o140000)),
        // CPython's Linux build defines these against the `0` fallback
        // constants, so they are constant `False` on this platform.
        "S_ISDOOR" | "S_ISPORT" | "S_ISWHT" => FALSE,
        "filemode" => value::str_alloc(filemode(mode)),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'stat' has no attribute '{function}'"),
        ),
    }
}

/// Resolves a `stat`/`_stat` constant by name. Unknown names report
/// `AttributeError` and yield `None`, so a caller may return the result
/// directly.
pub(crate) fn constant(name: &str) -> i64 {
    match TABLE.iter().find(|(existing, _)| *existing == name) {
        Some(&(_, raw)) => int_value(raw),
        None => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'stat' has no attribute '{name}'"),
        ),
    }
}

/// `(mode & S_IFMT) == kind`, the test CPython's predicates perform.
fn is_type(mode: i64, kind: i64) -> bool {
    mode & TYPE_MASK == kind
}

fn int_value(raw: i64) -> i64 {
    value::encode_int(raw).unwrap_or(NONE)
}

fn bool_value(flag: bool) -> i64 {
    if flag {
        TRUE
    } else {
        FALSE
    }
}

/// `stat.filemode`: a ten-character rendering of a mode word
/// (`-rw-r--r--`, `drwxr-xr-x`, `lrwxrwxrwx`, ...). Unknown file types use
/// `?`, and unset permission bits use `-`.
fn filemode(mode: i64) -> String {
    let mut out = String::with_capacity(10);
    out.push(match mode & TYPE_MASK {
        0o120000 => 'l',
        0o140000 => 's', // Must precede REG/DIR: S_IFSOCK overlaps them.
        0o100000 => '-',
        0o060000 => 'b',
        0o040000 => 'd',
        0o020000 => 'c',
        0o010000 => 'p',
        _ => '?',
    });
    push_read(&mut out, mode, 0o400);
    push_write(&mut out, mode, 0o200);
    push_exec(&mut out, mode, 0o100, 0o4000, 's', 'S');
    push_read(&mut out, mode, 0o040);
    push_write(&mut out, mode, 0o020);
    push_exec(&mut out, mode, 0o010, 0o2000, 's', 'S');
    push_read(&mut out, mode, 0o004);
    push_write(&mut out, mode, 0o002);
    push_exec(&mut out, mode, 0o001, 0o1000, 't', 'T');
    out
}

fn push_read(out: &mut String, mode: i64, bit: i64) {
    out.push(if mode & bit != 0 { 'r' } else { '-' });
}

fn push_write(out: &mut String, mode: i64, bit: i64) {
    out.push(if mode & bit != 0 { 'w' } else { '-' });
}

/// Execute bit with the setuid/setgid/sticky override: the special bit shows
/// lowercase when the execute bit is also set and uppercase otherwise.
fn push_exec(out: &mut String, mode: i64, exec: i64, special: i64, both: char, only: char) {
    out.push(if mode & special != 0 {
        if mode & exec != 0 {
            both
        } else {
            only
        }
    } else if mode & exec != 0 {
        'x'
    } else {
        '-'
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(v: i64) -> i64 {
        value::encode_int(v).expect("tagged int")
    }

    fn run(name: &str, v: i64) -> i64 {
        call(name, &[mode(v)])
    }

    #[test]
    fn type_predicates_match_linux_values() {
        exc::init();
        assert_eq!(run("S_ISDIR", 0o040755), TRUE);
        assert_eq!(run("S_ISREG", 0o100644), TRUE);
        assert_eq!(run("S_ISLNK", 0o120777), TRUE);
        assert_eq!(run("S_ISSOCK", 0o140777), TRUE);
        assert_eq!(run("S_ISBLK", 0o060644), TRUE);
        assert_eq!(run("S_ISCHR", 0o020644), TRUE);
        assert_eq!(run("S_ISFIFO", 0o010644), TRUE);
        assert_eq!(run("S_ISDIR", 0o100644), FALSE);
        assert_eq!(run("S_ISREG", 0o040755), FALSE);
        assert!(!exc::pending());
    }

    #[test]
    fn portable_predicates_are_false_on_linux() {
        exc::init();
        assert_eq!(run("S_ISDOOR", 0o040755), FALSE);
        assert_eq!(run("S_ISPORT", 0o040755), FALSE);
        assert_eq!(run("S_ISWHT", 0o040755), FALSE);
        assert!(!exc::pending());
    }

    #[test]
    fn imode_and_ifmt_mask_the_mode_word() {
        exc::init();
        assert_eq!(run("S_IMODE", 0o104755), mode(0o4755));
        assert_eq!(run("S_IMODE", 0o100644), mode(0o644));
        assert_eq!(run("S_IFMT", 0o104755), mode(0o100000));
        assert_eq!(run("S_IFMT", 0o120777), mode(0o120000));
        assert!(!exc::pending());
    }

    #[test]
    fn filemode_matches_cpython() {
        exc::init();
        let render = |m: i64| value::as_str(run("filemode", m)).map(str::to_owned);
        assert_eq!(render(0o100644).as_deref(), Some("-rw-r--r--"));
        assert_eq!(render(0o040755).as_deref(), Some("drwxr-xr-x"));
        assert_eq!(render(0o120777).as_deref(), Some("lrwxrwxrwx"));
        assert_eq!(render(0o140000).as_deref(), Some("s---------"));
        assert_eq!(render(0o010000).as_deref(), Some("p---------"));
        assert_eq!(render(0o020000).as_deref(), Some("c---------"));
        assert_eq!(render(0o060000).as_deref(), Some("b---------"));
        assert_eq!(render(0).as_deref(), Some("?---------"));
        assert_eq!(render(0o160000).as_deref(), Some("?---------"));
        assert!(!exc::pending());
    }

    #[test]
    fn filemode_renders_special_bits() {
        exc::init();
        let render = |m: i64| value::as_str(run("filemode", m)).map(str::to_owned);
        assert_eq!(render(0o104755).as_deref(), Some("-rwsr-xr-x"));
        assert_eq!(render(0o102755).as_deref(), Some("-rwxr-sr-x"));
        assert_eq!(render(0o101777).as_deref(), Some("-rwxrwxrwt"));
        // Special bit without the matching execute bit is uppercase.
        assert_eq!(render(0o104644).as_deref(), Some("-rwSr--r--"));
        assert_eq!(render(0o102644).as_deref(), Some("-rw-r-Sr--"));
        assert_eq!(render(0o101776).as_deref(), Some("-rwxrwxrwT"));
        assert_eq!(render(0o106000).as_deref(), Some("---S--S---"));
        assert!(!exc::pending());
    }

    #[test]
    fn constants_match_linux_stat() {
        exc::init();
        assert_eq!(constant("ST_MODE"), mode(0));
        assert_eq!(constant("ST_CTIME"), mode(9));
        assert_eq!(constant("S_IFDIR"), mode(0o040000));
        assert_eq!(constant("S_IFSOCK"), mode(0o140000));
        assert_eq!(constant("S_IFDOOR"), mode(0));
        assert_eq!(constant("S_ISUID"), mode(0o4000));
        assert_eq!(constant("S_ISVTX"), mode(0o1000));
        assert_eq!(constant("S_IRWXU"), mode(0o700));
        assert_eq!(constant("S_IROTH"), mode(0o004));
        assert_eq!(constant("S_IREAD"), mode(0o400));
        assert_eq!(constant("S_ENFMT"), mode(0o2000));
        assert_eq!(constant("UF_HIDDEN"), mode(0o100000));
        assert_eq!(constant("SF_SETTABLE"), mode(0xffff_0000));
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_constant_raises_attribute_error() {
        exc::init();
        assert_eq!(constant("NOT_A_CONSTANT"), NONE);
        assert!(exc::pending());
        exc::clear();
        assert!(!exc::pending());
    }

    #[test]
    fn non_integer_mode_reports_type_error() {
        exc::init();
        let text = value::str_from_bytes(b"dir");
        assert_eq!(call("S_ISDIR", &[text]), NONE);
        assert!(exc::pending());
        exc::clear();
        assert!(!exc::pending());
    }
}
