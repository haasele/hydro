//! `sqlite3`: the native DB-API 2.0 surface backed by `libsqlite3`.
//!
//! `sqlite3.connect(...)` (alias `_sqlite3.connect`) opens a real SQLite
//! connection through `sqlite3_open_v2` and returns a `sqlite3.Connection`
//! instance. The connection owns the `sqlite3*` handle and a statement
//! registry so `close()` can finalize every live cursor before
//! `sqlite3_close_v2` runs. `Connection.cursor(...)` returns a
//! `sqlite3.Cursor` bound to the same handle; `Connection.execute` /
//! `executemany` create a cursor, run the statement and return it, matching
//! CPython.
//!
//! Python values map onto SQLite storage classes exactly like CPython's
//! `_sqlite3`: `None`<->NULL, `int`/`bool`<->INTEGER, `float`<->REAL,
//! `str`<->TEXT (UTF-8) and `bytes`/`bytearray`<->BLOB. Parameters are bound
//! by position (`?`) or by name (`:name`, `$name`, `@name`) from a tuple,
//! list or dict, with CPython's "Incorrect number of bindings" and
//! "did not supply a value" diagnostics. Queries must contain a single
//! statement (the prepare tail is checked, like CPython).
//!
//! Transactions follow the legacy DB-API behaviour: with a non-`None`
//! `isolation_level` a DML statement (`INSERT`/`UPDATE`/`DELETE`/`REPLACE`)
//! implicitly begins `BEGIN <level>` when SQLite is in autocommit mode.
//! `commit`, `rollback` and the context manager (`__enter__`/`__exit__`)
//! drive the transaction explicitly. `in_transaction` mirrors
//! `sqlite3_get_autocommit` after every state change.
//!
//! `Connection.create_function(name, narg, func[, deterministic])` is a real
//! scalar UDF registration: the Python callable is invoked from the SQLite
//! callback in the caller's thread and its result is copied back into SQLite.
//! A callback that raises surfaces as CPython's
//! `OperationalError("user-defined function raised exception")`.
//!
//! `detect_types` (decltype/colname converters) and per-call `factory`
//! policies other than `sqlite3.Connection`/`sqlite3.Cursor` subclasses are
//! not implemented; both report `sqlite3.NotSupportedError` instead of
//! silently returning unconverted or unattached values.
//!
//! Wiring (the orchestrator applies these outside this file):
//! * `hydro_ir::stdlib::specs`: `SQLITE3` and `_SQLITE3` `ModuleSpec`s with
//!   `kw_dynamic("connect", 0, 8, Type::Any, &[])` (0 so a keyword-only
//!   `database=` call reaches the runtime binder), the class constructors
//!   `f("Connection", 1, 8, Type::Any)` / `f("Cursor", 1, 1, Type::Any)`,
//!   `f("complete_statement", 1, 1, Type::Bool)`, and the constants
//!   materialised by [`constant`]; both specs registered in
//!   `hydro_ir::stdlib::MODULES`.
//! * `modules/mod.rs`: `pub(crate) mod sqlite3_native;`, dispatch arms
//!   `"sqlite3" | "_sqlite3" => sqlite3_native::call(function, args)`, the
//!   keyword arm `"sqlite3" => sqlite3_native::call_kw(...)`, the constant arm
//!   `("sqlite3" | "_sqlite3", name) => sqlite3_native::constant(name)`, a
//!   `sqlite3_native::class_value(module, name)` check in
//!   `module_get_default_named` (so `sqlite3.Connection` reads as the cached
//!   class while calls still lower as module calls, exactly like
//!   `array.array`), and `SUPPLEMENTAL_FUNCTIONS` entries for every qualified
//!   native method name (`sqlite3.Connection.execute`,
//!   `sqlite3.Cursor.fetchone`, ...).
//! * Link flag: the module declares `#[link(name = "sqlite3")]`, so the final
//!   binary links `-lsqlite3` (same pattern as `zlib`'s `-lz`).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::ptr;
use std::sync::{Mutex, MutexGuard};
use std::thread::ThreadId;

use hydro_ir::MethodKind;
use num_traits::ToPrimitive;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, EXCEPTION, MEMORY_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::ops;
use crate::value::{self, FALSE, NONE, TRUE};

const MODULE: &str = "sqlite3";

/// Registry keys of the cached class values.
const CONNECTION_KEY: &str = "sqlite3.Connection.class";
const CURSOR_KEY: &str = "sqlite3.Cursor.class";

/// Hidden instance attribute holding the leaked connection state capsule.
const CONNECTION_ATTR: &str = "\u{0}sqlite3.connection";
/// Hidden instance attribute holding the leaked cursor state capsule.
const CURSOR_ATTR: &str = "\u{0}sqlite3.cursor";
/// Capsule names that tag a handle created by this module.
const CONNECTION_CAPSULE: &str = "hydro.sqlite3.connection";
const CURSOR_CAPSULE: &str = "hydro.sqlite3.cursor";

/// Positional parameter names of `connect`/`Connection.__init__`.
const CONNECT_PARAMS: [&str; 8] = [
    "database",
    "timeout",
    "detect_types",
    "isolation_level",
    "check_same_thread",
    "factory",
    "cached_statements",
    "uri",
];

// -------------------------------------------------------------------------
// libsqlite3 FFI
// -------------------------------------------------------------------------

/// Opaque `sqlite3` connection handle.
#[repr(C)]
struct Sqlite3 {
    _private: [u8; 0],
}

/// Opaque prepared-statement handle.
#[repr(C)]
struct Sqlite3Stmt {
    _private: [u8; 0],
}

/// Opaque SQL function callback context.
#[repr(C)]
struct SqliteContext {
    _private: [u8; 0],
}

/// Opaque SQL function argument value.
#[repr(C)]
struct SqliteValue {
    _private: [u8; 0],
}

/// Destructor passed to the `sqlite3_bind_*` calls (`SQLITE_TRANSIENT`).
type Destructor = Option<unsafe extern "C" fn(*mut c_void)>;
/// Scalar SQL function implementation.
type UdfCallback = unsafe extern "C" fn(*mut SqliteContext, c_int, *mut *mut SqliteValue);
/// Scalar SQL function slot (`sqlite3_create_function_v2` parameter).
type UdfFunction = Option<UdfCallback>;
/// Aggregate step callback (never installed by this module).
type UdfStep = Option<UdfCallback>;
/// Aggregate final callback (never installed by this module).
type UdfFinal = Option<unsafe extern "C" fn(*mut SqliteContext)>;
/// Destructor of a `sqlite3_create_function_v2` application pointer.
type UdfDestroyFn = unsafe extern "C" fn(*mut c_void);
/// Destructor slot.
type UdfDestroy = Option<UdfDestroyFn>;
/// `sqlite3_exec` callback (unused: this module always passes `NULL`).
type ExecCallback =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>;

// Result codes (`sqlite3.h`).
const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_INTERNAL: c_int = 2;
const SQLITE_PERM: c_int = 3;
const SQLITE_ABORT: c_int = 4;
const SQLITE_BUSY: c_int = 5;
const SQLITE_LOCKED: c_int = 6;
const SQLITE_NOMEM: c_int = 7;
const SQLITE_READONLY: c_int = 8;
const SQLITE_INTERRUPT: c_int = 9;
const SQLITE_IOERR: c_int = 10;
const SQLITE_CORRUPT: c_int = 11;
const SQLITE_NOTFOUND: c_int = 12;
const SQLITE_FULL: c_int = 13;
const SQLITE_CANTOPEN: c_int = 14;
const SQLITE_PROTOCOL: c_int = 15;
const SQLITE_EMPTY: c_int = 16;
const SQLITE_SCHEMA: c_int = 17;
const SQLITE_TOOBIG: c_int = 18;
const SQLITE_CONSTRAINT: c_int = 19;
const SQLITE_MISMATCH: c_int = 20;
const SQLITE_MISUSE: c_int = 21;
const SQLITE_RANGE: c_int = 25;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;

// Open flags and value types (`sqlite3.h`).
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
const SQLITE_OPEN_URI: c_int = 0x0000_0040;
const SQLITE_UTF8: c_int = 1;
const SQLITE_DETERMINISTIC: c_int = 0x0000_0800;

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
const SQLITE_NULL: c_int = 5;

#[link(name = "sqlite3")]
extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        database: *mut *mut Sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close_v2(database: *mut Sqlite3) -> c_int;
    fn sqlite3_errmsg(database: *mut Sqlite3) -> *const c_char;
    fn sqlite3_errcode(database: *mut Sqlite3) -> c_int;
    fn sqlite3_busy_timeout(database: *mut Sqlite3, milliseconds: c_int) -> c_int;
    fn sqlite3_prepare_v2(
        database: *mut Sqlite3,
        sql: *const c_char,
        length: c_int,
        statement: *mut *mut Sqlite3Stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_finalize(statement: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_reset(statement: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_clear_bindings(statement: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_step(statement: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_stmt_readonly(statement: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_bind_parameter_count(statement: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_bind_parameter_name(statement: *mut Sqlite3Stmt, index: c_int) -> *const c_char;
    fn sqlite3_bind_null(statement: *mut Sqlite3Stmt, index: c_int) -> c_int;
    fn sqlite3_bind_int64(statement: *mut Sqlite3Stmt, index: c_int, value: i64) -> c_int;
    fn sqlite3_bind_double(statement: *mut Sqlite3Stmt, index: c_int, value: f64) -> c_int;
    fn sqlite3_bind_text(
        statement: *mut Sqlite3Stmt,
        index: c_int,
        text: *const c_char,
        length: c_int,
        destructor: Destructor,
    ) -> c_int;
    fn sqlite3_bind_blob(
        statement: *mut Sqlite3Stmt,
        index: c_int,
        blob: *const c_void,
        length: c_int,
        destructor: Destructor,
    ) -> c_int;
    fn sqlite3_column_count(statement: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_column_name(statement: *mut Sqlite3Stmt, index: c_int) -> *const c_char;
    fn sqlite3_column_type(statement: *mut Sqlite3Stmt, index: c_int) -> c_int;
    fn sqlite3_column_int64(statement: *mut Sqlite3Stmt, index: c_int) -> i64;
    fn sqlite3_column_double(statement: *mut Sqlite3Stmt, index: c_int) -> f64;
    fn sqlite3_column_text(statement: *mut Sqlite3Stmt, index: c_int) -> *const c_uchar;
    fn sqlite3_column_blob(statement: *mut Sqlite3Stmt, index: c_int) -> *const c_void;
    fn sqlite3_column_bytes(statement: *mut Sqlite3Stmt, index: c_int) -> c_int;
    fn sqlite3_changes(database: *mut Sqlite3) -> c_int;
    fn sqlite3_last_insert_rowid(database: *mut Sqlite3) -> i64;
    fn sqlite3_get_autocommit(database: *mut Sqlite3) -> c_int;
    fn sqlite3_exec(
        database: *mut Sqlite3,
        sql: *const c_char,
        callback: ExecCallback,
        argument: *mut c_void,
        error: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_complete(sql: *const c_char) -> c_int;
    fn sqlite3_libversion() -> *const c_char;
    fn sqlite3_libversion_number() -> c_int;
    fn sqlite3_user_data(context: *mut SqliteContext) -> *mut c_void;
    fn sqlite3_value_type(value: *mut SqliteValue) -> c_int;
    fn sqlite3_value_int64(value: *mut SqliteValue) -> i64;
    fn sqlite3_value_double(value: *mut SqliteValue) -> f64;
    fn sqlite3_value_text(value: *mut SqliteValue) -> *const c_uchar;
    fn sqlite3_value_blob(value: *mut SqliteValue) -> *const c_void;
    fn sqlite3_value_bytes(value: *mut SqliteValue) -> c_int;
    fn sqlite3_result_null(context: *mut SqliteContext);
    fn sqlite3_result_int64(context: *mut SqliteContext, value: i64);
    fn sqlite3_result_double(context: *mut SqliteContext, value: f64);
    fn sqlite3_result_text(
        context: *mut SqliteContext,
        text: *const c_char,
        length: c_int,
        destructor: Destructor,
    );
    fn sqlite3_result_blob(
        context: *mut SqliteContext,
        blob: *const c_void,
        length: c_int,
        destructor: Destructor,
    );
    fn sqlite3_result_error(context: *mut SqliteContext, text: *const c_char, length: c_int);
    #[allow(clippy::too_many_arguments)]
    fn sqlite3_create_function_v2(
        database: *mut Sqlite3,
        name: *const c_char,
        nargs: c_int,
        text_rep: c_int,
        application: *mut c_void,
        function: UdfFunction,
        step: UdfStep,
        final_function: UdfFinal,
        destroy: UdfDestroy,
    ) -> c_int;
}

/// `SQLITE_TRANSIENT`: SQLite copies the buffer before returning.
fn transient() -> Destructor {
    // SAFETY: `SQLITE_TRANSIENT` is the documented sentinel `(void(*)(void*))-1`
    // and is never invoked; a function pointer and `isize` have equal size on
    // every supported target.
    unsafe { std::mem::transmute::<isize, Destructor>(-1) }
}

// -------------------------------------------------------------------------
// Error reporting
// -------------------------------------------------------------------------

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn memory_error() -> i64 {
    exc::raise_with_message(MEMORY_ERROR, "out of memory")
}

fn overflow_error(message: &str) -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, message)
}

/// Cached `sqlite3.<name>` exception class.
fn exception_class(name: &str) -> i64 {
    let key = format!("sqlite3.exception.{name}");
    if let Some(class) = globals::registry_get(&key) {
        return class;
    }
    let base = match name {
        "Error" | "Warning" => exc::exception_class(EXCEPTION),
        _ => exception_class("Error"),
    };
    let class = value::class_new(name.to_string(), base, Vec::new());
    globals::registry_set(&key, class);
    class
}

/// Raises `sqlite3.<name>(message)` and returns the `None` dummy value.
fn sqlite_error(name: &str, message: &str) -> i64 {
    let class = exception_class(name);
    let message_value = value::str_alloc(message.to_string());
    let roots = heap::root_values(&[message_value]);
    let instance = crate::builtins::call(class, 1, &message_value as *const i64);
    heap::release_roots(roots);
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

fn programming_error(message: &str) -> i64 {
    sqlite_error("ProgrammingError", message)
}

fn not_supported(message: &str) -> i64 {
    sqlite_error("NotSupportedError", message)
}

/// Reads a NUL-terminated libsqlite3 string, tolerating `NULL`.
fn c_message(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: libsqlite3 returns a NUL-terminated string valid until the next
    // call on the same handle; the bytes are copied immediately.
    unsafe { std::ffi::CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

/// Maps `sqlite3_errcode` onto the DB-API exception hierarchy and raises it.
///
/// The primary code chooses the class (CPython's `get_exception_class`), and
/// the message comes from `sqlite3_errmsg`.
fn raise_db_error(database: *mut Sqlite3) -> i64 {
    if database.is_null() {
        return sqlite_error("OperationalError", "unable to open database file");
    }
    let code = unsafe { sqlite3_errcode(database) };
    let message = c_message(unsafe { sqlite3_errmsg(database) });
    match code {
        SQLITE_OK => NONE,
        SQLITE_NOMEM => memory_error(),
        SQLITE_INTERNAL | SQLITE_NOTFOUND => sqlite_error("InternalError", &message),
        SQLITE_ERROR | SQLITE_PERM | SQLITE_ABORT | SQLITE_BUSY | SQLITE_LOCKED
        | SQLITE_READONLY | SQLITE_INTERRUPT | SQLITE_IOERR | SQLITE_FULL | SQLITE_CANTOPEN
        | SQLITE_PROTOCOL | SQLITE_EMPTY | SQLITE_SCHEMA => {
            sqlite_error("OperationalError", &message)
        }
        SQLITE_CORRUPT => sqlite_error("DatabaseError", &message),
        SQLITE_TOOBIG => sqlite_error("DataError", &message),
        SQLITE_CONSTRAINT | SQLITE_MISMATCH => sqlite_error("IntegrityError", &message),
        SQLITE_MISUSE | SQLITE_RANGE => sqlite_error("InterfaceError", &message),
        _ => sqlite_error("DatabaseError", &message),
    }
}

// -------------------------------------------------------------------------
// Connection and cursor state
// -------------------------------------------------------------------------

/// Live statements and cursors owned by one connection.
///
/// Everything under this mutex is touched only while a session is held, so a
/// connection closed on one thread cannot race a statement stepping on
/// another: the second operation either runs before `close()` or sees
/// `closed`.
struct ConnOps {
    closed: bool,
    statements: Vec<usize>,
    cursors: Vec<usize>,
}

/// A real SQLite connection. Leaked for the process lifetime (like the
/// `memcached` client); `close()` releases the native handle and every
/// statement, so the leak is the small Rust shell only.
struct ConnectionState {
    db: usize,
    thread: ThreadId,
    check_same_thread: bool,
    ops: Mutex<ConnOps>,
}

/// A cursor over one connection. Fields live in `Cell`s and are mutated only
/// while the owning connection's `ops` mutex is held.
struct CursorState {
    conn: &'static ConnectionState,
    statement: Cell<usize>,
    closed: Cell<bool>,
    /// The statement currently has a row available for `fetch*`.
    at_row: Cell<bool>,
    /// The prepared statement is a DML statement (rowcount accumulates).
    dml: Cell<bool>,
}

// Recursion guard: a connection may not re-enter itself from a UDF callback.
// Cross-thread use blocks on the connection mutex instead.
thread_local! {
    static ACTIVE: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

struct OpGuard {
    db: usize,
}

impl OpGuard {
    fn enter(db: usize) -> Option<OpGuard> {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&db) {
                None
            } else {
                active.insert(db);
                Some(OpGuard { db })
            }
        })
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            active.borrow_mut().remove(&self.db);
        });
    }
}

/// One locked connection operation.
struct ConnSession {
    db: usize,
    /// Dropped before `_recursion` so the mutex is free before the thread is
    /// removed from the recursion set.
    ops: MutexGuard<'static, ConnOps>,
    _recursion: OpGuard,
}

impl std::ops::Deref for ConnSession {
    type Target = ConnOps;

    fn deref(&self) -> &ConnOps {
        &self.ops
    }
}

impl std::ops::DerefMut for ConnSession {
    fn deref_mut(&mut self) -> &mut ConnOps {
        &mut self.ops
    }
}

fn lock_ops(state: &'static ConnectionState) -> MutexGuard<'static, ConnOps> {
    state
        .ops
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Raises CPython's cross-thread `ProgrammingError` when it applies.
fn check_thread(state: &'static ConnectionState) -> Option<i64> {
    if !state.check_same_thread {
        return None;
    }
    let current = std::thread::current().id();
    if current == state.thread {
        return None;
    }
    Some(programming_error(&format!(
        "SQLite objects created in a thread can only be used in that same thread. \
         The object was created in thread id {:?} and this is thread id {:?}.",
        state.thread, current
    )))
}

/// Acquires the connection mutex and installs the recursion guard.
fn open_session(state: &'static ConnectionState) -> Result<ConnSession, i64> {
    if let Some(error) = check_thread(state) {
        return Err(error);
    }
    let recursion = OpGuard::enter(state.db)
        .ok_or_else(|| programming_error("Recursive use of cursors not allowed."))?;
    let ops = lock_ops(state);
    if ops.closed {
        return Err(programming_error("Cannot operate on a closed database."));
    }
    Ok(ConnSession {
        db: state.db,
        _recursion: recursion,
        ops,
    })
}

fn open_cursor_session(cursor: &'static CursorState) -> Result<ConnSession, i64> {
    let session = open_session(cursor.conn)?;
    if cursor.closed.get() {
        return Err(programming_error("Cannot operate on a closed cursor."));
    }
    Ok(session)
}

/// Borrows the connection state behind a validated instance.
fn connection_state(instance: i64) -> Option<&'static ConnectionState> {
    let capsule = value::instance_attr(instance, CONNECTION_ATTR)?;
    let (pointer, name) = value::as_capsule(capsule)?;
    if pointer == 0 || name != Some(CONNECTION_CAPSULE) {
        return None;
    }
    // SAFETY: only `init_connection` stores this capsule, and it points at an
    // intentionally leaked `ConnectionState`.
    Some(unsafe { &*(pointer as *const ConnectionState) })
}

/// Borrows the cursor state behind a validated instance.
fn cursor_state(instance: i64) -> Option<&'static CursorState> {
    let capsule = value::instance_attr(instance, CURSOR_ATTR)?;
    let (pointer, name) = value::as_capsule(capsule)?;
    if pointer == 0 || name != Some(CURSOR_CAPSULE) {
        return None;
    }
    // SAFETY: only `init_cursor` stores this capsule, and it points at an
    // intentionally leaked `CursorState`.
    Some(unsafe { &*(pointer as *const CursorState) })
}

/// Finalizes one statement and drops it from the connection registry.
fn finalize_statement(ops: &mut ConnOps, statement: usize) {
    if statement == 0 {
        return;
    }
    // SAFETY: registered statements are live handles created by
    // `sqlite3_prepare_v2` and finalized exactly once.
    unsafe { sqlite3_finalize(statement as *mut Sqlite3Stmt) };
    if let Some(index) = ops.statements.iter().position(|&value| value == statement) {
        ops.statements.remove(index);
    }
}

/// Releases the native handle and marks every cursor closed.
fn close_locked(ops: &mut ConnOps, db: usize) {
    if ops.closed {
        return;
    }
    for statement in ops.statements.drain(..) {
        // SAFETY: as in `finalize_statement`.
        unsafe { sqlite3_finalize(statement as *mut Sqlite3Stmt) };
    }
    for cursor in ops.cursors.drain(..) {
        // SAFETY: the registry holds pointers of leaked `CursorState` boxes.
        let cursor = unsafe { &*(cursor as *const CursorState) };
        cursor.statement.set(0);
        cursor.at_row.set(false);
        cursor.closed.set(true);
    }
    ops.closed = true;
    // SAFETY: `db` came from `sqlite3_open_v2` and is closed once; `_v2`
    // tolerates statements that are still outstanding.
    unsafe { sqlite3_close_v2(db as *mut Sqlite3) };
}

/// Closes a connection outside the normal session path (re-initialisation).
fn close_state(state: &'static ConnectionState) {
    let mut ops = lock_ops(state);
    close_locked(&mut ops, state.db);
}

// -------------------------------------------------------------------------
// Class construction
// -------------------------------------------------------------------------

/// Cached native class through the runtime registry.
fn cached_class(key: &str, build: impl FnOnce() -> i64) -> i64 {
    if let Some(class) = globals::registry_get(key) {
        return class;
    }
    let class = build();
    globals::registry_set(key, class);
    class
}

fn build_class(key: &str, name: &str, specs: &[(&str, &'static str, usize)]) -> i64 {
    cached_class(key, || {
        let mut methods = Vec::with_capacity(specs.len());
        let mut values = Vec::with_capacity(specs.len());
        for (method, native, arity) in specs {
            let function = value::native_function_new(MODULE, native, *arity);
            values.push(function);
            methods.push(((*method).to_string(), function, MethodKind::Normal));
        }
        let method_roots = heap::root_values(&values);
        let class = value::class_new_full(name.to_string(), NONE, methods, Vec::new());
        let class_roots = heap::root_values(std::slice::from_ref(&class));
        heap::release_roots(class_roots);
        heap::release_roots(method_roots);
        class
    })
}

/// Cached `sqlite3.Connection` class.
fn connection_class() -> i64 {
    const SPECS: [(&str, &str, usize); 10] = [
        ("__init__", "sqlite3.Connection.__init__", 9),
        ("cursor", "sqlite3.Connection.cursor", 2),
        ("execute", "sqlite3.Connection.execute", 3),
        ("executemany", "sqlite3.Connection.executemany", 3),
        ("commit", "sqlite3.Connection.commit", 1),
        ("rollback", "sqlite3.Connection.rollback", 1),
        ("close", "sqlite3.Connection.close", 1),
        ("create_function", "sqlite3.Connection.create_function", 4),
        ("__enter__", "sqlite3.Connection.__enter__", 1),
        ("__exit__", "sqlite3.Connection.__exit__", 4),
    ];
    build_class(CONNECTION_KEY, "Connection", &SPECS)
}

/// Cached `sqlite3.Cursor` class.
fn cursor_class() -> i64 {
    const SPECS: [(&str, &str, usize); 9] = [
        ("__init__", "sqlite3.Cursor.__init__", 2),
        ("execute", "sqlite3.Cursor.execute", 3),
        ("executemany", "sqlite3.Cursor.executemany", 3),
        ("fetchone", "sqlite3.Cursor.fetchone", 1),
        ("fetchmany", "sqlite3.Cursor.fetchmany", 2),
        ("fetchall", "sqlite3.Cursor.fetchall", 1),
        ("close", "sqlite3.Cursor.close", 1),
        ("__iter__", "sqlite3.Cursor.__iter__", 1),
        ("__next__", "sqlite3.Cursor.__next__", 1),
    ];
    build_class(CURSOR_KEY, "Cursor", &SPECS)
}

// -------------------------------------------------------------------------
// Argument helpers
// -------------------------------------------------------------------------

fn is_callable(candidate: i64) -> bool {
    value::as_function(candidate).is_some() || value::as_class(candidate).is_some()
}

/// Converts the `database` operand to filesystem bytes (UTF-8).
fn database_argument(value_arg: i64) -> Result<Vec<u8>, i64> {
    if let Some(text) = value::as_str(value_arg) {
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(bytes) = value::as_bytes(value_arg) {
        return Ok(bytes.to_vec());
    }
    Err(type_error(&format!(
        "argument 'database' must be str or bytes, not '{}'",
        value::type_name(value_arg)
    )))
}

fn timeout_argument(value_arg: i64) -> Result<f64, i64> {
    if let Some(number) = value::as_float(value_arg) {
        return Ok(number);
    }
    if let Some(number) = value::as_integer(value_arg) {
        return Ok(number as f64);
    }
    Err(type_error(&format!(
        "argument 'timeout' must be float, not '{}'",
        value::type_name(value_arg)
    )))
}

fn int_argument(value_arg: i64, name: &str) -> Result<i64, i64> {
    value::as_integer(value_arg).ok_or_else(|| {
        type_error(&format!(
            "argument '{name}' must be int, not '{}'",
            value::type_name(value_arg)
        ))
    })
}

/// Canonicalises an isolation level (`""`, `DEFERRED`, `IMMEDIATE`,
/// `EXCLUSIVE`, ASCII case-insensitively, like `sqlite3_stricmp`).
fn canonical_isolation_level(text: &str) -> Result<String, i64> {
    const ALLOWED: [&str; 4] = ["", "DEFERRED", "IMMEDIATE", "EXCLUSIVE"];
    for allowed in ALLOWED {
        if text.eq_ignore_ascii_case(allowed) {
            return Ok(allowed.to_string());
        }
    }
    Err(value_error(
        "isolation_level string must be '', 'DEFERRED', 'IMMEDIATE', or 'EXCLUSIVE'",
    ))
}

fn isolation_level_argument(value_arg: i64) -> Result<Option<String>, i64> {
    if value_arg == NONE {
        return Ok(None);
    }
    match value::as_str(value_arg) {
        Some(text) => canonical_isolation_level(text).map(Some),
        None => Err(type_error("isolation_level must be str or None")),
    }
}

/// The connection's current `isolation_level` attribute.
fn isolation_level(instance: i64) -> Result<Option<String>, i64> {
    match value::instance_attr(instance, "isolation_level") {
        None => Err(programming_error("Base Connection.__init__ not called.")),
        Some(value_arg) => isolation_level_argument(value_arg),
    }
}

fn truthy_argument(value_arg: i64) -> bool {
    value::truthy(value_arg)
}

/// Parsed `connect()` / `Connection.__init__()` options.
struct ConnectOptions {
    database: Vec<u8>,
    timeout: f64,
    detect_types: i64,
    isolation_level: Option<String>,
    check_same_thread: bool,
    /// `None` = keyword absent (default `sqlite3.Connection`).
    factory: Option<i64>,
    cached_statements: i64,
    uri: bool,
}

/// Resolves an optional slot in the keyword-call path.
///
/// The runtime keyword binder may pad an omitted slot before a later named
/// argument with `None`; such a padding must behave like an omitted argument.
/// A `None` supplied under the slot's own keyword name is explicit.
fn effective_slot(
    slot: Option<i64>,
    name: &str,
    keywords: Option<&[(String, i64)]>,
) -> Option<i64> {
    match slot {
        Some(value_arg)
            if value_arg == NONE
                && keywords.is_some_and(|keywords| {
                    !keywords.iter().any(|(key, _)| key.as_str() == name)
                }) =>
        {
            None
        }
        other => other,
    }
}

/// Binds positional arguments and keywords onto the eight `connect` slots.
///
/// `keywords` is `None` for the positional ABI path, where an explicit `None`
/// always means what it says. The keyword path keeps "absent" and "explicitly
/// `None`" distinct: a slot named by a keyword keeps its `None`, while a
/// padding `None` in an unnamed slot is treated as omitted by
/// [`effective_slot`].
fn parse_connect_options(
    args: &[i64],
    keywords: Option<&[(String, i64)]>,
) -> Result<ConnectOptions, i64> {
    if args.len() > CONNECT_PARAMS.len() {
        return Err(type_error(&format!(
            "connect() takes at most {} arguments ({} given)",
            CONNECT_PARAMS.len(),
            args.len()
        )));
    }
    let mut slots: [Option<i64>; 8] = [None; 8];
    for (index, value_arg) in args.iter().enumerate() {
        slots[index] = Some(*value_arg);
    }
    if let Some(keywords) = keywords {
        for (name, value_arg) in keywords {
            let Some(index) = CONNECT_PARAMS
                .iter()
                .position(|param| *param == name.as_str())
            else {
                return Err(type_error(&format!(
                    "connect() got an unexpected keyword argument '{name}'"
                )));
            };
            if index < args.len() {
                return Err(type_error(&format!(
                    "connect() got multiple values for argument '{name}'"
                )));
            }
            slots[index] = Some(*value_arg);
        }
    }
    let database_value = slots[0]
        .ok_or_else(|| type_error("connect() missing required argument 'database' (pos 1)"))?;
    let database = database_argument(database_value)?;
    let timeout = match effective_slot(slots[1], "timeout", keywords) {
        Some(value_arg) => timeout_argument(value_arg)?,
        None => 5.0,
    };
    let detect_types = match effective_slot(slots[2], "detect_types", keywords) {
        Some(value_arg) => int_argument(value_arg, "detect_types")?,
        None => 0,
    };
    let isolation_level = match effective_slot(slots[3], "isolation_level", keywords) {
        Some(value_arg) => isolation_level_argument(value_arg)?,
        None => Some(String::new()),
    };
    let check_same_thread = effective_slot(slots[4], "check_same_thread", keywords)
        .map(truthy_argument)
        .unwrap_or(true);
    let factory = effective_slot(slots[5], "factory", keywords);
    let cached_statements = match effective_slot(slots[6], "cached_statements", keywords) {
        Some(value_arg) => int_argument(value_arg, "cached_statements")?,
        None => 128,
    };
    let uri = effective_slot(slots[7], "uri", keywords)
        .map(truthy_argument)
        .unwrap_or(false);
    Ok(ConnectOptions {
        database,
        timeout,
        detect_types,
        isolation_level,
        check_same_thread,
        factory,
        cached_statements,
        uri,
    })
}

// -------------------------------------------------------------------------
// Connection construction and teardown
// -------------------------------------------------------------------------

fn init_connection(instance: i64, options: &ConnectOptions) -> Result<(), i64> {
    if options.detect_types != 0 {
        return Err(not_supported(
            "detect_types is not supported by the native sqlite3 module; pass 0",
        ));
    }
    let Ok(path) = CString::new(options.database.clone()) else {
        return Err(value_error("embedded null character in database path"));
    };
    if let Some(existing) = connection_state(instance) {
        if let Some(error) = check_thread(existing) {
            return Err(error);
        }
        let Some(_recursion) = OpGuard::enter(existing.db) else {
            return Err(programming_error("Recursive use of cursors not allowed."));
        };
        close_state(existing);
    }
    let mut database: *mut Sqlite3 = ptr::null_mut();
    let flags =
        SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | if options.uri { SQLITE_OPEN_URI } else { 0 };
    // SAFETY: `path` is NUL-terminated and `database` is a valid out-pointer.
    let status = unsafe { sqlite3_open_v2(path.as_ptr(), &mut database, flags, ptr::null()) };
    if status != SQLITE_OK {
        if database.is_null() && status == SQLITE_NOMEM {
            return Err(memory_error());
        }
        let error = if database.is_null() {
            sqlite_error("OperationalError", "unable to open database file")
        } else {
            let error = raise_db_error(database);
            // SAFETY: the connection libsqlite3 created is closed exactly once.
            unsafe { sqlite3_close_v2(database) };
            error
        };
        return Err(error);
    }
    if database.is_null() {
        return Err(sqlite_error(
            "OperationalError",
            "unable to open database file",
        ));
    }
    // Statement caching is a performance knob only: every execute prepares a
    // fresh statement, so the requested cache size is accepted and ignored.
    let _ = options.cached_statements;
    let milliseconds = (options.timeout * 1000.0) as c_int;
    // SAFETY: `database` is a live connection handle.
    unsafe { sqlite3_busy_timeout(database, milliseconds.max(0)) };

    let state = Box::new(ConnectionState {
        db: database as usize,
        thread: std::thread::current().id(),
        check_same_thread: options.check_same_thread,
        ops: Mutex::new(ConnOps {
            closed: false,
            statements: Vec::new(),
            cursors: Vec::new(),
        }),
    });
    let pointer = Box::into_raw(state) as usize;
    let capsule = value::capsule_new(pointer, Some(CONNECTION_CAPSULE.to_string()));
    set_cursor_attr(instance, CONNECTION_ATTR, capsule);
    let isolation = match &options.isolation_level {
        Some(level) => value::str_alloc(level.clone()),
        None => NONE,
    };
    set_cursor_attr(instance, "isolation_level", isolation);
    set_cursor_attr(instance, "in_transaction", FALSE);
    Ok(())
}

/// Initialises a fresh cursor instance around `connection`.
fn init_cursor(cursor: i64, connection: i64, state: &'static ConnectionState) -> Result<(), i64> {
    let mut session = open_session(state)?;
    if let Some(existing) = cursor_state(cursor) {
        // Re-initialising a cursor drops its pending statement. The replaced
        // state is leaked but stays registered with its old connection so a
        // later close only touches that dead shell.
        let statement = existing.statement.replace(0);
        existing.at_row.set(false);
        if std::ptr::eq(
            existing.conn as *const ConnectionState,
            state as *const ConnectionState,
        ) {
            finalize_statement(&mut session, statement);
        } else if let Ok(mut old_session) = open_session(existing.conn) {
            finalize_statement(&mut old_session, statement);
        }
    }
    let cursor_state = Box::new(CursorState {
        conn: state,
        statement: Cell::new(0),
        closed: Cell::new(false),
        at_row: Cell::new(false),
        dml: Cell::new(false),
    });
    let pointer = Box::into_raw(cursor_state) as usize;
    let capsule = value::capsule_new(pointer, Some(CURSOR_CAPSULE.to_string()));
    set_cursor_attr(cursor, CURSOR_ATTR, capsule);
    set_cursor_attr(cursor, "connection", connection);
    set_cursor_attr(cursor, "description", NONE);
    set_cursor_attr(cursor, "rowcount", ops::int_result(-1));
    set_cursor_attr(cursor, "lastrowid", NONE);
    set_cursor_attr(cursor, "arraysize", ops::int_result(1));
    session.cursors.push(pointer);
    Ok(())
}

// -------------------------------------------------------------------------
// Statement preparation
// -------------------------------------------------------------------------

/// SQL whitespace, matching CPython's `lstrip_sql` (`isspace`).
fn is_sql_space(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte == 0x0b
}

/// Whether the statement text starts with a DML keyword.
fn is_dml(sql: &str) -> bool {
    let text = sql.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '\u{0b}');
    const KEYWORDS: [&str; 4] = ["insert", "update", "delete", "replace"];
    KEYWORDS.iter().any(|keyword| {
        text.len() >= keyword.len()
            && text.is_char_boundary(keyword.len())
            && text[..keyword.len()].eq_ignore_ascii_case(keyword)
    })
}

/// Prepares one statement, rejecting trailing SQL like CPython.
///
/// Returns `0` for an empty statement text (SQLite prepares no handle).
fn prepare_statement(ops: &mut ConnOps, db: usize, sql: &str) -> Result<usize, i64> {
    let bytes = sql.as_bytes();
    if bytes.contains(&0) {
        return Err(programming_error("the query contains a null character"));
    }
    let Ok(c_sql) = CString::new(bytes) else {
        return Err(programming_error("the query contains a null character"));
    };
    let database = db as *mut Sqlite3;
    let mut statement: *mut Sqlite3Stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null();
    // SAFETY: `c_sql` is NUL-terminated; both out-pointers are valid.
    let status =
        unsafe { sqlite3_prepare_v2(database, c_sql.as_ptr(), -1, &mut statement, &mut tail) };
    if status != SQLITE_OK {
        return Err(raise_db_error(database));
    }
    if !tail.is_null() {
        // SAFETY: the prepare tail points into `c_sql` and is NUL-terminated.
        let rest = unsafe { std::ffi::CStr::from_ptr(tail) }.to_bytes();
        if rest.iter().any(|byte| !is_sql_space(*byte)) {
            if !statement.is_null() {
                // SAFETY: prepare succeeded and handed over this statement.
                unsafe { sqlite3_finalize(statement) };
            }
            return Err(programming_error(
                "You can only execute one statement at a time.",
            ));
        }
    }
    if statement.is_null() {
        return Ok(0);
    }
    let pointer = statement as usize;
    ops.statements.push(pointer);
    Ok(pointer)
}

// -------------------------------------------------------------------------
// Parameter binding
// -------------------------------------------------------------------------

/// Binds one Python value at the given 1-based index.
fn bind_value(
    database: *mut Sqlite3,
    statement: usize,
    index: c_int,
    operand: i64,
) -> Result<(), i64> {
    let statement = statement as *mut Sqlite3Stmt;
    let status = if operand == NONE {
        // SAFETY: the statement handle is live for the duration of the call.
        unsafe { sqlite3_bind_null(statement, index) }
    } else if let Some(integer) = value::to_bigint(operand) {
        let Some(number) = integer.to_i64() else {
            return Err(overflow_error(
                "Python int too large to convert to SQLite INTEGER",
            ));
        };
        // SAFETY: as above.
        unsafe { sqlite3_bind_int64(statement, index, number) }
    } else if let Some(number) = value::as_float(operand) {
        // SAFETY: as above.
        unsafe { sqlite3_bind_double(statement, index, number) }
    } else if let Some(text) = value::as_str(operand) {
        let Ok(length) = c_int::try_from(text.len()) else {
            return Err(overflow_error("string longer than INT_MAX bytes"));
        };
        // SAFETY: `text` is valid UTF-8 of `length` bytes; `SQLITE_TRANSIENT`
        // makes SQLite copy it before the borrow ends.
        unsafe {
            sqlite3_bind_text(
                statement,
                index,
                text.as_ptr() as *const c_char,
                length,
                transient(),
            )
        }
    } else if let Some(bytes) = value::as_bytes(operand) {
        let Ok(length) = c_int::try_from(bytes.len()) else {
            return Err(overflow_error("BLOB longer than INT_MAX bytes"));
        };
        // SAFETY: as above; SQLite copies the buffer.
        unsafe {
            sqlite3_bind_blob(
                statement,
                index,
                bytes.as_ptr() as *const c_void,
                length,
                transient(),
            )
        }
    } else {
        return Err(programming_error(&format!(
            "Error binding parameter {index}: type '{}' is not supported",
            value::type_name(operand)
        )));
    };
    if status != SQLITE_OK {
        return Err(raise_db_error(database));
    }
    Ok(())
}

/// Reads the name of the 1-based parameter, if SQLite assigned one.
fn parameter_name(statement: *mut Sqlite3Stmt, index: c_int) -> Option<String> {
    // SAFETY: the statement handle is live while the caller holds the session.
    let pointer = unsafe { sqlite3_bind_parameter_name(statement, index) };
    if pointer.is_null() {
        return None;
    }
    // SAFETY: libsqlite3 returns a NUL-terminated UTF-8 string owned by the
    // prepared statement; it is copied immediately.
    Some(
        unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Binds one parameter set (sequence or mapping) onto `statement`.
fn bind_parameters(
    database: *mut Sqlite3,
    statement: usize,
    parameters: Option<i64>,
) -> Result<(), i64> {
    let handle = statement as *mut Sqlite3Stmt;
    // SAFETY: the prepared statement handle is live.
    let needed = unsafe { sqlite3_bind_parameter_count(handle) }.max(0) as usize;
    let Some(parameters) = parameters else {
        if needed == 0 {
            return Ok(());
        }
        return Err(programming_error(&format!(
            "Incorrect number of bindings supplied. The current statement uses {needed}, \
             and there are 0 supplied."
        )));
    };
    if parameters == NONE {
        return Err(programming_error("parameters are of unsupported type"));
    }
    if let Some(items) = value::as_tuple(parameters) {
        return bind_sequence(database, statement, items, needed);
    }
    if let Some(items) = value::as_list(parameters) {
        return bind_sequence(database, statement, items, needed);
    }
    if let Some(pairs) = value::as_dict(parameters) {
        return bind_mapping(database, statement, pairs, needed);
    }
    Err(programming_error("parameters are of unsupported type"))
}

fn bind_sequence(
    database: *mut Sqlite3,
    statement: usize,
    items: &[i64],
    needed: usize,
) -> Result<(), i64> {
    if items.len() != needed {
        return Err(programming_error(&format!(
            "Incorrect number of bindings supplied. The current statement uses {needed}, \
             and there are {} supplied.",
            items.len()
        )));
    }
    let handle = statement as *mut Sqlite3Stmt;
    for (offset, value_arg) in items.iter().enumerate() {
        let index = offset as c_int + 1;
        if let Some(name) = parameter_name(handle, index) {
            if !name.starts_with('?') {
                return Err(programming_error(&format!(
                    "Binding {index} ('{name}') is a named parameter, but you supplied a \
                     sequence which requires nameless (qmark) placeholders."
                )));
            }
        }
        bind_value(database, statement, index, *value_arg)?;
    }
    Ok(())
}

fn bind_mapping(
    database: *mut Sqlite3,
    statement: usize,
    pairs: &[(i64, i64)],
    needed: usize,
) -> Result<(), i64> {
    let handle = statement as *mut Sqlite3Stmt;
    for offset in 0..needed {
        let index = offset as c_int + 1;
        let Some(name) = parameter_name(handle, index) else {
            // SAFETY: the parameter count bounded this loop.
            return Err(programming_error(&format!(
                "Binding {index} has no name, but you supplied a dictionary \
                 (which has only names)."
            )));
        };
        let key = name.get(1..).unwrap_or("");
        let found = pairs.iter().find_map(|(pair_key, pair_value)| {
            if value::as_str(*pair_key) == Some(key) {
                Some(*pair_value)
            } else {
                None
            }
        });
        let Some(value_arg) = found else {
            return Err(programming_error(&format!(
                "You did not supply a value for binding parameter {name}."
            )));
        };
        bind_value(database, statement, index, value_arg)?;
    }
    Ok(())
}

// -------------------------------------------------------------------------
// Result conversion
// -------------------------------------------------------------------------

fn slice_bytes(pointer: *const u8, length: c_int) -> &'static [u8] {
    if pointer.is_null() || length <= 0 {
        return &[];
    }
    // SAFETY: SQLite guarantees `length` readable bytes for the column/value
    // while the statement/value is live; the caller copies them.
    unsafe { std::slice::from_raw_parts(pointer, length as usize) }
}

fn column_name(statement: *mut Sqlite3Stmt, index: c_int) -> String {
    // SAFETY: the statement is live for the duration of the call.
    let pointer = unsafe { sqlite3_column_name(statement, index) };
    c_message(pointer)
}

/// Converts one result column into a Hydro value.
fn column_value(statement: *mut Sqlite3Stmt, index: c_int) -> Result<i64, i64> {
    // SAFETY: the statement is live and positioned on a row.
    let kind = unsafe { sqlite3_column_type(statement, index) };
    match kind {
        SQLITE_NULL => Ok(NONE),
        // SAFETY: the accessor matching `SQLITE_INTEGER` is valid.
        SQLITE_INTEGER => Ok(ops::int_result(unsafe {
            sqlite3_column_int64(statement, index)
        })),
        // SAFETY: the accessor matching `SQLITE_FLOAT` is valid.
        SQLITE_FLOAT => Ok(value::float_new(unsafe {
            sqlite3_column_double(statement, index)
        })),
        SQLITE_TEXT => {
            // SAFETY: `sqlite3_column_bytes` is called after `column_text`.
            let pointer = unsafe { sqlite3_column_text(statement, index) };
            let length = unsafe { sqlite3_column_bytes(statement, index) };
            let bytes = slice_bytes(pointer, length);
            match std::str::from_utf8(bytes) {
                Ok(text) => Ok(value::str_alloc(text.to_string())),
                Err(_) => Err(sqlite_error(
                    "OperationalError",
                    &format!(
                        "Could not decode to UTF-8 column '{}' with text '{}'",
                        column_name(statement, index),
                        String::from_utf8_lossy(bytes)
                    ),
                )),
            }
        }
        SQLITE_BLOB => {
            // SAFETY: `sqlite3_column_bytes` is called after `column_blob`.
            let pointer = unsafe { sqlite3_column_blob(statement, index) } as *const u8;
            let length = unsafe { sqlite3_column_bytes(statement, index) };
            Ok(value::bytes_new(slice_bytes(pointer, length).to_vec()))
        }
        _ => Ok(NONE),
    }
}

/// Builds a result row tuple, keeping every fresh element rooted while the
/// next one is allocated.
fn build_row(statement: *mut Sqlite3Stmt) -> Result<i64, i64> {
    // SAFETY: the statement is live.
    let columns = unsafe { sqlite3_column_count(statement) }.max(0);
    let roots = heap::root_values(&[]);
    let mut values = Vec::with_capacity(columns as usize);
    for index in 0..columns {
        match column_value(statement, index) {
            Ok(value_arg) => {
                heap::root_values(&[value_arg]);
                values.push(value_arg);
            }
            Err(error) => {
                heap::release_roots(roots);
                return Err(error);
            }
        }
    }
    let row = value::tuple_new(values);
    heap::release_roots(roots);
    Ok(row)
}

// -------------------------------------------------------------------------
// Cursor operations
// -------------------------------------------------------------------------

fn set_cursor_attr(cursor: i64, name: &str, value_arg: i64) {
    let roots = heap::root_values(&[value_arg]);
    value::instance_setattr(cursor, name.to_string(), value_arg);
    heap::release_roots(roots);
}

fn set_rowcount(cursor: i64, count: i64) {
    set_cursor_attr(cursor, "rowcount", ops::int_result(count));
}

fn set_lastrowid(cursor: i64, database: *mut Sqlite3) {
    // SAFETY: the connection is live.
    let last = unsafe { sqlite3_last_insert_rowid(database) };
    set_cursor_attr(cursor, "lastrowid", ops::int_result(last));
}

/// `sqlite3_changes` after a DML statement.
fn changes(database: *mut Sqlite3) -> i64 {
    // SAFETY: the connection is live.
    unsafe { sqlite3_changes(database) as i64 }
}

/// Builds `cursor.description` from the prepared statement's columns.
fn set_description(cursor: i64, statement: *mut Sqlite3Stmt) {
    // SAFETY: the statement is live.
    let columns = unsafe { sqlite3_column_count(statement) }.max(0);
    if columns == 0 {
        return;
    }
    let roots = heap::root_values(&[]);
    let mut descriptors = Vec::with_capacity(columns as usize);
    for index in 0..columns {
        let name = value::str_alloc(column_name(statement, index));
        let descriptor = value::tuple_new(vec![name, NONE, NONE, NONE, NONE, NONE, NONE]);
        heap::root_values(&[descriptor]);
        descriptors.push(descriptor);
    }
    let description = value::tuple_new(descriptors);
    heap::release_roots(roots);
    set_cursor_attr(cursor, "description", description);
}

fn refresh_in_transaction(connection: i64, database: *mut Sqlite3) {
    // SAFETY: the connection is live.
    let autocommit = unsafe { sqlite3_get_autocommit(database) } != 0;
    set_cursor_attr(
        connection,
        "in_transaction",
        if autocommit { FALSE } else { TRUE },
    );
}

/// Finalizes the cursor's current statement in place.
fn finish_statement(cursor: &'static CursorState, session: &mut ConnSession) {
    let statement = cursor.statement.replace(0);
    cursor.at_row.set(false);
    finalize_statement(session, statement);
}

/// Executes `sql` and leaves the statement positioned on its first row.
fn run_execute(
    cursor_value: i64,
    cursor: &'static CursorState,
    sql: &str,
    parameters: Option<i64>,
) -> i64 {
    let mut session = match open_cursor_session(cursor) {
        Ok(session) => session,
        Err(error) => return error,
    };
    finish_statement(cursor, &mut session);
    set_cursor_attr(cursor_value, "description", NONE);
    let Some(connection) = value::instance_attr(cursor_value, "connection") else {
        return programming_error("Base Cursor.__init__ not called.");
    };
    let dml = is_dml(sql);
    cursor.dml.set(dml);
    let db = session.db;
    let statement = match prepare_statement(&mut session, db, sql) {
        Ok(statement) => statement,
        Err(error) => {
            set_rowcount(cursor_value, -1);
            return error;
        }
    };
    if statement == 0 {
        set_rowcount(cursor_value, -1);
        set_lastrowid(cursor_value, session.db as *mut Sqlite3);
        refresh_in_transaction(connection, session.db as *mut Sqlite3);
        return cursor_value;
    }
    cursor.statement.set(statement);
    let level = match isolation_level(connection) {
        Ok(level) => level,
        Err(error) => {
            finish_statement(cursor, &mut session);
            set_rowcount(cursor_value, -1);
            return error;
        }
    };
    let database = session.db as *mut Sqlite3;
    if dml && level.is_some() && unsafe { sqlite3_get_autocommit(database) } != 0 {
        if let Err(error) = exec_sql(database, &format!("BEGIN {}", level.unwrap_or_default())) {
            finish_statement(cursor, &mut session);
            set_rowcount(cursor_value, -1);
            return error;
        }
    }
    if let Err(error) = bind_parameters(database, statement, parameters) {
        finish_statement(cursor, &mut session);
        set_rowcount(cursor_value, -1);
        refresh_in_transaction(connection, database);
        return error;
    }
    set_rowcount(cursor_value, if dml { 0 } else { -1 });
    // SAFETY: the statement handle is live and fully bound.
    let status = unsafe { sqlite3_step(statement as *mut Sqlite3Stmt) };
    match status {
        SQLITE_ROW => {
            cursor.at_row.set(true);
            set_description(cursor_value, statement as *mut Sqlite3Stmt);
        }
        SQLITE_DONE => {
            set_description(cursor_value, statement as *mut Sqlite3Stmt);
            if dml {
                set_rowcount(cursor_value, changes(database));
            }
            finish_statement(cursor, &mut session);
        }
        _ => {
            let error = raise_db_error(database);
            finish_statement(cursor, &mut session);
            set_rowcount(cursor_value, -1);
            refresh_in_transaction(connection, database);
            return error;
        }
    }
    set_lastrowid(cursor_value, database);
    refresh_in_transaction(connection, database);
    cursor_value
}

/// Runs one parameter set, draining any RETURNING rows (they are not part of
/// the DB-API `executemany` result).
fn executemany_row(session: &mut ConnSession, statement: usize, row: i64) -> Result<(), i64> {
    let database = session.db as *mut Sqlite3;
    // SAFETY: the statement is live; reset returns it to its initial state.
    unsafe {
        sqlite3_reset(statement as *mut Sqlite3Stmt);
        sqlite3_clear_bindings(statement as *mut Sqlite3Stmt);
    }
    bind_parameters(database, statement, Some(row))?;
    let mut status = unsafe { sqlite3_step(statement as *mut Sqlite3Stmt) };
    while status == SQLITE_ROW {
        status = unsafe { sqlite3_step(statement as *mut Sqlite3Stmt) };
    }
    if status != SQLITE_DONE {
        return Err(raise_db_error(database));
    }
    Ok(())
}

fn cursor_execute(args: &[i64]) -> i64 {
    let Some(&cursor_value) = args.first() else {
        return type_error("execute() missing cursor receiver");
    };
    let Some(sql) = args.get(1).and_then(|value_arg| value::as_str(*value_arg)) else {
        return type_error("execute() argument 'sql' must be str");
    };
    let sql = sql.to_string();
    let parameters = args.get(2).copied();
    let Some(cursor) = cursor_state(cursor_value) else {
        return programming_error("Base Cursor.__init__ not called.");
    };
    run_execute(cursor_value, cursor, &sql, parameters)
}

fn cursor_executemany(args: &[i64]) -> i64 {
    let Some(&cursor_value) = args.first() else {
        return type_error("executemany() missing cursor receiver");
    };
    let Some(sql) = args.get(1).and_then(|value_arg| value::as_str(*value_arg)) else {
        return type_error("executemany() argument 'sql' must be str");
    };
    let sql = sql.to_string();
    let Some(parameters) = args.get(2).copied() else {
        return type_error("executemany() missing required argument 'seq_of_parameters' (pos 2)");
    };
    let Some(cursor) = cursor_state(cursor_value) else {
        return programming_error("Base Cursor.__init__ not called.");
    };
    let mut session = match open_cursor_session(cursor) {
        Ok(session) => session,
        Err(error) => return error,
    };
    finish_statement(cursor, &mut session);
    set_cursor_attr(cursor_value, "description", NONE);
    let Some(connection) = value::instance_attr(cursor_value, "connection") else {
        return programming_error("Base Cursor.__init__ not called.");
    };
    let dml = is_dml(&sql);
    let db = session.db;
    let statement = match prepare_statement(&mut session, db, &sql) {
        Ok(statement) => statement,
        Err(error) => {
            set_rowcount(cursor_value, -1);
            return error;
        }
    };
    if statement == 0 {
        return programming_error("executemany() can only execute DML statements.");
    }
    cursor.statement.set(statement);
    cursor.dml.set(dml);
    let database = session.db as *mut Sqlite3;
    // SAFETY: the prepared statement is live.
    if unsafe { sqlite3_stmt_readonly(statement as *mut Sqlite3Stmt) } != 0 {
        finish_statement(cursor, &mut session);
        set_rowcount(cursor_value, -1);
        return programming_error("executemany() can only execute DML statements.");
    }
    let level = match isolation_level(connection) {
        Ok(level) => level,
        Err(error) => {
            finish_statement(cursor, &mut session);
            set_rowcount(cursor_value, -1);
            return error;
        }
    };
    if dml && level.is_some() && unsafe { sqlite3_get_autocommit(database) } != 0 {
        if let Err(error) = exec_sql(database, &format!("BEGIN {}", level.unwrap_or_default())) {
            finish_statement(cursor, &mut session);
            set_rowcount(cursor_value, -1);
            return error;
        }
    }
    set_rowcount(cursor_value, if dml { 0 } else { -1 });
    let iterator = crate::iter::iter_of(parameters);
    if exc::pending() {
        finish_statement(cursor, &mut session);
        set_rowcount(cursor_value, -1);
        return NONE;
    }
    let iterator_roots = heap::root_values(&[iterator]);
    let mut total = if dml { 0 } else { -1 };
    let mut failure: Option<i64> = None;
    loop {
        let mut row = NONE;
        let status = crate::iter::iter_next(iterator, &mut row);
        if exc::pending() {
            failure = Some(NONE);
            break;
        }
        if status == 0 {
            break;
        }
        let roots = heap::root_values(&[row]);
        let result = executemany_row(&mut session, statement, row);
        heap::release_roots(roots);
        if let Err(error) = result {
            failure = Some(error);
            break;
        }
        if dml {
            total += changes(database);
        }
    }
    heap::release_roots(iterator_roots);
    finish_statement(cursor, &mut session);
    if let Some(error) = failure {
        set_rowcount(cursor_value, -1);
        refresh_in_transaction(connection, database);
        return error;
    }
    if dml {
        set_rowcount(cursor_value, total);
    }
    refresh_in_transaction(connection, database);
    cursor_value
}

/// Fetches the current row and advances the statement.
fn fetch_one(cursor_value: i64, cursor: &'static CursorState, session: &mut ConnSession) -> i64 {
    let statement = cursor.statement.get();
    if statement == 0 || !cursor.at_row.get() {
        return NONE;
    }
    let handle = statement as *mut Sqlite3Stmt;
    let row = match build_row(handle) {
        Ok(row) => row,
        Err(error) => {
            finish_statement(cursor, session);
            return error;
        }
    };
    cursor.at_row.set(false);
    // The next step may run user-defined functions, so the fetched row must
    // stay rooted until it is returned.
    let row_roots = heap::root_values(&[row]);
    // SAFETY: the statement is live and positioned on a row.
    let status = unsafe { sqlite3_step(handle) };
    match status {
        SQLITE_ROW => cursor.at_row.set(true),
        SQLITE_DONE => {
            if cursor.dml.get() {
                set_rowcount(cursor_value, changes(session.db as *mut Sqlite3));
            }
            finish_statement(cursor, session);
        }
        _ => {
            let error = raise_db_error(session.db as *mut Sqlite3);
            set_rowcount(cursor_value, -1);
            finish_statement(cursor, session);
            heap::release_roots(row_roots);
            return error;
        }
    }
    heap::release_roots(row_roots);
    row
}

fn fetch_size(value_arg: i64) -> Result<usize, i64> {
    let Some(number) = value::as_integer(value_arg) else {
        return Err(type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(value_arg)
        )));
    };
    if number < 0 {
        return Err(value_error("Cannot convert negative int"));
    }
    if number > u32::MAX as i64 {
        return Err(overflow_error(
            "Python int too large to convert to C unsigned int",
        ));
    }
    Ok(number as usize)
}

fn cursor_fetchone(args: &[i64]) -> i64 {
    let Some(&cursor_value) = args.first() else {
        return type_error("fetchone() missing cursor receiver");
    };
    let Some(cursor) = cursor_state(cursor_value) else {
        return programming_error("Base Cursor.__init__ not called.");
    };
    let mut session = match open_cursor_session(cursor) {
        Ok(session) => session,
        Err(error) => return error,
    };
    fetch_one(cursor_value, cursor, &mut session)
}

fn cursor_fetchmany(args: &[i64]) -> i64 {
    let Some(&cursor_value) = args.first() else {
        return type_error("fetchmany() missing cursor receiver");
    };
    let Some(cursor) = cursor_state(cursor_value) else {
        return programming_error("Base Cursor.__init__ not called.");
    };
    let size = match args.get(1).copied() {
        Some(value_arg) => match fetch_size(value_arg) {
            Ok(size) => size,
            Err(error) => return error,
        },
        None => match value::instance_attr(cursor_value, "arraysize") {
            Some(value_arg) => match fetch_size(value_arg) {
                Ok(size) => size,
                Err(error) => return error,
            },
            None => 1,
        },
    };
    let mut session = match open_cursor_session(cursor) {
        Ok(session) => session,
        Err(error) => return error,
    };
    let result = value::list_new(Vec::new());
    let roots = heap::root_values(&[result]);
    for _ in 0..size {
        let row = fetch_one(cursor_value, cursor, &mut session);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
        if row == NONE {
            break;
        }
        let row_roots = heap::root_values(&[row]);
        crate::container::list_append(result, row);
        heap::release_roots(row_roots);
    }
    heap::release_roots(roots);
    result
}

fn cursor_fetchall(args: &[i64]) -> i64 {
    let Some(&cursor_value) = args.first() else {
        return type_error("fetchall() missing cursor receiver");
    };
    let Some(cursor) = cursor_state(cursor_value) else {
        return programming_error("Base Cursor.__init__ not called.");
    };
    let mut session = match open_cursor_session(cursor) {
        Ok(session) => session,
        Err(error) => return error,
    };
    let result = value::list_new(Vec::new());
    let roots = heap::root_values(&[result]);
    loop {
        let row = fetch_one(cursor_value, cursor, &mut session);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
        if row == NONE {
            break;
        }
        let row_roots = heap::root_values(&[row]);
        crate::container::list_append(result, row);
        heap::release_roots(row_roots);
    }
    heap::release_roots(roots);
    result
}

fn cursor_close(args: &[i64]) -> i64 {
    let Some(&cursor_value) = args.first() else {
        return type_error("close() missing cursor receiver");
    };
    let Some(cursor) = cursor_state(cursor_value) else {
        return programming_error("Base Cursor.__init__ not called.");
    };
    // `Cursor.close` does not reject an already closed cursor, but it still
    // requires a live connection (CPython's `check_connection`).
    let mut session = match open_session(cursor.conn) {
        Ok(session) => session,
        Err(error) => return error,
    };
    let statement = cursor.statement.replace(0);
    cursor.at_row.set(false);
    finalize_statement(&mut session, statement);
    cursor.closed.set(true);
    NONE
}

fn cursor_iter(args: &[i64]) -> i64 {
    let Some(&cursor_value) = args.first() else {
        return type_error("__iter__() missing cursor receiver");
    };
    let Some(cursor) = cursor_state(cursor_value) else {
        return programming_error("Base Cursor.__init__ not called.");
    };
    if let Err(error) = open_cursor_session(cursor) {
        return error;
    }
    cursor_value
}

fn cursor_next(args: &[i64]) -> i64 {
    let row = cursor_fetchone(args);
    if exc::pending() {
        return row;
    }
    if row == NONE {
        return exc::raise_with_message(exc::STOP_ITERATION, "");
    }
    row
}

// -------------------------------------------------------------------------
// Connection operations
// -------------------------------------------------------------------------

fn connection_init(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("__init__() missing connection receiver");
    };
    if value::as_instance(instance).is_none() {
        return type_error("Connection.__init__() requires a Connection instance");
    }
    let options = match parse_connect_options(&args[1..], None) {
        Ok(options) => options,
        Err(error) => return error,
    };
    match init_connection(instance, &options) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

/// Builds and initialises a connection of `class`.
fn new_connection(class: i64, options: &ConnectOptions) -> Result<i64, i64> {
    let instance = value::instance_new(class);
    let roots = heap::root_values(&[instance]);
    let result = init_connection(instance, options);
    heap::release_roots(roots);
    match result {
        Ok(()) => Ok(instance),
        Err(error) => Err(error),
    }
}

/// `sqlite3.Connection(...)` reached through the module-call lowering: the
/// spec declares the class as a constructor, so the value read returns the
/// cached class (see [`class_value`]) while direct calls land here.
fn connection_construct(args: &[i64], keywords: Option<&[(String, i64)]>) -> i64 {
    let options = match parse_connect_options(args, keywords) {
        Ok(options) => options,
        Err(error) => return error,
    };
    match new_connection(connection_class(), &options) {
        Ok(instance) => instance,
        Err(error) => error,
    }
}

/// `sqlite3.Cursor(connection)` reached through the module-call lowering.
fn cursor_construct(args: &[i64]) -> i64 {
    let Some(&connection) = args.first() else {
        return type_error("Cursor() missing required argument 'connection' (pos 1)");
    };
    let Some(state) = connection_state(connection) else {
        return programming_error("Base Connection.__init__ not called.");
    };
    let cursor = value::instance_new(cursor_class());
    let roots = heap::root_values(&[cursor]);
    let result = init_cursor(cursor, connection, state);
    heap::release_roots(roots);
    match result {
        Ok(()) => cursor,
        Err(error) => error,
    }
}

/// `sqlite3.connect(...)`: parse, then construct the selected class.
fn connect_entry(args: &[i64], keywords: Option<&[(String, i64)]>) -> i64 {
    let options = match parse_connect_options(args, keywords) {
        Ok(options) => options,
        Err(error) => return error,
    };
    let default_class = connection_class();
    let class = match options.factory {
        None => default_class,
        // The documented signature spells the default as `factory=None`, so
        // an explicit `None` selects the built-in class too.
        Some(factory) if factory == NONE => default_class,
        Some(factory) => {
            if value::is_subclass_of(factory, default_class) {
                factory
            } else {
                return not_supported("factory must be sqlite3.Connection or a subclass");
            }
        }
    };
    match new_connection(class, &options) {
        Ok(instance) => instance,
        Err(error) => error,
    }
}

fn connection_cursor(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("cursor() missing connection receiver");
    };
    let factory = args.get(1).copied().unwrap_or(NONE);
    let Some(state) = connection_state(instance) else {
        return programming_error("Base Connection.__init__ not called.");
    };
    if let Err(error) = open_session(state) {
        return error;
    }
    let default_class = cursor_class();
    let class = if factory == NONE {
        default_class
    } else if value::is_subclass_of(factory, default_class) {
        factory
    } else {
        return not_supported("cursor factory must be sqlite3.Cursor or a subclass");
    };
    let cursor = value::instance_new(class);
    let roots = heap::root_values(&[cursor]);
    let result = init_cursor(cursor, instance, state);
    heap::release_roots(roots);
    match result {
        Ok(()) => cursor,
        Err(error) => error,
    }
}

fn connection_execute(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("execute() missing connection receiver");
    };
    if args
        .get(1)
        .and_then(|value_arg| value::as_str(*value_arg))
        .is_none()
    {
        return type_error("execute() argument 'sql' must be str");
    }
    let cursor = connection_cursor(&[instance]);
    if exc::pending() {
        return NONE;
    }
    let mut cursor_args = Vec::with_capacity(args.len());
    cursor_args.push(cursor);
    cursor_args.push(args[1]);
    if let Some(parameters) = args.get(2) {
        cursor_args.push(*parameters);
    }
    let roots = heap::root_values(&cursor_args);
    let result = cursor_execute(&cursor_args);
    heap::release_roots(roots);
    if exc::pending() {
        return NONE;
    }
    result
}

fn connection_executemany(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("executemany() missing connection receiver");
    };
    if args
        .get(1)
        .and_then(|value_arg| value::as_str(*value_arg))
        .is_none()
    {
        return type_error("executemany() argument 'sql' must be str");
    }
    let Some(parameters) = args.get(2).copied() else {
        return type_error("executemany() missing required argument 'seq_of_parameters' (pos 2)");
    };
    let cursor = connection_cursor(&[instance]);
    if exc::pending() {
        return NONE;
    }
    let cursor_args = [cursor, args[1], parameters];
    let roots = heap::root_values(&cursor_args);
    let result = cursor_executemany(&cursor_args);
    heap::release_roots(roots);
    if exc::pending() {
        return NONE;
    }
    result
}

fn connection_commit(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("commit() missing connection receiver");
    };
    let Some(state) = connection_state(instance) else {
        return programming_error("Base Connection.__init__ not called.");
    };
    let session = match open_session(state) {
        Ok(session) => session,
        Err(error) => return error,
    };
    let database = session.db as *mut Sqlite3;
    // SAFETY: the connection is live.
    if unsafe { sqlite3_get_autocommit(database) } == 0 {
        if let Err(error) = exec_sql(database, "COMMIT") {
            return error;
        }
    }
    refresh_in_transaction(instance, database);
    NONE
}

fn connection_rollback(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("rollback() missing connection receiver");
    };
    let Some(state) = connection_state(instance) else {
        return programming_error("Base Connection.__init__ not called.");
    };
    let session = match open_session(state) {
        Ok(session) => session,
        Err(error) => return error,
    };
    let database = session.db as *mut Sqlite3;
    // SAFETY: the connection is live.
    if unsafe { sqlite3_get_autocommit(database) } == 0 {
        if let Err(error) = exec_sql(database, "ROLLBACK") {
            return error;
        }
    }
    refresh_in_transaction(instance, database);
    NONE
}

fn connection_close(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("close() missing connection receiver");
    };
    let Some(state) = connection_state(instance) else {
        return programming_error("Base Connection.__init__ not called.");
    };
    // Closing checks the thread first, like CPython; a second close succeeds.
    if let Some(error) = check_thread(state) {
        return error;
    }
    let Some(_recursion) = OpGuard::enter(state.db) else {
        return programming_error("Recursive use of cursors not allowed.");
    };
    let mut ops = lock_ops(state);
    close_locked(&mut ops, state.db);
    drop(ops);
    set_cursor_attr(instance, "in_transaction", FALSE);
    NONE
}

fn connection_enter(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("__enter__() missing connection receiver");
    };
    let Some(state) = connection_state(instance) else {
        return programming_error("Base Connection.__init__ not called.");
    };
    match open_session(state) {
        Ok(_session) => instance,
        Err(error) => error,
    }
}

fn connection_exit(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("__exit__() missing connection receiver");
    };
    let exception = args.get(1).copied().unwrap_or(NONE);
    let result = if exception == NONE {
        connection_commit(&[instance])
    } else {
        connection_rollback(&[instance])
    };
    if exc::pending() {
        return result;
    }
    FALSE
}

fn cursor_init(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("__init__() missing cursor receiver");
    };
    if value::as_instance(instance).is_none() {
        return type_error("Cursor.__init__() requires a Cursor instance");
    }
    let Some(&connection) = args.get(1) else {
        return type_error("__init__() missing required argument 'connection' (pos 1)");
    };
    let Some(state) = connection_state(connection) else {
        return programming_error("Base Connection.__init__ not called.");
    };
    match init_cursor(instance, connection, state) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

// -------------------------------------------------------------------------
// User-defined functions
// -------------------------------------------------------------------------

/// Registered scalar function: the Python callable, rooted for the process
/// lifetime. SQLite owns the allocation through `xDestroy`.
struct Udf {
    function: i64,
}

unsafe extern "C" fn udf_destroy(application: *mut c_void) {
    if application.is_null() {
        return;
    }
    // SAFETY: `application` is the pointer handed to
    // `sqlite3_create_function_v2`, allocated by `connection_create_function`.
    drop(unsafe { Box::from_raw(application as *mut Udf) });
}

fn udf_argument(argument: *mut SqliteValue) -> Result<i64, String> {
    if argument.is_null() {
        return Ok(NONE);
    }
    // SAFETY: SQLite passes a live argument value to the callback.
    let kind = unsafe { sqlite3_value_type(argument) };
    match kind {
        SQLITE_NULL => Ok(NONE),
        SQLITE_INTEGER => Ok(ops::int_result(unsafe { sqlite3_value_int64(argument) })),
        SQLITE_FLOAT => Ok(value::float_new(unsafe { sqlite3_value_double(argument) })),
        SQLITE_TEXT => {
            // SAFETY: `sqlite3_value_bytes` is called after `value_text`.
            let pointer = unsafe { sqlite3_value_text(argument) };
            let length = unsafe { sqlite3_value_bytes(argument) };
            let bytes = slice_bytes(pointer, length);
            match std::str::from_utf8(bytes) {
                Ok(text) => Ok(value::str_alloc(text.to_string())),
                Err(_) => Err("user-defined function raised exception".to_string()),
            }
        }
        SQLITE_BLOB => {
            // SAFETY: `sqlite3_value_bytes` is called after `value_blob`.
            let pointer = unsafe { sqlite3_value_blob(argument) } as *const u8;
            let length = unsafe { sqlite3_value_bytes(argument) };
            Ok(value::bytes_new(slice_bytes(pointer, length).to_vec()))
        }
        _ => Ok(NONE),
    }
}

fn sqlite_result_error(context: *mut SqliteContext, message: &str) {
    let text = message.replace('\0', " ");
    if let Ok(text) = CString::new(text) {
        // SAFETY: the context is live for the callback's duration.
        unsafe { sqlite3_result_error(context, text.as_ptr(), -1) };
    }
}

/// Publishes one Python value as the SQL function result.
fn udf_result(context: *mut SqliteContext, result: i64) {
    if result == NONE {
        // SAFETY: the context is live.
        unsafe { sqlite3_result_null(context) };
        return;
    }
    if let Some(integer) = value::to_bigint(result) {
        let Some(number) = integer.to_i64() else {
            sqlite_result_error(context, "user-defined function raised exception");
            return;
        };
        // SAFETY: the context is live.
        unsafe { sqlite3_result_int64(context, number) };
        return;
    }
    if let Some(number) = value::as_float(result) {
        // SAFETY: the context is live.
        unsafe { sqlite3_result_double(context, number) };
        return;
    }
    if let Some(text) = value::as_str(result) {
        let Ok(length) = c_int::try_from(text.len()) else {
            sqlite_result_error(context, "user-defined function raised exception");
            return;
        };
        // SAFETY: SQLite copies the text (`SQLITE_TRANSIENT`).
        unsafe {
            sqlite3_result_text(context, text.as_ptr() as *const c_char, length, transient())
        };
        return;
    }
    if let Some(bytes) = value::as_bytes(result) {
        let Ok(length) = c_int::try_from(bytes.len()) else {
            sqlite_result_error(context, "user-defined function raised exception");
            return;
        };
        // SAFETY: SQLite copies the blob (`SQLITE_TRANSIENT`).
        unsafe {
            sqlite3_result_blob(
                context,
                bytes.as_ptr() as *const c_void,
                length,
                transient(),
            )
        };
        return;
    }
    sqlite_result_error(context, "user-defined function returned unsupported type");
}

fn udf_invoke(context: *mut SqliteContext, argc: c_int, argv: *mut *mut SqliteValue) {
    // SAFETY: the callback receives its application pointer.
    let application = unsafe { sqlite3_user_data(context) };
    if application.is_null() {
        sqlite_result_error(context, "user-defined function context is missing");
        return;
    }
    // SAFETY: `application` is the `Udf` registered for this function.
    let udf = unsafe { &*(application as *const Udf) };
    let count = argc.max(0) as usize;
    let roots = heap::root_values(&[]);
    let mut arguments = Vec::with_capacity(count);
    for index in 0..count {
        // SAFETY: `argv` holds `argc` argument pointers.
        let argument = unsafe { *argv.add(index) };
        match udf_argument(argument) {
            Ok(value_arg) => {
                heap::root_values(&[value_arg]);
                arguments.push(value_arg);
            }
            Err(message) => {
                exc::clear();
                sqlite_result_error(context, &message);
                heap::release_roots(roots);
                return;
            }
        }
    }
    let result = crate::builtins::call(udf.function, arguments.len(), arguments.as_ptr());
    if exc::pending() {
        // CPython clears the callback exception and lets step() raise the
        // generic OperationalError; mirror that instead of leaking a second
        // pending exception.
        exc::clear();
        sqlite_result_error(context, "user-defined function raised exception");
        heap::release_roots(roots);
        return;
    }
    let result_roots = heap::root_values(&[result]);
    udf_result(context, result);
    heap::release_roots(result_roots);
    heap::release_roots(roots);
}

unsafe extern "C" fn udf_callback(
    context: *mut SqliteContext,
    argc: c_int,
    argv: *mut *mut SqliteValue,
) {
    // No Rust unwind may cross the C frame.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        udf_invoke(context, argc, argv)
    }));
    if outcome.is_err() {
        sqlite_result_error(context, "user-defined function raised exception");
    }
}

fn create_function(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    for (name, _) in keywords {
        if name.as_str() != "deterministic" {
            return type_error(&format!(
                "create_function() got an unexpected keyword argument '{name}'"
            ));
        }
    }
    let Some(&instance) = args.first() else {
        return type_error("create_function() missing connection receiver");
    };
    let Some(name) = args.get(1).and_then(|value_arg| value::as_str(*value_arg)) else {
        return type_error("create_function() argument 'name' must be str");
    };
    let name = name.to_string();
    let Some(nargs) = args
        .get(2)
        .and_then(|value_arg| value::as_integer(*value_arg))
    else {
        return type_error("create_function() argument 'narg' must be int");
    };
    let Ok(nargs) = c_int::try_from(nargs) else {
        return value_error("argument 'narg' is out of range");
    };
    let Some(&function) = args.get(3) else {
        return type_error("create_function() missing required argument 'func' (pos 3)");
    };
    if !is_callable(function) {
        return type_error(&format!(
            "create_function() argument 'func' must be callable, not '{}'",
            value::type_name(function)
        ));
    }
    let mut deterministic = false;
    if let Some(&positional) = args.get(4) {
        if positional != NONE {
            deterministic = truthy_argument(positional);
        }
    }
    if let Some((_, value_arg)) = keywords
        .iter()
        .find(|(name, _)| name.as_str() == "deterministic")
    {
        deterministic = truthy_argument(*value_arg);
    }
    let Some(state) = connection_state(instance) else {
        return programming_error("Base Connection.__init__ not called.");
    };
    let session = match open_session(state) {
        Ok(session) => session,
        Err(error) => return error,
    };
    let Ok(c_name) = CString::new(name.clone()) else {
        return value_error("embedded null character in function name");
    };
    let application = Box::into_raw(Box::new(Udf { function }));
    heap::add_global_root(function);
    // SAFETY: the connection is live; the application pointer stays valid
    // until SQLite calls `udf_destroy`.
    let status = unsafe {
        sqlite3_create_function_v2(
            session.db as *mut Sqlite3,
            c_name.as_ptr(),
            nargs,
            SQLITE_UTF8
                | if deterministic {
                    SQLITE_DETERMINISTIC
                } else {
                    0
                },
            application as *mut c_void,
            Some(udf_callback as UdfCallback),
            None,
            None,
            Some(udf_destroy as UdfDestroyFn),
        )
    };
    if status != SQLITE_OK {
        // SQLite invokes `udf_destroy` itself when registration fails, so the
        // application pointer must not be freed here.
        return raise_db_error(session.db as *mut Sqlite3);
    }
    NONE
}

// -------------------------------------------------------------------------
// Module surface
// -------------------------------------------------------------------------

fn complete_statement(args: &[i64]) -> i64 {
    let Some(&statement) = args.first() else {
        return type_error("complete_statement() missing required argument 'statement' (pos 1)");
    };
    let Some(text) = value::as_str(statement) else {
        return type_error("complete_statement() argument 'statement' must be str");
    };
    let bytes = text.as_bytes();
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    let Ok(text) = CString::new(&bytes[..end]) else {
        return FALSE;
    };
    // SAFETY: `text` is NUL-terminated.
    let complete = unsafe { sqlite3_complete(text.as_ptr()) };
    if complete != 0 {
        TRUE
    } else {
        FALSE
    }
}

fn sqlite_version() -> String {
    // SAFETY: libsqlite3 returns a static NUL-terminated version string.
    let pointer = unsafe { sqlite3_libversion() };
    c_message(pointer)
}

fn sqlite_version_info() -> i64 {
    // SAFETY: static version number.
    let number = unsafe { sqlite3_libversion_number() };
    let major = (number / 1_000_000) as i64;
    let minor = ((number / 1_000) % 1_000) as i64;
    let patch = (number % 1_000) as i64;
    value::tuple_new(vec![
        ops::int_result(major),
        ops::int_result(minor),
        ops::int_result(patch),
    ])
}

/// Class values that must be returned when the module attribute is read.
///
/// The spec declares `Connection`/`Cursor` as constructors so calls lower as
/// module calls; reading `sqlite3.Connection` (for `isinstance`, subclassing
/// or `factory=`) yields the cached class instead.
pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    if !matches!(module, "sqlite3" | "_sqlite3") {
        return None;
    }
    match name {
        "Connection" => Some(connection_class()),
        "Cursor" => Some(cursor_class()),
        _ => None,
    }
}

/// Materialises a module constant or class value.
///
/// The spec declares these names as constants; `connect`/`complete_statement`
/// are spec functions and are materialised by the central dispatcher.
pub(crate) fn constant(name: &str) -> Option<i64> {
    if let Some(class) = class_value(MODULE, name) {
        return Some(class);
    }
    match name {
        "Warning" | "Error" | "InterfaceError" | "DatabaseError" | "DataError"
        | "OperationalError" | "IntegrityError" | "InternalError" | "ProgrammingError"
        | "NotSupportedError" => Some(exception_class(name)),
        "apilevel" => Some(value::str_alloc("2.0".to_string())),
        "threadsafety" => Some(ops::int_result(3)),
        "paramstyle" => Some(value::str_alloc("qmark".to_string())),
        "sqlite_version" => Some(value::str_alloc(sqlite_version())),
        "sqlite_version_info" => Some(sqlite_version_info()),
        "PARSE_DECLTYPES" => Some(ops::int_result(1)),
        "PARSE_COLNAMES" => Some(ops::int_result(2)),
        _ => None,
    }
}

/// Dispatches one `sqlite3` function or qualified native method.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "connect" => connect_entry(args, None),
        "complete_statement" => complete_statement(args),
        "Connection" => connection_construct(args, None),
        "Cursor" => cursor_construct(args),
        "sqlite3.Connection.__init__" => connection_init(args),
        "sqlite3.Connection.cursor" => connection_cursor(args),
        "sqlite3.Connection.execute" => connection_execute(args),
        "sqlite3.Connection.executemany" => connection_executemany(args),
        "sqlite3.Connection.commit" => connection_commit(args),
        "sqlite3.Connection.rollback" => connection_rollback(args),
        "sqlite3.Connection.close" => connection_close(args),
        "sqlite3.Connection.create_function" => create_function(args, &[]),
        "sqlite3.Connection.__enter__" => connection_enter(args),
        "sqlite3.Connection.__exit__" => connection_exit(args),
        "sqlite3.Cursor.__init__" => cursor_init(args),
        "sqlite3.Cursor.execute" => cursor_execute(args),
        "sqlite3.Cursor.executemany" => cursor_executemany(args),
        "sqlite3.Cursor.fetchone" => cursor_fetchone(args),
        "sqlite3.Cursor.fetchmany" => cursor_fetchmany(args),
        "sqlite3.Cursor.fetchall" => cursor_fetchall(args),
        "sqlite3.Cursor.close" => cursor_close(args),
        "sqlite3.Cursor.__iter__" => cursor_iter(args),
        "sqlite3.Cursor.__next__" => cursor_next(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'sqlite3' has no attribute '{function}'"),
        ),
    }
}

/// Keyword form: `connect` binds its eight documented names, and
/// `create_function` accepts the keyword-only `deterministic` flag.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        "connect" => connect_entry(args, Some(keywords)),
        "Connection" => connection_construct(args, Some(keywords)),
        "sqlite3.Connection.create_function" => create_function(args, keywords),
        _ => {
            if keywords.is_empty() {
                call(function, args)
            } else {
                let message = format!("{function}() does not accept keyword arguments");
                type_error(&message)
            }
        }
    }
}

/// `sqlite3_exec` for BEGIN/COMMIT/ROLLBACK.
fn exec_sql(database: *mut Sqlite3, sql: &str) -> Result<(), i64> {
    let Ok(text) = CString::new(sql) else {
        return Err(value_error("embedded null character in SQL"));
    };
    // SAFETY: the connection is live and `text` is NUL-terminated.
    let status = unsafe {
        sqlite3_exec(
            database,
            text.as_ptr(),
            None,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if status != SQLITE_OK {
        return Err(raise_db_error(database));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(source: &str) -> i64 {
        value::str_alloc(source.to_string())
    }

    fn int(number: i64) -> i64 {
        ops::int_result(number)
    }

    fn method(name: &str, args: &[i64]) -> i64 {
        call(name, args)
    }

    fn connection() -> i64 {
        let database = text(":memory:");
        let handle = call("connect", &[database]);
        assert!(!exc::pending(), "connect() failed: {:?}", pending_text());
        handle
    }

    fn pending_text() -> String {
        if exc::pending() {
            value::to_string(exc::pending_value())
        } else {
            String::new()
        }
    }

    fn expect_error(class: &str) -> String {
        assert!(exc::pending(), "expected {class}, no exception was raised");
        let pending = exc::pending_value();
        let actual = value::instance_class_value(pending)
            .map(|class_value| {
                value::class_display_name(class_value)
                    .unwrap_or("?")
                    .to_string()
            })
            .unwrap_or_else(|| value::type_name(pending).to_string());
        let message = value::to_string(pending);
        exc::clear();
        assert_eq!(
            actual, class,
            "unexpected exception class (message: {message})"
        );
        message
    }

    fn rows(value_arg: i64) -> Vec<i64> {
        value::as_list(value_arg).expect("list").to_vec()
    }

    #[test]
    fn create_insert_select_update_and_description() {
        exc::init();
        let conn = connection();

        let cursor = method(
            "sqlite3.Connection.execute",
            &[
                conn,
                text("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, weight REAL)"),
            ],
        );
        assert!(!exc::pending(), "create failed: {:?}", pending_text());
        assert_eq!(
            value::as_integer(value::instance_attr(cursor, "rowcount").unwrap()),
            Some(-1)
        );
        assert_eq!(value::instance_attr(cursor, "description").unwrap(), NONE);

        let cursor = method(
            "sqlite3.Connection.execute",
            &[
                conn,
                text("INSERT INTO t (name, weight) VALUES (?, ?)"),
                value::tuple_new(vec![text("a"), value::float_new(1.5)]),
            ],
        );
        assert!(!exc::pending(), "insert failed: {:?}", pending_text());
        assert_eq!(
            value::as_integer(value::instance_attr(cursor, "rowcount").unwrap()),
            Some(1)
        );
        assert_eq!(
            value::as_integer(value::instance_attr(cursor, "lastrowid").unwrap()),
            Some(1)
        );

        let cursor = method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT id, name, weight, NULL FROM t")],
        );
        assert!(!exc::pending(), "select failed: {:?}", pending_text());
        let description = value::instance_attr(cursor, "description").unwrap();
        let columns = value::as_tuple(description).expect("description tuple");
        assert_eq!(columns.len(), 4);
        let first = value::as_tuple(columns[0]).expect("descriptor tuple");
        assert_eq!(first.len(), 7);
        assert_eq!(value::as_str(first[0]), Some("id"));
        assert_eq!(first[1], NONE);

        let row = method("sqlite3.Cursor.fetchone", &[cursor]);
        let fields = value::as_tuple(row).expect("row tuple");
        assert_eq!(value::as_integer(fields[0]), Some(1));
        assert_eq!(value::as_str(fields[1]), Some("a"));
        assert_eq!(value::as_float(fields[2]), Some(1.5));
        assert_eq!(fields[3], NONE);
        assert_eq!(method("sqlite3.Cursor.fetchone", &[cursor]), NONE);

        // update through executemany
        let sets = value::list_new(vec![
            value::tuple_new(vec![text("b"), int(2)]),
            value::tuple_new(vec![text("c"), int(3)]),
        ]);
        let cursor = method(
            "sqlite3.Connection.executemany",
            &[
                conn,
                text("INSERT INTO t (name, weight) VALUES (?, ?)"),
                sets,
            ],
        );
        assert!(!exc::pending(), "executemany failed: {:?}", pending_text());
        assert_eq!(
            value::as_integer(value::instance_attr(cursor, "rowcount").unwrap()),
            Some(2)
        );

        let cursor = method(
            "sqlite3.Connection.execute",
            &[
                conn,
                text("UPDATE t SET name = ? WHERE id = ?"),
                value::tuple_new(vec![text("z"), int(1)]),
            ],
        );
        assert!(!exc::pending(), "update failed: {:?}", pending_text());
        assert_eq!(
            value::as_integer(value::instance_attr(cursor, "rowcount").unwrap()),
            Some(1)
        );

        let cursor = method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT name FROM t ORDER BY id")],
        );
        let all = rows(method("sqlite3.Cursor.fetchall", &[cursor]));
        let names: Vec<String> = all
            .into_iter()
            .map(|row| {
                let fields = value::as_tuple(row).expect("row");
                value::as_str(fields[0]).expect("name").to_string()
            })
            .collect();
        assert_eq!(names, vec!["z", "b", "c"]);

        method("sqlite3.Connection.close", &[conn]);
        assert!(!exc::pending());
    }

    #[test]
    fn rollback_context_manager_and_in_transaction() {
        exc::init();
        let conn = connection();
        method(
            "sqlite3.Connection.execute",
            &[conn, text("CREATE TABLE t (a INTEGER)")],
        );
        assert!(!exc::pending(), "{:?}", pending_text());

        method(
            "sqlite3.Connection.execute",
            &[conn, text("INSERT INTO t VALUES (1)")],
        );
        assert!(!exc::pending(), "{:?}", pending_text());
        assert_eq!(value::instance_attr(conn, "in_transaction").unwrap(), TRUE);

        method("sqlite3.Connection.rollback", &[conn]);
        assert_eq!(value::instance_attr(conn, "in_transaction").unwrap(), FALSE);

        let cursor = method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT COUNT(*) FROM t")],
        );
        let row = value::as_tuple(method("sqlite3.Cursor.fetchone", &[cursor])).expect("row");
        assert_eq!(value::as_integer(row[0]), Some(0));

        // A failing `with` body rolls back; a successful one commits.
        method(
            "sqlite3.Connection.execute",
            &[conn, text("INSERT INTO t VALUES (2)")],
        );
        assert_eq!(value::instance_attr(conn, "in_transaction").unwrap(), TRUE);
        let failure = method("sqlite3.Connection.__exit__", &[conn, int(1), NONE, NONE]);
        assert_eq!(failure, FALSE);
        assert_eq!(value::instance_attr(conn, "in_transaction").unwrap(), FALSE);
        let cursor = method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT COUNT(*) FROM t")],
        );
        let row = value::as_tuple(method("sqlite3.Cursor.fetchone", &[cursor])).expect("row");
        assert_eq!(value::as_integer(row[0]), Some(0));

        method(
            "sqlite3.Connection.execute",
            &[conn, text("INSERT INTO t VALUES (3)")],
        );
        let success = method("sqlite3.Connection.__exit__", &[conn, NONE, NONE, NONE]);
        assert_eq!(success, FALSE);
        assert_eq!(value::instance_attr(conn, "in_transaction").unwrap(), FALSE);
        let cursor = method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT COUNT(*) FROM t")],
        );
        let row = value::as_tuple(method("sqlite3.Cursor.fetchone", &[cursor])).expect("row");
        assert_eq!(value::as_integer(row[0]), Some(1));
    }

    #[test]
    fn parameter_styles_and_binding_errors() {
        exc::init();
        let conn = connection();
        method(
            "sqlite3.Connection.execute",
            &[conn, text("CREATE TABLE t (a INTEGER, b TEXT)")],
        );
        assert!(!exc::pending());

        method(
            "sqlite3.Connection.execute",
            &[
                conn,
                text("INSERT INTO t VALUES (?, ?)"),
                value::list_new(vec![int(1), text("x")]),
            ],
        );
        assert!(!exc::pending(), "{:?}", pending_text());

        // Named binding: `:a` selects the row inserted above (value 1); the
        // unused `b` entry proves extra mapping keys are ignored.
        let mapping = value::dict_new(vec![(text("a"), int(1)), (text("b"), text("y"))]);
        let cursor = method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT a FROM t WHERE a = :a"), mapping],
        );
        let row = value::as_tuple(method("sqlite3.Cursor.fetchone", &[cursor])).expect("row");
        assert_eq!(value::as_integer(row[0]), Some(1));
        let _ = cursor;

        // `$name` is accepted like `:name`; the statement returns the bound
        // values directly, so this does not depend on table contents.
        let cursor = method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT :a, $b"), mapping],
        );
        let row = value::as_tuple(method("sqlite3.Cursor.fetchone", &[cursor])).expect("row");
        assert_eq!(value::as_integer(row[0]), Some(1));
        assert_eq!(value::as_str(row[1]), Some("y"));
        let _ = cursor;

        method(
            "sqlite3.Connection.execute",
            &[
                conn,
                text("SELECT ?"),
                value::tuple_new(vec![int(1), int(2)]),
            ],
        );
        let message = expect_error("ProgrammingError");
        assert!(
            message.contains("Incorrect number of bindings supplied"),
            "{message}"
        );

        method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT ?"), text("not a sequence")],
        );
        assert_eq!(
            expect_error("ProgrammingError"),
            "parameters are of unsupported type"
        );

        method(
            "sqlite3.Connection.execute",
            &[
                conn,
                text("SELECT :a"),
                value::dict_new(vec![(text("b"), int(1))]),
            ],
        );
        let message = expect_error("ProgrammingError");
        assert!(message.contains("did not supply a value"), "{message}");

        method(
            "sqlite3.Connection.execute",
            &[
                conn,
                text("SELECT ?"),
                value::tuple_new(vec![value::dict_new(Vec::new())]),
            ],
        );
        let message = expect_error("ProgrammingError");
        assert!(message.contains("is not supported"), "{message}");
    }

    #[test]
    fn executemany_binds_each_row_and_reports_rowcount() {
        exc::init();
        let conn = connection();
        method(
            "sqlite3.Connection.execute",
            &[conn, text("CREATE TABLE t (a INTEGER, b TEXT)")],
        );
        assert!(!exc::pending(), "{:?}", pending_text());
        let batch = value::list_new(vec![
            value::tuple_new(vec![int(1), text("a")]),
            value::tuple_new(vec![int(2), text("b")]),
            value::tuple_new(vec![int(3), text("c")]),
        ]);
        let cursor = method(
            "sqlite3.Connection.executemany",
            &[conn, text("INSERT INTO t VALUES (?, ?)"), batch],
        );
        assert!(!exc::pending(), "{:?}", pending_text());
        assert_eq!(
            value::as_integer(value::instance_attr(cursor, "rowcount").unwrap()),
            Some(3)
        );

        let cursor = method(
            "sqlite3.Connection.executemany",
            &[
                conn,
                text("SELECT ?"),
                value::list_new(vec![value::tuple_new(vec![int(1)])]),
            ],
        );
        assert_eq!(
            expect_error("ProgrammingError"),
            "executemany() can only execute DML statements."
        );
        let _ = cursor;

        // fetchmany honours the default arraysize (1) and an explicit size.
        let cursor = method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT a FROM t ORDER BY a")],
        );
        let pair = rows(method("sqlite3.Cursor.fetchmany", &[cursor, int(2)]));
        assert_eq!(pair.len(), 2);
        let single = rows(method("sqlite3.Cursor.fetchmany", &[cursor]));
        assert_eq!(single.len(), 1);
        assert_eq!(method("sqlite3.Cursor.fetchone", &[cursor]), NONE);

        // The cursor is its own iterator and raises StopIteration at the end.
        let cursor = method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT a FROM t ORDER BY a")],
        );
        assert_eq!(method("sqlite3.Cursor.__iter__", &[cursor]), cursor);
        let first = method("sqlite3.Cursor.__next__", &[cursor]);
        assert!(value::as_tuple(first).is_some());
        let _ = method("sqlite3.Cursor.__next__", &[cursor]);
        let _ = method("sqlite3.Cursor.__next__", &[cursor]);
        let _ = method("sqlite3.Cursor.__next__", &[cursor]);
        assert_eq!(
            exc::pending_tag(),
            i64::from(exc::STOP_ITERATION),
            "iterator did not stop"
        );
        exc::clear();
    }

    #[test]
    fn errors_map_onto_dbapi_classes() {
        exc::init();
        let conn = connection();
        method(
            "sqlite3.Connection.execute",
            &[conn, text("CREATE TABLE t (id INTEGER PRIMARY KEY)")],
        );
        method(
            "sqlite3.Connection.execute",
            &[conn, text("INSERT INTO t VALUES (1)")],
        );
        assert!(!exc::pending(), "{:?}", pending_text());

        method(
            "sqlite3.Connection.execute",
            &[conn, text("INSERT INTO t VALUES (1)")],
        );
        assert!(expect_error("IntegrityError").contains("UNIQUE constraint failed"));

        method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT * FROM missing")],
        );
        assert!(expect_error("OperationalError").contains("no such table"));

        method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT 1; SELECT 2")],
        );
        let message = expect_error("ProgrammingError");
        assert_eq!(message, "You can only execute one statement at a time.");

        let cursor = method("sqlite3.Connection.cursor", &[conn]);
        method("sqlite3.Cursor.close", &[cursor]);
        method("sqlite3.Cursor.fetchone", &[cursor]);
        assert_eq!(
            expect_error("ProgrammingError"),
            "Cannot operate on a closed cursor."
        );

        method("sqlite3.Connection.close", &[conn]);
        method("sqlite3.Connection.execute", &[conn, text("SELECT 1")]);
        assert_eq!(
            expect_error("ProgrammingError"),
            "Cannot operate on a closed database."
        );
    }

    #[test]
    fn create_function_is_a_real_udf() {
        exc::init();
        let conn = connection();
        // A real native callable (`math.sqrt`) as the SQL function.
        let function = value::native_function_new("math", "sqrt", 1);
        assert!(is_callable(function));
        let result = create_function(&[conn, text("mysqrt"), int(1), function], &[]);
        assert!(
            !exc::pending(),
            "create_function failed: {:?}",
            pending_text()
        );
        assert_eq!(result, NONE);

        let cursor = method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT mysqrt(16)")],
        );
        let row = value::as_tuple(method("sqlite3.Cursor.fetchone", &[cursor])).expect("row");
        assert_eq!(value::as_float(row[0]), Some(4.0));

        // `math.sqrt(-1)` raises inside the callback; CPython reports the
        // generic OperationalError and clears the Python exception.
        create_function(&[conn, text("mysqrt"), int(1), function], &[]);
        assert!(!exc::pending(), "{:?}", pending_text());
        method(
            "sqlite3.Connection.execute",
            &[conn, text("SELECT mysqrt(-1)")],
        );
        assert_eq!(
            expect_error("OperationalError"),
            "user-defined function raised exception"
        );
    }

    #[test]
    fn connect_accepts_keywords_and_autocommit_isolation_level() {
        exc::init();
        let keyword_timeout = call_kw(
            "connect",
            &[text(":memory:")],
            &[("timeout".to_string(), value::float_new(1.0))],
        );
        assert!(
            !exc::pending(),
            "keyword connect failed: {:?}",
            pending_text()
        );

        // `isolation_level=None` disables the implicit BEGIN on DML.
        let autocommit = call_kw(
            "connect",
            &[text(":memory:")],
            &[("isolation_level".to_string(), NONE)],
        );
        assert!(!exc::pending(), "{:?}", pending_text());
        assert_eq!(
            value::instance_attr(autocommit, "isolation_level").unwrap(),
            NONE
        );
        method(
            "sqlite3.Connection.execute",
            &[autocommit, text("CREATE TABLE t (a INTEGER)")],
        );
        method(
            "sqlite3.Connection.execute",
            &[autocommit, text("INSERT INTO t VALUES (1)")],
        );
        assert!(!exc::pending(), "{:?}", pending_text());
        assert_eq!(
            value::instance_attr(autocommit, "in_transaction").unwrap(),
            FALSE
        );
        let cursor = method(
            "sqlite3.Connection.execute",
            &[autocommit, text("SELECT COUNT(*) FROM t")],
        );
        let row = value::as_tuple(method("sqlite3.Cursor.fetchone", &[cursor])).expect("row");
        assert_eq!(value::as_integer(row[0]), Some(1));

        // Unknown and duplicated keyword names are rejected.
        call_kw(
            "connect",
            &[text(":memory:")],
            &[("bogus".to_string(), int(1))],
        );
        assert_eq!(
            expect_error("TypeError"),
            "connect() got an unexpected keyword argument 'bogus'"
        );
        call_kw(
            "connect",
            &[text(":memory:")],
            &[("database".to_string(), text(":memory:"))],
        );
        assert_eq!(
            expect_error("TypeError"),
            "connect() got multiple values for argument 'database'"
        );
        let _ = keyword_timeout;
    }

    #[test]
    fn connection_statements_and_module_constants() {
        exc::init();
        assert_eq!(
            constant("apilevel").map(value::to_string),
            Some("2.0".to_string())
        );
        assert_eq!(
            constant("paramstyle").map(value::to_string),
            Some("qmark".to_string())
        );
        assert_eq!(
            constant("threadsafety").and_then(value::as_integer),
            Some(3)
        );
        let version = constant("sqlite_version").expect("sqlite_version");
        assert!(!value::to_string(version).is_empty());
        let info = constant("sqlite_version_info").expect("version_info");
        assert_eq!(value::as_tuple(info).map(<[i64]>::len), Some(3));
        assert_eq!(complete_statement(&[text("SELECT 1;")]), TRUE);
        assert_eq!(complete_statement(&[text("SELECT 1")]), FALSE);

        let conn = connection();
        assert_eq!(method("sqlite3.Connection.__enter__", &[conn]), conn);
    }
}
