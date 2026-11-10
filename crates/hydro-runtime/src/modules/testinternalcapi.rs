//! `_testinternalcapi`: the internal interpreter state this runtime really has.
//!
//! Hydro is a single-interpreter, tagged-value, mark-&-sweep runtime. The
//! module therefore reports only state that exists:
//!
//! * `get_recursion_depth`/`get_recursion_remaining`/`get_recursion_limit`
//!   read the live frame stack and the process-wide limit that `sys` owns;
//!   `set_recursion_limit` writes the same setting through the `sys` path.
//! * `get_configs`/`set_config` describe the fixed interpreter configuration
//!   (no isolated mode, hash randomisation disabled, UTF-8 source) and the one
//!   writable option, `int_max_str_digits`.
//! * `get_rare_event_counters` exposes real counters for the events this
//!   module itself performs; the interpreter never specialises bytecode, so
//!   the adaptation counters stay zero.
//! * `write_perf_map_entry`/`perf_map_state_teardown` maintain the real Linux
//!   `/tmp/perf-<pid>.map` file.
//! * `normalize_path` ports CPython's `_Py_normpath` (POSIX branch).
//! * interpreter-ID bookkeeping models the single interpreter that exists;
//!   creating or running a subinterpreter raises `NotImplementedError`
//!   because Hydro links modules statically and has no second interpreter.
//! * hardware-stack pointers, CPython bytecode/TLBC and uop inspection raise
//!   `NotImplementedError` instead of inventing addresses or instruction
//!   streams.
//!
//! `SIZEOF_PYGC_HEAD` is `0`: the Hydro collector stores no CPython GC header
//! per object. `SIZEOF_PYOBJECT` reports the real Hydro heap-object header
//! size.

use std::io::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::ops;
use crate::value::{self, NONE};

// -------------------------------------------------------------------------
// Module constants
// -------------------------------------------------------------------------

/// Size of CPython's `PyGC_Head`. Hydro's collector keeps object metadata in
/// the `HydroObj` header and has no separate GC header, so this is `0`.
pub(crate) const SIZEOF_PYGC_HEAD: i64 = 0;

/// CPython's `SIZEOF_MANAGED_PRE_HEADER`. Hydro has no managed pre-header.
pub(crate) const SIZEOF_MANAGED_PRE_HEADER: i64 = 0;

/// Size of the `HydroObj` header (`kind`, `marked`, `class`, `data`).
pub(crate) const SIZEOF_PYOBJECT: i64 = 16;

/// CPython monitoring records do not exist in this runtime.
pub(crate) const SIZEOF_PYTHON_RECORD: i64 = 0;

/// No tier-2 executor, specialisation or shared-key dict exists.
pub(crate) const NO_ADAPTATION: i64 = 0;

pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "SIZEOF_PYGC_HEAD" => ops::int_result(SIZEOF_PYGC_HEAD),
        "SIZEOF_MANAGED_PRE_HEADER" => ops::int_result(SIZEOF_MANAGED_PRE_HEADER),
        "SIZEOF_PYOBJECT" => ops::int_result(SIZEOF_PYOBJECT),
        "SIZEOF_PYTHON_RECORD" => ops::int_result(SIZEOF_PYTHON_RECORD),
        "SIZEOF_TIME_T" => ops::int_result(std::mem::size_of::<libc::time_t>() as i64),
        "SIZEOF_PYHASH" => ops::int_result(std::mem::size_of::<isize>() as i64),
        "TIER2_THRESHOLD" => ops::int_result(NO_ADAPTATION),
        "SPECIALIZATION_THRESHOLD" => ops::int_result(NO_ADAPTATION),
        "SPECIALIZATION_COOLDOWN" => ops::int_result(NO_ADAPTATION),
        "SHARED_KEYS_MAX_SIZE" => ops::int_result(NO_ADAPTATION),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_testinternalcapi' has no attribute '{name}'"),
        ),
    }
}

// -------------------------------------------------------------------------
// Dictionary helpers
// -------------------------------------------------------------------------

fn dict_of(pairs: Vec<(&str, i64)>) -> i64 {
    let dict = value::dict_new(Vec::new());
    let dict_root = crate::heap::root_values(std::slice::from_ref(&dict));
    for (name, item) in pairs {
        let key = value::str_alloc(name.to_string());
        let roots = crate::heap::root_values(&[key, item]);
        crate::container::dict_set(dict, key, item);
        crate::heap::release_roots(roots);
    }
    crate::heap::release_roots(dict_root);
    dict
}

fn list_of(items: Vec<i64>) -> i64 {
    let roots = crate::heap::root_values(&items);
    let list = value::list_new(items);
    crate::heap::release_roots(roots);
    list
}

fn num(n: i64) -> i64 {
    ops::int_result(n)
}

fn bool_value(b: bool) -> i64 {
    value::bool_value(b)
}

fn argument(args: &[i64], index: usize) -> i64 {
    args.get(index).copied().unwrap_or(NONE)
}

fn int_argument(args: &[i64], index: usize, function: &str) -> i64 {
    let argument = argument(args, index);
    if let Some(n) = value::as_integer(argument) {
        return ops::int_result(n);
    }
    if value::to_bigint(argument).is_some() {
        return exc::raise_with_message(
            OVERFLOW_ERROR,
            &format!("{function}: integer does not fit in a C ssize_t"),
        );
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(argument)
        ),
    )
}

fn string_argument(args: &[i64], index: usize, function: &str) -> Option<String> {
    match value::as_str(argument(args, index)) {
        Some(text) => Some(text.to_string()),
        None => {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{function}(): argument {} must be str, not {}",
                    index + 1,
                    value::type_name(argument(args, index))
                ),
            );
            None
        }
    }
}

// -------------------------------------------------------------------------
// Recursion state
// -------------------------------------------------------------------------

fn recursion_limit() -> i64 {
    let limit = super::call_native("sys", "getrecursionlimit", 0, std::ptr::null());
    if exc::pending() {
        exc::clear();
        return 1000;
    }
    value::as_integer(limit).unwrap_or(1000)
}

fn recursion_depth() -> i64 {
    let mut depth = 0i64;
    while depth < 1_000_000 && exc::frame_at(depth as usize).is_some() {
        depth += 1;
    }
    depth
}

/// POSIX `_Py_normpath` (see CPython `Python/fileutils.c`).
fn normpath(path: &str) -> String {
    let mut buf: Vec<char> = path.chars().collect();
    if buf.is_empty() {
        return String::new();
    }
    let len = buf.len();
    fn is_sep(buf: &[char], index: usize, len: usize) -> bool {
        index < len && buf[index] == '/'
    }
    fn sep_or_end(buf: &[char], index: usize, len: usize) -> bool {
        index >= len || buf[index] == '/'
    }

    let rootsize = if !is_sep(&buf, 0, len) {
        0
    } else if !is_sep(&buf, 1, len) || is_sep(&buf, 2, len) {
        1
    } else {
        2
    };

    let mut p1 = rootsize;
    let mut p2 = rootsize;
    let min_p2 = if rootsize > 0 { rootsize - 1 } else { 0 };
    let mut last_c = if rootsize > 0 { buf[min_p2] } else { '\0' };

    // Skip a leading "./".
    if p1 < len && buf[p1] == '.' && sep_or_end(&buf, p1 + 1, len) {
        p1 += 1;
        last_c = if p1 < len { buf[p1] } else { '\0' };
        while is_sep(&buf, p1, len) {
            p1 += 1;
        }
    }

    while p1 < len {
        let c = buf[p1];
        if last_c == '/' {
            if c == '.' {
                let sep_at_1 = sep_or_end(&buf, p1 + 1, len);
                let sep_at_2 = !sep_at_1 && sep_or_end(&buf, p1 + 2, len);
                if sep_at_2 && p1 + 1 < len && buf[p1 + 1] == '.' {
                    let mut p3 = p2;
                    loop {
                        if p3 == min_p2 {
                            break;
                        }
                        p3 -= 1;
                        if buf[p3] != '/' {
                            break;
                        }
                    }
                    while p3 != min_p2 && buf[p3 - 1] != '/' {
                        p3 -= 1;
                    }
                    let previous_is_parent = p3 + 1 < len
                        && buf[p3] == '.'
                        && buf[p3 + 1] == '.'
                        && sep_or_end(&buf, p3 + 2, len);
                    if p2 == min_p2 || previous_is_parent {
                        buf[p2] = '.';
                        p2 += 1;
                        buf[p2] = '.';
                        p2 += 1;
                        last_c = '.';
                    } else if buf[p3] == '/' {
                        p2 = p3 + 1;
                    } else {
                        p2 = p3;
                    }
                    p1 += 2;
                    continue;
                } else if sep_at_1 {
                    // skip "."
                } else {
                    buf[p2] = c;
                    p2 += 1;
                    last_c = c;
                }
            } else if c == '/' {
                // collapse repeated separators
            } else {
                buf[p2] = c;
                p2 += 1;
                last_c = c;
            }
        } else {
            buf[p2] = c;
            p2 += 1;
            last_c = c;
        }
        p1 += 1;
    }

    if p2 != min_p2 {
        let mut q = p2;
        loop {
            if q == 0 {
                break;
            }
            q -= 1;
            if q == min_p2 {
                break;
            }
            if buf[q] != '/' {
                break;
            }
        }
        buf.truncate(q + 1);
    } else {
        buf.truncate(min_p2);
    }
    buf.into_iter().collect()
}

// -------------------------------------------------------------------------
// Rare-event counters
// -------------------------------------------------------------------------

static RARE_SET_CLASS: AtomicU64 = AtomicU64::new(0);
static RARE_SET_BASES: AtomicU64 = AtomicU64::new(0);
static RARE_SET_EVAL_FRAME_FUNC: AtomicU64 = AtomicU64::new(0);
static RARE_BUILTIN_DICT: AtomicU64 = AtomicU64::new(0);
static RARE_FUNC_MODIFICATION: AtomicU64 = AtomicU64::new(0);

const EVAL_RECORD_KEY: &str = "_testinternalcapi.eval_record";

fn rare_event_counters() -> i64 {
    dict_of(vec![
        (
            "set_class",
            num(RARE_SET_CLASS.load(Ordering::Relaxed) as i64),
        ),
        (
            "set_bases",
            num(RARE_SET_BASES.load(Ordering::Relaxed) as i64),
        ),
        (
            "set_eval_frame_func",
            num(RARE_SET_EVAL_FRAME_FUNC.load(Ordering::Relaxed) as i64),
        ),
        (
            "builtin_dict",
            num(RARE_BUILTIN_DICT.load(Ordering::Relaxed) as i64),
        ),
        (
            "func_modification",
            num(RARE_FUNC_MODIFICATION.load(Ordering::Relaxed) as i64),
        ),
    ])
}

// -------------------------------------------------------------------------
// Perf map
// -------------------------------------------------------------------------

static PERF_MAP: Mutex<Option<std::fs::File>> = Mutex::new(None);

fn perf_map_path() -> String {
    if cfg!(target_os = "linux") {
        format!("/tmp/perf-{}.map", std::process::id())
    } else {
        std::env::temp_dir()
            .join(format!("perf-{}.map", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }
}

fn write_perf_map_entry(args: &[i64]) -> i64 {
    let address = match value::as_integer(argument(args, 0)) {
        Some(address) => address as u64,
        None => {
            return exc::raise_with_message(
                TYPE_ERROR,
                "write_perf_map_entry: code address must be an integer",
            )
        }
    };
    let size = match value::as_integer(argument(args, 1)) {
        Some(size) => size as u32,
        None => {
            return exc::raise_with_message(
                TYPE_ERROR,
                "write_perf_map_entry: code size must be an integer",
            )
        }
    };
    let Some(name) = string_argument(args, 2, "write_perf_map_entry") else {
        return NONE;
    };
    let mut guard = match PERF_MAP.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_none() {
        let path = perf_map_path();
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => *guard = Some(file),
            Err(error) => {
                return exc::raise_with_message(
                    exc::OS_ERROR,
                    &format!("cannot open {path}: {error}"),
                )
            }
        }
    }
    let file = guard.as_mut().expect("perf map file is open");
    match writeln!(file, "{address:x} {size:x} {name}") {
        Ok(()) => num(0),
        Err(error) => exc::raise_with_message(
            exc::OS_ERROR,
            &format!("cannot write the perf map: {error}"),
        ),
    }
}

// -------------------------------------------------------------------------
// Interpreter IDs (single interpreter model)
// -------------------------------------------------------------------------

static NEXT_INTERPRETER_ID: AtomicI64 = AtomicI64::new(1);
static MAIN_INTERPRETER_REFS: AtomicI64 = AtomicI64::new(1);
static MAIN_LINKED: Mutex<bool> = Mutex::new(false);

fn interpreter_id(value: i64) -> Option<i64> {
    value::as_integer(value)
}

fn interpreter_exists(id: i64) -> i64 {
    bool_value(id == 0)
}

fn get_interpreter_refcount(id: i64) -> i64 {
    if id == 0 {
        return num(MAIN_INTERPRETER_REFS.load(Ordering::Relaxed));
    }
    exc::raise_with_message(
        VALUE_ERROR,
        &format!("interpreter id {id} does not exist in this runtime"),
    )
}

fn link_interpreter_refcount(id: i64) -> i64 {
    if id != 0 {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("interpreter id {id} does not exist in this runtime"),
        );
    }
    let mut linked = match MAIN_LINKED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !*linked {
        *linked = true;
        MAIN_INTERPRETER_REFS.fetch_add(1, Ordering::Relaxed);
    }
    NONE
}

fn unlink_interpreter_refcount(id: i64) -> i64 {
    if id != 0 {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("interpreter id {id} does not exist in this runtime"),
        );
    }
    let mut linked = match MAIN_LINKED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if *linked {
        *linked = false;
        MAIN_INTERPRETER_REFS.fetch_sub(1, Ordering::Relaxed);
    }
    NONE
}

fn interpreter_refcount_linked(id: i64) -> i64 {
    if id != 0 {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("interpreter id {id} does not exist in this runtime"),
        );
    }
    let linked = match MAIN_LINKED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    bool_value(*linked)
}

fn interp_settings() -> i64 {
    dict_of(vec![
        ("isolated", bool_value(true)),
        ("use_main_obmalloc", bool_value(false)),
        ("allow_fork", bool_value(true)),
        ("allow_exec", bool_value(true)),
        ("allow_threads", bool_value(true)),
        ("allow_daemon_threads", bool_value(true)),
        ("check_multi_interp_extensions", bool_value(false)),
        ("gil", value::str_alloc("shared".to_string())),
    ])
}

fn pystate_getmodule(name: &str) -> i64 {
    let native = hydro_ir::stdlib::module(name).is_some();
    let loaded = globals::lookup(name, "__name__").is_some();
    if native || loaded {
        super::module_value_by_name(name)
    } else {
        NONE
    }
}

// -------------------------------------------------------------------------
// Configuration
// -------------------------------------------------------------------------

fn sys_int_max_str_digits() -> i64 {
    let value_arg = super::call_native("sys", "get_int_max_str_digits", 0, std::ptr::null());
    if exc::pending() {
        exc::clear();
        return num(0);
    }
    value_arg
}

fn optional_sys(name: &str) -> i64 {
    let value_arg = super::module_get_by_name("sys", name);
    if exc::pending() {
        exc::clear();
        NONE
    } else {
        value_arg
    }
}

fn config_dict() -> i64 {
    let argv = optional_sys("argv");
    let executable = optional_sys("executable");
    let path = optional_sys("path");
    let config = dict_of(vec![
        ("use_hash_seed", bool_value(false)),
        ("hash_seed", num(0)),
        ("int_max_str_digits", sys_int_max_str_digits()),
        ("isolated", num(0)),
        ("use_environment", num(1)),
        ("dev_mode", num(0)),
        ("parse_argv", num(0)),
        ("optimization_level", num(0)),
        ("inspect", num(0)),
        ("interactive", num(0)),
        ("verbose", num(0)),
        ("bytes_warning", num(0)),
        ("write_bytecode", num(1)),
        ("user_site_directory", num(1)),
        ("buffered_stdio", num(1)),
        ("configure_c_stdio", num(0)),
        ("safe_path", num(0)),
        ("pathconfig_warnings", num(1)),
        ("use_frozen_modules", num(0)),
        (
            "check_hash_pycs_mode",
            value::str_alloc("default".to_string()),
        ),
        ("stdio_encoding", value::str_alloc("utf-8".to_string())),
        ("stdio_errors", value::str_alloc("strict".to_string())),
        ("program_name", value::str_alloc("hydro".to_string())),
        ("executable", executable),
        ("module_search_paths", path),
        ("argv", argv),
    ]);
    let config_root = crate::heap::root_values(std::slice::from_ref(&config));
    let global_config = dict_of(vec![
        ("use_hash_seed", bool_value(false)),
        ("hash_seed", num(0)),
        ("isolated", num(0)),
        ("use_environment", num(1)),
        ("dev_mode", num(0)),
        ("parse_argv", num(0)),
        ("configure_c_stdio", num(0)),
        ("coerce_c_locale", num(0)),
        ("coerce_c_locale_warn", num(0)),
        ("inspect", num(0)),
        ("interactive", num(0)),
        ("optimization_level", num(0)),
        ("parser_debug", num(0)),
        ("verbose", num(0)),
        ("warn_default_encoding", num(0)),
        ("bytes_warning", num(0)),
        ("quiet", num(0)),
        ("user_site_directory", num(1)),
        ("write_bytecode", num(1)),
        ("safe_path", num(0)),
        ("pathconfig_warnings", num(1)),
        ("use_frozen_modules", num(0)),
        ("module_search_paths_set", bool_value(false)),
    ]);
    let global_root = crate::heap::root_values(std::slice::from_ref(&global_config));
    let pre_config = dict_of(vec![
        ("isolated", num(0)),
        ("use_environment", num(1)),
        ("dev_mode", num(0)),
        ("utf8_mode", num(0)),
        ("coerce_c_locale", num(0)),
        ("coerce_c_locale_warn", num(0)),
        ("parse_argv", num(0)),
    ]);
    let pre_root = crate::heap::root_values(std::slice::from_ref(&pre_config));
    let result = dict_of(vec![
        ("config", config),
        ("global_config", global_config),
        ("pre_config", pre_config),
    ]);
    crate::heap::release_roots(pre_root);
    crate::heap::release_roots(global_root);
    crate::heap::release_roots(config_root);
    result
}

fn set_config(args: &[i64]) -> i64 {
    let Some(name) = string_argument(args, 0, "set_config") else {
        return NONE;
    };
    let new_value = argument(args, 1);
    match name.as_str() {
        "int_max_str_digits" => {
            super::call_native("sys", "set_int_max_str_digits", 1, &new_value as *const i64)
        }
        "hash_seed" | "use_hash_seed" => {
            if value::as_integer(new_value) == Some(0) || new_value == value::FALSE {
                NONE
            } else {
                exc::raise_with_message(
                    NOT_IMPLEMENTED_ERROR,
                    "hash randomisation is disabled in this runtime; the seed cannot be changed",
                )
            }
        }
        _ => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            &format!("interpreter configuration option '{name}' is fixed in this runtime"),
        ),
    }
}

// -------------------------------------------------------------------------
// Dispatch
// -------------------------------------------------------------------------

/// Dispatches a native `_testinternalcapi` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "get_configs" => config_dict(),
        "set_config" => set_config(args),
        "get_hash_seed" => num(0),
        "get_recursion_depth" => num(recursion_depth()),
        "get_recursion_remaining" => {
            num(recursion_limit().saturating_sub(recursion_depth()).max(0))
        }
        "get_recursion_limit" => num(recursion_limit()),
        "set_recursion_limit" => {
            let limit = int_argument(args, 0, "set_recursion_limit");
            if exc::pending() {
                return NONE;
            }
            super::call_native("sys", "setrecursionlimit", 1, &limit as *const i64)
        }
        "get_c_recursion_remaining" => num(c_recursion_remaining()),
        "normalize_path" => {
            let Some(path) = string_argument(args, 0, "normalize_path") else {
                return NONE;
            };
            value::str_alloc(normpath(&path))
        }
        "get_rare_event_counters" => rare_event_counters(),
        "reset_rare_event_counters" => {
            RARE_SET_CLASS.store(0, Ordering::Relaxed);
            RARE_SET_BASES.store(0, Ordering::Relaxed);
            RARE_SET_EVAL_FRAME_FUNC.store(0, Ordering::Relaxed);
            RARE_BUILTIN_DICT.store(0, Ordering::Relaxed);
            RARE_FUNC_MODIFICATION.store(0, Ordering::Relaxed);
            NONE
        }
        "set_eval_frame_record" => {
            let record = argument(args, 0);
            if value::as_list(record).is_none() {
                return exc::raise_with_message(TYPE_ERROR, "argument must be a list");
            }
            globals::registry_set(EVAL_RECORD_KEY, record);
            RARE_SET_EVAL_FRAME_FUNC.fetch_add(1, Ordering::Relaxed);
            NONE
        }
        "set_eval_frame_default" => {
            globals::registry_set(EVAL_RECORD_KEY, NONE);
            RARE_SET_EVAL_FRAME_FUNC.fetch_add(1, Ordering::Relaxed);
            NONE
        }
        "has_inline_values"
        | "has_split_table"
        | "has_deferred_refcount"
        | "is_static_immortal" => value::FALSE,
        "get_static_builtin_types" | "identify_type_slot_wrappers" => list_of(Vec::new()),
        "write_perf_map_entry" => write_perf_map_entry(args),
        "perf_map_state_teardown" => {
            let mut guard = match PERF_MAP.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *guard = None;
            NONE
        }
        "get_tlbc" | "get_tlbc_id" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "this runtime does not execute CPython bytecode, so it has no thread-local bytecode",
        ),
        "get_stack_pointer" | "get_stack_margin" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "the machine stack pointer is not exposed to Python code",
        ),
        "pymem_getallocatorsname" => value::str_alloc("hydro-gc".to_string()),
        "py_thread_id" => {
            // SAFETY: gettid(2) takes no arguments and returns the caller's TID.
            let tid = unsafe { libc::syscall(libc::SYS_gettid) };
            num(tid as i64)
        }
        "interpreter_exists" => match interpreter_id(argument(args, 0)) {
            Some(id) => interpreter_exists(id),
            None => value::FALSE,
        },
        "next_interpreter_id" | "unused_interpreter_id" => {
            num(NEXT_INTERPRETER_ID.fetch_add(1, Ordering::Relaxed))
        }
        "normalize_interp_id" => {
            let candidate = argument(args, 0);
            match value::as_integer(candidate) {
                Some(id) if id >= 0 => num(id),
                Some(id) => exc::raise_with_message(
                    VALUE_ERROR,
                    &format!("interpreter id {id} is not valid"),
                ),
                None => exc::raise_with_message(TYPE_ERROR, "interpreter id must be an integer"),
            }
        }
        "get_interpreter_refcount" => match interpreter_id(argument(args, 0)) {
            Some(id) => get_interpreter_refcount(id),
            None => exc::raise_with_message(TYPE_ERROR, "interpreter id must be an integer"),
        },
        "link_interpreter_refcount" => match interpreter_id(argument(args, 0)) {
            Some(id) => link_interpreter_refcount(id),
            None => exc::raise_with_message(TYPE_ERROR, "interpreter id must be an integer"),
        },
        "unlink_interpreter_refcount" => match interpreter_id(argument(args, 0)) {
            Some(id) => unlink_interpreter_refcount(id),
            None => exc::raise_with_message(TYPE_ERROR, "interpreter id must be an integer"),
        },
        "interpreter_refcount_linked" => match interpreter_id(argument(args, 0)) {
            Some(id) => interpreter_refcount_linked(id),
            None => exc::raise_with_message(TYPE_ERROR, "interpreter id must be an integer"),
        },
        "get_interp_settings" => match interpreter_id(argument(args, 0)) {
            Some(0) => interp_settings(),
            Some(id) => exc::raise_with_message(
                VALUE_ERROR,
                &format!("interpreter id {id} does not exist in this runtime"),
            ),
            None => exc::raise_with_message(TYPE_ERROR, "interpreter id must be an integer"),
        },
        "create_interpreter"
        | "destroy_interpreter"
        | "exec_interpreter"
        | "run_in_subinterp_with_config" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "Hydro runs a single interpreter: modules are linked statically and \
             no subinterpreter can be created or executed",
        ),
        "pystate_getmodule" => {
            let Some(name) = string_argument(args, 0, "pystate_getmodule") else {
                return NONE;
            };
            pystate_getmodule(&name)
        }
        "pystate_addmodule_run" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "module init functions are registered statically; there is no separate init run",
        ),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_testinternalcapi' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware dispatch. The module's own functions take no keywords; a
/// non-empty keyword list is therefore an error unless the name is unknown.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if keywords.is_empty() {
        return call(function, args);
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "{function}() got an unexpected keyword argument '{}'",
            keywords[0].0
        ),
    )
}

/// Hardware-stack estimate in units of 50 interpreter-loop frames, matching
/// CPython's `get_c_recursion_remaining` scaling. `-1` means the estimate
/// overflowed (unlimited stack).
fn c_recursion_remaining() -> i64 {
    // SAFETY: pthread_getattr_np/attr_getstack/attr_destroy operate on the
    // initialised attribute object only and report failure through the return
    // code; the address of a live local is a valid stack pointer.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return 0;
        }
        let mut stack_address: *mut libc::c_void = std::ptr::null_mut();
        let mut stack_size: libc::size_t = 0;
        let ok = libc::pthread_attr_getstack(&attr, &mut stack_address, &mut stack_size) == 0;
        libc::pthread_attr_destroy(&mut attr);
        if !ok {
            return 0;
        }
        let here = &stack_size as *const libc::size_t as usize;
        let top = stack_address as usize + stack_size;
        let available = top.saturating_sub(here);
        const MARGIN_BYTES: usize = 2048 * std::mem::size_of::<usize>();
        i64::try_from(available / MARGIN_BYTES)
            .ok()
            .and_then(|frames| frames.checked_mul(50))
            .unwrap_or(-1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normpath_matches_posix_cases() {
        for (input, expected) in [
            ("/", "/"),
            ("/.", "/"),
            ("/./", "/"),
            ("/.//.", "/"),
            ("/./foo/bar", "/foo/bar"),
            ("/foo/bar", "/foo/bar"),
            ("//", "//"),
            ("///", "/"),
            ("///foo/.//bar//", "/foo/bar"),
            ("///foo/.//bar//.//..//.//baz///", "/foo/baz"),
            ("///..//./foo/.//bar", "/foo/bar"),
            ("/..", "/"),
            ("/../foo", "/foo"),
            ("/../foo/../bar", "/bar"),
            ("//../foo", "//foo"),
            ("/foo/../..", "/"),
            ("//foo/..", "//"),
        ] {
            assert_eq!(normpath(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn sparse_constants_are_truthful() {
        exc::init();
        assert_eq!(SIZEOF_PYGC_HEAD, 0);
        assert_eq!(SIZEOF_MANAGED_PRE_HEADER, 0);
        assert_eq!(SIZEOF_PYTHON_RECORD, 0);
    }
}
