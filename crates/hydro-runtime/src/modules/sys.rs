//! Frame introspection and runtime metadata for the `sys` module.
//!
//! `sys._getframe(depth)` returns a real frame object backed by the call
//! records the runtime pushes around every generated (and interpreted)
//! activation. `f_locals` reads the activation's registered GC slots while it
//! runs, so it reflects generated-code locals at safepoints; after the frame
//! returns, the object keeps the snapshot taken when the activation ended.
//!
//! No frame is invented: a depth beyond the live stack raises CPython's
//! `ValueError: call stack is not deep enough`, and a depth argument that is
//! not an integer raises its `TypeError`/`OverflowError`.
//!
//! `sys.thread_info` is computed from the C library this binary links
//! against; see [`thread_info`].

use crate::core::exc;
use crate::core::globals;
use crate::core::value;
use crate::exc::{OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::NONE;

/// `sys._getframe(depth=0)`.
pub(crate) fn getframe(args: &[i64]) -> i64 {
    let depth = match args.first().copied() {
        Some(value) => match frame_depth(value) {
            Ok(depth) => depth,
            Err(raised) => return raised,
        },
        None => 0,
    };
    if depth < 0 {
        return exc::raise_with_message(VALUE_ERROR, "call stack is not deep enough");
    }
    exc::getframe(depth as usize)
}

/// `sys._getframemodulename(depth=0)`.
///
/// Returns the module key of the requested frame (the same key a frame's
/// `f_globals` maps), or `None` when the stack is shallower than `depth`,
/// exactly like CPython's C implementation.
pub(crate) fn getframemodulename(args: &[i64]) -> i64 {
    let depth = match args.first().copied() {
        Some(value) => match frame_depth(value) {
            Ok(depth) => depth,
            Err(raised) => return raised,
        },
        None => 0,
    };
    if depth < 0 {
        return exc::raise_with_message(VALUE_ERROR, "call stack is not deep enough");
    }
    match exc::frame_module_at(depth as usize) {
        Some(module) => value::str_alloc(module),
        None => value::NONE,
    }
}

/// `sys.thread_info`: the native thread implementation, its lock strategy and
/// the thread library's own version string.
///
/// CPython builds this three-field struct sequence from the C library it was
/// compiled against (`Python/thread.c: PyThread_GetInfo`): the implementation
/// name (`"pthread"` on POSIX, `"nt"` on Windows), the lock primitive
/// (`"mutex+cond"` for pthreads, `None` elsewhere) and, on glibc, the
/// `confstr(_CS_GNU_LIBPTHREAD_VERSION)` text (`"NPTL 2.39"`). Hydro reports
/// the same values for the C library it links against instead of inventing a
/// build-time constant; when the platform has no `confstr` name, `version` is
/// `None` exactly as in CPython.
///
/// The object is cached so repeated reads share identity, like CPython's
/// once-per-interpreter `sys.thread_info`.
pub(crate) fn thread_info() -> i64 {
    const KEY: &str = "sys.thread_info";
    if let Some(cached) = globals::registry_get(KEY) {
        return cached;
    }
    let (name, lock) = thread_implementation();
    let info = value::namespace_new(vec![
        ("name".to_string(), value::str_alloc(name.to_string())),
        (
            "lock".to_string(),
            match lock {
                Some(lock) => value::str_alloc(lock.to_string()),
                None => NONE,
            },
        ),
        ("version".to_string(), thread_library_version()),
    ]);
    globals::registry_set(KEY, info);
    info
}

/// The thread implementation and lock primitive, as `PyThread_GetInfo`
/// selects them at compile time.
#[cfg(windows)]
fn thread_implementation() -> (&'static str, Option<&'static str>) {
    ("nt", None)
}

#[cfg(all(unix, not(target_os = "emscripten")))]
fn thread_implementation() -> (&'static str, Option<&'static str>) {
    // CPython's `HAVE_PTHREAD_STUBS` path only exists for Emscripten/WASI
    // builds; every other POSIX target uses real pthreads with a
    // mutex + condition-variable lock.
    ("pthread", Some("mutex+cond"))
}

#[cfg(target_os = "emscripten")]
fn thread_implementation() -> (&'static str, Option<&'static str>) {
    // The wasm build has no pthread runtime, so CPython reports the stub
    // implementation and no lock primitive.
    ("pthread-stubs", None)
}

#[cfg(not(any(unix, windows, target_os = "emscripten")))]
fn thread_implementation() -> (&'static str, Option<&'static str>) {
    ("", None)
}

/// `confstr(_CS_GNU_LIBPTHREAD_VERSION)`: the C thread library's version.
///
/// Only glibc provides the name; every other target reports `None` rather
/// than a fabricated version.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn thread_library_version() -> i64 {
    let mut buffer = [0u8; 256];
    // SAFETY: `confstr` writes at most `buffer.len()` bytes and returns the
    // length including the NUL terminator when it fits.
    let len = unsafe {
        libc::confstr(
            libc::_CS_GNU_LIBPTHREAD_VERSION,
            buffer.as_mut_ptr().cast::<libc::c_char>(),
            buffer.len(),
        )
    };
    if len > 1 && len < buffer.len() {
        value::str_from_bytes(&buffer[..len - 1])
    } else {
        NONE
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn thread_library_version() -> i64 {
    NONE
}

/// `inspect.currentframe()`: the innermost running Hydro activation.
pub(crate) fn current_frame() -> i64 {
    exc::current_frame()
}

/// Validates the optional `depth` argument of the frame APIs.
///
/// CPython converts the value to a C `int`: an integer that does not fit is an
/// `OverflowError`, any other type a `TypeError` naming its type. Negative
/// depths are accepted here and rejected by the callers as CPython does.
fn frame_depth(depth: i64) -> Result<i64, i64> {
    if let Some(depth) = value::as_integer(depth) {
        return Ok(depth);
    }
    if value::to_bigint(depth).is_some() {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "Python int too large to convert to C int",
        ));
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(depth)
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sys.thread_info` must describe the C thread library this binary is
    /// linked against, and stay a single cached object across reads.
    #[test]
    fn thread_info_reports_the_native_thread_library() {
        exc::init();
        let info = thread_info();
        let fields = value::as_namespace(info).expect("namespace value");
        assert_eq!(
            fields
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["name", "lock", "version"]
        );
        let field = |wanted: &str| {
            fields
                .iter()
                .find(|(name, _)| name == wanted)
                .map(|(_, value)| *value)
                .expect("field")
        };

        #[cfg(windows)]
        {
            assert_eq!(value::as_str(field("name")), Some("nt"));
            assert_eq!(field("lock"), NONE);
        }
        #[cfg(all(unix, not(target_os = "emscripten")))]
        {
            assert_eq!(value::as_str(field("name")), Some("pthread"));
            assert_eq!(value::as_str(field("lock")), Some("mutex+cond"));
        }
        #[cfg(target_os = "emscripten")]
        {
            assert_eq!(value::as_str(field("name")), Some("pthread-stubs"));
            assert_eq!(field("lock"), NONE);
        }

        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            let version = value::as_str(field("version")).expect("glibc pthread version");
            assert!(version.starts_with("NPTL"), "{version}");
        }
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        assert_eq!(field("version"), NONE);

        // CPython builds the struct sequence once per interpreter, so
        // repeated reads share identity.
        assert_eq!(thread_info(), info);
        assert!(!exc::pending());
    }
}
