//! `_gdbm` and `_dbm`: the native database backends behind `dbm.gnu` and
//! `dbm.ndbm`.
//!
//! Both modules drive a real system library; nothing is persisted in the
//! Hydro heap:
//!
//! * `_gdbm` links `libgdbm` and opens databases through `gdbm_open`. A
//!   `_gdbm.open(filename, flags='r', mode=0o666)` call returns a
//!   `_gdbm.gdbm` mapping object supporting `db[key]`, `db[key] = value`,
//!   `del db[key]`, `key in db`, `len`, truth testing, iteration-by-key
//!   (`firstkey`/`nextkey`), `keys`, `get`, `setdefault`, `clear`, `sync`,
//!   `reorganize`, `close` and the context-manager protocol.
//! * `_dbm` links the NDBM compatibility layer (`libgdbm_compat`, whose
//!   `dbm_*` entry points wrap gdbm on this system) and returns a
//!   `_dbm.dbm` object with `keys`, `get`, `setdefault`, `clear`, `close`
//!   plus the same mapping protocol (no `firstkey`/`nextkey`/`sync`, matching
//!   CPython).
//!
//! The observable CPython 3.14 semantics are reproduced closely because
//! `dbm.gnu` and `dbm.ndbm` are thin `from _gdbm import *` /
//! `from _dbm import *` shims:
//!
//! * `flags` is a string. `_gdbm` requires `r`/`w`/`c`/`n` first and accepts
//!   the `f` (fast), `s` (sync) and `u` (no-lock) suffixes; the module
//!   constant `open_flags` is `"rwcnfsu"`. `_dbm` accepts exactly `r`, `w`,
//!   `rw` (legacy), `c` or `n`, mapped to the `O_*` flags CPython passes to
//!   `dbm_open`.
//! * Keys and values are `bytes` or `str` (UTF-8); `bytearray` and
//!   `memoryview` are rejected with CPython's read-only-bytes diagnostics.
//!   `__contains__` has its own `... key must be bytes or string` message.
//! * Missing `__getitem__`/`__delitem__` keys raise `KeyError`; a closed
//!   database raises the module's `error` class (an `OSError` subclass).
//!   gdbm failures report `gdbm_strerror`, system failures report
//!   `[Errno N] ...` (with the filename for `open`).
//! * CPython's cached-size quirk is preserved: `_gdbm.setdefault` invalidates
//!   the cached length, `_dbm.setdefault` does not, and `_dbm.setdefault`
//!   with an omitted default fails with `cannot add item to database`.
//!
//! ## Handles
//!
//! Instances never carry raw pointers. A private instance attribute stores an
//! integer handle into a process-global registry that owns the
//! `GDBM_FILE`/`DBM *`. Every operation looks the handle up while holding the
//! registry mutex, and `close` removes the entry, so a closed handle can
//! never be dereferenced. Hydro has no finaliser hook for native instances, so
//! a database abandoned without `close` stays open until the process exits;
//! `close` is idempotent and otherwise the only release path.
//!
//! ## Wiring
//!
//! This file deliberately owns only the implementation. The orchestrator
//! wires it into the build as follows:
//!
//! * `crates/hydro-ir/src/stdlib/specs.rs`: add `GDBM` and `DBM` specs and
//!   register them in [`hydro_ir::stdlib::MODULES`]:
//!
//!   ```text
//!   pub static GDBM: ModuleSpec = ModuleSpec {
//!       name: "_gdbm",
//!       functions: &[
//!           f("open", 1, 3, Type::Any),
//!           f("_gdbm_close", 1, 1, Type::None),
//!           f("_gdbm_keys", 1, 1, Type::List),
//!           f("_gdbm_get", 1, 3, Type::Any),
//!           f("_gdbm_setdefault", 1, 3, Type::Any),
//!           f("_gdbm_firstkey", 1, 1, Type::Any),
//!           f("_gdbm_nextkey", 2, 2, Type::Any),
//!           f("_gdbm_reorganize", 1, 1, Type::None),
//!           f("_gdbm_sync", 1, 1, Type::None),
//!           f("_gdbm_clear", 1, 1, Type::None),
//!           f("_gdbm_contains", 2, 2, Type::Bool),
//!           f("_gdbm_len", 1, 1, Type::Int),
//!           f("_gdbm_bool", 1, 1, Type::Bool),
//!           f("_gdbm_getitem", 2, 2, Type::Any),
//!           f("_gdbm_setitem", 3, 3, Type::None),
//!           f("_gdbm_delitem", 2, 2, Type::None),
//!           f("_gdbm_enter", 1, 1, Type::Any),
//!           f("_gdbm_exit", 1, 4, Type::None),
//!       ],
//!       constants: &[
//!           c("error", Type::Any),
//!           c("open_flags", Type::Str),
//!           c("_GDBM_VERSION", Type::Tuple),
//!       ],
//!   };
//!
//!   pub static DBM: ModuleSpec = ModuleSpec {
//!       name: "_dbm",
//!       functions: &[
//!           f("open", 1, 3, Type::Any),
//!           f("_dbm_close", 1, 1, Type::None),
//!           f("_dbm_keys", 1, 1, Type::List),
//!           f("_dbm_get", 1, 3, Type::Any),
//!           f("_dbm_setdefault", 1, 3, Type::Any),
//!           f("_dbm_clear", 1, 1, Type::None),
//!           f("_dbm_contains", 2, 2, Type::Bool),
//!           f("_dbm_len", 1, 1, Type::Int),
//!           f("_dbm_bool", 1, 1, Type::Bool),
//!           f("_dbm_getitem", 2, 2, Type::Any),
//!           f("_dbm_setitem", 3, 3, Type::None),
//!           f("_dbm_delitem", 2, 2, Type::None),
//!           f("_dbm_enter", 1, 1, Type::Any),
//!           f("_dbm_exit", 1, 4, Type::None),
//!       ],
//!       constants: &[c("error", Type::Any), c("library", Type::Str)],
//!   };
//!   ```
//!
//! * `modules/mod.rs`: `pub(crate) mod dbm_native;`, a dispatch arm
//!   `"_gdbm" | "_dbm" => dbm_native::call(module, function, args)`, the
//!   class hook `if let Some(class) = dbm_native::class_value(module, name)`,
//!   and the constant arm
//!   `("_gdbm" | "_dbm", name) => dbm_native::constant(module, name)`.
//! * Link flags: the file declares `#[link(name = "gdbm")]` and
//!   `#[link(name = "gdbm_compat")]`, so the final binary links
//!   `-lgdbm -lgdbm_compat`.
//!
//! The `dbm` package itself (`dbm/__init__.py`, `dbm/gnu.py`,
//! `dbm/ndbm.py`) is plain Python and is not part of this module.

#[cfg(unix)]
mod imp {
    use std::collections::HashMap;
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int, c_void};
    use std::sync::{LazyLock, Mutex, MutexGuard};

    use hydro_ir::MethodKind;

    use crate::core::globals;
    use crate::exc::{
        self, ATTRIBUTE_ERROR, KEY_ERROR, OS_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
    };
    use crate::value::{self, NONE};

    // ---------------------------------------------------------------------
    // libgdbm / ndbm FFI
    // ---------------------------------------------------------------------

    /// `datum` from `gdbm.h` / `ndbm.h`. Both libraries use the same layout.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Datum {
        dptr: *mut c_char,
        dsize: c_int,
    }

    /// `gdbm_open` flags (`gdbm.h`).
    const GDBM_READER: c_int = 0;
    const GDBM_WRITER: c_int = 1;
    const GDBM_WRCREAT: c_int = 2;
    const GDBM_NEWDB: c_int = 3;
    const GDBM_FAST: c_int = 0x0010;
    const GDBM_SYNC: c_int = 0x0020;
    const GDBM_NOLOCK: c_int = 0x0040;
    const GDBM_ITEM_NOT_FOUND: c_int = 15;

    /// `gdbm_store` insert policy (`gdbm.h`).
    const GDBM_REPLACE: c_int = 1;
    /// NDBM insert policy (`ndbm.h`), numerically the same as gdbm's.
    const DBM_REPLACE: c_int = 1;
    const DBM_INSERT: c_int = 0;

    #[link(name = "gdbm")]
    extern "C" {
        fn gdbm_open(
            file: *const c_char,
            block_size: c_int,
            flags: c_int,
            mode: c_int,
            fatal_func: Option<unsafe extern "C" fn(*const c_char)>,
        ) -> *mut c_void;
        fn gdbm_close(dbf: *mut c_void) -> c_int;
        fn gdbm_store(dbf: *mut c_void, key: Datum, content: Datum, flag: c_int) -> c_int;
        fn gdbm_fetch(dbf: *mut c_void, key: Datum) -> Datum;
        fn gdbm_delete(dbf: *mut c_void, key: Datum) -> c_int;
        fn gdbm_firstkey(dbf: *mut c_void) -> Datum;
        fn gdbm_nextkey(dbf: *mut c_void, key: Datum) -> Datum;
        fn gdbm_reorganize(dbf: *mut c_void) -> c_int;
        fn gdbm_sync(dbf: *mut c_void) -> c_int;
        fn gdbm_exists(dbf: *mut c_void, key: Datum) -> c_int;
        fn gdbm_count(dbf: *mut c_void, count: *mut u64) -> c_int;
        fn gdbm_strerror(error: c_int) -> *const c_char;
        fn gdbm_errno_location() -> *mut c_int;
        static gdbm_version_number: [c_int; 3];
    }

    #[link(name = "gdbm_compat")]
    extern "C" {
        fn dbm_open(file: *mut c_char, flags: c_int, mode: c_int) -> *mut c_void;
        fn dbm_close(dbf: *mut c_void);
        fn dbm_fetch(dbf: *mut c_void, key: Datum) -> Datum;
        fn dbm_store(dbf: *mut c_void, key: Datum, content: Datum, flags: c_int) -> c_int;
        fn dbm_delete(dbf: *mut c_void, key: Datum) -> c_int;
        fn dbm_firstkey(dbf: *mut c_void) -> Datum;
        fn dbm_nextkey(dbf: *mut c_void) -> Datum;
        fn dbm_error(dbf: *mut c_void) -> c_int;
        fn dbm_clearerr(dbf: *mut c_void);
    }

    // On glibc-based and Apple/BSD systems the errno accessor is one of two
    // symbols; gdbm itself is only shipped on Unix-like hosts.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe fn errno_location() -> *mut c_int {
        libc::__errno_location()
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    unsafe fn errno_location() -> *mut c_int {
        libc::__error()
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    unsafe fn errno_location() -> *mut c_int {
        // Remaining Unix targets expose errno through the same name on the
        // libc versions Hydro supports.
        libc::__errno_location()
    }

    fn clear_errno() {
        // SAFETY: the accessor returns a valid pointer to this thread's errno.
        unsafe {
            *errno_location() = 0;
        }
    }

    fn current_errno() -> c_int {
        // SAFETY: as above.
        unsafe { *errno_location() }
    }

    /// Text of `errno` without Rust's ` (os error N)` suffix.
    fn errno_detail(code: c_int) -> String {
        let text = std::io::Error::from_raw_os_error(code).to_string();
        text.split(" (os error").next().unwrap_or(&text).to_string()
    }

    // ---------------------------------------------------------------------
    // Registry
    // ---------------------------------------------------------------------

    const GDBM_MODULE: &str = "_gdbm";
    const DBM_MODULE: &str = "_dbm";

    const GDBM_CLASS_KEY: &str = "_gdbm.gdbm.class";
    const DBM_CLASS_KEY: &str = "_dbm.dbm.class";

    /// Private instance attribute holding the registry handle. The leading NUL
    /// keeps the internal name out of ordinary attribute listings, matching the
    /// other native classes in this crate.
    const HANDLE_ATTR: &str = "\u{0}dbm.handle";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Gdbm,
        Ndbm,
    }

    enum Backend {
        Gdbm(*mut c_void),
        Ndbm(*mut c_void),
    }

    struct Entry {
        backend: Backend,
        /// Cached entry count; `None` means "recompute".
        size: Option<i64>,
        /// Whether the underlying DBM was opened for writing (`O_RDWR`).
        writable: bool,
    }

    // SAFETY: the pointers are only dereferenced while the registry mutex is
    // held, and nothing hands them out of the registry.
    unsafe impl Send for Entry {}

    struct Registry {
        next_handle: u64,
        entries: HashMap<u64, Entry>,
    }

    static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
        Mutex::new(Registry {
            next_handle: 1,
            entries: HashMap::new(),
        })
    });

    fn registry() -> MutexGuard<'static, Registry> {
        REGISTRY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn register(backend: Backend, writable: bool) -> u64 {
        let mut registry = registry();
        let handle = registry.next_handle;
        registry.next_handle = handle.wrapping_add(1).max(1);
        registry.entries.insert(
            handle,
            Entry {
                backend,
                size: None,
                writable,
            },
        );
        handle
    }

    fn handle_of(instance: i64) -> Option<u64> {
        let raw = value::instance_attr(instance, HANDLE_ATTR).and_then(value::as_integer)?;
        u64::try_from(raw).ok().filter(|handle| *handle != 0)
    }

    /// Runs `body` on the live database behind `instance`.
    fn with_db<R>(
        module: &str,
        instance: i64,
        kind: Kind,
        body: impl FnOnce(*mut c_void, &mut Entry) -> Result<R, i64>,
    ) -> Result<R, i64> {
        let Some(handle) = handle_of(instance) else {
            return Err(closed_error(module));
        };
        let mut registry = registry();
        let Some(entry) = registry.entries.get_mut(&handle) else {
            return Err(closed_error(module));
        };
        let pointer = match (kind, &entry.backend) {
            (Kind::Gdbm, Backend::Gdbm(pointer)) => *pointer,
            (Kind::Ndbm, Backend::Ndbm(pointer)) => *pointer,
            _ => return Err(closed_error(module)),
        };
        body(pointer, entry)
    }

    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    fn closed_error(module: &str) -> i64 {
        let message = if module == GDBM_MODULE {
            "GDBM object has already been closed"
        } else {
            "DBM object has already been closed"
        };
        raise_error(module, message)
    }

    /// `_gdbm.error` / `_dbm.error`: an `OSError` subclass.
    fn error_class(module: &str) -> i64 {
        let key = format!("{module}.error");
        if let Some(class) = globals::registry_get(&key) {
            return class;
        }
        let base = exc::exception_class(OS_ERROR);
        let class = value::class_new("error".to_string(), base, Vec::new());
        globals::registry_set(&key, class);
        class
    }

    /// Raises an instance of the module's `error` class.
    fn raise_error(module: &str, message: &str) -> i64 {
        let class = error_class(module);
        let argument = value::str_alloc(message.to_string());
        let args = [argument];
        // Use a real instance so `except _gdbm.error` / `except OSError` and
        // `str(err)` behave like CPython.
        let instance = crate::builtins::call(class, args.len(), args.as_ptr());
        if exc::pending() {
            return instance;
        }
        exc::raise_value(instance)
    }

    /// `[Errno N] detail` (plus `: 'filename'` when the failure names a file).
    fn raise_errno_error(module: &str, filename: Option<&[u8]>) -> i64 {
        let code = current_errno();
        let detail = errno_detail(code);
        let message = match filename {
            Some(path) => format!("[Errno {code}] {detail}: {}", display_path(path)),
            None => format!("[Errno {code}] {detail}"),
        };
        raise_error(module, &message)
    }

    fn display_path(bytes: &[u8]) -> String {
        match std::str::from_utf8(bytes) {
            Ok(text) => format!("'{text}'"),
            Err(_) => value::repr(value::bytes_new(bytes.to_vec())),
        }
    }

    fn gdbm_error_text(code: c_int) -> Option<String> {
        // SAFETY: gdbm_strerror returns a static NUL-terminated string.
        let pointer = unsafe { gdbm_strerror(code) };
        if pointer.is_null() {
            return None;
        }
        // SAFETY: the library guarantees NUL termination.
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn gdbm_errno() -> c_int {
        // SAFETY: gdbm_errno_location returns a pointer to the thread's
        // gdbm error code.
        unsafe { *gdbm_errno_location() }
    }

    /// Reports a failed gdbm call: system errno when set, otherwise
    /// `gdbm_strerror(gdbm_errno)`.
    fn gdbm_failure(module: &str, generic: &str) -> i64 {
        if current_errno() != 0 {
            return raise_errno_error(module, None);
        }
        gdbm_strerror_error(module, generic)
    }

    /// Reports a logical gdbm failure (`gdbm_errno` only), as CPython does for
    /// `gdbm_delete` and other calls that do not clear/consult `errno`.
    fn gdbm_strerror_error(module: &str, generic: &str) -> i64 {
        match gdbm_error_text(gdbm_errno()) {
            Some(text) => raise_error(module, &text),
            None => raise_error(module, generic),
        }
    }

    fn key_error(key: i64) -> i64 {
        exc::raise(KEY_ERROR, key)
    }

    // ---------------------------------------------------------------------
    // Argument conversion
    // ---------------------------------------------------------------------

    fn integer(n: i64) -> i64 {
        value::encode_int(n).unwrap_or_else(|| value::bigint_new(num_bigint::BigInt::from(n)))
    }

    /// A C `int` argument with CPython's clinic conversion diagnostics.
    fn c_int_argument(argument: i64) -> Result<c_int, i64> {
        match value::as_integer(argument) {
            Some(number) => c_int::try_from(number).map_err(|_| {
                exc::raise_with_message(OVERFLOW_ERROR, "Python int too large to convert to C int")
            }),
            None if value::as_bigint(argument).is_some() => Err(exc::raise_with_message(
                OVERFLOW_ERROR,
                "Python int too large to convert to C int",
            )),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "'{}' object cannot be interpreted as an integer",
                    value::type_name_of(argument)
                ),
            )),
        }
    }

    /// `PyUnicode_FSConverter`: `str`, `bytes`, `pathlib.Path` and any object
    /// whose `__fspath__()` returns one of those.
    fn path_bytes(argument: i64) -> Result<Vec<u8>, i64> {
        if let Some(text) = value::as_str(argument) {
            return Ok(text.as_bytes().to_vec());
        }
        if value::as_bytearray(argument).is_none() {
            if let Some(bytes) = value::as_bytes(argument) {
                return Ok(bytes.to_vec());
            }
        }
        if let Some(state) = value::as_path(argument) {
            return Ok(state.path().as_bytes().to_vec());
        }
        if let Some(instance) = value::as_instance(argument) {
            if let Some(value::ClassEntry::Method(kind, function)) =
                value::class_lookup(instance.class, "__fspath__")
            {
                // Special methods follow the descriptor protocol.
                let argv: Vec<i64> = match kind {
                    MethodKind::Normal | MethodKind::Property => vec![argument],
                    MethodKind::Classmethod => vec![instance.class],
                    MethodKind::Staticmethod => Vec::new(),
                };
                let result = crate::builtins::call(function, argv.len(), argv.as_ptr());
                if exc::pending() {
                    return Err(NONE);
                }
                if let Some(text) = value::as_str(result) {
                    return Ok(text.as_bytes().to_vec());
                }
                if value::as_bytearray(result).is_none() {
                    if let Some(bytes) = value::as_bytes(result) {
                        return Ok(bytes.to_vec());
                    }
                }
                let name = value::class_display_name(instance.class).unwrap_or("object");
                return Err(exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "expected {name}.__fspath__() to return str or bytes, not {}",
                        value::type_name_of(result)
                    ),
                ));
            }
        }
        Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "expected str, bytes or os.PathLike object, not {}",
                value::type_name_of(argument)
            ),
        ))
    }

    fn c_string(path: Vec<u8>) -> Result<CString, i64> {
        CString::new(path).map_err(|_| exc::raise_with_message(VALUE_ERROR, "embedded null byte"))
    }

    fn check_datum_length(bytes: &[u8]) -> Result<(), i64> {
        if bytes.len() > c_int::MAX as usize {
            return Err(exc::raise_with_message(
                OVERFLOW_ERROR,
                "size does not fit in an int",
            ));
        }
        Ok(())
    }

    /// Accesses a value as a `str`/`bytes` datum. `fail` overrides the parse
    /// diagnostic exactly like `parse_datum(..., failmsg)` in CPython.
    fn datum_bytes(argument: i64, fail: Option<&str>) -> Result<Vec<u8>, i64> {
        let bytes = if let Some(text) = value::as_str(argument) {
            text.as_bytes().to_vec()
        } else if value::as_bytearray(argument).is_none() {
            match value::as_bytes(argument) {
                Some(bytes) => bytes.to_vec(),
                None => return Err(datum_parse_error(argument, fail, "argument ")),
            }
        } else {
            return Err(datum_parse_error(argument, fail, "argument "));
        };
        check_datum_length(&bytes)?;
        Ok(bytes)
    }

    /// Accesses a read-only bytes-like argument the way a clinic
    /// `str(accept={str, robuffer})` converter does. `prefix` is the
    /// `"function() argument 1 "` part of the read-only diagnostic.
    fn read_only_datum_bytes(argument: i64, prefix: &str) -> Result<Vec<u8>, i64> {
        let bytes = if let Some(text) = value::as_str(argument) {
            text.as_bytes().to_vec()
        } else if value::as_bytearray(argument).is_none() {
            match value::as_bytes(argument) {
                Some(bytes) => bytes.to_vec(),
                None => return Err(datum_parse_error(argument, None, prefix)),
            }
        } else {
            return Err(datum_parse_error(argument, None, prefix));
        };
        check_datum_length(&bytes)?;
        Ok(bytes)
    }

    fn datum_parse_error(argument: i64, fail: Option<&str>, prefix: &str) -> i64 {
        if let Some(message) = fail {
            return exc::raise_with_message(TYPE_ERROR, message);
        }
        let name = value::type_name_of(argument);
        let read_only =
            value::as_bytearray(argument).is_some() || value::as_memoryview(argument).is_some();
        let message = if read_only {
            format!("{prefix}must be read-only bytes-like object, not {name}")
        } else {
            format!("a bytes-like object is required, not '{name}'")
        };
        exc::raise_with_message(TYPE_ERROR, &message)
    }

    /// `__contains__`'s narrower operand rule: only `str` and `bytes`.
    fn contains_key(module: &str, argument: i64) -> Result<Vec<u8>, i64> {
        let bytes = if let Some(text) = value::as_str(argument) {
            text.as_bytes().to_vec()
        } else if value::as_bytearray(argument).is_none() {
            match value::as_bytes(argument) {
                Some(bytes) => bytes.to_vec(),
                None => {
                    return Err(exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "{} key must be bytes or string, not {}",
                            if module == GDBM_MODULE { "gdbm" } else { "dbm" },
                            value::type_name_of(argument)
                        ),
                    ))
                }
            }
        } else {
            return Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{} key must be bytes or string, not {}",
                    if module == GDBM_MODULE { "gdbm" } else { "dbm" },
                    value::type_name_of(argument)
                ),
            ));
        };
        check_datum_length(&bytes)?;
        Ok(bytes)
    }

    /// A byte the empty datum points at, so `dptr` is never null and never a
    /// dangling slice pointer when `dsize` is zero.
    static EMPTY_DATUM_BYTE: u8 = 0;

    fn datum(bytes: &[u8]) -> Datum {
        let pointer = if bytes.is_empty() {
            std::ptr::addr_of!(EMPTY_DATUM_BYTE)
        } else {
            bytes.as_ptr()
        };
        Datum {
            dptr: pointer as *mut c_char,
            dsize: bytes.len() as c_int,
        }
    }

    /// Copies a malloc'd gdbm datum and releases it.
    ///
    /// # Safety
    ///
    /// `result` must be a datum returned by gdbm whose `dptr` is either null or
    /// a malloc'd block owned by the caller.
    unsafe fn take_datum(result: Datum) -> Option<Vec<u8>> {
        if result.dptr.is_null() {
            return None;
        }
        let length = if result.dsize < 0 {
            0
        } else {
            result.dsize as usize
        };
        let bytes = std::slice::from_raw_parts(result.dptr as *const u8, length).to_vec();
        libc::free(result.dptr as *mut c_void);
        Some(bytes)
    }

    /// Copies an NDBM datum without freeing it (the compatibility layer owns
    /// the returned buffer until the next call on the handle).
    ///
    /// # Safety
    ///
    /// `result.dptr` must be null or point at `dsize` readable bytes.
    unsafe fn borrow_datum(result: Datum) -> Option<Vec<u8>> {
        if result.dptr.is_null() {
            return None;
        }
        let length = if result.dsize < 0 {
            0
        } else {
            result.dsize as usize
        };
        Some(std::slice::from_raw_parts(result.dptr as *const u8, length).to_vec())
    }

    // ---------------------------------------------------------------------
    // Classes
    // ---------------------------------------------------------------------

    fn native(module: &'static str, function: &'static str) -> i64 {
        value::native_function_new(module, function, usize::MAX)
    }

    /// The cached `_gdbm.gdbm` class.
    fn gdbm_class() -> i64 {
        if let Some(class) = globals::registry_get(GDBM_CLASS_KEY) {
            return class;
        }
        let methods = vec![
            (
                "__getitem__".to_string(),
                native(GDBM_MODULE, "_gdbm_getitem"),
            ),
            (
                "__setitem__".to_string(),
                native(GDBM_MODULE, "_gdbm_setitem"),
            ),
            (
                "__delitem__".to_string(),
                native(GDBM_MODULE, "_gdbm_delitem"),
            ),
            (
                "__contains__".to_string(),
                native(GDBM_MODULE, "_gdbm_contains"),
            ),
            ("__len__".to_string(), native(GDBM_MODULE, "_gdbm_len")),
            ("__bool__".to_string(), native(GDBM_MODULE, "_gdbm_bool")),
            ("__enter__".to_string(), native(GDBM_MODULE, "_gdbm_enter")),
            ("__exit__".to_string(), native(GDBM_MODULE, "_gdbm_exit")),
            ("close".to_string(), native(GDBM_MODULE, "_gdbm_close")),
            ("keys".to_string(), native(GDBM_MODULE, "_gdbm_keys")),
            ("get".to_string(), native(GDBM_MODULE, "_gdbm_get")),
            (
                "setdefault".to_string(),
                native(GDBM_MODULE, "_gdbm_setdefault"),
            ),
            (
                "firstkey".to_string(),
                native(GDBM_MODULE, "_gdbm_firstkey"),
            ),
            ("nextkey".to_string(), native(GDBM_MODULE, "_gdbm_nextkey")),
            (
                "reorganize".to_string(),
                native(GDBM_MODULE, "_gdbm_reorganize"),
            ),
            ("sync".to_string(), native(GDBM_MODULE, "_gdbm_sync")),
            ("clear".to_string(), native(GDBM_MODULE, "_gdbm_clear")),
        ];
        let class = value::class_new("gdbm".to_string(), NONE, methods);
        globals::registry_set(GDBM_CLASS_KEY, class);
        class
    }

    /// The cached `_dbm.dbm` class.
    fn ndbm_class() -> i64 {
        if let Some(class) = globals::registry_get(DBM_CLASS_KEY) {
            return class;
        }
        let methods = vec![
            (
                "__getitem__".to_string(),
                native(DBM_MODULE, "_dbm_getitem"),
            ),
            (
                "__setitem__".to_string(),
                native(DBM_MODULE, "_dbm_setitem"),
            ),
            (
                "__delitem__".to_string(),
                native(DBM_MODULE, "_dbm_delitem"),
            ),
            (
                "__contains__".to_string(),
                native(DBM_MODULE, "_dbm_contains"),
            ),
            ("__len__".to_string(), native(DBM_MODULE, "_dbm_len")),
            ("__bool__".to_string(), native(DBM_MODULE, "_dbm_bool")),
            ("__enter__".to_string(), native(DBM_MODULE, "_dbm_enter")),
            ("__exit__".to_string(), native(DBM_MODULE, "_dbm_exit")),
            ("close".to_string(), native(DBM_MODULE, "_dbm_close")),
            ("keys".to_string(), native(DBM_MODULE, "_dbm_keys")),
            ("get".to_string(), native(DBM_MODULE, "_dbm_get")),
            (
                "setdefault".to_string(),
                native(DBM_MODULE, "_dbm_setdefault"),
            ),
            ("clear".to_string(), native(DBM_MODULE, "_dbm_clear")),
        ];
        let class = value::class_new("dbm".to_string(), NONE, methods);
        globals::registry_set(DBM_CLASS_KEY, class);
        class
    }

    fn instance_for(class: i64, backend: Backend, writable: bool) -> i64 {
        let handle = register(backend, writable);
        let instance = value::instance_new(class);
        value::instance_setattr(instance, HANDLE_ATTR.to_string(), integer(handle as i64));
        instance
    }

    // ---------------------------------------------------------------------
    // _gdbm
    // ---------------------------------------------------------------------

    fn gdbm_flags(flags: &str) -> Result<c_int, i64> {
        let bytes = flags.as_bytes();
        let Some(&first) = bytes.first() else {
            return Err(raise_error(
                GDBM_MODULE,
                "First flag must be one of 'r', 'w', 'c' or 'n'",
            ));
        };
        let mut result = match first {
            b'r' => GDBM_READER,
            b'w' => GDBM_WRITER,
            b'c' => GDBM_WRCREAT,
            b'n' => GDBM_NEWDB,
            _ => {
                return Err(raise_error(
                    GDBM_MODULE,
                    "First flag must be one of 'r', 'w', 'c' or 'n'",
                ))
            }
        };
        for &byte in &bytes[1..] {
            match byte {
                b'f' => result |= GDBM_FAST,
                b's' => result |= GDBM_SYNC,
                b'u' => result |= GDBM_NOLOCK,
                other => {
                    return Err(raise_error(
                        GDBM_MODULE,
                        &format!("Flag '{}' is not supported.", other as char),
                    ))
                }
            }
        }
        Ok(result)
    }

    fn gdbm_open_call(args: &[i64]) -> i64 {
        if args.is_empty() {
            return exc::raise_with_message(TYPE_ERROR, "open expected at least 1 argument, got 0");
        }
        if args.len() > 3 {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("open expected at most 3 arguments, got {}", args.len()),
            );
        }
        // Clinic converts `flags` (str) and `mode` (int) before the body, so
        // those diagnostics precede flag-character and filename errors.
        let flags_text = match args.get(1) {
            None => "r".to_string(),
            Some(&argument) => match value::as_str(argument) {
                Some(text) => text.to_string(),
                None => {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "open() argument 2 must be str, not {}",
                            value::type_name_of(argument)
                        ),
                    )
                }
            },
        };
        let mode = match args.get(2) {
            None => 0o666,
            Some(&argument) => match c_int_argument(argument) {
                Ok(mode) => mode,
                Err(error) => return error,
            },
        };
        let iflags = match gdbm_flags(&flags_text) {
            Ok(flags) => flags,
            Err(error) => return error,
        };
        let path = match path_bytes(args[0]).and_then(c_string) {
            Ok(path) => path,
            Err(error) => return error,
        };
        // SAFETY: `path` is NUL-terminated; the remaining arguments are plain
        // integers and a null fatal-error callback.
        clear_errno();
        let pointer = unsafe { gdbm_open(path.as_ptr(), 0, iflags, mode, None) };
        if pointer.is_null() {
            if current_errno() != 0 {
                return raise_errno_error(GDBM_MODULE, Some(path.as_bytes()));
            }
            return match gdbm_error_text(gdbm_errno()) {
                Some(text) => raise_error(GDBM_MODULE, &text),
                None => raise_error(GDBM_MODULE, "gdbm_open() error"),
            };
        }
        instance_for(gdbm_class(), Backend::Gdbm(pointer), true)
    }

    fn gdbm_close_method(instance: i64) -> i64 {
        if let Some(handle) = handle_of(instance) {
            let entry = registry().entries.remove(&handle);
            if let Some(entry) = entry {
                match entry.backend {
                    Backend::Gdbm(pointer) => {
                        // SAFETY: the registry owns this open handle.
                        unsafe {
                            gdbm_close(pointer);
                        }
                    }
                    Backend::Ndbm(pointer) => {
                        // SAFETY: as above.
                        unsafe {
                            dbm_close(pointer);
                        }
                    }
                }
            }
        }
        NONE
    }

    /// Reads `db[key]`; `default` is returned instead of raising `KeyError`.
    fn gdbm_get(instance: i64, key_value: i64, default: Option<i64>) -> i64 {
        let key = match datum_bytes(key_value, None) {
            Ok(key) => key,
            Err(error) => return error,
        };
        let fetched = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, _| {
            // SAFETY: `pointer` is a live gdbm handle and `key` outlives the
            // call.
            let result = unsafe { gdbm_fetch(pointer, datum(&key)) };
            // SAFETY: gdbm_fetch returns a malloc'd datum owned by us.
            Ok(unsafe { take_datum(result) })
        });
        match fetched {
            Ok(Some(bytes)) => value::bytes_new(bytes),
            Ok(None) => match default {
                Some(default) => default,
                None => key_error(key_value),
            },
            Err(error) => error,
        }
    }

    fn gdbm_setdefault(args: &[i64]) -> i64 {
        let given = args.len().saturating_sub(1);
        if given < 1 {
            return exc::raise_with_message(
                TYPE_ERROR,
                "setdefault expected at least 1 argument, got 0",
            );
        }
        if given > 2 {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("setdefault expected at most 2 arguments, got {given}"),
            );
        }
        let instance = args.first().copied().unwrap_or(NONE);
        let key_value = args[1];
        let default = args.get(2).copied().unwrap_or(NONE);
        let key = match datum_bytes(key_value, None) {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, entry| {
            // SAFETY: handle and key are valid for this call.
            let found = unsafe { gdbm_fetch(pointer, datum(&key)) };
            // SAFETY: fetched datum is malloc'd by gdbm.
            if let Some(bytes) = unsafe { take_datum(found) } {
                return Ok(Some(bytes));
            }
            entry.size = None;
            let stored = datum_bytes(
                default,
                Some("gdbm mappings have bytes or string indices only"),
            )?;
            clear_errno();
            // SAFETY: live handle; key/value buffers outlive the call.
            if unsafe { gdbm_store(pointer, datum(&key), datum(&stored), GDBM_REPLACE) } < 0 {
                return Err(gdbm_failure(GDBM_MODULE, "gdbm_store() error"));
            }
            Ok(Some(stored))
        });
        match outcome {
            Ok(Some(bytes)) => value::bytes_new(bytes),
            Ok(None) => key_error(key_value),
            Err(error) => error,
        }
    }

    fn gdbm_firstkey_method(instance: i64) -> i64 {
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, _| {
            // SAFETY: live handle.
            let result = unsafe { gdbm_firstkey(pointer) };
            // SAFETY: gdbm_firstkey returns a malloc'd datum.
            Ok(unsafe { take_datum(result) })
        });
        match outcome {
            Ok(Some(bytes)) => value::bytes_new(bytes),
            Ok(None) => NONE,
            Err(error) => error,
        }
    }

    fn gdbm_nextkey_method(args: &[i64]) -> i64 {
        let given = args.len().saturating_sub(1);
        if given == 0 {
            return exc::raise_with_message(
                TYPE_ERROR,
                "nextkey() takes exactly 1 positional argument (0 given)",
            );
        }
        if given > 1 {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("nextkey() takes at most 1 argument ({given} given)"),
            );
        }
        let instance = args.first().copied().unwrap_or(NONE);
        let key = match read_only_datum_bytes(args[1], "nextkey() argument 1 ") {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, _| {
            // SAFETY: live handle; `key` outlives the call.
            let result = unsafe { gdbm_nextkey(pointer, datum(&key)) };
            // SAFETY: gdbm_nextkey returns a malloc'd datum.
            Ok(unsafe { take_datum(result) })
        });
        match outcome {
            Ok(Some(bytes)) => value::bytes_new(bytes),
            Ok(None) => NONE,
            Err(error) => error,
        }
    }

    fn gdbm_keys(instance: i64) -> i64 {
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, _| {
            let mut collected = Vec::new();
            // SAFETY: live handle.
            let mut key = unsafe { gdbm_firstkey(pointer) };
            while !key.dptr.is_null() {
                // SAFETY: key is a live malloc'd datum.
                collected.push(unsafe { borrow_datum(key) }.unwrap_or_default());
                // SAFETY: nextkey consumes the previous key and returns a new
                // malloc'd datum.
                let next = unsafe { gdbm_nextkey(pointer, key) };
                // SAFETY: the previous datum is no longer needed.
                unsafe {
                    libc::free(key.dptr as *mut c_void);
                }
                key = next;
            }
            Ok(collected)
        });
        match outcome {
            Ok(collected) => value::list_new(
                collected
                    .into_iter()
                    .map(value::bytes_new)
                    .collect::<Vec<i64>>(),
            ),
            Err(error) => error,
        }
    }

    fn gdbm_len(instance: i64) -> i64 {
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, entry| {
            if let Some(size) = entry.size {
                return Ok(size);
            }
            let mut count: u64 = 0;
            clear_errno();
            // SAFETY: live handle and a valid out-parameter.
            if unsafe { gdbm_count(pointer, &mut count) } == -1 {
                return Err(gdbm_failure(GDBM_MODULE, "gdbm_count() error"));
            }
            let size = i64::try_from(count).map_err(|_| {
                exc::raise_with_message(OVERFLOW_ERROR, "count exceeds PY_SSIZE_T_MAX")
            })?;
            entry.size = Some(size);
            Ok(size)
        });
        match outcome {
            Ok(size) => integer(size),
            Err(error) => error,
        }
    }

    fn gdbm_bool(instance: i64) -> i64 {
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, entry| {
            if let Some(size) = entry.size {
                return Ok(size != 0);
            }
            // SAFETY: live handle; the returned key is malloc'd.
            let key = unsafe { gdbm_firstkey(pointer) };
            let present = !key.dptr.is_null();
            if present {
                // SAFETY: release the probe key.
                unsafe {
                    libc::free(key.dptr as *mut c_void);
                }
            } else {
                entry.size = Some(0);
            }
            Ok(present)
        });
        match outcome {
            Ok(present) => value::bool_value(present),
            Err(error) => error,
        }
    }

    fn gdbm_setitem(args: &[i64]) -> i64 {
        let instance = args.first().copied().unwrap_or(NONE);
        let key_value = args.get(1).copied().unwrap_or(NONE);
        let value = args.get(2).copied().unwrap_or(NONE);
        let key = match datum_bytes(
            key_value,
            Some("gdbm mappings have bytes or string indices only"),
        ) {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, entry| {
            // The value is parsed only after the closed check, like CPython.
            let stored = datum_bytes(
                value,
                Some("gdbm mappings have bytes or string indices only"),
            )?;
            entry.size = None;
            clear_errno();
            // SAFETY: live handle; buffers outlive the call.
            if unsafe { gdbm_store(pointer, datum(&key), datum(&stored), GDBM_REPLACE) } < 0 {
                return Err(gdbm_failure(GDBM_MODULE, "gdbm_store() error"));
            }
            Ok(())
        });
        match outcome {
            Ok(()) => NONE,
            Err(error) => error,
        }
    }

    fn gdbm_delitem(args: &[i64]) -> i64 {
        let instance = args.first().copied().unwrap_or(NONE);
        let key_value = args.get(1).copied().unwrap_or(NONE);
        let key = match datum_bytes(
            key_value,
            Some("gdbm mappings have bytes or string indices only"),
        ) {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, entry| {
            entry.size = None;
            // SAFETY: live handle; key outlives the call.
            if unsafe { gdbm_delete(pointer, datum(&key)) } < 0 {
                if gdbm_errno() == GDBM_ITEM_NOT_FOUND {
                    return Err(key_error(key_value));
                }
                return Err(gdbm_strerror_error(GDBM_MODULE, "gdbm_delete() error"));
            }
            Ok(())
        });
        match outcome {
            Ok(()) => NONE,
            Err(error) => error,
        }
    }

    fn gdbm_contains(args: &[i64]) -> i64 {
        let instance = args.first().copied().unwrap_or(NONE);
        // Closed databases are reported before the key type is examined.
        if handle_of(instance).is_none() {
            return closed_error(GDBM_MODULE);
        }
        let key_value = args.get(1).copied().unwrap_or(NONE);
        let key = match contains_key(GDBM_MODULE, key_value) {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, _| {
            // SAFETY: live handle; key outlives the call.
            Ok(unsafe { gdbm_exists(pointer, datum(&key)) } != 0)
        });
        match outcome {
            Ok(found) => value::bool_value(found),
            Err(error) => error,
        }
    }

    fn gdbm_reorganize_method(instance: i64) -> i64 {
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, _| {
            clear_errno();
            // SAFETY: live handle.
            if unsafe { gdbm_reorganize(pointer) } < 0 {
                return Err(gdbm_failure(GDBM_MODULE, "gdbm_reorganize() error"));
            }
            Ok(())
        });
        match outcome {
            Ok(()) => NONE,
            Err(error) => error,
        }
    }

    fn gdbm_sync_method(instance: i64) -> i64 {
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, _| {
            // SAFETY: live handle; CPython ignores the return value.
            unsafe {
                gdbm_sync(pointer);
            }
            Ok(())
        });
        match outcome {
            Ok(()) => NONE,
            Err(error) => error,
        }
    }

    fn gdbm_clear(instance: i64) -> i64 {
        let outcome = with_db(GDBM_MODULE, instance, Kind::Gdbm, |pointer, entry| {
            entry.size = None;
            loop {
                // SAFETY: live handle.
                let key = unsafe { gdbm_firstkey(pointer) };
                if key.dptr.is_null() {
                    break;
                }
                // SAFETY: live handle and malloc'd key.
                let deleted = unsafe { gdbm_delete(pointer, key) };
                // SAFETY: gdbm_delete does not retain the key.
                unsafe {
                    libc::free(key.dptr as *mut c_void);
                }
                if deleted < 0 {
                    return Err(raise_error(GDBM_MODULE, "cannot delete item from database"));
                }
            }
            entry.size = Some(0);
            Ok(())
        });
        match outcome {
            Ok(()) => NONE,
            Err(error) => error,
        }
    }

    // ---------------------------------------------------------------------
    // _dbm
    // ---------------------------------------------------------------------

    fn dbm_open_call(args: &[i64]) -> i64 {
        if args.is_empty() {
            return exc::raise_with_message(TYPE_ERROR, "open expected at least 1 argument, got 0");
        }
        if args.len() > 3 {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("open expected at most 3 arguments, got {}", args.len()),
            );
        }
        let flags_text = match args.get(1) {
            None => "r".to_string(),
            Some(&argument) => match value::as_str(argument) {
                Some(text) => text.to_string(),
                None => {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "open() argument 2 must be str, not {}",
                            value::type_name_of(argument)
                        ),
                    )
                }
            },
        };
        let mode = match args.get(2) {
            None => 0o666,
            Some(&argument) => match c_int_argument(argument) {
                Ok(mode) => mode,
                Err(error) => return error,
            },
        };
        let iflags = match flags_text.as_str() {
            "r" => libc::O_RDONLY,
            "w" => libc::O_RDWR,
            "rw" => libc::O_RDWR | libc::O_CREAT,
            "c" => libc::O_RDWR | libc::O_CREAT,
            "n" => libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
            _ => return raise_error(DBM_MODULE, "arg 2 to open should be 'r', 'w', 'c', or 'n'"),
        };
        let path = match path_bytes(args[0]).and_then(c_string) {
            Ok(path) => path,
            Err(error) => return error,
        };
        clear_errno();
        // SAFETY: `path` is NUL-terminated; `dbm_open` takes a mutable C
        // string but does not modify the name.
        let pointer = unsafe { dbm_open(path.as_ptr() as *mut c_char, iflags, mode) };
        if pointer.is_null() {
            return raise_errno_error(DBM_MODULE, Some(path.as_bytes()));
        }
        let writable = iflags & libc::O_RDWR != 0;
        instance_for(ndbm_class(), Backend::Ndbm(pointer), writable)
    }

    fn ndbm_close_method(instance: i64) -> i64 {
        if let Some(handle) = handle_of(instance) {
            let entry = registry().entries.remove(&handle);
            if let Some(entry) = entry {
                match entry.backend {
                    Backend::Ndbm(pointer) => {
                        // SAFETY: the registry owns this open handle.
                        unsafe {
                            dbm_close(pointer);
                        }
                    }
                    Backend::Gdbm(pointer) => {
                        // SAFETY: as above.
                        unsafe {
                            gdbm_close(pointer);
                        }
                    }
                }
            }
        }
        NONE
    }

    fn ndbm_error_set(pointer: *mut c_void) -> bool {
        // SAFETY: live NDBM handle.
        unsafe { dbm_error(pointer) != 0 }
    }

    fn ndbm_clear_error(pointer: *mut c_void) {
        // SAFETY: live NDBM handle.
        unsafe {
            dbm_clearerr(pointer);
        }
    }

    fn ndbm_fetch(pointer: *mut c_void, key: &[u8]) -> Option<Vec<u8>> {
        // SAFETY: live handle; key outlives the call. The result is copied
        // immediately and not freed: the compatibility layer owns it.
        unsafe { borrow_datum(dbm_fetch(pointer, datum(key))) }
    }

    fn ndbm_get(instance: i64, key_value: i64, default: i64) -> i64 {
        let key = match read_only_datum_bytes(key_value, "get() argument 1 ") {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(DBM_MODULE, instance, Kind::Ndbm, |pointer, _| {
            Ok(ndbm_fetch(pointer, &key))
        });
        match outcome {
            Ok(Some(bytes)) => value::bytes_new(bytes),
            Ok(None) => default,
            Err(error) => error,
        }
    }

    fn ndbm_setdefault(args: &[i64]) -> i64 {
        let given = args.len().saturating_sub(1);
        if given < 1 {
            return exc::raise_with_message(
                TYPE_ERROR,
                "setdefault() takes at least 1 positional argument (0 given)",
            );
        }
        if given > 2 {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("setdefault() takes at most 2 arguments ({given} given)"),
            );
        }
        let instance = args.first().copied().unwrap_or(NONE);
        let key_value = args[1];
        let key = match read_only_datum_bytes(key_value, "setdefault() argument 1 ") {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(DBM_MODULE, instance, Kind::Ndbm, |pointer, _| {
            if let Some(bytes) = ndbm_fetch(pointer, &key) {
                return Ok(Some(value::bytes_new(bytes)));
            }
            let Some(&default) = args.get(2) else {
                // CPython passes a NULL datum for the omitted default; every
                // NDBM implementation accepts the call but rejects the store.
                return Err(raise_error(DBM_MODULE, "cannot add item to database"));
            };
            let stored = datum_bytes(
                default,
                Some("dbm mappings have bytes or string elements only"),
            )?;
            // SAFETY: live handle; buffers outlive the call.
            let status = unsafe { dbm_store(pointer, datum(&key), datum(&stored), DBM_INSERT) };
            if status < 0 {
                ndbm_clear_error(pointer);
                return Err(raise_error(DBM_MODULE, "cannot add item to database"));
            }
            // CPython returns the default object itself, not a bytes copy.
            Ok(Some(default))
        });
        match outcome {
            Ok(Some(result)) => result,
            Ok(None) => NONE,
            Err(error) => error,
        }
    }

    fn ndbm_keys(instance: i64) -> i64 {
        let outcome = with_db(DBM_MODULE, instance, Kind::Ndbm, |pointer, _| {
            let mut collected = Vec::new();
            // SAFETY: live handle.
            let mut key = unsafe { dbm_firstkey(pointer) };
            while !key.dptr.is_null() {
                // SAFETY: the compatibility layer keeps the key buffer alive
                // until the next traversal call.
                collected.push(unsafe { borrow_datum(key) }.unwrap_or_default());
                // SAFETY: live handle.
                key = unsafe { dbm_nextkey(pointer) };
            }
            Ok(collected)
        });
        match outcome {
            Ok(collected) => value::list_new(
                collected
                    .into_iter()
                    .map(value::bytes_new)
                    .collect::<Vec<i64>>(),
            ),
            Err(error) => error,
        }
    }

    fn ndbm_len(instance: i64) -> i64 {
        let outcome = with_db(DBM_MODULE, instance, Kind::Ndbm, |pointer, entry| {
            if let Some(size) = entry.size {
                return Ok(size);
            }
            let mut size: i64 = 0;
            // SAFETY: live handle.
            let mut key = unsafe { dbm_firstkey(pointer) };
            while !key.dptr.is_null() {
                size = size.saturating_add(1);
                // SAFETY: live handle.
                key = unsafe { dbm_nextkey(pointer) };
            }
            entry.size = Some(size);
            Ok(size)
        });
        match outcome {
            Ok(size) => integer(size),
            Err(error) => error,
        }
    }

    fn ndbm_bool(instance: i64) -> i64 {
        let outcome = with_db(DBM_MODULE, instance, Kind::Ndbm, |pointer, entry| {
            if let Some(size) = entry.size {
                return Ok(size != 0);
            }
            // SAFETY: live handle.
            let key = unsafe { dbm_firstkey(pointer) };
            let present = !key.dptr.is_null();
            if !present {
                entry.size = Some(0);
            }
            Ok(present)
        });
        match outcome {
            Ok(present) => value::bool_value(present),
            Err(error) => error,
        }
    }

    fn ndbm_getitem(args: &[i64]) -> i64 {
        let instance = args.first().copied().unwrap_or(NONE);
        let key_value = args.get(1).copied().unwrap_or(NONE);
        let key = match datum_bytes(key_value, None) {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(DBM_MODULE, instance, Kind::Ndbm, |pointer, _| {
            match ndbm_fetch(pointer, &key) {
                Some(bytes) => {
                    // CPython checks the NDBM error flag only for a value it
                    // actually found; a missing key is a plain KeyError.
                    if ndbm_error_set(pointer) {
                        ndbm_clear_error(pointer);
                        return Err(raise_error(DBM_MODULE, ""));
                    }
                    Ok(Some(bytes))
                }
                None => Ok(None),
            }
        });
        match outcome {
            Ok(Some(bytes)) => value::bytes_new(bytes),
            Ok(None) => key_error(key_value),
            Err(error) => error,
        }
    }

    fn ndbm_setitem(args: &[i64]) -> i64 {
        let instance = args.first().copied().unwrap_or(NONE);
        let key_value = args.get(1).copied().unwrap_or(NONE);
        let value = args.get(2).copied().unwrap_or(NONE);
        let key = match datum_bytes(
            key_value,
            Some("dbm mappings have bytes or string keys only"),
        ) {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(DBM_MODULE, instance, Kind::Ndbm, |pointer, entry| {
            // The value is parsed only after the closed check, like CPython.
            let stored = datum_bytes(
                value,
                Some("dbm mappings have bytes or string elements only"),
            )?;
            entry.size = None;
            // SAFETY: live handle; buffers outlive the call.
            let status = unsafe { dbm_store(pointer, datum(&key), datum(&stored), DBM_REPLACE) };
            if status < 0 {
                ndbm_clear_error(pointer);
                return Err(raise_error(DBM_MODULE, "cannot add item to database"));
            }
            if ndbm_error_set(pointer) {
                ndbm_clear_error(pointer);
                return Err(raise_error(DBM_MODULE, ""));
            }
            Ok(())
        });
        match outcome {
            Ok(()) => NONE,
            Err(error) => error,
        }
    }

    fn ndbm_delitem(args: &[i64]) -> i64 {
        let instance = args.first().copied().unwrap_or(NONE);
        let key_value = args.get(1).copied().unwrap_or(NONE);
        let key = match datum_bytes(
            key_value,
            Some("dbm mappings have bytes or string keys only"),
        ) {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(DBM_MODULE, instance, Kind::Ndbm, |pointer, entry| {
            entry.size = None;
            // SAFETY: live handle; key outlives the call.
            if unsafe { dbm_delete(pointer, datum(&key)) } < 0 {
                ndbm_clear_error(pointer);
                if entry.writable {
                    return Err(key_error(key_value));
                }
                return Err(raise_error(DBM_MODULE, "cannot delete item from database"));
            }
            if ndbm_error_set(pointer) {
                ndbm_clear_error(pointer);
                return Err(raise_error(DBM_MODULE, ""));
            }
            Ok(())
        });
        match outcome {
            Ok(()) => NONE,
            Err(error) => error,
        }
    }

    fn ndbm_contains(args: &[i64]) -> i64 {
        let instance = args.first().copied().unwrap_or(NONE);
        if handle_of(instance).is_none() {
            return closed_error(DBM_MODULE);
        }
        let key_value = args.get(1).copied().unwrap_or(NONE);
        let key = match contains_key(DBM_MODULE, key_value) {
            Ok(key) => key,
            Err(error) => return error,
        };
        let outcome = with_db(DBM_MODULE, instance, Kind::Ndbm, |pointer, _| {
            Ok(ndbm_fetch(pointer, &key).is_some())
        });
        match outcome {
            Ok(found) => value::bool_value(found),
            Err(error) => error,
        }
    }

    fn ndbm_clear(instance: i64) -> i64 {
        let outcome = with_db(DBM_MODULE, instance, Kind::Ndbm, |pointer, entry| {
            entry.size = None;
            loop {
                // SAFETY: live handle.
                let key = unsafe { dbm_firstkey(pointer) };
                if key.dptr.is_null() {
                    break;
                }
                // SAFETY: live handle.
                if unsafe { dbm_delete(pointer, key) } < 0 {
                    ndbm_clear_error(pointer);
                    return Err(raise_error(DBM_MODULE, "cannot delete item from database"));
                }
            }
            Ok(())
        });
        match outcome {
            Ok(()) => NONE,
            Err(error) => error,
        }
    }

    // ---------------------------------------------------------------------
    // Dispatch
    // ---------------------------------------------------------------------

    fn missing_attribute(module: &str, function: &str) -> i64 {
        exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{module}' has no attribute '{function}'"),
        )
    }

    fn gdbm_call(function: &str, args: &[i64]) -> i64 {
        let instance = args.first().copied().unwrap_or(NONE);
        match function {
            "open" => gdbm_open_call(args),
            "_gdbm_close" => gdbm_close_method(instance),
            "_gdbm_keys" => gdbm_keys(instance),
            "_gdbm_firstkey" => gdbm_firstkey_method(instance),
            "_gdbm_nextkey" => gdbm_nextkey_method(args),
            "_gdbm_reorganize" => gdbm_reorganize_method(instance),
            "_gdbm_sync" => gdbm_sync_method(instance),
            "_gdbm_clear" => gdbm_clear(instance),
            "_gdbm_contains" => gdbm_contains(args),
            "_gdbm_len" => gdbm_len(instance),
            "_gdbm_bool" => gdbm_bool(instance),
            "_gdbm_getitem" => {
                let key_value = args.get(1).copied().unwrap_or(NONE);
                gdbm_get(instance, key_value, None)
            }
            "_gdbm_get" => {
                let given = args.len().saturating_sub(1);
                if given < 1 {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "get expected at least 1 argument, got 0",
                    );
                }
                if given > 2 {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("get expected at most 2 arguments, got {given}"),
                    );
                }
                let key_value = args.get(1).copied().unwrap_or(NONE);
                let default = args.get(2).copied().unwrap_or(NONE);
                gdbm_get(instance, key_value, Some(default))
            }
            "_gdbm_setdefault" => gdbm_setdefault(args),
            "_gdbm_setitem" => gdbm_setitem(args),
            "_gdbm_delitem" => gdbm_delitem(args),
            "_gdbm_enter" => instance,
            "_gdbm_exit" => gdbm_close_method(instance),
            _ => missing_attribute(GDBM_MODULE, function),
        }
    }

    fn ndbm_call(function: &str, args: &[i64]) -> i64 {
        let instance = args.first().copied().unwrap_or(NONE);
        match function {
            "open" => dbm_open_call(args),
            "_dbm_close" => ndbm_close_method(instance),
            "_dbm_keys" => ndbm_keys(instance),
            "_dbm_clear" => ndbm_clear(instance),
            "_dbm_contains" => ndbm_contains(args),
            "_dbm_len" => ndbm_len(instance),
            "_dbm_bool" => ndbm_bool(instance),
            "_dbm_getitem" => ndbm_getitem(args),
            "_dbm_get" => {
                let given = args.len().saturating_sub(1);
                if given < 1 {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "get() takes at least 1 positional argument (0 given)",
                    );
                }
                if given > 2 {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("get() takes at most 2 arguments ({given} given)"),
                    );
                }
                let key_value = args.get(1).copied().unwrap_or(NONE);
                let default = args.get(2).copied().unwrap_or(NONE);
                ndbm_get(instance, key_value, default)
            }
            "_dbm_setdefault" => ndbm_setdefault(args),
            "_dbm_setitem" => ndbm_setitem(args),
            "_dbm_delitem" => ndbm_delitem(args),
            "_dbm_enter" => instance,
            "_dbm_exit" => ndbm_close_method(instance),
            _ => missing_attribute(DBM_MODULE, function),
        }
    }

    /// Dispatches `_gdbm.*` and `_dbm.*`, including the native method bodies
    /// installed on `_gdbm.gdbm` and `_dbm.dbm`.
    pub(crate) fn call(module: &str, function: &str, args: &[i64]) -> i64 {
        let bare = function.rsplit('.').next().unwrap_or(function);
        match module {
            GDBM_MODULE => gdbm_call(bare, args),
            DBM_MODULE => ndbm_call(bare, args),
            _ => missing_attribute(module, function),
        }
    }

    /// Class-valued module members (`error`).
    pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
        match (module, name) {
            (GDBM_MODULE | DBM_MODULE, "error") => Some(error_class(module)),
            _ => None,
        }
    }

    /// Materialises a module constant (or a class-valued member).
    pub(crate) fn constant(module: &str, name: &str) -> i64 {
        if let Some(class) = class_value(module, name) {
            return class;
        }
        match (module, name) {
            // `open_flags` reflects the suffixes this gdbm build accepts.
            (GDBM_MODULE, "open_flags") => value::str_alloc("rwcnfsu".to_string()),
            (GDBM_MODULE, "_GDBM_VERSION") => {
                // SAFETY: `gdbm_version_number` is a three-element constant
                // array exported by libgdbm.
                let version = unsafe { gdbm_version_number };
                value::tuple_new(vec![
                    integer(i64::from(version[0])),
                    integer(i64::from(version[1])),
                    integer(i64::from(version[2])),
                ])
            }
            (DBM_MODULE, "library") => value::str_alloc("GNU gdbm".to_string()),
            _ => missing_attribute(module, name),
        }
    }

    // ---------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::value::TRUE;

        fn int(n: i64) -> i64 {
            value::encode_int(n).expect("representable integer")
        }

        fn text(value_arg: &str) -> i64 {
            value::str_alloc(value_arg.to_string())
        }

        fn bytes_arg(data: &[u8]) -> i64 {
            value::bytes_new(data.to_vec())
        }

        fn contents(value_arg: i64) -> Vec<u8> {
            value::as_bytes(value_arg).expect("bytes result").to_vec()
        }

        fn pending_message() -> String {
            value::to_string(exc::pending_value())
        }

        fn call(module: &str, function: &str, args: &[i64]) -> i64 {
            super::call(module, function, args)
        }

        fn scratch(tag: &str) -> String {
            let path = std::env::temp_dir().join(format!(
                "hydro-dbm-{}-{tag}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create scratch directory");
            path.to_string_lossy().into_owned()
        }

        fn cleanup(directory: &str) {
            let _ = std::fs::remove_dir_all(directory);
        }

        #[test]
        fn gdbm_inserts_reads_iterates_deletes_and_persists() {
            exc::init();
            let directory = scratch("gdbm-roundtrip");
            let path = format!("{directory}/data");
            let db = call("_gdbm", "open", &[text(&path), text("c")]);
            assert!(!exc::pending(), "open failed: {}", pending_message());
            assert_eq!(call("_gdbm", "_gdbm_len", &[db]), int(0));

            call(
                "_gdbm",
                "_gdbm_setitem",
                &[db, bytes_arg(b"alpha"), bytes_arg(b"1")],
            );
            call("_gdbm", "_gdbm_setitem", &[db, text("beta"), text("2")]);
            assert!(!exc::pending(), "store failed: {}", pending_message());
            assert_eq!(call("_gdbm", "_gdbm_len", &[db]), int(2));
            assert_eq!(call("_gdbm", "_gdbm_contains", &[db, text("alpha")]), TRUE);
            assert_eq!(
                contents(call("_gdbm", "_gdbm_getitem", &[db, bytes_arg(b"beta")])),
                b"2".to_vec()
            );
            assert_eq!(
                contents(call(
                    "_gdbm",
                    "_gdbm_get",
                    &[db, bytes_arg(b"missing"), bytes_arg(b"fallback")]
                )),
                b"fallback".to_vec()
            );

            let keys = call("_gdbm", "_gdbm_keys", &[db]);
            assert!(!exc::pending(), "keys failed: {}", pending_message());
            let mut listed: Vec<Vec<u8>> = value::as_list(keys)
                .expect("keys() returns a list")
                .iter()
                .map(|&key| contents(key))
                .collect();
            listed.sort();
            assert_eq!(listed, vec![b"alpha".to_vec(), b"beta".to_vec()]);

            let mut traversed = Vec::new();
            let mut key = call("_gdbm", "_gdbm_firstkey", &[db]);
            while key != NONE {
                traversed.push(contents(key));
                key = call("_gdbm", "_gdbm_nextkey", &[db, key]);
            }
            traversed.sort();
            assert_eq!(traversed, vec![b"alpha".to_vec(), b"beta".to_vec()]);

            call("_gdbm", "_gdbm_delitem", &[db, bytes_arg(b"alpha")]);
            assert!(!exc::pending(), "delete failed: {}", pending_message());
            assert_eq!(call("_gdbm", "_gdbm_len", &[db]), int(1));

            let _missing = call("_gdbm", "_gdbm_getitem", &[db, bytes_arg(b"alpha")]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "b'alpha'");
            exc::clear();

            call("_gdbm", "_gdbm_close", &[db]);
            assert!(!exc::pending());
            // Closing twice is a no-op.
            call("_gdbm", "_gdbm_close", &[db]);
            assert!(!exc::pending());
            let _closed = call("_gdbm", "_gdbm_keys", &[db]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "GDBM object has already been closed");
            exc::clear();

            // Persistence: a fresh reader sees the surviving key.
            let reopened = call("_gdbm", "open", &[text(&path), text("r")]);
            assert!(!exc::pending(), "reopen failed: {}", pending_message());
            assert_eq!(call("_gdbm", "_gdbm_len", &[reopened]), int(1));
            assert_eq!(
                contents(call(
                    "_gdbm",
                    "_gdbm_getitem",
                    &[reopened, bytes_arg(b"beta")]
                )),
                b"2".to_vec()
            );
            call("_gdbm", "_gdbm_close", &[reopened]);
            assert!(!exc::pending());
            cleanup(&directory);
        }

        #[test]
        fn gdbm_setdefault_reorganize_sync_and_reader_errors() {
            exc::init();
            let directory = scratch("gdbm-extras");
            let path = format!("{directory}/data");
            let db = call("_gdbm", "open", &[text(&path), text("c")]);
            assert!(!exc::pending(), "open failed: {}", pending_message());

            assert_eq!(
                contents(call(
                    "_gdbm",
                    "_gdbm_setdefault",
                    &[db, bytes_arg(b"new"), bytes_arg(b"value")]
                )),
                b"value".to_vec()
            );
            assert_eq!(call("_gdbm", "_gdbm_len", &[db]), int(1));
            assert_eq!(
                contents(call(
                    "_gdbm",
                    "_gdbm_setdefault",
                    &[db, bytes_arg(b"new"), bytes_arg(b"other")]
                )),
                b"value".to_vec()
            );
            assert!(!exc::pending());
            call("_gdbm", "_gdbm_sync", &[db]);
            assert!(!exc::pending());
            call("_gdbm", "_gdbm_reorganize", &[db]);
            assert!(!exc::pending());
            call("_gdbm", "_gdbm_clear", &[db]);
            assert!(!exc::pending());
            assert_eq!(call("_gdbm", "_gdbm_len", &[db]), int(0));

            call(
                "_gdbm",
                "_gdbm_setitem",
                &[db, bytes_arg(b"keep"), bytes_arg(b"1")],
            );
            call("_gdbm", "_gdbm_close", &[db]);
            assert!(!exc::pending());

            let reader = call("_gdbm", "open", &[text(&path), text("r")]);
            assert!(!exc::pending(), "open reader: {}", pending_message());
            let _stored = call(
                "_gdbm",
                "_gdbm_setitem",
                &[reader, bytes_arg(b"x"), bytes_arg(b"y")],
            );
            assert!(exc::pending());
            assert_eq!(pending_message(), "Reader can't store");
            exc::clear();

            let _deleted = call("_gdbm", "_gdbm_delitem", &[reader, bytes_arg(b"keep")]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "Reader can't delete");
            exc::clear();

            let _cleared = call("_gdbm", "_gdbm_clear", &[reader]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "cannot delete item from database");
            exc::clear();

            let _reorganized = call("_gdbm", "_gdbm_reorganize", &[reader]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "Reader can't reorganize");
            exc::clear();

            call("_gdbm", "_gdbm_sync", &[reader]);
            assert!(!exc::pending());
            call("_gdbm", "_gdbm_close", &[reader]);
            assert!(!exc::pending());
            cleanup(&directory);
        }

        #[test]
        fn gdbm_open_flag_validation_matches_cpython() {
            exc::init();
            let directory = scratch("gdbm-flags");
            let path = format!("{directory}/data");
            let db = call("_gdbm", "open", &[text(&path), text("cfu")]);
            assert!(!exc::pending(), "open failed: {}", pending_message());
            call("_gdbm", "_gdbm_close", &[db]);
            assert!(!exc::pending());

            let _bad = call("_gdbm", "open", &[text(&path), text("cw")]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "Flag 'w' is not supported.");
            exc::clear();

            let _bad = call("_gdbm", "open", &[text(&path), text("x")]);
            assert!(exc::pending());
            assert_eq!(
                pending_message(),
                "First flag must be one of 'r', 'w', 'c' or 'n'"
            );
            exc::clear();

            let _bad = call("_gdbm", "open", &[text(&path), bytes_arg(b"c")]);
            assert!(exc::pending());
            assert_eq!(
                pending_message(),
                "open() argument 2 must be str, not bytes"
            );
            exc::clear();

            let _missing = call("_gdbm", "open", &[text(&format!("{directory}/missing"))]);
            assert!(exc::pending());
            assert!(pending_message().starts_with("[Errno 2] No such file"));
            exc::clear();
            cleanup(&directory);
        }

        #[test]
        fn dbm_inserts_reads_iterates_deletes_and_persists() {
            exc::init();
            let directory = scratch("dbm-roundtrip");
            let path = format!("{directory}/data");
            let db = call("_dbm", "open", &[text(&path), text("c")]);
            assert!(!exc::pending(), "open failed: {}", pending_message());
            assert_eq!(call("_dbm", "_dbm_len", &[db]), int(0));

            call(
                "_dbm",
                "_dbm_setitem",
                &[db, bytes_arg(b"alpha"), bytes_arg(b"1")],
            );
            call("_dbm", "_dbm_setitem", &[db, text("beta"), text("2")]);
            assert!(!exc::pending(), "store failed: {}", pending_message());
            assert_eq!(call("_dbm", "_dbm_len", &[db]), int(2));
            assert_eq!(call("_dbm", "_dbm_contains", &[db, text("alpha")]), TRUE);
            assert_eq!(
                contents(call("_dbm", "_dbm_getitem", &[db, bytes_arg(b"beta")])),
                b"2".to_vec()
            );
            assert_eq!(
                contents(call(
                    "_dbm",
                    "_dbm_get",
                    &[db, bytes_arg(b"missing"), bytes_arg(b"fallback")]
                )),
                b"fallback".to_vec()
            );

            let keys = call("_dbm", "_dbm_keys", &[db]);
            assert!(!exc::pending(), "keys failed: {}", pending_message());
            let mut listed: Vec<Vec<u8>> = value::as_list(keys)
                .expect("keys() returns a list")
                .iter()
                .map(|&key| contents(key))
                .collect();
            listed.sort();
            assert_eq!(listed, vec![b"alpha".to_vec(), b"beta".to_vec()]);

            call("_dbm", "_dbm_delitem", &[db, bytes_arg(b"alpha")]);
            assert!(!exc::pending(), "delete failed: {}", pending_message());
            assert_eq!(call("_dbm", "_dbm_len", &[db]), int(1));

            let _missing = call("_dbm", "_dbm_getitem", &[db, bytes_arg(b"alpha")]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "b'alpha'");
            exc::clear();

            call("_dbm", "_dbm_close", &[db]);
            assert!(!exc::pending());
            call("_dbm", "_dbm_close", &[db]);
            assert!(!exc::pending());
            let _closed = call("_dbm", "_dbm_keys", &[db]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "DBM object has already been closed");
            exc::clear();

            let reopened = call("_dbm", "open", &[text(&path), text("r")]);
            assert!(!exc::pending(), "reopen failed: {}", pending_message());
            assert_eq!(call("_dbm", "_dbm_len", &[reopened]), int(1));
            assert_eq!(
                contents(call(
                    "_dbm",
                    "_dbm_getitem",
                    &[reopened, bytes_arg(b"beta")]
                )),
                b"2".to_vec()
            );
            call("_dbm", "_dbm_close", &[reopened]);
            assert!(!exc::pending());
            cleanup(&directory);
        }

        #[test]
        fn dbm_setdefault_returns_the_default_object_and_preserves_cached_len() {
            exc::init();
            let directory = scratch("dbm-setdefault");
            let path = format!("{directory}/data");
            let db = call("_dbm", "open", &[text(&path), text("c")]);
            assert!(!exc::pending(), "open failed: {}", pending_message());
            assert_eq!(call("_dbm", "_dbm_len", &[db]), int(0));

            let returned = call(
                "_dbm",
                "_dbm_setdefault",
                &[db, bytes_arg(b"text"), text("value")],
            );
            assert!(!exc::pending(), "setdefault failed: {}", pending_message());
            // CPython returns the original default object, so a str stays str.
            assert_eq!(value::as_str(returned), Some("value"));
            // ... and does not invalidate the cached length (CPython quirk).
            assert_eq!(call("_dbm", "_dbm_len", &[db]), int(0));

            let _omitted = call("_dbm", "_dbm_setdefault", &[db, bytes_arg(b"other")]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "cannot add item to database");
            exc::clear();
            call("_dbm", "_dbm_close", &[db]);
            assert!(!exc::pending());

            // A fresh handle recomputes and sees the stored entry.
            let reopened = call("_dbm", "open", &[text(&path), text("c")]);
            assert_eq!(call("_dbm", "_dbm_len", &[reopened]), int(1));
            assert_eq!(
                contents(call(
                    "_dbm",
                    "_dbm_getitem",
                    &[reopened, bytes_arg(b"text")]
                )),
                b"value".to_vec()
            );
            call("_dbm", "_dbm_close", &[reopened]);
            cleanup(&directory);
        }

        #[test]
        fn dbm_reader_rejects_mutation_and_open_flags() {
            exc::init();
            let directory = scratch("dbm-reader");
            let path = format!("{directory}/data");
            let db = call("_dbm", "open", &[text(&path), text("c")]);
            call(
                "_dbm",
                "_dbm_setitem",
                &[db, bytes_arg(b"keep"), bytes_arg(b"1")],
            );
            call("_dbm", "_dbm_close", &[db]);
            assert!(!exc::pending());

            let reader = call("_dbm", "open", &[text(&path), text("r")]);
            assert!(!exc::pending(), "open reader: {}", pending_message());
            assert_eq!(call("_dbm", "_dbm_len", &[reader]), int(1));

            let _stored = call(
                "_dbm",
                "_dbm_setitem",
                &[reader, bytes_arg(b"x"), bytes_arg(b"y")],
            );
            assert!(exc::pending());
            assert_eq!(pending_message(), "cannot add item to database");
            exc::clear();

            let _deleted = call("_dbm", "_dbm_delitem", &[reader, bytes_arg(b"keep")]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "cannot delete item from database");
            exc::clear();

            let _cleared = call("_dbm", "_dbm_clear", &[reader]);
            assert!(exc::pending());
            assert_eq!(pending_message(), "cannot delete item from database");
            exc::clear();
            call("_dbm", "_dbm_close", &[reader]);
            assert!(!exc::pending());

            let _bad = call("_dbm", "open", &[text(&path), text("cfu")]);
            assert!(exc::pending());
            assert_eq!(
                pending_message(),
                "arg 2 to open should be 'r', 'w', 'c', or 'n'"
            );
            exc::clear();

            // The legacy `rw` spelling is accepted by `_dbm`.
            let legacy = call("_dbm", "open", &[text(&path), text("rw")]);
            assert!(!exc::pending(), "legacy open: {}", pending_message());
            call("_dbm", "_dbm_close", &[legacy]);
            assert!(!exc::pending());
            cleanup(&directory);
        }

        #[test]
        fn module_constants_reflect_the_system_libraries() {
            exc::init();
            let flags = constant("_gdbm", "open_flags");
            assert_eq!(value::as_str(flags), Some("rwcnfsu"));
            let library = constant("_dbm", "library");
            assert_eq!(value::as_str(library), Some("GNU gdbm"));
            let version = constant("_gdbm", "_GDBM_VERSION");
            let parts = value::as_tuple(version).expect("version tuple");
            assert_eq!(parts.len(), 3);
            assert!(value::as_integer(parts[0]).unwrap_or(0) >= 1);
            let error = constant("_gdbm", "error");
            assert!(value::as_class(error).is_some());
        }
    }
}

#[cfg(unix)]
pub(crate) use imp::{call, class_value, constant};

#[cfg(not(unix))]
mod imp {
    use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, RUNTIME_ERROR};
    use crate::value;

    pub(crate) fn class_value(_module: &str, _name: &str) -> Option<i64> {
        None
    }

    pub(crate) fn constant(module: &str, name: &str) -> i64 {
        match (module, name) {
            ("_gdbm", "open_flags") => value::str_alloc("rwcnfsu".to_string()),
            ("_dbm", "library") => value::str_alloc(String::new()),
            ("_gdbm" | "_dbm", "error") => exc::exception_class(OS_ERROR),
            _ => exc::raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("module '{module}' has no attribute '{name}'"),
            ),
        }
    }

    pub(crate) fn call(module: &str, function: &str, _args: &[i64]) -> i64 {
        exc::raise_with_message(
            RUNTIME_ERROR,
            &format!("{module}.{function} is not available on this platform"),
        )
    }
}
