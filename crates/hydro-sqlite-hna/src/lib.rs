//! HNA v1 pilot binding for `libsqlite3` (module `hna_sqlite`).
//!
//! This is a *handwritten* binding, deliberately not generated: it exercises
//! the parts of the HNA surface a header-only generator cannot express —
//! opaque handles, process-global state, C error strings and `str` arguments
//! copied through `hydro_str_copy`.
//!
//! Surface (`docs/NATIVE_BINDINGS.md` §5.4, NB-3):
//!
//! - `open(path) -> int` opens a database and returns an opaque handle,
//! - `exec(handle, sql) -> None` runs one SQL statement,
//! - `changes(handle) -> int` rows changed by the most recent statement,
//! - `total_changes(handle) -> int` rows changed since the connection opened,
//! - `last_insert_rowid(handle) -> int` rowid of the most recent insert,
//! - `close(handle) -> None` closes and invalidates the handle.
//!
//! Handles are indices into a process-wide `Mutex<HashMap<u64, *mut sqlite3>>`.
//! The map owns the only pointer to each `sqlite3`; a Hydro value is never
//! interpreted as a pointer, and an unknown or closed handle raises
//! `ValueError` before any C call. The caller never sees a raw address.
//!
//! `thread_safe = false` in the descriptor: the runtime serializes calls into
//! the module. The mutex protects the handle table anyway, and is held for the
//! duration of each C call so a concurrent `close` cannot free a database
//! another call is using.
//!
//! The binding links `libsqlite3` only. It never links `libpython`.

#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_char, c_int, c_void};
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use hydro_hna::{HydroFunction, HydroNativeModule, HydroValue, HNA_VERSION, RUNTIME_ABI};

/// HNA build counter reported by `hydro_native_abi_build`.
const BUILD: u32 = 1;

/// Opaque `sqlite3` connection. The binding only ever passes pointers of this
/// type straight back to libsqlite3; it never dereferences them.
#[repr(C)]
struct Sqlite3 {
    _private: [u8; 0],
}

#[link(name = "sqlite3")]
unsafe extern "C" {
    fn sqlite3_open(filename: *const c_char, db: *mut *mut Sqlite3) -> c_int;
    fn sqlite3_close(db: *mut Sqlite3) -> c_int;
    fn sqlite3_exec(
        db: *mut Sqlite3,
        sql: *const c_char,
        callback: *const c_void,
        context: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_errmsg(db: *mut Sqlite3) -> *const c_char;
    fn sqlite3_changes(db: *mut Sqlite3) -> c_int;
    fn sqlite3_total_changes(db: *mut Sqlite3) -> c_int;
    fn sqlite3_last_insert_rowid(db: *mut Sqlite3) -> i64;
}

// HNA runtime services, resolved from the loading process. The binding never
// defines or links these itself.
unsafe extern "C" {
    fn hydro_none() -> HydroValue;
    fn hydro_int(v: HydroValue) -> HydroValue;
    fn hydro_int_value(value: HydroValue, out: *mut i64) -> i32;
    fn hydro_str_copy(value: HydroValue, buffer: *mut u8, capacity: u64, needed: *mut u64) -> i32;
    fn hydro_raise_type_error(message: *const u8, len: usize) -> HydroValue;
    fn hydro_raise_value_error(message: *const u8, len: usize) -> HydroValue;
}

fn value_error(message: &[u8]) -> HydroValue {
    // SAFETY: `message` is a valid byte slice; the runtime copies it.
    unsafe { hydro_raise_value_error(message.as_ptr(), message.len()) }
}

fn type_error(message: &[u8]) -> HydroValue {
    // SAFETY: `message` is a valid byte slice; the runtime copies it.
    unsafe { hydro_raise_type_error(message.as_ptr(), message.len()) }
}

/// `function(): detail`, the diagnostic shape used for every failure.
fn failure(function: &str, detail: &str) -> String {
    format!("{function}(): {detail}")
}

/// Validates the call arity defensively. The loader enforces min/max first,
/// but a binding must never read past `argv` even if reached another way.
unsafe fn arguments<'a>(
    argc: u64,
    argv: *const HydroValue,
    expected: usize,
    function: &str,
) -> Result<&'a [HydroValue], HydroValue> {
    if argc != expected as u64 {
        let message = format!("{function}() requires exactly {expected} argument(s)");
        return Err(type_error(message.as_bytes()));
    }
    if expected == 0 {
        return Ok(&[]);
    }
    if argv.is_null() {
        return Err(type_error(b"null argument vector"));
    }
    // SAFETY: `argv` points at `argc` readable tagged values by contract.
    Ok(unsafe { core::slice::from_raw_parts(argv, expected) })
}

/// Copies a Hydro `str` into a NUL-terminated byte buffer via
/// `hydro_str_copy`. A non-str leaves a pending `TypeError`, in which case the
/// caller returns the `None` immediate.
unsafe fn str_arg(value: HydroValue, function: &str, param: &str) -> Result<Vec<u8>, HydroValue> {
    let mut needed = 0u64;
    // SAFETY: a NULL probe buffer with capacity 0 is the documented query.
    let probe = unsafe { hydro_str_copy(value, ptr::null_mut(), 0, &mut needed) };
    if probe == 0 {
        // SAFETY: the projection failed and set a pending TypeError.
        return Err(unsafe { hydro_none() });
    }
    if needed > usize::MAX as u64 {
        return Err(value_error(
            failure(function, "string argument is too large").as_bytes(),
        ));
    }
    let mut buffer = vec![0u8; needed as usize];
    if needed > 0 {
        let mut copied_needed = needed;
        // SAFETY: `buffer` has `capacity == needed` writable bytes.
        let copied =
            unsafe { hydro_str_copy(value, buffer.as_mut_ptr(), needed, &mut copied_needed) };
        if copied != 1 {
            // SAFETY: either a type error (already pending) or a short copy.
            return Err(unsafe { hydro_none() });
        }
    }
    if buffer.contains(&0) {
        let detail = format!("argument '{param}' contains a NUL byte");
        return Err(value_error(failure(function, &detail).as_bytes()));
    }
    buffer.push(0);
    Ok(buffer)
}

/// Validates a Hydro `int` argument. Failure leaves a pending `TypeError`.
unsafe fn int_arg(value: HydroValue, function: &str, param: &str) -> Result<i64, HydroValue> {
    let mut out = 0i64;
    // SAFETY: `out` is a valid i64 destination.
    if unsafe { hydro_int_value(value, &mut out) } == 0 {
        // SAFETY: the projection failed and set a pending TypeError.
        return Err(unsafe { hydro_none() });
    }
    let _ = (function, param);
    Ok(out)
}

/// Copies a NUL-terminated C string into an owned Rust `String`.
unsafe fn c_message(message: *const c_char) -> String {
    if message.is_null() {
        return "no error message".to_owned();
    }
    let mut len = 0usize;
    // SAFETY: libsqlite3 returns a NUL-terminated string valid until the next
    // call on the same connection. We copy it before returning.
    while unsafe { *message.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the bytes up to the NUL are readable.
    let bytes = unsafe { core::slice::from_raw_parts(message as *const u8, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Handle counter. Zero is never assigned, so it stays an obvious sentinel.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

/// One open connection. The newtype lets the table be `Send` (and therefore
/// live in a `static Mutex`) while keeping the pointer typed.
#[derive(Clone, Copy)]
struct Connection(*mut Sqlite3);

// SAFETY: the pointer is only dereferenced while the handle-table mutex is
// held, and the entry is removed before `sqlite3_close` is called. No other
// code duplicates the pointer.
unsafe impl Send for Connection {}

/// Process-wide handle table: hydro values never address this directly.
static DATABASES: Mutex<Option<HashMap<u64, Connection>>> = Mutex::new(None);

fn lock_databases() -> MutexGuard<'static, Option<HashMap<u64, Connection>>> {
    DATABASES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn invalid_handle(function: &str, handle: i64) -> HydroValue {
    value_error(failure(function, &format!("invalid sqlite3 handle {handle}")).as_bytes())
}

/// Runs `body` with the live connection for `handle`, holding the handle table
/// lock for the whole call. Unknown and closed handles raise `ValueError`
/// without touching a pointer.
fn with_database<R>(
    function: &str,
    handle: i64,
    body: impl FnOnce(*mut Sqlite3) -> R,
) -> Result<R, HydroValue> {
    if handle <= 0 {
        return Err(invalid_handle(function, handle));
    }
    let mut guard = lock_databases();
    let Some(map) = guard.as_mut() else {
        return Err(invalid_handle(function, handle));
    };
    let Some(&Connection(database)) = map.get(&(handle as u64)) else {
        return Err(invalid_handle(function, handle));
    };
    Ok(body(database))
}

unsafe extern "C" fn open_call(
    _self: HydroValue,
    argc: u64,
    argv: *const HydroValue,
) -> HydroValue {
    let args = match unsafe { arguments(argc, argv, 1, "open") } {
        Ok(args) => args,
        Err(value) => return value,
    };
    let path = match unsafe { str_arg(args[0], "open", "path") } {
        Ok(path) => path,
        Err(value) => return value,
    };

    let mut database: *mut Sqlite3 = ptr::null_mut();
    // SAFETY: `path` is NUL-terminated and outlives the call; `database` is a
    // valid out-pointer. libsqlite3 may set it even when it returns nonzero.
    let rc = unsafe { sqlite3_open(path.as_ptr() as *const c_char, &mut database) };
    if rc != 0 {
        let detail = if database.is_null() {
            format!("SQLite error {rc}")
        } else {
            // SAFETY: `database` is the connection libsqlite3 just created.
            let message = unsafe { c_message(sqlite3_errmsg(database)) };
            // SAFETY: closing the failed-open connection is required by the
            // libsqlite3 documentation.
            unsafe { sqlite3_close(database) };
            format!("SQLite error {rc}: {message}")
        };
        return value_error(failure("open", &detail).as_bytes());
    }
    if database.is_null() {
        return value_error(failure("open", "libsqlite3 returned a null connection").as_bytes());
    }

    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    if handle > i64::MAX as u64 {
        // SAFETY: no table entry exists yet; close the connection directly.
        unsafe { sqlite3_close(database) };
        return value_error(b"open(): handle space exhausted");
    }
    {
        let mut guard = lock_databases();
        let _ = guard
            .get_or_insert_with(HashMap::new)
            .insert(handle, Connection(database));
    }
    // SAFETY: small positive integer; `hydro_int` tags it.
    unsafe { hydro_int(handle as i64) }
}

unsafe extern "C" fn exec_call(
    _self: HydroValue,
    argc: u64,
    argv: *const HydroValue,
) -> HydroValue {
    let args = match unsafe { arguments(argc, argv, 2, "exec") } {
        Ok(args) => args,
        Err(value) => return value,
    };
    let handle = match unsafe { int_arg(args[0], "exec", "handle") } {
        Ok(handle) => handle,
        Err(value) => return value,
    };
    let sql = match unsafe { str_arg(args[1], "exec", "sql") } {
        Ok(sql) => sql,
        Err(value) => return value,
    };

    let outcome = with_database("exec", handle, |database| {
        // SAFETY: `database` came from the validated handle table and is live.
        // A NULL callback and errmsg pointer select the documented mode where
        // the result is reported synchronously through the return code and
        // `sqlite3_errmsg`.
        let rc = unsafe {
            sqlite3_exec(
                database,
                sql.as_ptr() as *const c_char,
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc == 0 {
            None
        } else {
            // SAFETY: `database` is still live and owned by the table.
            Some((rc, unsafe { c_message(sqlite3_errmsg(database)) }))
        }
    });
    match outcome {
        Err(value) => value,
        Ok(None) => unsafe { hydro_none() },
        Ok(Some((rc, message))) => {
            let detail = format!("SQLite error {rc}: {message}");
            value_error(failure("exec", &detail).as_bytes())
        }
    }
}

unsafe extern "C" fn changes_call(
    _self: HydroValue,
    argc: u64,
    argv: *const HydroValue,
) -> HydroValue {
    let args = match unsafe { arguments(argc, argv, 1, "changes") } {
        Ok(args) => args,
        Err(value) => return value,
    };
    let handle = match unsafe { int_arg(args[0], "changes", "handle") } {
        Ok(handle) => handle,
        Err(value) => return value,
    };
    match with_database("changes", handle, |database| {
        // SAFETY: `database` came from the validated handle table.
        unsafe { sqlite3_changes(database) }
    }) {
        // SAFETY: `hydro_int` tags the scalar.
        Ok(value) => unsafe { hydro_int(value as i64) },
        Err(value) => value,
    }
}

unsafe extern "C" fn total_changes_call(
    _self: HydroValue,
    argc: u64,
    argv: *const HydroValue,
) -> HydroValue {
    let args = match unsafe { arguments(argc, argv, 1, "total_changes") } {
        Ok(args) => args,
        Err(value) => return value,
    };
    let handle = match unsafe { int_arg(args[0], "total_changes", "handle") } {
        Ok(handle) => handle,
        Err(value) => return value,
    };
    match with_database("total_changes", handle, |database| {
        // SAFETY: `database` came from the validated handle table.
        unsafe { sqlite3_total_changes(database) }
    }) {
        Ok(value) => unsafe { hydro_int(value as i64) },
        Err(value) => value,
    }
}

unsafe extern "C" fn last_insert_rowid_call(
    _self: HydroValue,
    argc: u64,
    argv: *const HydroValue,
) -> HydroValue {
    let args = match unsafe { arguments(argc, argv, 1, "last_insert_rowid") } {
        Ok(args) => args,
        Err(value) => return value,
    };
    let handle = match unsafe { int_arg(args[0], "last_insert_rowid", "handle") } {
        Ok(handle) => handle,
        Err(value) => return value,
    };
    match with_database("last_insert_rowid", handle, |database| {
        // SAFETY: `database` came from the validated handle table.
        unsafe { sqlite3_last_insert_rowid(database) }
    }) {
        Ok(value) => unsafe { hydro_int(value) },
        Err(value) => value,
    }
}

unsafe extern "C" fn close_call(
    _self: HydroValue,
    argc: u64,
    argv: *const HydroValue,
) -> HydroValue {
    let args = match unsafe { arguments(argc, argv, 1, "close") } {
        Ok(args) => args,
        Err(value) => return value,
    };
    let handle = match unsafe { int_arg(args[0], "close", "handle") } {
        Ok(handle) => handle,
        Err(value) => return value,
    };
    if handle <= 0 {
        return invalid_handle("close", handle);
    }

    let database = {
        let mut guard = lock_databases();
        match guard.as_mut().and_then(|map| map.remove(&(handle as u64))) {
            Some(Connection(database)) => database,
            None => return invalid_handle("close", handle),
        }
    };

    // SAFETY: the table entry was removed, so this is the only reference and
    // the connection came from libsqlite3.
    let rc = unsafe { sqlite3_close(database) };
    if rc != 0 {
        // libsqlite3 leaves a busy connection open; restore the handle so the
        // caller can finalize and retry rather than leaking the pointer.
        // SAFETY: the connection is still live after a failed close.
        let message = unsafe { c_message(sqlite3_errmsg(database)) };
        {
            let mut guard = lock_databases();
            let _ = guard
                .get_or_insert_with(HashMap::new)
                .insert(handle as u64, Connection(database));
        }
        let detail = format!("SQLite error {rc}: {message}");
        return value_error(failure("close", &detail).as_bytes());
    }
    // SAFETY: `hydro_none` returns the None immediate.
    unsafe { hydro_none() }
}

/// Wrapper that makes the descriptor array usable in a process-lifetime
/// `static` (raw and function pointers are not `Sync` by themselves).
#[repr(transparent)]
struct SyncFunctions([HydroFunction; 6]);
// SAFETY: immutable static data pointing at static strings and functions; the
// descriptor is never mutated.
unsafe impl Sync for SyncFunctions {}

#[repr(transparent)]
struct SyncModule(HydroNativeModule);
// SAFETY: same as `SyncFunctions`; the module only points at the static below.
unsafe impl Sync for SyncModule {}

static FUNCTIONS: SyncFunctions = SyncFunctions([
    HydroFunction {
        name: c"open".as_ptr(),
        min_args: 1,
        max_args: 1,
        flags: 0,
        params: c"path".as_ptr(),
        call: Some(open_call),
    },
    HydroFunction {
        name: c"exec".as_ptr(),
        min_args: 2,
        max_args: 2,
        flags: 0,
        params: c"handle,sql".as_ptr(),
        call: Some(exec_call),
    },
    HydroFunction {
        name: c"changes".as_ptr(),
        min_args: 1,
        max_args: 1,
        flags: 0,
        params: c"handle".as_ptr(),
        call: Some(changes_call),
    },
    HydroFunction {
        name: c"total_changes".as_ptr(),
        min_args: 1,
        max_args: 1,
        flags: 0,
        params: c"handle".as_ptr(),
        call: Some(total_changes_call),
    },
    HydroFunction {
        name: c"last_insert_rowid".as_ptr(),
        min_args: 1,
        max_args: 1,
        flags: 0,
        params: c"handle".as_ptr(),
        call: Some(last_insert_rowid_call),
    },
    HydroFunction {
        name: c"close".as_ptr(),
        min_args: 1,
        max_args: 1,
        flags: 0,
        params: c"handle".as_ptr(),
        call: Some(close_call),
    },
]);

static MODULE: SyncModule = SyncModule(HydroNativeModule {
    struct_size: size_of::<HydroNativeModule>() as u32,
    hna_version: HNA_VERSION,
    runtime_abi_min: RUNTIME_ABI as u64,
    module_name: c"hna_sqlite".as_ptr(),
    provider: c"hydro-sqlite-hna 0.1.0 (libsqlite3)".as_ptr(),
    platform: c"linux-x86_64".as_ptr(),
    // thread_safe = false: the runtime serializes calls into this module.
    flags: 0,
    init: None,
    function_count: 6,
    functions: &FUNCTIONS.0 as *const [HydroFunction; 6] as *const HydroFunction,
    constant_count: 0,
    constants: ptr::null(),
});

/// HNA entry point: the major version this binding implements.
#[no_mangle]
pub extern "C" fn hydro_native_abi() -> u32 {
    HNA_VERSION
}

/// HNA entry point: build counter for reports.
#[no_mangle]
pub extern "C" fn hydro_native_abi_build() -> u32 {
    BUILD
}

/// HNA entry point: the static, read-only module descriptor.
#[no_mangle]
pub extern "C" fn hydro_native_module_v1() -> *const HydroNativeModule {
    &MODULE.0 as *const HydroNativeModule
}
