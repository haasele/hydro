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
//! * the `_PyTime_*` helpers of `Modules/_testinternalcapi/pytime.c` convert
//!   between seconds, `_PyTime_t` nanoseconds and `struct timeval`/
//!   `struct timespec` with CPython's four rounding modes and its
//!   overflow diagnostics.
//! * `EncodeLocaleEx`/`DecodeLocaleEx` run the UTF-8 filesystem codec under
//!   the error handlers CPython's locale conversion accepts (`strict` and
//!   `surrogateescape`); every other handler is the documented
//!   `ValueError("unsupported error handler")`.
//! * `_PyUnicode_TransformDecimalAndSpaceToASCII` ports CPython's loop over
//!   the Unicode decimal/whitespace tables.
//! * `get_co_framesize`/`get_co_localskinds` derive CPython's frame size and
//!   locals-plus kind bits from the code object's own `co_varnames`,
//!   `co_cellvars`, `co_freevars`, argument counts and flags.
//! * interpreter-ID bookkeeping models the single interpreter that exists;
//!   creating or running a subinterpreter raises `NotImplementedError`
//!   because Hydro links modules statically and has no second interpreter.
//! * hardware-stack pointers, CPython bytecode/TLBC and uop inspection raise
//!   `NotImplementedError` instead of inventing addresses or instruction
//!   streams. The CPython compiler and instruction-sequence entry points
//!   (`new_instruction_sequence`, `optimize_cfg`, `compiler_codegen`,
//!   `assemble_code_object`, `get_getpath_codeobject`), the bytecode-analysing
//!   code helpers (`code_returns_only_none`, `get_code_var_counts`,
//!   `verify_stateless_code`) and `_PyTraceMalloc_GetTraceback` are not
//!   module attributes at all: this runtime has no CPython instruction stream
//!   and keeps no allocation trace, so there is nothing truthful to return
//!   for them.
//!
//! `SIZEOF_PYGC_HEAD` is `0`: the Hydro collector stores no CPython GC header
//! per object. `SIZEOF_PYOBJECT` reports the real Hydro heap-object header
//! size.

use std::io::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, OVERFLOW_ERROR, RUNTIME_ERROR, SYSTEM_ERROR,
    TYPE_ERROR, VALUE_ERROR,
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
// Time conversions (`Modules/_testinternalcapi/pytime.c`)
// -------------------------------------------------------------------------

/// Nanoseconds per second, microsecond and millisecond (`pycore_time.h`).
const SEC_TO_NS: i64 = 1_000_000_000;
const SEC_TO_US: i64 = 1_000_000;
const NS_TO_US: i64 = 1_000;
const NS_TO_MS: i64 = 1_000_000;

/// `_PyTime_round_t` (`Include/internal/pycore_time.h`).
const ROUND_FLOOR: i64 = 0;
const ROUND_CEILING: i64 = 1;
const ROUND_HALF_EVEN: i64 = 2;
const ROUND_UP: i64 = 3;

/// `pytime_overflow`: the value does not fit a `_PyTime_t` (`i64`).
fn time_overflow() -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, "timestamp out of range for C PyTime_t")
}

/// `pytime_time_t_overflow`: the seconds do not fit the platform `time_t`.
fn time_t_overflow() -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, "timestamp out of range for platform time_t")
}

/// `(minimum, maximum)` of the platform's `time_t`.
fn time_t_range() -> (i64, i64) {
    if std::mem::size_of::<libc::time_t>() <= 4 {
        (i64::from(i32::MIN), i64::from(i32::MAX))
    } else {
        (i64::MIN, i64::MAX)
    }
}

/// `pytime_round`: CPython's rounding of a scaled double.
fn time_round(value: f64, round: i64) -> f64 {
    match round {
        ROUND_CEILING => value.ceil(),
        ROUND_FLOOR => value.floor(),
        ROUND_UP => {
            if value >= 0.0 {
                value.ceil()
            } else {
                value.floor()
            }
        }
        _ => {
            let rounded = value.round();
            if (value - rounded).abs() == 0.5 {
                2.0 * (value / 2.0).round()
            } else {
                rounded
            }
        }
    }
}

/// `pytime_divide_round_up`: away from zero on an inexact division.
fn time_divide_round_up(t: i64, k: i64) -> i64 {
    let quotient = t / k;
    if t % k == 0 {
        quotient
    } else if t >= 0 {
        quotient + 1
    } else {
        quotient - 1
    }
}

/// `pytime_divide`: `t / k` under `round`.
fn time_divide(t: i64, k: i64, round: i64) -> i64 {
    match round {
        ROUND_HALF_EVEN => {
            let quotient = t / k;
            let remainder = t % k;
            let magnitude = remainder.abs();
            if magnitude > k / 2 || (magnitude == k / 2 && quotient.abs() % 2 == 1) {
                if t >= 0 {
                    quotient + 1
                } else {
                    quotient - 1
                }
            } else {
                quotient
            }
        }
        ROUND_CEILING => {
            if t >= 0 {
                time_divide_round_up(t, k)
            } else {
                t / k
            }
        }
        ROUND_FLOOR => {
            if t >= 0 {
                t / k
            } else {
                time_divide_round_up(t, k)
            }
        }
        _ => time_divide_round_up(t, k),
    }
}

/// `pytime_divmod`: floor division with a non-negative remainder.
///
/// `Err(())` mirrors CPython's underflow for a `PyTime_t`-minimum quotient.
fn time_divmod(t: i64, k: i64) -> Result<(i64, i64), ()> {
    let mut quotient = t / k;
    let mut remainder = t % k;
    if remainder < 0 {
        if quotient == i64::MIN {
            return Err(());
        }
        remainder += k;
        quotient -= 1;
    }
    Ok((quotient, remainder))
}

/// `_PyTime_FromLong`'s argument conversion: `expect int, got <type>`.
fn time_integer_argument(args: &[i64], index: usize) -> Result<i64, i64> {
    let raw = argument(args, index);
    if let Some(value) = value::as_integer(raw) {
        return Ok(value);
    }
    // `PyLong_AsLongLong` reports a bigint outside the 64-bit range as an
    // overflow, which `_PyTime_FromLong` turns into a PyTime_t overflow.
    if value::to_bigint(raw).is_some() {
        return Err(time_overflow());
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!("expect int, got {}", value::type_name(raw)),
    ))
}

/// `check_time_rounding`: the rounding argument must be a `_PyTime_round_t`.
fn time_rounding(args: &[i64], index: usize) -> Result<i64, i64> {
    let round = int_argument(args, index, "pytime");
    if exc::pending() {
        return Err(NONE);
    }
    match round {
        ROUND_FLOOR | ROUND_CEILING | ROUND_HALF_EVEN | ROUND_UP => Ok(round),
        _ => Err(exc::raise_with_message(VALUE_ERROR, "invalid rounding")),
    }
}

/// `pytime_from_double`: scales a seconds value into nanoseconds and rounds.
fn time_from_double(value: f64, round: i64, unit_to_ns: i64) -> Result<i64, i64> {
    let scaled = time_round(value * unit_to_ns as f64, round);
    // The range test runs before the cast: `PyTime_t` is `i64`, and an
    // out-of-range double must raise instead of saturating.
    if !((i64::MIN as f64) <= scaled && scaled < -(i64::MIN as f64)) {
        return Err(time_overflow());
    }
    Ok(scaled as i64)
}

/// `pytime_from_object` for `_PyTime_FromSecondsObject`.
fn time_from_object(raw: i64, round: i64) -> Result<i64, i64> {
    if let Some(float) = value::as_float(raw) {
        if float.is_nan() {
            return Err(exc::raise_with_message(
                VALUE_ERROR,
                "Invalid value NaN (not a number)",
            ));
        }
        return time_from_double(float, round, SEC_TO_NS);
    }
    if let Some(seconds) = value::as_integer(raw) {
        return match seconds.checked_mul(SEC_TO_NS) {
            Some(nanoseconds) => Ok(nanoseconds),
            None => Err(time_overflow()),
        };
    }
    if value::to_bigint(raw).is_some() {
        return Err(time_overflow());
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "'{}' object cannot be interpreted as an integer or float",
            value::type_name(raw)
        ),
    ))
}

/// `pytime_as_timeval`: `(tv_sec, tv_usec)` for a nanosecond timestamp.
fn time_as_timeval(timestamp: i64, round: i64, raise_exception: bool) -> Result<(i64, i64), i64> {
    let microseconds = time_divide(timestamp, NS_TO_US, round);
    let (mut seconds, mut subsecond, underflow) = match time_divmod(microseconds, SEC_TO_US) {
        Ok((seconds, remainder)) => (seconds, remainder, false),
        Err(()) => (i64::MIN, 0, true),
    };
    let (minimum, maximum) = time_t_range();
    let fits = (minimum..=maximum).contains(&seconds);
    if !fits {
        seconds = seconds.clamp(minimum, maximum);
        subsecond = 0;
    }
    if raise_exception && (underflow || !fits) {
        return Err(time_t_overflow());
    }
    Ok((seconds, subsecond))
}

/// `pytime_as_timespec`: `(tv_sec, tv_nsec)` for a nanosecond timestamp.
fn time_as_timespec(timestamp: i64, raise_exception: bool) -> Result<(i64, i64), i64> {
    let (mut seconds, mut subsecond, underflow) = match time_divmod(timestamp, SEC_TO_NS) {
        Ok((seconds, remainder)) => (seconds, remainder, false),
        Err(()) => (i64::MIN, 0, true),
    };
    let (minimum, maximum) = time_t_range();
    let fits = (minimum..=maximum).contains(&seconds);
    if !fits {
        seconds = seconds.clamp(minimum, maximum);
        subsecond = 0;
    }
    if raise_exception && (underflow || !fits) {
        return Err(time_t_overflow());
    }
    Ok((seconds, subsecond))
}

/// `pytime_double_to_denominator`: splits seconds into `(sec, fraction)`.
fn time_double_to_denominator(value: f64, denominator: i64, round: i64) -> Result<(i64, i64), i64> {
    let mut integral = value.trunc();
    let scale = denominator as f64;
    let mut fractional = (value - integral) * scale;
    fractional = time_round(fractional, round);
    if fractional >= scale {
        fractional -= scale;
        integral += 1.0;
    } else if fractional < 0.0 {
        fractional += scale;
        integral -= 1.0;
    }
    let (minimum, _) = time_t_range();
    let (minimum_float, maximum_float) = (minimum as f64, -(minimum as f64));
    // NaN fails every comparison, exactly as CPython's range test does.
    if !(minimum_float <= integral && integral < maximum_float) {
        return Err(time_t_overflow());
    }
    Ok((integral as i64, fractional as i64))
}

/// `pytime_object_to_denominator`: `(sec, fraction)` for an int or float.
fn time_object_to_denominator(raw: i64, denominator: i64, round: i64) -> Result<(i64, i64), i64> {
    if let Some(float) = value::as_float(raw) {
        if float.is_nan() {
            return Err(exc::raise_with_message(
                VALUE_ERROR,
                "Invalid value NaN (not a number)",
            ));
        }
        return time_double_to_denominator(float, denominator, round);
    }
    let (minimum, maximum) = time_t_range();
    if let Some(seconds) = value::as_integer(raw) {
        if !(minimum..=maximum).contains(&seconds) {
            return Err(time_t_overflow());
        }
        return Ok((seconds, 0));
    }
    if value::to_bigint(raw).is_some() {
        return Err(time_t_overflow());
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "argument must be int or float, not {}",
            value::type_name(raw)
        ),
    ))
}

/// `_PyTime_ObjectToTime_t`: whole seconds as `time_t`.
fn time_object_to_time_t(raw: i64, round: i64) -> Result<i64, i64> {
    if let Some(float) = value::as_float(raw) {
        if float.is_nan() {
            return Err(exc::raise_with_message(
                VALUE_ERROR,
                "Invalid value NaN (not a number)",
            ));
        }
        let integral = time_round(float, round).trunc();
        let (minimum, _) = time_t_range();
        let (minimum_float, maximum_float) = (minimum as f64, -(minimum as f64));
        if !(minimum_float <= integral && integral < maximum_float) {
            return Err(time_t_overflow());
        }
        return Ok(integral as i64);
    }
    let (minimum, maximum) = time_t_range();
    if let Some(seconds) = value::as_integer(raw) {
        if !(minimum..=maximum).contains(&seconds) {
            return Err(time_t_overflow());
        }
        return Ok(seconds);
    }
    if value::to_bigint(raw).is_some() {
        return Err(time_t_overflow());
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(raw)
        ),
    ))
}

/// Builds the `(seconds, fraction)` result tuple, rooting both integers.
fn time_pair(first: i64, second: i64) -> i64 {
    let roots = crate::heap::root_values(&[first, second]);
    let tuple = value::tuple_new(vec![first, second]);
    crate::heap::release_roots(roots);
    tuple
}

/// `_PyTime_FromSeconds`: a C `int` number of seconds.
fn pytime_from_seconds(args: &[i64]) -> i64 {
    let raw = argument(args, 0);
    let seconds = if let Some(seconds) = value::as_integer(raw) {
        match i32::try_from(seconds) {
            Ok(seconds) => i64::from(seconds),
            Err(_) => {
                return exc::raise_with_message(
                    OVERFLOW_ERROR,
                    "Python int too large to convert to C int",
                )
            }
        }
    } else if value::to_bigint(raw).is_some() {
        return exc::raise_with_message(OVERFLOW_ERROR, "Python int too large to convert to C int");
    } else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "'{}' object cannot be interpreted as an integer",
                value::type_name(raw)
            ),
        );
    };
    num(seconds * SEC_TO_NS)
}

/// `_PyTime_FromSecondsObject`.
fn pytime_from_seconds_object(args: &[i64]) -> i64 {
    let round = match time_rounding(args, 1) {
        Ok(round) => round,
        Err(value) => return value,
    };
    match time_from_object(argument(args, 0), round) {
        Ok(nanoseconds) => num(nanoseconds),
        Err(value) => value,
    }
}

/// `_PyTime_AsMilliseconds`/`_PyTime_AsMicroseconds`.
fn pytime_as_unit(args: &[i64], unit: i64) -> i64 {
    let timestamp = match time_integer_argument(args, 0) {
        Ok(timestamp) => timestamp,
        Err(value) => return value,
    };
    let round = match time_rounding(args, 1) {
        Ok(round) => round,
        Err(value) => return value,
    };
    num(time_divide(timestamp, unit, round))
}

/// `_PyTime_AsTimeval` (raising) and `_PyTime_AsTimeval_clamp`.
fn pytime_as_timeval(args: &[i64], clamp: bool) -> i64 {
    let timestamp = match time_integer_argument(args, 0) {
        Ok(timestamp) => timestamp,
        Err(value) => return value,
    };
    let round = match time_rounding(args, 1) {
        Ok(round) => round,
        Err(value) => return value,
    };
    match time_as_timeval(timestamp, round, !clamp) {
        Ok((seconds, microseconds)) => time_pair(seconds, microseconds),
        Err(value) => value,
    }
}

/// `_PyTime_AsTimespec` (raising) and `_PyTime_AsTimespec_clamp`.
fn pytime_as_timespec(args: &[i64], clamp: bool) -> i64 {
    let timestamp = match time_integer_argument(args, 0) {
        Ok(timestamp) => timestamp,
        Err(value) => return value,
    };
    match time_as_timespec(timestamp, !clamp) {
        Ok((seconds, nanoseconds)) => time_pair(seconds, nanoseconds),
        Err(value) => value,
    }
}

/// `_PyTime_ObjectToTime_t`.
fn pytime_object_to_time_t(args: &[i64]) -> i64 {
    let round = match time_rounding(args, 1) {
        Ok(round) => round,
        Err(value) => return value,
    };
    match time_object_to_time_t(argument(args, 0), round) {
        Ok(seconds) => num(seconds),
        Err(value) => value,
    }
}

/// `_PyTime_ObjectToTimespec` and `_PyTime_ObjectToTimeval`.
fn pytime_object_to_denominator(args: &[i64], denominator: i64) -> i64 {
    let round = match time_rounding(args, 1) {
        Ok(round) => round,
        Err(value) => return value,
    };
    match time_object_to_denominator(argument(args, 0), denominator, round) {
        Ok((seconds, fraction)) => time_pair(seconds, fraction),
        Err(value) => value,
    }
}

// -------------------------------------------------------------------------
// Unicode decimal/space transformation
// -------------------------------------------------------------------------

/// `Py_UNICODE_ISSPACE`: CPython's whitespace table
/// (`_PyUnicode_IsWhitespace` in `Objects/unicodetype_db.h`).
fn is_python_space(code: u32) -> bool {
    matches!(
        code,
        0x09..=0x0D
            | 0x1C..=0x1F
            | 0x20
            | 0x85
            | 0xA0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
    )
}

/// `_PyUnicode_TransformDecimalAndSpaceToASCII`.
///
/// Every code point below U+007F is kept, any other whitespace becomes a
/// space and a decimal digit becomes its ASCII digit; the first code point
/// that is none of those is written as `?` and ends the string, exactly like
/// CPython's loop.
fn transform_decimal_and_space(args: &[i64]) -> i64 {
    let raw = argument(args, 0);
    let Some(codes) = value::as_str_codes(raw) else {
        return exc::raise_with_message(SYSTEM_ERROR, "bad argument to internal function");
    };
    if codes.iter().all(|&code| code < 0x80) {
        return raw;
    }
    let mut output: Vec<u32> = Vec::with_capacity(codes.len());
    for code in codes {
        if code < 0x7F {
            output.push(code);
        } else if is_python_space(code) {
            output.push(u32::from(b' '));
        } else {
            match super::unicodedata::numeric_entry(code) {
                Some((_, decimal, _, _)) if decimal >= 0 => {
                    output.push(u32::from(b'0') + u32::from(decimal as u8));
                }
                _ => {
                    output.push(u32::from(b'?'));
                    break;
                }
            }
        }
    }
    value::str_alloc(value::str_from_codes(output))
}

// -------------------------------------------------------------------------
// Locale encoding (`EncodeLocaleEx`/`DecodeLocaleEx`)
// -------------------------------------------------------------------------

/// The error handlers `_Py_EncodeUTF8Ex`/`_Py_DecodeUTF8Ex` accept.
#[derive(Clone, Copy, PartialEq)]
enum LocaleErrors {
    Strict,
    SurrogateEscape,
}

/// `_Py_GetErrorHandler` restricted to the locale conversion's handlers.
///
/// `None` is CPython's `-3` result, "unsupported error handler".
fn locale_errors(errors: Option<&str>) -> Option<LocaleErrors> {
    match errors {
        None | Some("strict") => Some(LocaleErrors::Strict),
        Some("surrogateescape") => Some(LocaleErrors::SurrogateEscape),
        _ => None,
    }
}

/// Resolves the optional `errors` argument (position 3, format `"s"`).
fn locale_errors_argument(args: &[i64]) -> Result<Option<String>, i64> {
    match args.get(2).copied() {
        None => Ok(None),
        Some(value) => match value::as_str(value) {
            Some(errors) => Ok(Some(errors.to_string())),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!("argument 3 must be str, not {}", value::type_name(value)),
            )),
        },
    }
}

/// Resolves the optional `current_locale` argument (position 2, format `"i"`).
///
/// The value only selects a conversion on Windows; the UTF-8 filesystem
/// encoding of this runtime is locale-independent, so it is validated and
/// otherwise ignored.
fn locale_current_locale(args: &[i64]) -> Result<(), i64> {
    match args.get(1).copied() {
        None => Ok(()),
        Some(value) => match value::as_integer(value) {
            Some(_) => Ok(()),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!("argument 2 must be int, not {}", value::type_name(value)),
            )),
        },
    }
}

/// `EncodeLocaleEx(text, current_locale=0, errors=None)`.
fn encode_locale_ex(args: &[i64]) -> i64 {
    let raw = argument(args, 0);
    if value::as_str(raw).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("argument 1 must be str, not {}", value::type_name(raw)),
        );
    }
    if let Err(value) = locale_current_locale(args) {
        return value;
    }
    let errors = match locale_errors_argument(args) {
        Ok(errors) => errors,
        Err(value) => return value,
    };
    let Some(mode) = locale_errors(errors.as_deref()) else {
        return exc::raise_with_message(VALUE_ERROR, "unsupported error handler");
    };
    let codes = value::as_str_codes(raw).unwrap_or_default();
    let encoding = value::str_alloc("utf-8".to_string());
    let errors_value = value::str_alloc(errors.unwrap_or_else(|| "strict".to_string()));
    let call_args = [raw, encoding, errors_value];
    let roots = crate::heap::root_values(&call_args);
    let encoded = super::codecs::call("encode", &call_args);
    crate::heap::release_roots(roots);
    if !exc::pending() {
        return encoded;
    }
    exc::clear();
    // The UTF-8 locale codec rejects exactly the surrogates the mode does not
    // escape; the position is the code point index CPython reports.
    let position = codes
        .iter()
        .position(|&code| {
            value::is_surrogate_code(code)
                && !(mode == LocaleErrors::SurrogateEscape && (0xDC80..=0xDCFF).contains(&code))
        })
        .unwrap_or(0);
    exc::raise_with_message(
        RUNTIME_ERROR,
        &format!("encode error: pos={position}, reason=surrogates not allowed"),
    )
}

/// `DecodeLocaleEx(bytes, current_locale=0, errors=None)`.
fn decode_locale_ex(args: &[i64]) -> i64 {
    let raw = argument(args, 0);
    let Some(data) = value::as_bytes(raw) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "argument 1 must be a bytes-like object, not {}",
                value::type_name(raw)
            ),
        );
    };
    if let Err(value) = locale_current_locale(args) {
        return value;
    }
    let errors = match locale_errors_argument(args) {
        Ok(errors) => errors,
        Err(value) => return value,
    };
    if locale_errors(errors.as_deref()).is_none() {
        return exc::raise_with_message(VALUE_ERROR, "unsupported error handler");
    }
    let bytes_value = value::bytes_new(data.to_vec());
    let encoding = value::str_alloc("utf-8".to_string());
    let errors_value = value::str_alloc(errors.unwrap_or_else(|| "strict".to_string()));
    let call_args = [bytes_value, encoding, errors_value];
    let roots = crate::heap::root_values(&call_args);
    let decoded = super::codecs::call("decode", &call_args);
    crate::heap::release_roots(roots);
    if !exc::pending() {
        return decoded;
    }
    exc::clear();
    let (position, reason) = match std::str::from_utf8(data) {
        Ok(_) => (0, "decoding error"),
        Err(error) => {
            let reason = match error.error_len() {
                None => "unexpected end of data",
                Some(1) => "invalid start byte",
                Some(_) => "invalid continuation byte",
            };
            (error.valid_up_to(), reason)
        }
    };
    exc::raise_with_message(
        RUNTIME_ERROR,
        &format!("decode error: pos={position}, reason={reason}"),
    )
}

// -------------------------------------------------------------------------
// Code-object metadata (`get_co_framesize`, `get_co_localskinds`)
// -------------------------------------------------------------------------

/// `FRAME_SPECIALS_SIZE` for x86_64's `_PyInterpreterFrame`:
/// `(sizeof(_PyInterpreterFrame) - 1) / sizeof(PyObject *)` = `(88 - 1) / 8`.
const FRAME_SPECIALS_SIZE: i64 = 10;

/// `CO_FAST_*` bits from `Include/internal/pycore_code.h`.
const CO_FAST_LOCAL: i64 = 0x20;
const CO_FAST_CELL: i64 = 0x40;
const CO_FAST_FREE: i64 = 0x80;

/// Argument-kind bits recorded in `co_localspluskinds`.
const CO_FAST_ARG_POS: i64 = 0x02;
const CO_FAST_ARG_KW: i64 = 0x04;
const CO_FAST_ARG_VAR: i64 = 0x08;

/// `CO_VARARGS`/`CO_VARKEYWORDS` from `Include/cpython/code.h`.
const CO_VARARGS: i64 = 0x0004;
const CO_VARKEYWORDS: i64 = 0x0008;

/// Reads one of a code object's name tuples as strings.
fn code_strings(code: i64, attribute: &str) -> Option<Vec<String>> {
    let names = crate::exc::code_attr(code, attribute)?;
    let root = crate::heap::root_values(std::slice::from_ref(&names));
    let items = value::as_tuple(names);
    let mut texts = Vec::new();
    let mut complete = true;
    if let Some(items) = items {
        texts.reserve(items.len());
        for &item in items {
            match value::as_str(item) {
                Some(text) => texts.push(text.to_string()),
                None => {
                    complete = false;
                    break;
                }
            }
        }
    } else {
        complete = false;
    }
    crate::heap::release_roots(root);
    if complete {
        Some(texts)
    } else {
        None
    }
}

/// Reads one of a code object's integer attributes.
fn code_int(code: i64, attribute: &str) -> Option<i64> {
    value::as_integer(crate::exc::code_attr(code, attribute)?)
}

/// Merges a code object's legacy name tuples into CPython's locals-plus table.
///
/// The slot order and the `CO_FAST_*` bits follow
/// `PyUnstable_Code_NewWithPosOnlyArgs`/`_PyCode_New`: positional-only slots
/// first, then positional-or-keyword, keyword-only, `*args`, `**kwargs`, plain
/// locals, cells and finally free variables. A cell that shadows an argument
/// shares the argument's slot with `CO_FAST_CELL` added.
fn code_localsplus(code: i64) -> Option<(Vec<String>, Vec<i64>)> {
    let varnames = code_strings(code, "co_varnames")?;
    let cellvars = code_strings(code, "co_cellvars")?;
    let freevars = code_strings(code, "co_freevars")?;
    let argcount = code_int(code, "co_argcount")?;
    let posonlyargcount = code_int(code, "co_posonlyargcount")?;
    let kwonlyargcount = code_int(code, "co_kwonlyargcount")?;
    let flags = code_int(code, "co_flags")?;
    let varargs = i64::from(flags & CO_VARARGS != 0);
    let varkwargs = i64::from(flags & CO_VARKEYWORDS != 0);
    let mut names: Vec<String> =
        Vec::with_capacity(varnames.len() + cellvars.len() + freevars.len());
    let mut kinds: Vec<i64> = Vec::with_capacity(names.capacity());
    for (index, name) in varnames.into_iter().enumerate() {
        let position = index as i64;
        let kind = if position < posonlyargcount {
            CO_FAST_LOCAL | CO_FAST_ARG_POS
        } else if position < argcount {
            CO_FAST_LOCAL | CO_FAST_ARG_POS | CO_FAST_ARG_KW
        } else if position < argcount + kwonlyargcount {
            CO_FAST_LOCAL | CO_FAST_ARG_KW
        } else if varargs == 1 && position == argcount + kwonlyargcount {
            CO_FAST_LOCAL | CO_FAST_ARG_VAR | CO_FAST_ARG_POS
        } else if varkwargs == 1 && position == argcount + kwonlyargcount + varargs {
            CO_FAST_LOCAL | CO_FAST_ARG_VAR | CO_FAST_ARG_KW
        } else {
            CO_FAST_LOCAL
        };
        names.push(name);
        kinds.push(kind);
    }
    for name in cellvars {
        if let Some(index) = names.iter().position(|existing| *existing == name) {
            kinds[index] |= CO_FAST_CELL;
        } else {
            names.push(name);
            kinds.push(CO_FAST_CELL);
        }
    }
    for name in freevars {
        names.push(name);
        kinds.push(CO_FAST_FREE);
    }
    Some((names, kinds))
}

/// `get_co_framesize`: the frame size CPython stores in `co_framesize`.
///
/// `init_code` computes `co_nlocalsplus + co_stacksize + FRAME_SPECIALS_SIZE`;
/// this runtime's code objects report `nlocals` locals and no cell or free
/// variables, so the same formula describes them.
fn code_framesize(args: &[i64]) -> i64 {
    let code = argument(args, 0);
    if value::as_code(code).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "argument must be a code object");
    }
    let (Some((names, _)), Some(stacksize)) =
        (code_localsplus(code), code_int(code, "co_stacksize"))
    else {
        return exc::raise_with_message(TYPE_ERROR, "argument must be a code object");
    };
    num((names.len() as i64)
        .saturating_add(stacksize)
        .saturating_add(FRAME_SPECIALS_SIZE))
}

/// `get_co_localskinds`: a dict from locals-plus name to kind bits.
fn code_localskinds(args: &[i64]) -> i64 {
    let code = argument(args, 0);
    if value::as_code(code).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "argument must be a code object");
    }
    let Some((names, kinds)) = code_localsplus(code) else {
        return exc::raise_with_message(TYPE_ERROR, "argument must be a code object");
    };
    let dict = value::dict_new(Vec::new());
    let root = crate::heap::root_values(std::slice::from_ref(&dict));
    for (name, kind) in names.into_iter().zip(kinds) {
        let key = value::str_alloc(name);
        let kind_value = ops::int_result(kind);
        let entry_root = crate::heap::root_values(&[key, kind_value]);
        crate::container::dict_set(dict, key, kind_value);
        crate::heap::release_roots(entry_root);
    }
    crate::heap::release_roots(root);
    dict
}

// -------------------------------------------------------------------------
// Dispatch
// -------------------------------------------------------------------------

/// Dispatches a native `_testinternalcapi` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "get_configs" => config_dict(),
        "set_config" => set_config(args),
        "_PyTime_FromSeconds" => pytime_from_seconds(args),
        "_PyTime_FromSecondsObject" => pytime_from_seconds_object(args),
        "_PyTime_AsTimeval" => pytime_as_timeval(args, false),
        "_PyTime_AsTimeval_clamp" => pytime_as_timeval(args, true),
        "_PyTime_AsTimespec" => pytime_as_timespec(args, false),
        "_PyTime_AsTimespec_clamp" => pytime_as_timespec(args, true),
        "_PyTime_AsMilliseconds" => pytime_as_unit(args, NS_TO_MS),
        "_PyTime_AsMicroseconds" => pytime_as_unit(args, NS_TO_US),
        "_PyTime_ObjectToTime_t" => pytime_object_to_time_t(args),
        "_PyTime_ObjectToTimespec" => pytime_object_to_denominator(args, SEC_TO_NS),
        "_PyTime_ObjectToTimeval" => pytime_object_to_denominator(args, SEC_TO_US),
        "_PyUnicode_TransformDecimalAndSpaceToASCII" => transform_decimal_and_space(args),
        "EncodeLocaleEx" => encode_locale_ex(args),
        "DecodeLocaleEx" => decode_locale_ex(args),
        "get_co_framesize" => code_framesize(args),
        "get_co_localskinds" => code_localskinds(args),
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

    #[test]
    fn pytime_division_matches_the_c_rounding_modes() {
        // pytime_divide's four modes, checked on the half-way cases CPython's
        // own rounding tests use.
        assert_eq!(time_divide(1500, 1000, ROUND_HALF_EVEN), 2);
        assert_eq!(time_divide(2500, 1000, ROUND_HALF_EVEN), 2);
        assert_eq!(time_divide(3500, 1000, ROUND_HALF_EVEN), 4);
        assert_eq!(time_divide(-1500, 1000, ROUND_HALF_EVEN), -2);
        assert_eq!(time_divide(1500, 1000, ROUND_CEILING), 2);
        assert_eq!(time_divide(-1500, 1000, ROUND_CEILING), -1);
        assert_eq!(time_divide(1500, 1000, ROUND_FLOOR), 1);
        assert_eq!(time_divide(-1500, 1000, ROUND_FLOOR), -2);
        assert_eq!(time_divide(1500, 1000, ROUND_UP), 2);
        assert_eq!(time_divide(-1500, 1000, ROUND_UP), -2);
        assert_eq!(time_divmod(-1, SEC_TO_NS), Ok((-1, SEC_TO_NS - 1)));
        assert_eq!(time_divmod(0, SEC_TO_NS), Ok((0, 0)));
    }

    #[test]
    fn transform_decimal_stops_at_the_first_other_code_point() {
        exc::init();
        for (input, expected) in [
            ("123", "123"),
            ("\u{0663}.\u{0661}\u{0664}", "3.14"),
            ("\u{2003}3.14\u{2002}", " 3.14 "),
            ("12\u{20ac}3", "12?"),
            ("", ""),
        ] {
            let text = value::str_alloc(input.to_string());
            let result = transform_decimal_and_space(&[text]);
            assert_eq!(value::as_str(result), Some(expected), "input {input:?}");
        }
    }
}
