//! CPython 3.14 internals that the pinned `lib-dynload` extensions import.
//!
//! The extensions under `lib-dynload` are built against CPython's *internal*
//! headers as well as the public ones, so they reference a number of `_Py*`
//! entry points, tables and one data object that the public-API surface in
//! [`super`] does not provide. This module implements them against the same
//! shim primitives as the rest of the C API; nothing here is a success-returning
//! placeholder:
//!
//! * `_PyArg_CheckPositional`, `_PyArg_BadArgument` and a full
//!   `_PyArg_UnpackKeywords` run the clinic-generated argument parsing of
//!   `_lsprof`, `_elementtree`, `_curses_panel`, `_posixsubprocess` and
//!   `_remote_debugging`, including the `_PyArg_Parser` static tables.
//! * `_PyObject_MakeTpCall`, `_Py_CheckFunctionResult` and
//!   `_Py_CheckRecursiveCall` route through the real vectorcall/call machinery
//!   and the host's recursion counter.
//! * `_PyLong_AsByteArray`/`_PyLong_FromByteArray`/`_PyLong_Format` perform
//!   the two's-complement and base conversions within this host's documented
//!   64-bit integer model (out-of-range values raise `OverflowError` instead of
//!   being truncated).
//! * `_PyBytes_Resize`/`_PyTuple_Resize` allocate a replacement object and move
//!   the references, matching CPython's contract that the caller drops its own
//!   reference.
//! * `_PyTime_*` convert seconds/integers with CPython's rounding modes.
//! * `_Py_hashtable_*` back a real pointer-keyed table with the CPython
//!   `_Py_hashtable_t` layout, including rehashing and destroy callbacks.
//! * `_Py_ctype_table`, `_Py_ctype_tolower` and `_Py_HashSecret` are the data
//!   objects the extensions read directly (the latter seeded from the OS).
//! * `_Py_open_noraise`, `_Py_write_noraise`, `_Py_set_inheritable_async_safe`
//!   and `_Py_RestoreSignals` are the async-signal-safe POSIX helpers used by
//!   `_posixsubprocess`.
//!
//! `_PyErr_Format`/`_PyErr_FormatFromCause` are C variadic entry points; they
//! are defined here because Rust 1.99 supports defining C variadic functions
//! (and reading them through `VaList::next_arg`).

#![allow(non_snake_case)]

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void, CStr, CString};
use std::ptr;
use std::sync::{LazyLock, Mutex, PoisonError};

use super::{
    as_c_string, as_float, as_int, as_predicate, as_string, bytes_data, dec_ref, dict_slots,
    dict_slots_mut, error_pending, exception_prefix, gil_acquire, gil_release, inc_ref, kind_ref,
    list_slots_mut, new_bytes, new_exception_instance, new_int, new_str, new_tuple,
    object_type_name, read_c_string, replace_error_state, set_error, set_type_error, shim_repr,
    sync_list_prefix, tuple_slots, types, PyBytes_FromStringAndSize, PyDict_SetItemString,
    PyImport_ImportModule, PyList_Append, PyList_GetItem, PyList_New, PyList_Size, PyLong_FromLong,
    PyModule_AddObject, PyModule_GetDef, PyObject_CallNoArgs, PyObject_GetAttrString,
    PySequence_Fast, PyThreadState, PyTuple_GetItem, ShimKind, ShimObject, TypeRole,
    IMMORTAL_REFCNT, RECURSION_DEPTH, RECURSION_LIMIT,
};

/// `Py_uhash_t` — the table's hash word type.
type PyUHash = usize;

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Mirrors CPython 3.14's `struct _PyArg_Parser` (`Include/cpython/modsupport.h`).
///
/// Clinic-generated code keeps a zero-initialised static of this type next to
/// its keyword list; the first call fills in `pos` (positional-only count) and
/// `fname`, and flips `once`. The field layout must match exactly because the
/// extension owns the storage.
#[repr(C)]
#[derive(Debug)]
pub struct PyArgParser {
    /// `PyArg_ParseTupleAndKeywords` format, or NULL for the clinic fast path.
    pub format: *const c_char,
    /// NULL-terminated keyword-name array; an empty name marks a
    /// positional-only parameter.
    pub keywords: *const *const c_char,
    /// Function name used in diagnostics (or NULL when `format` names it).
    pub fname: *const c_char,
    /// Custom error message after `;` in `format` (unused by this host).
    pub custom_msg: *const c_char,
    /// `_PyOnceFlag` (a single `uint8_t`).
    pub once: u8,
    /// Whether the table owns `kwtuple`.
    pub is_kwtuple_owned: c_int,
    /// Number of positional-only parameters.
    pub pos: c_int,
    /// Minimum positional count from `format` (informational here).
    pub min: c_int,
    /// Maximum positional count from `format` (informational here).
    pub max: c_int,
    /// Tuple of keyword parameter names (built by CPython; left NULL here).
    pub kwtuple: *mut ShimObject,
    /// The interpreter's registry of static parsers (unused here).
    pub next: *mut PyArgParser,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::offset_of!(PyArgParser, format) == 0);
    assert!(std::mem::offset_of!(PyArgParser, keywords) == 8);
    assert!(std::mem::offset_of!(PyArgParser, fname) == 16);
    assert!(std::mem::offset_of!(PyArgParser, custom_msg) == 24);
    assert!(std::mem::offset_of!(PyArgParser, once) == 32);
    assert!(std::mem::offset_of!(PyArgParser, is_kwtuple_owned) == 36);
    assert!(std::mem::offset_of!(PyArgParser, pos) == 40);
    assert!(std::mem::offset_of!(PyArgParser, min) == 44);
    assert!(std::mem::offset_of!(PyArgParser, max) == 48);
    assert!(std::mem::offset_of!(PyArgParser, kwtuple) == 56);
    assert!(std::mem::offset_of!(PyArgParser, next) == 64);
    assert!(std::mem::size_of::<PyArgParser>() == 72);
};

/// Value CPython stores in `_PyOnceFlag` after a successful initialisation.
const ONCE_INITIALIZED: u8 = 4;

/// Counts entries in a NULL-terminated keyword array.
unsafe fn keyword_count(keywords: *const *const c_char) -> usize {
    let mut count = 0;
    while !(*keywords.add(count)).is_null() {
        count += 1;
    }
    count
}

/// Initialises the host-visible parts of a static `_PyArg_Parser`.
///
/// CPython does this behind `_PyOnceFlag_CallOnce`; the parsed values are
/// immutable afterwards, so recomputing them would be harmless and this
/// implementation simply caches them. Returns false with an error set when the
/// table is malformed.
unsafe fn ensure_parser(parser: *mut PyArgParser) -> bool {
    if parser.is_null() {
        super::bad_internal_call();
        return false;
    }
    if (*parser).keywords.is_null() {
        super::bad_internal_call();
        return false;
    }
    if (*parser).once == ONCE_INITIALIZED {
        return true;
    }
    let keywords = (*parser).keywords;
    let mut pos = 0usize;
    while !(*keywords.add(pos)).is_null() && *(*keywords.add(pos)) == 0 {
        pos += 1;
    }
    let mut total = pos;
    while !(*keywords.add(total)).is_null() {
        if *(*keywords.add(total)) == 0 {
            set_error("SystemError: Empty keyword parameter name");
            return false;
        }
        total += 1;
    }
    let mut fname = (*parser).fname;
    let mut custom = (*parser).custom_msg;
    if !(*parser).format.is_null() {
        let text = read_c_string((*parser).format);
        if text.contains(':') {
            // The text after ':' is the function name. It lives inside the
            // static format string, so handing out a pointer into that same
            // allocation is sound.
            fname = (*parser).format.add(text.find(':').unwrap() + 1);
            custom = ptr::null();
        } else if let Some(index) = text.find(';') {
            custom = (*parser).format.add(index + 1);
            fname = ptr::null();
        }
    }
    (*parser).pos = pos as c_int;
    (*parser).fname = fname;
    (*parser).custom_msg = custom;
    let _ = total;
    (*parser).once = ONCE_INITIALIZED;
    true
}

/// Renders `fname` the way CPython's error formats expect: `"fname()"`, or
/// `"function"` when the parser has no name.
fn function_label(fname: *const c_char) -> String {
    if fname.is_null() {
        "function".to_string()
    } else {
        format!("{}()", read_c_string(fname))
    }
}

/// `_PyArg_CheckPositional`: validate a positional argument count.
#[no_mangle]
pub unsafe extern "C" fn _PyArg_CheckPositional(
    name: *const c_char,
    nargs: isize,
    min: isize,
    max: isize,
) -> c_int {
    let nargs = nargs.max(0) as usize;
    let min = min.max(0) as usize;
    let max = max.max(0) as usize;
    if nargs < min {
        let qualifier = if min == max { "" } else { "at least " };
        let plural = if min == 1 { "" } else { "s" };
        if name.is_null() {
            set_type_error(format!(
                "unpacked tuple should have {qualifier}{min} element{plural}, but has {nargs}"
            ));
        } else {
            set_type_error(format!(
                "{} expected {qualifier}{min} argument{plural}, got {nargs}",
                read_c_string(name)
            ));
        }
        return 0;
    }
    if nargs == 0 {
        return 1;
    }
    if nargs > max {
        let qualifier = if min == max { "" } else { "at most " };
        let plural = if max == 1 { "" } else { "s" };
        if name.is_null() {
            set_type_error(format!(
                "unpacked tuple should have {qualifier}{max} element{plural}, but has {nargs}"
            ));
        } else {
            set_type_error(format!(
                "{} expected {qualifier}{max} argument{plural}, got {nargs}",
                read_c_string(name)
            ));
        }
        return 0;
    }
    1
}

/// `_PyArg_BadArgument`: raise the TypeError clinic code uses after a failed
/// type check.
#[no_mangle]
pub unsafe extern "C" fn _PyArg_BadArgument(
    fname: *const c_char,
    displayname: *const c_char,
    expected: *const c_char,
    arg: *mut ShimObject,
) {
    let actual = if arg == ptr::addr_of_mut!(super::_Py_NoneStruct) {
        "None".to_string()
    } else if arg.is_null() {
        "NULL".to_string()
    } else {
        object_type_name(arg)
    };
    let fname = truncate_bytes(&read_c_string(fname), 200);
    let display = truncate_bytes(&read_c_string(displayname), 200);
    let expected = truncate_bytes(&read_c_string(expected), 50);
    let actual = truncate_bytes(&actual, 50);
    set_type_error(format!(
        "{fname}() {display} must be {expected}, not {actual}"
    ));
}

/// CPython truncates diagnostic strings with `%.200s`/`%.50s`; do the same on
/// byte boundaries without splitting UTF-8.
fn truncate_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Looks a keyword name up in the vectorcall `kwnames`/value stack.
unsafe fn find_keyword(
    kwnames: &[usize],
    kwstack: *const *mut ShimObject,
    name: &str,
) -> *mut ShimObject {
    for (index, &keyword) in kwnames.iter().enumerate() {
        let keyword = keyword as *mut ShimObject;
        if let Some(text) = as_string(keyword) {
            if text == name {
                return *kwstack.add(index);
            }
        }
    }
    ptr::null_mut()
}

/// True when `proof` matches `name` byte-for-byte (CPython's keyword names are
/// ASCII identifiers).
unsafe fn keyword_name_equals(proof: *const c_char, name: &str) -> bool {
    !proof.is_null() && read_c_string(proof) == name
}

/// True when `name` appears in the parser's keyword list, from `start` on.
unsafe fn keyword_list_contains(
    keywords: *const *const c_char,
    start: usize,
    total: usize,
    name: &str,
) -> bool {
    for index in start..total {
        if keyword_name_equals(*keywords.add(index), name) {
            return true;
        }
    }
    false
}

/// `_PyArg_UnpackKeywords`: the clinic fast-path argument unpacker.
///
/// `args`/`nargs` describe the positional vector, `kwnames` the trailing
/// keyword-name tuple (vectorcall convention) or `kwargs` a keyword dict.
/// `buf` receives one pointer per parameter: borrowed positional values in the
/// first slots and `NULL` for absent optionals. The fast path returns `args`
/// itself, exactly as CPython does, so callers must not write through it.
#[no_mangle]
pub unsafe extern "C" fn _PyArg_UnpackKeywords(
    args: *const *mut ShimObject,
    nargs: isize,
    kwargs: *mut ShimObject,
    kwnames: *mut ShimObject,
    parser: *mut PyArgParser,
    minpos: c_int,
    maxpos: c_int,
    minkw: c_int,
    varpos: c_int,
    buf: *mut *mut ShimObject,
) -> *const *mut ShimObject {
    if !ensure_parser(parser) {
        return ptr::null();
    }
    if nargs < 0 {
        super::bad_internal_call();
        return ptr::null();
    }
    if buf.is_null() {
        super::bad_internal_call();
        return ptr::null();
    }
    let mut args = args;
    if args.is_null() && nargs == 0 {
        args = buf;
    }
    if args.is_null() && nargs > 0 {
        super::bad_internal_call();
        return ptr::null();
    }
    if !kwargs.is_null() && !kwnames.is_null() {
        // CPython asserts this; reject instead of reading a second source.
        super::bad_internal_call();
        return ptr::null();
    }
    let kwargs_pairs: Option<&'static [(usize, usize)]> = if kwargs.is_null() {
        None
    } else {
        match dict_slots(kwargs) {
            Ok(pairs) => Some(pairs),
            Err(_) => {
                super::bad_internal_call();
                return ptr::null();
            }
        }
    };
    let kwname_items: Option<&'static [usize]> = if kwnames.is_null() {
        None
    } else {
        match tuple_slots(kwnames) {
            Ok(items) => Some(items),
            Err(_) => {
                super::bad_internal_call();
                return ptr::null();
            }
        }
    };

    let mut nargs = nargs as usize;
    let keywords = (*parser).keywords;
    let total = keyword_count(keywords);
    let posonly = if (*parser).pos > 0 {
        (*parser).pos as usize
    } else {
        0
    };
    let posonly = posonly.min(total);

    let nkwargs = kwargs_pairs
        .map(|pairs| pairs.len())
        .or_else(|| kwname_items.map(<[usize]>::len))
        .unwrap_or(0);
    let kwstack: *const *mut ShimObject = if kwname_items.is_some() {
        args.add(nargs)
    } else {
        ptr::null()
    };

    let minpos = minpos.max(0) as usize;
    let maxpos = maxpos.max(0) as usize;
    if nkwargs == 0 && minkw == 0 && minpos <= nargs && (varpos != 0 || nargs <= maxpos) {
        return args;
    }

    let maxargs = total;
    let fname = function_label((*parser).fname);
    let given = nargs + nkwargs;
    if varpos == 0 && given > maxargs {
        let keyword = if nargs == 0 { "keyword " } else { "" };
        let plural = if maxargs == 1 { "" } else { "s" };
        set_type_error(format!(
            "{fname} takes at most {maxargs} {keyword}argument{plural} ({given} given)"
        ));
        return ptr::null();
    }
    if varpos == 0 && nargs > maxpos {
        if maxpos == 0 {
            set_type_error(format!("{fname} takes no positional arguments"));
        } else {
            let qualifier = if minpos < maxpos {
                "at most"
            } else {
                "exactly"
            };
            let plural = if maxpos == 1 { "" } else { "s" };
            set_type_error(format!(
                "{fname} takes {qualifier} {maxpos} positional argument{plural} ({nargs} given)"
            ));
        }
        return ptr::null();
    }
    let minposonly = posonly.min(minpos);
    if nargs < minposonly {
        let plural = if minposonly == 1 { "" } else { "s" };
        let qualifier = if varpos != 0 || minposonly < maxpos {
            "at least"
        } else {
            "exactly"
        };
        set_type_error(format!(
            "{fname} takes {qualifier} {minposonly} positional argument{plural} ({nargs} given)"
        ));
        return ptr::null();
    }
    if varpos != 0 && nargs > maxpos {
        nargs = maxpos;
    }

    // Positional arguments are borrowed and copied verbatim.
    for index in 0..nargs {
        *buf.add(index) = *args.add(index);
    }

    let reqlimit = if minkw != 0 {
        maxpos + minkw.max(0) as usize
    } else {
        minpos
    };
    let mut remaining = nkwargs;
    let start = nargs.max(posonly);
    for index in start..maxargs {
        if remaining == 0 && index >= reqlimit {
            break;
        }
        let name = read_c_string(*keywords.add(index));
        let mut found = ptr::null_mut();
        if remaining > 0 {
            if kwargs_pairs.is_some() {
                found = super::PyDict_GetItemString(kwargs, *keywords.add(index));
            } else if let Some(names) = kwname_items {
                found = find_keyword(names, kwstack, &name);
            }
        }
        *buf.add(index) = found;
        if !found.is_null() {
            remaining -= 1;
        } else if index < minpos || (maxpos <= index && index < reqlimit) {
            set_type_error(format!(
                "{fname} missing required argument '{name}' (pos {})",
                index + 1
            ));
            return ptr::null();
        }
    }

    if remaining > 0 {
        // A keyword argument was also supplied positionally.
        for index in posonly..nargs {
            let name = read_c_string(*keywords.add(index));
            let present = if kwargs_pairs.is_some() {
                !super::PyDict_GetItemString(kwargs, *keywords.add(index)).is_null()
            } else if let Some(names) = kwname_items {
                !find_keyword(names, kwstack, &name).is_null()
            } else {
                false
            };
            if present {
                set_type_error(format!(
                    "argument for {fname} given by name ('{name}') and position ({})",
                    index + 1
                ));
                return ptr::null();
            }
        }
        // Otherwise report the first keyword the signature does not accept.
        if let Some(pairs) = kwargs_pairs {
            for &(key, _) in pairs {
                let key = key as *mut ShimObject;
                let Some(name) = as_string(key) else {
                    set_type_error("keywords must be strings");
                    return ptr::null();
                };
                if !keyword_list_contains(keywords, posonly, total, &name) {
                    set_type_error(format!(
                        "{fname} got an unexpected keyword argument '{name}'"
                    ));
                    return ptr::null();
                }
            }
        } else if let Some(names) = kwname_items {
            for &key in names {
                let key = key as *mut ShimObject;
                let Some(name) = as_string(key) else {
                    set_type_error("keywords must be strings");
                    return ptr::null();
                };
                if !keyword_list_contains(keywords, posonly, total, &name) {
                    set_type_error(format!(
                        "{fname} got an unexpected keyword argument '{name}'"
                    ));
                    return ptr::null();
                }
            }
        }
        // Every supplied keyword matched a positional-only parameter.
        set_type_error(format!("{fname} got an unexpected keyword argument"));
        return ptr::null();
    }

    buf as *const *mut ShimObject
}

// ---------------------------------------------------------------------------
// Calls and recursion
// ---------------------------------------------------------------------------

/// `_PyObject_MakeTpCall`: call `callable` with a plain argument vector and
/// either a keyword-name tuple (vectorcall convention) or a keyword dict.
#[no_mangle]
pub unsafe extern "C" fn _PyObject_MakeTpCall(
    _tstate: *mut PyThreadState,
    callable: *mut ShimObject,
    args: *const *mut ShimObject,
    nargs: isize,
    keywords: *mut ShimObject,
) -> *mut ShimObject {
    if callable.is_null() {
        set_error("SystemError: null argument to internal routine");
        return ptr::null_mut();
    }
    if nargs < 0 || (nargs > 0 && args.is_null()) {
        super::bad_internal_call();
        return ptr::null_mut();
    }
    let nargs = nargs as usize;
    let result = if keywords.is_null() || matches!(kind_ref(keywords), Some(ShimKind::Tuple(_))) {
        super::PyObject_Vectorcall(callable, args, nargs, keywords)
    } else if matches!(kind_ref(keywords), Some(ShimKind::Dict(_))) {
        super::PyObject_VectorcallDict(callable, args, nargs, keywords)
    } else {
        super::bad_internal_call();
        return ptr::null_mut();
    };
    _Py_CheckFunctionResult(ptr::null_mut(), callable, result, ptr::null())
}

/// `_Py_CheckFunctionResult`: enforce the C-API contract that a NULL result
/// carries an exception and a non-NULL result does not.
#[no_mangle]
pub unsafe extern "C" fn _Py_CheckFunctionResult(
    _tstate: *mut PyThreadState,
    callable: *mut ShimObject,
    result: *mut ShimObject,
    where_: *const c_char,
) -> *mut ShimObject {
    if result.is_null() {
        if !error_pending() {
            let caller = if !callable.is_null() {
                shim_repr(callable)
            } else if !where_.is_null() {
                read_c_string(where_)
            } else {
                "function".to_string()
            };
            set_error(format!(
                "SystemError: {caller} returned NULL without setting an exception"
            ));
        }
        return ptr::null_mut();
    }
    if error_pending() {
        dec_ref(result);
        let caller = if !callable.is_null() {
            shim_repr(callable)
        } else if !where_.is_null() {
            read_c_string(where_)
        } else {
            "function".to_string()
        };
        raise_with_context(
            super::PyExc_SystemError,
            &format!("{caller} returned a result with an exception set"),
        );
        return ptr::null_mut();
    }
    result
}

/// Creates a new exception instance carrying `message`, chains the currently
/// raised exception as `__context__`, and installs it as the raised exception.
///
/// `class` must be an exception class.
unsafe fn raise_with_context(class: *mut ShimObject, message: &str) {
    let cause = if error_pending() {
        super::PyErr_GetRaisedException()
    } else {
        ptr::null_mut()
    };
    let class = if class.is_null() {
        super::initialize_exception_types();
        super::PyExc_SystemError
    } else {
        class
    };
    let text = new_str(message.to_string());
    let instance = new_exception_instance(class as usize, vec![text]);
    dec_ref(text);
    if !cause.is_null() {
        super::PyException_SetContext(instance, cause);
    }
    replace_error_state(
        Some(format!("{}: {message}", exception_prefix(class))),
        Some(instance as usize),
    );
}

/// `_Py_CheckRecursiveCall`: CPython's out-of-line recursion check.
///
/// The host tracks recursion in [`RECURSION_DEPTH`]; the inline
/// `_Py_MakeRecCheck` in the extension counts down CPython's per-thread budget,
/// which this host does not model. Raising the same `RecursionError` once the
/// host limit is reached keeps the failure mode faithful.
#[no_mangle]
pub unsafe extern "C" fn _Py_CheckRecursiveCall(
    _tstate: *mut PyThreadState,
    where_: *const c_char,
) -> c_int {
    let depth = RECURSION_DEPTH.with(Cell::get);
    if depth < RECURSION_LIMIT {
        return 0;
    }
    let suffix = if where_.is_null() {
        String::new()
    } else {
        read_c_string(where_)
    };
    set_error(format!(
        "RecursionError: maximum recursion depth exceeded{suffix}"
    ));
    -1
}

/// `_PyThreadState_GetCurrent`: the calling thread's token.
#[no_mangle]
pub unsafe extern "C" fn _PyThreadState_GetCurrent() -> *mut PyThreadState {
    super::thread_state_token()
}

std::thread_local! {
    /// Installed profile function (`Py_tracefunc`) as a raw address.
    static PROFILE_FUNC: Cell<usize> = const { Cell::new(0) };
    /// Owned reference to the profile function's argument.
    static PROFILE_ARG: Cell<usize> = const { Cell::new(0) };
}

/// `_PyEval_SetProfile`: install (or clear) the thread's profile callback.
///
/// The host has no CPython evaluation loop to emit `PyTrace_*` events for, but
/// the call itself is real state: the callback and its argument are stored per
/// thread with correct reference accounting, so a later
/// `_PyEval_SetProfile(tstate, NULL, NULL)` releases exactly one reference.
#[no_mangle]
pub unsafe extern "C" fn _PyEval_SetProfile(
    _tstate: *mut PyThreadState,
    func: *mut c_void,
    arg: *mut ShimObject,
) -> c_int {
    let new_arg = arg as usize;
    if !arg.is_null() {
        inc_ref(arg);
    }
    let previous = PROFILE_ARG.with(|slot| slot.replace(new_arg));
    if previous != 0 {
        dec_ref(previous as *mut ShimObject);
    }
    PROFILE_FUNC.with(|slot| slot.set(func as usize));
    0
}

/// `_PyErr_ChainExceptions1`: make `exc` the context of the raised exception,
/// or raise it when there is none.
#[no_mangle]
pub unsafe extern "C" fn _PyErr_ChainExceptions1(exc: *mut ShimObject) {
    if exc.is_null() {
        return;
    }
    if error_pending() {
        let raised = super::PyErr_GetRaisedException();
        if raised.is_null() {
            super::PyErr_SetRaisedException(exc);
            return;
        }
        super::PyException_SetContext(raised, exc);
        super::PyErr_SetRaisedException(raised);
    } else {
        super::PyErr_SetRaisedException(exc);
    }
}

// ---------------------------------------------------------------------------
// Error formatting (C variadic)
// ---------------------------------------------------------------------------

/// Length modifier of a conversion.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Length {
    Default,
    Short,
    Long,
    LongLong,
    Size,
    PtrDiff,
    IntMax,
}

/// Formats a CPython mini-language message from a C `va_list`.
///
/// Supports the conversions `PyUnicode_FromFormat`/`PyErr_Format` accept for
/// the extensions in `lib-dynload`: `%s`, `%R`, `%S`, `%A`, `%U`, `%V`, `%T`,
/// `%d`/`%i`, `%u`, `%x`/`%X`, `%o`, `%c`, `%p`, `%f`/`%e`/`%g` with the
/// `h`/`hh`/`l`/`ll`/`z`/`t`/`j`/`L` modifiers, flags, width and precision.
/// `%n` is rejected (it would write through a caller pointer); an unknown
/// conversion raises `SystemError` and yields `None`. The caller must not
/// distinguish a failure by the pending-error slot: `_PyErr_FormatFromCause`
/// legitimately runs with an exception already set.
unsafe fn format_message(
    format: *const c_char,
    args: &mut core::ffi::VaList<'_>,
) -> Option<String> {
    let bytes = CStr::from_ptr(format).to_bytes();
    let mut out = String::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            out.push(bytes[index] as char);
            index += 1;
            continue;
        }
        index += 1;
        if index >= bytes.len() {
            out.push('%');
            break;
        }
        let flag_start = index;
        while index < bytes.len() && matches!(bytes[index], b'-' | b'0' | b'+' | b' ' | b'#') {
            index += 1;
        }
        let flags = &bytes[flag_start..index];
        let width_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        let width: Option<usize> = if index > width_start {
            std::str::from_utf8(&bytes[width_start..index])
                .ok()
                .and_then(|text| text.parse().ok())
        } else {
            None
        };
        let mut precision: Option<usize> = None;
        if index < bytes.len() && bytes[index] == b'.' {
            index += 1;
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            precision = std::str::from_utf8(&bytes[start..index])
                .ok()
                .and_then(|text| text.parse().ok())
                .or(Some(0));
        }
        let mut length = Length::Default;
        while index < bytes.len() {
            match bytes[index] {
                b'h' => {
                    if index + 1 < bytes.len() && bytes[index + 1] == b'h' {
                        length = Length::Short;
                        index += 2;
                    } else {
                        length = Length::Short;
                        index += 1;
                    }
                }
                b'l' => {
                    if index + 1 < bytes.len() && bytes[index + 1] == b'l' {
                        length = Length::LongLong;
                        index += 2;
                    } else {
                        length = Length::Long;
                        index += 1;
                    }
                }
                b'z' => {
                    length = Length::Size;
                    index += 1;
                }
                b't' => {
                    length = Length::PtrDiff;
                    index += 1;
                }
                b'j' => {
                    length = Length::IntMax;
                    index += 1;
                }
                b'L' => {
                    length = Length::LongLong;
                    index += 1;
                }
                _ => break,
            }
        }
        if index >= bytes.len() {
            break;
        }
        let conversion = bytes[index];
        index += 1;
        match conversion {
            b'%' => out.push('%'),
            b'd' | b'i' => {
                let value: i64 = match length {
                    Length::Long => unsafe { args.next_arg::<c_long>() },
                    Length::LongLong | Length::IntMax => unsafe { args.next_arg::<i64>() },
                    Length::Size | Length::PtrDiff => (unsafe { args.next_arg::<isize>() }) as i64,
                    Length::Short | Length::Default => (unsafe { args.next_arg::<c_int>() }) as i64,
                };
                let text = value.to_string();
                out.push_str(&pad_numeric(&text, width, flags));
            }
            b'u' => {
                let value: u64 = match length {
                    Length::Long => unsafe { args.next_arg::<c_ulong>() },
                    Length::LongLong | Length::IntMax => unsafe { args.next_arg::<u64>() },
                    Length::Size | Length::PtrDiff => (unsafe { args.next_arg::<usize>() }) as u64,
                    Length::Short | Length::Default => {
                        (unsafe { args.next_arg::<c_uint>() }) as u64
                    }
                };
                let text = value.to_string();
                out.push_str(&pad_numeric(&text, width, flags));
            }
            b'x' | b'X' | b'o' => {
                let value: u64 = match length {
                    Length::Long => unsafe { args.next_arg::<c_ulong>() },
                    Length::LongLong | Length::IntMax => unsafe { args.next_arg::<u64>() },
                    Length::Size | Length::PtrDiff => (unsafe { args.next_arg::<usize>() }) as u64,
                    Length::Short | Length::Default => {
                        (unsafe { args.next_arg::<c_uint>() }) as u64
                    }
                };
                let mut text = match conversion {
                    b'x' => format!("{value:x}"),
                    b'X' => format!("{value:X}"),
                    _ => format!("{value:o}"),
                };
                if conversion != b'o' && flags.contains(&b'#') && value != 0 {
                    text.insert_str(0, if conversion == b'x' { "0x" } else { "0X" });
                }
                out.push_str(&pad_numeric(&text, width, flags));
            }
            b'c' => {
                let value = (unsafe { args.next_arg::<c_int>() }) as u32;
                out.push(char::from_u32(value).unwrap_or('\u{fffd}'));
            }
            b'p' => {
                let value = (unsafe { args.next_arg::<*const c_void>() }) as usize;
                let text = format!("0x{value:x}");
                out.push_str(&pad_numeric(&text, width, flags));
            }
            b's' => {
                let pointer = unsafe { args.next_arg::<*const c_char>() };
                let mut text = if pointer.is_null() {
                    "(null)".to_string()
                } else {
                    read_c_string(pointer)
                };
                if let Some(limit) = precision {
                    text = truncate_bytes(&text, limit);
                }
                out.push_str(&pad_text(&text, width, flags));
            }
            b'R' | b'S' | b'A' | b'U' => {
                let object = unsafe { args.next_arg::<*mut ShimObject>() };
                let mut text = match conversion {
                    b'R' => shim_repr(object),
                    b'A' => {
                        let ascii = super::hydro_capi_ascii_repr(object);
                        if ascii.is_null() {
                            String::new()
                        } else {
                            let text = as_string(ascii).unwrap_or_default();
                            dec_ref(ascii);
                            text
                        }
                    }
                    b'S' => object_str(object),
                    _ => as_string(object).unwrap_or_else(|| object_str(object)),
                };
                if let Some(limit) = precision {
                    text = truncate_bytes(&text, limit);
                }
                out.push_str(&pad_text(&text, width, flags));
            }
            b'V' => {
                let object = unsafe { args.next_arg::<*mut ShimObject>() };
                let fallback = unsafe { args.next_arg::<*const c_char>() };
                let text = if object.is_null() {
                    if fallback.is_null() {
                        String::new()
                    } else {
                        read_c_string(fallback)
                    }
                } else {
                    object_str(object)
                };
                out.push_str(&pad_text(&text, width, flags));
            }
            b'T' => {
                let object = unsafe { args.next_arg::<*mut ShimObject>() };
                let text = if !object.is_null() && types::is_type_pointer(object) {
                    types::type_name(object as *mut types::TypeObject)
                } else if object.is_null() {
                    "NULL".to_string()
                } else {
                    object_type_name(object)
                };
                out.push_str(&text);
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                let value = unsafe { args.next_arg::<f64>() };
                let digits = precision.unwrap_or(6);
                let text = match conversion {
                    b'f' | b'F' => format!("{:.*}", digits, value),
                    b'e' | b'E' => {
                        let text = format!("{:.*e}", digits, value);
                        if conversion == b'E' {
                            text.to_uppercase()
                        } else {
                            text
                        }
                    }
                    _ => {
                        let text = format!("{value}");
                        if conversion == b'G' {
                            text.to_uppercase()
                        } else {
                            text
                        }
                    }
                };
                out.push_str(&pad_numeric(&text, width, flags));
            }
            b'n' => {
                set_error("SystemError: %n is not supported in _PyErr_Format format strings");
                return None;
            }
            other => {
                set_error(format!(
                    "SystemError: unsupported format character '{}' (0x{:02x}) in _PyErr_Format",
                    if other.is_ascii_graphic() {
                        other as char
                    } else {
                        '?'
                    },
                    other
                ));
                return None;
            }
        }
    }
    Some(out)
}

/// `str(obj)` falling back to `repr(obj)` for non-string results.
unsafe fn object_str(object: *mut ShimObject) -> String {
    if object.is_null() {
        return "<NULL>".to_string();
    }
    if let Some(text) = as_string(object) {
        return text;
    }
    let string = super::PyObject_Str(object);
    if string.is_null() {
        return shim_repr(object);
    }
    let text = as_string(string).unwrap_or_default();
    dec_ref(string);
    text
}

/// Applies a numeric field width, honouring the `0`-pad flag.
fn pad_numeric(text: &str, width: Option<usize>, flags: &[u8]) -> String {
    let Some(width) = width else {
        return text.to_string();
    };
    if text.len() >= width {
        return text.to_string();
    }
    let padding = width - text.len();
    let left = flags.contains(&b'-');
    if left {
        format!("{text}{}", " ".repeat(padding))
    } else if flags.contains(&b'0') {
        // Keep the sign or prefix in front of the zero padding.
        let split = if text.starts_with('-') || text.starts_with('+') {
            1
        } else if text.starts_with("0x") || text.starts_with("0X") {
            2
        } else {
            0
        };
        format!(
            "{}{}{}",
            &text[..split],
            "0".repeat(padding),
            &text[split..]
        )
    } else {
        format!("{}{text}", " ".repeat(padding))
    }
}

/// Applies a field width to a string, left- or right-aligning it.
fn pad_text(text: &str, width: Option<usize>, flags: &[u8]) -> String {
    let Some(width) = width else {
        return text.to_string();
    };
    if text.len() >= width {
        return text.to_string();
    }
    let padding = width - text.len();
    if flags.contains(&b'-') {
        format!("{text}{}", " ".repeat(padding))
    } else {
        format!("{}{text}", " ".repeat(padding))
    }
}

/// `_PyErr_Format`: format and raise `exception`.
#[no_mangle]
pub unsafe extern "C" fn _PyErr_Format(
    _tstate: *mut PyThreadState,
    exception: *mut ShimObject,
    format: *const c_char,
    mut args: ...
) -> *mut ShimObject {
    if exception.is_null() {
        set_error("SystemError: _PyErr_Format: exception is NULL");
        return ptr::null_mut();
    }
    if format.is_null() {
        set_error("SystemError: _PyErr_Format: format is NULL");
        return ptr::null_mut();
    }
    let Some(message) = (unsafe { format_message(format, &mut args) }) else {
        return ptr::null_mut();
    };
    if error_pending() {
        return ptr::null_mut();
    }
    set_error(format!("{}: {message}", exception_prefix(exception)));
    ptr::null_mut()
}

/// `_PyErr_FormatFromCause`: like [`_PyErr_Format`], but the exception already
/// set becomes the new exception's `__context__`.
#[no_mangle]
pub unsafe extern "C" fn _PyErr_FormatFromCause(
    exception: *mut ShimObject,
    format: *const c_char,
    mut args: ...
) -> *mut ShimObject {
    if exception.is_null() {
        set_error("SystemError: _PyErr_FormatFromCause: exception is NULL");
        return ptr::null_mut();
    }
    if format.is_null() {
        set_error("SystemError: _PyErr_FormatFromCause: format is NULL");
        return ptr::null_mut();
    }
    let Some(message) = (unsafe { format_message(format, &mut args) }) else {
        return ptr::null_mut();
    };
    // An error is *expected* here: `_PyErr_FormatFromCause` chains the
    // already-raised exception as the new one's context, so only a formatting
    // failure (reported by `format_message`) aborts.
    raise_with_context(exception, &message);
    ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Integer, string, tuple and bytes conversions
// ---------------------------------------------------------------------------

/// Error string for values outside this host's 64-bit integer model.
const LONG_OVERFLOW: &str = "OverflowError: int too large to convert";

/// `_PyNumber_Index`: the `__index__` protocol.
#[no_mangle]
pub unsafe extern "C" fn _PyNumber_Index(object: *mut ShimObject) -> *mut ShimObject {
    types::PyNumber_Index(object)
}

/// `_PyType_LookupRef`: new reference to `name` on `type_object`'s MRO.
#[no_mangle]
pub unsafe extern "C" fn _PyType_LookupRef(
    type_object: *mut ShimObject,
    name: *mut ShimObject,
) -> *mut ShimObject {
    let found = super::object_api::_PyType_Lookup(type_object as *mut types::TypeObject, name);
    if !found.is_null() {
        inc_ref(found);
    }
    found
}

/// `_PyUnicode_EqualToASCIIString`: 1 when `unicode` is exactly the ASCII
/// string `text`.
#[no_mangle]
pub unsafe extern "C" fn _PyUnicode_EqualToASCIIString(
    unicode: *mut ShimObject,
    text: *const c_char,
) -> c_int {
    if unicode.is_null() || text.is_null() {
        return 0;
    }
    match as_string(unicode) {
        Some(value) => (value == read_c_string(text)) as c_int,
        None => 0,
    }
}

/// `_PyUnicode_AsUTF8String`: UTF-8 encoding as a new bytes object.
#[no_mangle]
pub unsafe extern "C" fn _PyUnicode_AsUTF8String(object: *mut ShimObject) -> *mut ShimObject {
    super::PyUnicode_AsUTF8String(object)
}

/// `_PySys_GetOptionalAttrString`: look `name` up in `sys`, returning a new
/// reference in `*value`.
///
/// Returns 1 when found, 0 when absent (with `*value = NULL`) and -1 on error.
#[no_mangle]
pub unsafe extern "C" fn _PySys_GetOptionalAttrString(
    name: *const c_char,
    value: *mut *mut ShimObject,
) -> c_int {
    if value.is_null() {
        super::bad_internal_call();
        return -1;
    }
    if name.is_null() {
        super::bad_internal_call();
        return -1;
    }
    *value = ptr::null_mut();
    let dict = super::sys_dict();
    if dict == 0 {
        return 0;
    }
    let found = super::PyDict_GetItemString(dict as *mut ShimObject, name);
    if found.is_null() {
        return 0;
    }
    inc_ref(found);
    *value = found;
    1
}

/// `_PyLong_AsByteArray`: write `value` as a two's-complement (or unsigned)
/// byte array of exactly `n` bytes.
///
/// Fits and overflow follow CPython's documented contract: when the value does
/// not fit, the low `n` bytes are still written and (with `with_exceptions`) an
/// `OverflowError` is raised.
#[no_mangle]
pub unsafe extern "C" fn _PyLong_AsByteArray(
    value: *mut ShimObject,
    bytes: *mut u8,
    n: usize,
    little_endian: c_int,
    is_signed: c_int,
    with_exceptions: c_int,
) -> c_int {
    if value.is_null() {
        set_error("SystemError: _PyLong_AsByteArray: value is NULL");
        return -1;
    }
    if bytes.is_null() && n != 0 {
        set_error("SystemError: _PyLong_AsByteArray: bytes is NULL");
        return -1;
    }
    let number = match as_int(value) {
        Ok(number) => number,
        Err(_) => {
            set_type_error("an integer is required");
            return -1;
        }
    };
    let signed = is_signed != 0;
    if number < 0 && !signed {
        if with_exceptions != 0 {
            set_error("OverflowError: can't convert negative int to unsigned");
        }
        return -1;
    }
    let fits = if signed {
        match n {
            0 => number == 0,
            1..=7 => {
                let bound = 1i128 << (n * 8 - 1);
                (number as i128) >= -bound && (number as i128) < bound
            }
            _ => true,
        }
    } else {
        match n {
            0 => number == 0,
            1..=7 => (number as u128) < (1u128 << (n * 8)),
            _ => true,
        }
    };
    let pattern = number as u64;
    for index in 0..n {
        let byte = if index < 8 {
            ((pattern >> (8 * index)) & 0xff) as u8
        } else if number < 0 {
            0xff
        } else {
            0x00
        };
        let position = if little_endian != 0 {
            index
        } else {
            n - 1 - index
        };
        *bytes.add(position) = byte;
    }
    if !fits {
        if with_exceptions != 0 {
            set_error(LONG_OVERFLOW);
        }
        return -1;
    }
    0
}

/// `_PyLong_FromByteArray`: decode a two's-complement (or unsigned) byte array.
///
/// Values outside this host's 64-bit model raise `OverflowError` (CPython would
/// return a bignum; this runtime has no bignum representation).
#[no_mangle]
pub unsafe extern "C" fn _PyLong_FromByteArray(
    bytes: *const u8,
    n: usize,
    little_endian: c_int,
    is_signed: c_int,
) -> *mut ShimObject {
    if bytes.is_null() && n != 0 {
        set_error("SystemError: _PyLong_FromByteArray: bytes is NULL");
        return ptr::null_mut();
    }
    if n == 0 {
        return new_int(0);
    }
    // Byte at `significance` (0 = least significant) for either endianness.
    let byte_at = |significance: usize| -> u8 {
        let position = if little_endian != 0 {
            significance
        } else {
            n - 1 - significance
        };
        *bytes.add(position)
    };
    let most_significant = byte_at(n - 1);
    let negative = is_signed != 0 && (most_significant & 0x80) != 0;
    let extension = if negative { 0xffu8 } else { 0x00u8 };
    // Any byte above the low 64 bits must be a copy of the sign byte; a
    // non-redundant high byte means the value needs a bignum.
    let mut overflow = false;
    for significance in 8..n {
        if byte_at(significance) != extension {
            overflow = true;
            break;
        }
    }
    let mut low: u64 = 0;
    for significance in 0..n.min(8) {
        low |= (byte_at(significance) as u64) << (8 * significance);
    }
    let value: i64 = if overflow {
        set_error(LONG_OVERFLOW);
        return ptr::null_mut();
    } else if negative {
        if n >= 8 {
            low as i64
        } else {
            let shift = 64 - 8 * n;
            ((low << shift) as i64) >> shift
        }
    } else if low > i64::MAX as u64 {
        set_error(LONG_OVERFLOW);
        return ptr::null_mut();
    } else {
        low as i64
    };
    new_int(value)
}

/// `_PyLong_Format`: a string for `value` in `base`.
///
/// Base 10 is plain decimal; bases 2/8/16 carry CPython's `0b`/`0o`/`0x`
/// prefix (`long_format_binary(..., alternate=1, ...)`).
#[no_mangle]
pub unsafe extern "C" fn _PyLong_Format(value: *mut ShimObject, base: c_int) -> *mut ShimObject {
    if value.is_null() {
        set_error("SystemError: _PyLong_Format: value is NULL");
        return ptr::null_mut();
    }
    let number = match as_int(value) {
        Ok(number) => number,
        Err(_) => {
            set_type_error("an integer is required");
            return ptr::null_mut();
        }
    };
    let text = match base {
        10 => number.to_string(),
        2 => format!("0b{:b}", number.unsigned_abs()),
        8 => format!("0o{:o}", number.unsigned_abs()),
        16 => format!("0x{:x}", number.unsigned_abs()),
        _ => {
            set_error(format!(
                "SystemError: _PyLong_Format: unsupported base {base}"
            ));
            return ptr::null_mut();
        }
    };
    let text = if number < 0 && base != 10 {
        format!("-{text}")
    } else {
        text
    };
    new_str(text)
}

/// `_PyTuple_Resize`: replace `*pv` with a tuple of `newsize` items.
///
/// The caller must own the only reference to a non-empty tuple, as in CPython.
/// Items are moved with correct reference accounting; truncation releases the
/// dropped items (CPython's implementation does the same via `Py_CLEAR`).
#[no_mangle]
pub unsafe extern "C" fn _PyTuple_Resize(pv: *mut *mut ShimObject, newsize: isize) -> c_int {
    if pv.is_null() {
        super::bad_internal_call();
        return -1;
    }
    let old = *pv;
    if old.is_null() {
        super::bad_internal_call();
        return -1;
    }
    let old_items: Option<Vec<usize>> = match kind_ref(old) {
        Some(ShimKind::Tuple(items)) => Some(items.as_slice().to_vec()),
        _ => None,
    };
    let unique = {
        let count = (*old)
            .head
            .ob_refcnt
            .load(std::sync::atomic::Ordering::Relaxed);
        count == 1 || count == IMMORTAL_REFCNT
    };
    let invalid = newsize < 0
        || old_items.is_none()
        || (!old_items.as_deref().unwrap_or_default().is_empty() && !unique);
    if invalid {
        *pv = ptr::null_mut();
        dec_ref(old);
        super::bad_internal_call();
        return -1;
    }
    let old_items = old_items.unwrap_or_default();
    let newsize = newsize as usize;
    if old_items.len() == newsize {
        return 0;
    }
    if newsize == 0 {
        dec_ref(old);
        *pv = super::empty_tuple();
        return 0;
    }
    if old_items.is_empty() {
        let replacement = super::PyTuple_New(newsize as isize);
        if replacement.is_null() {
            *pv = ptr::null_mut();
            dec_ref(old);
            return -1;
        }
        *pv = replacement;
        dec_ref(old);
        return 0;
    }
    let mut items = vec![0usize; newsize];
    let moved = old_items.len().min(newsize);
    for index in 0..moved {
        if old_items[index] != 0 {
            inc_ref(old_items[index] as *mut ShimObject);
        }
        items[index] = old_items[index];
    }
    let replacement = super::new_tuple(items);
    *pv = replacement;
    // Releases the old tuple and every item it owned, including the ones that
    // were not moved (the copy holds its own references).
    dec_ref(old);
    0
}

/// `_PyBytes_Resize`: replace `*pv` with a bytes object of `newsize` bytes.
///
/// The replacement is zero-filled past the copied prefix; CPython leaves the
/// tail uninitialised, and the documented contract only guarantees the previous
/// contents. On failure `*pv` is cleared, matching CPython's wrapper.
#[no_mangle]
pub unsafe extern "C" fn _PyBytes_Resize(pv: *mut *mut ShimObject, newsize: isize) -> c_int {
    if pv.is_null() {
        super::bad_internal_call();
        return -1;
    }
    let old = *pv;
    if old.is_null() {
        super::bad_internal_call();
        return -1;
    }
    if newsize < 0 {
        *pv = ptr::null_mut();
        dec_ref(old);
        super::bad_internal_call();
        return -1;
    }
    let old_data = match kind_ref(old) {
        Some(ShimKind::Bytes { data, .. }) => data.clone(),
        _ => {
            *pv = ptr::null_mut();
            dec_ref(old);
            super::bad_internal_call();
            return -1;
        }
    };
    let newsize = newsize as usize;
    if old_data.len() == newsize {
        return 0;
    }
    let mut data = Vec::new();
    if data.try_reserve_exact(newsize).is_err() {
        *pv = ptr::null_mut();
        dec_ref(old);
        set_error("MemoryError: cannot allocate bytes");
        return -1;
    }
    data.resize(newsize, 0);
    let copied = old_data.len().min(newsize);
    data[..copied].copy_from_slice(&old_data[..copied]);
    let replacement = new_bytes(data);
    *pv = replacement;
    dec_ref(old);
    0
}

// ---------------------------------------------------------------------------
// Time conversions
// ---------------------------------------------------------------------------

/// `SEC_TO_NS`.
const SEC_TO_NS: i64 = 1_000_000_000;

/// `_PyTime_round_t` values (`Include/internal/pycore_time.h`).
const ROUND_FLOOR: c_int = 0;
const ROUND_CEILING: c_int = 1;
const ROUND_UP: c_int = 3;

fn round_time(value: f64, round: c_int) -> f64 {
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

fn time_overflow() {
    set_error("OverflowError: timestamp out of range for C PyTime_t");
}

/// `_PyTime_FromSeconds`: seconds to nanoseconds.
#[no_mangle]
pub extern "C" fn _PyTime_FromSeconds(seconds: c_int) -> i64 {
    (seconds as i64) * SEC_TO_NS
}

/// `_PyTime_FromLong`: a Python int number of nanoseconds.
#[no_mangle]
pub unsafe extern "C" fn _PyTime_FromLong(t: *mut i64, object: *mut ShimObject) -> c_int {
    if t.is_null() {
        super::bad_internal_call();
        return -1;
    }
    match kind_ref(object) {
        Some(ShimKind::Int(_) | ShimKind::Bool(_)) => {}
        _ => {
            set_type_error(format!("expect int, got {}", object_type_name(object)));
            return -1;
        }
    }
    match as_int(object) {
        Ok(value) => {
            *t = value;
            0
        }
        Err(_) => {
            set_type_error("an integer is required");
            -1
        }
    }
}

/// `_PyTime_FromSecondsObject`: seconds (int or float) to nanoseconds.
#[no_mangle]
pub unsafe extern "C" fn _PyTime_FromSecondsObject(
    t: *mut i64,
    object: *mut ShimObject,
    round: c_int,
) -> c_int {
    if t.is_null() {
        super::bad_internal_call();
        return -1;
    }
    match kind_ref(object) {
        Some(ShimKind::Float(value)) => {
            let value = *value;
            if value.is_nan() {
                set_error("ValueError: Invalid value NaN (not a number)");
                return -1;
            }
            let nanoseconds = round_time(value * SEC_TO_NS as f64, round);
            if !(i64::MIN as f64 <= nanoseconds && nanoseconds < -(i64::MIN as f64)) {
                time_overflow();
                *t = 0;
                return -1;
            }
            *t = nanoseconds as i64;
            0
        }
        Some(ShimKind::Int(_) | ShimKind::Bool(_)) => {
            let seconds = match as_int(object) {
                Ok(value) => value,
                Err(_) => {
                    set_type_error("an integer is required");
                    return -1;
                }
            };
            match seconds.checked_mul(SEC_TO_NS) {
                Some(nanoseconds) => {
                    *t = nanoseconds;
                    0
                }
                None => {
                    time_overflow();
                    -1
                }
            }
        }
        _ => {
            set_type_error(format!(
                "'{}' object cannot be interpreted as an integer or float",
                object_type_name(object)
            ));
            -1
        }
    }
}

/// `_PyTime_PerfCounterRaw`? Not provided here; `PyTime_PerfCounterRaw` is the
/// public entry point and lives in [`super`].
///
/// `_Py_Gid_Converter`: convert a Python object to `gid_t`.
#[no_mangle]
pub unsafe extern "C" fn _Py_Gid_Converter(
    object: *mut ShimObject,
    out: *mut libc::gid_t,
) -> c_int {
    convert_id(object, out, "gid")
}

/// `_Py_Uid_Converter`: convert a Python object to `uid_t`.
#[no_mangle]
pub unsafe extern "C" fn _Py_Uid_Converter(
    object: *mut ShimObject,
    out: *mut libc::uid_t,
) -> c_int {
    convert_id(object, out, "uid")
}

/// Shared body of [`_Py_Uid_Converter`] and [`_Py_Gid_Converter`].
///
/// The two differ only in the diagnostic text; `uid_t`/`gid_t` are 32-bit on
/// every supported Linux target.
unsafe fn convert_id(object: *mut ShimObject, out: *mut u32, label: &str) -> c_int {
    if out.is_null() {
        super::bad_internal_call();
        return 0;
    }
    if object.is_null() {
        set_type_error(format!("{label} should be integer, not NULL"));
        return 0;
    }
    let index = types::PyNumber_Index(object);
    if index.is_null() {
        set_type_error(format!(
            "{label} should be integer, not {}",
            object_type_name(object)
        ));
        return 0;
    }
    let mut overflow: c_int = 0;
    let signed = super::PyLong_AsLongAndOverflow(index, &mut overflow);
    if overflow == 0 {
        if signed == -1 && error_pending() {
            dec_ref(index);
            return 0;
        }
        if signed < 0 && signed != -1 {
            set_error(format!("OverflowError: {label} is less than minimum"));
            dec_ref(index);
            return 0;
        }
        if signed != -1 {
            let cast = signed as u32;
            if (cast as i64) != signed {
                set_error(format!("OverflowError: {label} is less than minimum"));
                dec_ref(index);
                return 0;
            }
            *out = cast;
            dec_ref(index);
            return 1;
        }
        // A genuine -1 means "no change" for chown and friends.
        *out = u32::MAX;
        dec_ref(index);
        return 1;
    }
    if overflow < 0 {
        set_error(format!("OverflowError: {label} is less than minimum"));
        dec_ref(index);
        return 0;
    }
    let unsigned = super::PyLong_AsUnsignedLong(index);
    if error_pending() {
        super::initialize_exception_types();
        let is_overflow = super::PyErr_ExceptionMatches(super::PyExc_OverflowError) != 0;
        dec_ref(index);
        if is_overflow {
            set_error(format!("OverflowError: {label} is greater than maximum"));
        }
        return 0;
    }
    let cast = unsigned as u32;
    if unsigned > u32::MAX as c_ulong || cast == u32::MAX {
        set_error(format!("OverflowError: {label} is greater than maximum"));
        dec_ref(index);
        return 0;
    }
    *out = cast;
    dec_ref(index);
    1
}

// ---------------------------------------------------------------------------
// POSIX helpers used by `_posixsubprocess`
// ---------------------------------------------------------------------------

/// `_Py_RestoreSignals`: reset the signals Python ignores back to `SIG_DFL`.
#[no_mangle]
pub unsafe extern "C" fn _Py_RestoreSignals() {
    #[cfg(unix)]
    {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            libc::signal(libc::SIGXFSZ, libc::SIG_DFL);
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            libc::signal(libc::SIGXFSZ, libc::SIG_DFL);
        }
    }
}

/// `_Py_open_noraise`: `open()` that sets `errno` instead of an exception.
///
/// The descriptor is created close-on-exec, like CPython's helper.
#[no_mangle]
pub unsafe extern "C" fn _Py_open_noraise(pathname: *const c_char, flags: c_int) -> c_int {
    if pathname.is_null() {
        *libc::__errno_location() = libc::EINVAL;
        return -1;
    }
    let flags = flags | libc::O_CLOEXEC;
    let fd = libc::open(pathname, flags);
    if fd < 0 {
        return -1;
    }
    if _Py_set_inheritable_async_safe(fd, 0, ptr::null_mut()) < 0 {
        let error = *libc::__errno_location();
        libc::close(fd);
        *libc::__errno_location() = error;
        return -1;
    }
    fd
}

/// `_PY_WRITE_MAX` from CPython: the largest single `write()` request.
const PY_WRITE_MAX: usize = 0x7fff_f000;

/// `_Py_write_noraise`: `write()` retrying `EINTR`, setting only `errno`.
#[no_mangle]
pub unsafe extern "C" fn _Py_write_noraise(fd: c_int, buf: *const c_void, count: usize) -> isize {
    if buf.is_null() && count != 0 {
        *libc::__errno_location() = libc::EINVAL;
        return -1;
    }
    let count = count.min(PY_WRITE_MAX);
    loop {
        let written = libc::write(fd, buf, count);
        if written >= 0 {
            return written as isize;
        }
        let error = *libc::__errno_location();
        if error != libc::EINTR {
            return -1;
        }
    }
}

/// `_Py_set_inheritable_async_safe`: toggle `FD_CLOEXEC` without raising.
///
/// `atomic_flag` follows CPython's protocol: `-1` means "probe the descriptor
/// once", `1` means "the flag is already applied", `0` means "apply it".
#[no_mangle]
pub unsafe extern "C" fn _Py_set_inheritable_async_safe(
    fd: c_int,
    inheritable: c_int,
    atomic_flag: *mut c_int,
) -> c_int {
    if !atomic_flag.is_null() && inheritable == 0 {
        let mut flag = *atomic_flag;
        if flag == -1 {
            let current = libc::fcntl(fd, libc::F_GETFD);
            if current < 0 {
                return -1;
            }
            flag = ((current & libc::FD_CLOEXEC) != 0) as c_int;
            *atomic_flag = flag;
        }
        if flag != 0 {
            return 0;
        }
    }
    let flags = libc::fcntl(fd, libc::F_GETFD);
    if flags < 0 {
        return -1;
    }
    let new_flags = if inheritable != 0 {
        flags & !libc::FD_CLOEXEC
    } else {
        flags | libc::FD_CLOEXEC
    };
    if new_flags == flags {
        return 0;
    }
    if libc::fcntl(fd, libc::F_SETFD, new_flags) < 0 {
        return -1;
    }
    0
}

// ---------------------------------------------------------------------------
// `_Py_hashtable`
// ---------------------------------------------------------------------------

/// One bucket-list link (`_Py_slist_item_t`).
#[repr(C)]
pub struct SlistItem {
    pub next: *mut SlistItem,
}

/// One bucket (`_Py_slist_t`).
#[repr(C)]
pub struct Slist {
    pub head: *mut SlistItem,
}

/// `_Py_hashtable_entry_t`.
#[repr(C)]
pub struct HashtableEntry {
    pub item: SlistItem,
    pub key_hash: PyUHash,
    pub key: *mut c_void,
    pub value: *mut c_void,
}

/// `_Py_hashtable_allocator_t`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HashtableAllocator {
    pub malloc: Option<unsafe extern "C" fn(usize) -> *mut c_void>,
    pub free: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// `_Py_hashtable_t`.
#[repr(C)]
pub struct Hashtable {
    pub nentries: usize,
    pub nbuckets: usize,
    pub buckets: *mut Slist,
    pub get_entry_func:
        Option<unsafe extern "C" fn(*mut Hashtable, *const c_void) -> *mut HashtableEntry>,
    pub hash_func: Option<unsafe extern "C" fn(*const c_void) -> PyUHash>,
    pub compare_func: Option<unsafe extern "C" fn(*const c_void, *const c_void) -> c_int>,
    pub key_destroy_func: Option<unsafe extern "C" fn(*mut c_void)>,
    pub value_destroy_func: Option<unsafe extern "C" fn(*mut c_void)>,
    pub alloc: HashtableAllocator,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::offset_of!(Hashtable, nentries) == 0);
    assert!(std::mem::offset_of!(Hashtable, nbuckets) == 8);
    assert!(std::mem::offset_of!(Hashtable, buckets) == 16);
    assert!(std::mem::offset_of!(Hashtable, get_entry_func) == 24);
    assert!(std::mem::offset_of!(Hashtable, hash_func) == 32);
    assert!(std::mem::offset_of!(Hashtable, compare_func) == 40);
    assert!(std::mem::offset_of!(Hashtable, key_destroy_func) == 48);
    assert!(std::mem::offset_of!(Hashtable, value_destroy_func) == 56);
    assert!(std::mem::offset_of!(Hashtable, alloc) == 64);
    assert!(std::mem::size_of::<Hashtable>() == 80);
    assert!(std::mem::size_of::<HashtableAllocator>() == 16);
    assert!(std::mem::size_of::<HashtableEntry>() == 32);
};

/// `HASHTABLE_MIN_SIZE`.
const HASHTABLE_MIN_SIZE: usize = 16;
/// `HASHTABLE_HIGH`.
const HASHTABLE_HIGH: f64 = 0.50;
/// `HASHTABLE_LOW`.
const HASHTABLE_LOW: f64 = 0.10;
/// `HASHTABLE_REHASH_FACTOR`.
const HASHTABLE_REHASH_FACTOR: f64 = 2.0 / (HASHTABLE_LOW + HASHTABLE_HIGH);

/// Default allocator's `malloc`.
unsafe extern "C" fn hashtable_malloc(size: usize) -> *mut c_void {
    libc::malloc(size)
}

/// Default allocator's `free`.
unsafe extern "C" fn hashtable_free(pointer: *mut c_void) {
    libc::free(pointer)
}

unsafe fn allocator_malloc(allocator: &HashtableAllocator, size: usize) -> *mut c_void {
    match allocator.malloc {
        Some(malloc) => malloc(size),
        None => ptr::null_mut(),
    }
}

unsafe fn allocator_free(allocator: &HashtableAllocator, pointer: *mut c_void) {
    if let Some(free) = allocator.free {
        free(pointer);
    }
}

/// Rounds a bucket count up to a power of two, with CPython's minimum.
fn round_size(size: usize) -> usize {
    if size < HASHTABLE_MIN_SIZE {
        return HASHTABLE_MIN_SIZE;
    }
    let mut power = 1usize;
    while power < size {
        power <<= 1;
    }
    power
}

/// `_Py_hashtable_hash_ptr`: rotate the pointer right by four bits, as CPython
/// does to avoid the low alignment bits.
#[no_mangle]
pub unsafe extern "C" fn _Py_hashtable_hash_ptr(key: *const c_void) -> PyUHash {
    (key as usize).rotate_right(4)
}

/// `_Py_hashtable_compare_direct`: pointer identity.
#[no_mangle]
pub unsafe extern "C" fn _Py_hashtable_compare_direct(
    key1: *const c_void,
    key2: *const c_void,
) -> c_int {
    (key1 == key2) as c_int
}

unsafe fn entry_at(ht: *mut Hashtable, bucket: usize) -> *mut HashtableEntry {
    let list = &*ht;
    let head = (*list.buckets.add(bucket)).head;
    head as *mut HashtableEntry
}

unsafe fn entry_next(entry: *mut HashtableEntry) -> *mut HashtableEntry {
    (*entry).item.next as *mut HashtableEntry
}

unsafe extern "C" fn get_entry_generic(
    ht: *mut Hashtable,
    key: *const c_void,
) -> *mut HashtableEntry {
    let table = &*ht;
    let hash = match table.hash_func {
        Some(hash) => hash(key),
        None => 0,
    };
    let compare = match table.compare_func {
        Some(compare) => compare,
        None => _Py_hashtable_compare_direct,
    };
    let mut entry = entry_at(ht, hash & (table.nbuckets - 1));
    while !entry.is_null() {
        if (*entry).key_hash == hash && compare(key, (*entry).key) != 0 {
            return entry;
        }
        entry = entry_next(entry);
    }
    ptr::null_mut()
}

unsafe extern "C" fn get_entry_ptr(ht: *mut Hashtable, key: *const c_void) -> *mut HashtableEntry {
    let table = &*ht;
    let hash = _Py_hashtable_hash_ptr(key);
    let mut entry = entry_at(ht, hash & (table.nbuckets - 1));
    while !entry.is_null() {
        if std::ptr::eq((*entry).key.cast_const(), key) {
            return entry;
        }
        entry = entry_next(entry);
    }
    ptr::null_mut()
}

unsafe fn hashtable_rehash(ht: *mut Hashtable) -> c_int {
    let nentries = (*ht).nentries;
    let nbuckets = (*ht).nbuckets;
    let buckets = (*ht).buckets;
    let alloc = (*ht).alloc;
    let new_size = round_size((nentries as f64 * HASHTABLE_REHASH_FACTOR) as usize);
    if new_size == nbuckets {
        return 0;
    }
    let buckets_size = new_size * std::mem::size_of::<Slist>();
    let new_buckets = allocator_malloc(&alloc, buckets_size) as *mut Slist;
    if new_buckets.is_null() {
        return -1;
    }
    ptr::write_bytes(new_buckets, 0, new_size);
    for bucket in 0..nbuckets {
        let mut entry = (*(buckets.add(bucket))).head as *mut HashtableEntry;
        while !entry.is_null() {
            let next = entry_next(entry);
            let index = (*entry).key_hash & (new_size - 1);
            (*entry).item.next = (*new_buckets.add(index)).head;
            (*new_buckets.add(index)).head = ptr::addr_of_mut!((*entry).item);
            entry = next;
        }
    }
    allocator_free(&alloc, buckets as *mut c_void);
    (*ht).nbuckets = new_size;
    (*ht).buckets = new_buckets;
    0
}

/// `_Py_hashtable_new_full`: allocate a pointer-keyed table.
#[no_mangle]
pub unsafe extern "C" fn _Py_hashtable_new_full(
    hash_func: Option<unsafe extern "C" fn(*const c_void) -> PyUHash>,
    compare_func: Option<unsafe extern "C" fn(*const c_void, *const c_void) -> c_int>,
    key_destroy_func: Option<unsafe extern "C" fn(*mut c_void)>,
    value_destroy_func: Option<unsafe extern "C" fn(*mut c_void)>,
    allocator: *mut HashtableAllocator,
) -> *mut Hashtable {
    let alloc = if allocator.is_null() {
        HashtableAllocator {
            malloc: Some(hashtable_malloc),
            free: Some(hashtable_free),
        }
    } else {
        HashtableAllocator {
            malloc: (*allocator).malloc,
            free: (*allocator).free,
        }
    };
    let table = allocator_malloc(&alloc, std::mem::size_of::<Hashtable>()) as *mut Hashtable;
    if table.is_null() {
        return ptr::null_mut();
    }
    let buckets_size = HASHTABLE_MIN_SIZE * std::mem::size_of::<Slist>();
    let buckets = allocator_malloc(&alloc, buckets_size) as *mut Slist;
    if buckets.is_null() {
        allocator_free(&alloc, table as *mut c_void);
        return ptr::null_mut();
    }
    ptr::write_bytes(buckets, 0, HASHTABLE_MIN_SIZE);
    // The specialized getter is only valid for the exact pointer hash/compare
    // pair; selecting it the same way CPython does keeps the fast path.
    let pointer_pair = hash_func.map(|hash| hash as *const () as usize)
        == Some(_Py_hashtable_hash_ptr as *const () as usize)
        && compare_func.map(|compare| compare as *const () as usize)
            == Some(_Py_hashtable_compare_direct as *const () as usize);
    let getter: unsafe extern "C" fn(*mut Hashtable, *const c_void) -> *mut HashtableEntry =
        if pointer_pair {
            get_entry_ptr
        } else {
            get_entry_generic
        };
    ptr::write(
        table,
        Hashtable {
            nentries: 0,
            nbuckets: HASHTABLE_MIN_SIZE,
            buckets,
            get_entry_func: Some(getter),
            hash_func,
            compare_func,
            key_destroy_func,
            value_destroy_func,
            alloc,
        },
    );
    table
}

/// Frees one entry, running the destroy callbacks.
unsafe fn destroy_entry(
    alloc: HashtableAllocator,
    key_destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    value_destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    entry: *mut HashtableEntry,
) {
    if let Some(destroy) = key_destroy {
        destroy((*entry).key);
    }
    if let Some(destroy) = value_destroy {
        destroy((*entry).value);
    }
    allocator_free(&alloc, entry as *mut c_void);
}

/// `_Py_hashtable_destroy`: free the table and every entry.
#[no_mangle]
pub unsafe extern "C" fn _Py_hashtable_destroy(ht: *mut Hashtable) {
    if ht.is_null() {
        return;
    }
    let nbuckets = (*ht).nbuckets;
    let buckets = (*ht).buckets;
    let alloc = (*ht).alloc;
    let key_destroy = (*ht).key_destroy_func;
    let value_destroy = (*ht).value_destroy_func;
    for bucket in 0..nbuckets {
        let mut entry = (*(buckets.add(bucket))).head as *mut HashtableEntry;
        while !entry.is_null() {
            let next = entry_next(entry);
            destroy_entry(alloc, key_destroy, value_destroy, entry);
            entry = next;
        }
    }
    allocator_free(&alloc, buckets as *mut c_void);
    allocator_free(&alloc, ht as *mut c_void);
}

/// `_Py_hashtable_clear`: drop every entry and shrink back to the minimum.
#[no_mangle]
pub unsafe extern "C" fn _Py_hashtable_clear(ht: *mut Hashtable) {
    if ht.is_null() {
        return;
    }
    let nbuckets = (*ht).nbuckets;
    let buckets = (*ht).buckets;
    let alloc = (*ht).alloc;
    let key_destroy = (*ht).key_destroy_func;
    let value_destroy = (*ht).value_destroy_func;
    for bucket in 0..nbuckets {
        let mut entry = (*(buckets.add(bucket))).head as *mut HashtableEntry;
        while !entry.is_null() {
            let next = entry_next(entry);
            destroy_entry(alloc, key_destroy, value_destroy, entry);
            entry = next;
        }
        (*(buckets.add(bucket))).head = ptr::null_mut();
    }
    (*ht).nentries = 0;
    if nbuckets != HASHTABLE_MIN_SIZE {
        let size = HASHTABLE_MIN_SIZE * std::mem::size_of::<Slist>();
        let replacement = allocator_malloc(&alloc, size) as *mut Slist;
        if !replacement.is_null() {
            ptr::write_bytes(replacement, 0, HASHTABLE_MIN_SIZE);
            allocator_free(&alloc, buckets as *mut c_void);
            (*ht).buckets = replacement;
            (*ht).nbuckets = HASHTABLE_MIN_SIZE;
        }
    }
}

/// `_Py_hashtable_set`: insert a new key/value pair.
#[no_mangle]
pub unsafe extern "C" fn _Py_hashtable_set(
    ht: *mut Hashtable,
    key: *const c_void,
    value: *mut c_void,
) -> c_int {
    if ht.is_null() {
        return -1;
    }
    let alloc = (*ht).alloc;
    let entry =
        allocator_malloc(&alloc, std::mem::size_of::<HashtableEntry>()) as *mut HashtableEntry;
    if entry.is_null() {
        return -1;
    }
    let hash = match (*ht).hash_func {
        Some(hash) => hash(key),
        None => 0,
    };
    ptr::write(
        entry,
        HashtableEntry {
            item: SlistItem {
                next: ptr::null_mut(),
            },
            key_hash: hash,
            key: key as *mut c_void,
            value,
        },
    );
    (*ht).nentries += 1;
    if ((*ht).nentries as f64) / ((*ht).nbuckets as f64) > HASHTABLE_HIGH
        && hashtable_rehash(ht) < 0
    {
        (*ht).nentries -= 1;
        allocator_free(&alloc, entry as *mut c_void);
        return -1;
    }
    let bucket = hash & ((*ht).nbuckets - 1);
    let list = (*ht).buckets.add(bucket);
    (*entry).item.next = (*list).head;
    (*list).head = ptr::addr_of_mut!((*entry).item);
    0
}

/// `_Py_hashtable_get`: the value stored for `key`, or NULL.
#[no_mangle]
pub unsafe extern "C" fn _Py_hashtable_get(ht: *mut Hashtable, key: *const c_void) -> *mut c_void {
    if ht.is_null() {
        return ptr::null_mut();
    }
    let getter: unsafe extern "C" fn(*mut Hashtable, *const c_void) -> *mut HashtableEntry =
        match (*ht).get_entry_func {
            Some(getter) => getter,
            None => get_entry_generic,
        };
    let entry = getter(ht, key);
    if entry.is_null() {
        ptr::null_mut()
    } else {
        (*entry).value
    }
}

// ---------------------------------------------------------------------------
// Data objects read directly by extensions
// ---------------------------------------------------------------------------

/// CPython's `_Py_HashSecret_t`: 24 bytes of hash salt.
#[repr(C, align(8))]
pub struct HashSecret {
    pub bytes: [u8; 24],
}

/// `_Py_HashSecret`: read by `_elementtree` for expat's hash salt.
///
/// The initial value is a fixed non-zero pattern; [`initialize_hash_secret`]
/// replaces it with OS entropy when the runtime initialises.
#[no_mangle]
pub static mut _Py_HashSecret: HashSecret = HashSecret {
    bytes: [
        0x53, 0x2b, 0x1d, 0x77, 0x9c, 0x41, 0xe2, 0x0b, 0x86, 0xd4, 0x3f, 0x6a, 0x19, 0xb8, 0x55,
        0xce, 0x27, 0x90, 0x0d, 0xf3, 0x64, 0xab, 0x38, 0x81,
    ],
};

/// CPython's `PY_CTF_*` bits (`Include/cpython/pyctype.h`).
const PY_CTF_LOWER: c_uint = 0x01;
const PY_CTF_UPPER: c_uint = 0x02;
const PY_CTF_DIGIT: c_uint = 0x04;
const PY_CTF_SPACE: c_uint = 0x08;
const PY_CTF_XDIGIT: c_uint = 0x10;

const fn ctype_entry(character: u8) -> c_uint {
    let mut flags = 0;
    if character >= b'a' && character <= b'z' {
        flags |= PY_CTF_LOWER;
    }
    if character >= b'A' && character <= b'Z' {
        flags |= PY_CTF_UPPER;
    }
    if character >= b'0' && character <= b'9' {
        flags |= PY_CTF_DIGIT | PY_CTF_XDIGIT;
    }
    if (character >= b'a' && character <= b'f') || (character >= b'A' && character <= b'F') {
        flags |= PY_CTF_XDIGIT;
    }
    if matches!(character, 0x09..=0x0d | 0x20) {
        flags |= PY_CTF_SPACE;
    }
    flags
}

const fn build_ctype_table() -> [c_uint; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        table[index] = ctype_entry(index as u8);
        index += 1;
    }
    table
}

const fn build_ctype_tolower() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut index = 0;
    while index < 256 {
        let character = index as u8;
        table[index] = if character >= b'A' && character <= b'Z' {
            character + (b'a' - b'A')
        } else {
            character
        };
        index += 1;
    }
    table
}

/// `_Py_ctype_table`: locale-independent `ctype` classification bits.
#[no_mangle]
pub static _Py_ctype_table: [c_uint; 256] = build_ctype_table();

/// `_Py_ctype_tolower`: ASCII-only lower-casing table.
#[no_mangle]
pub static _Py_ctype_tolower: [u8; 256] = build_ctype_tolower();

/// Seeds [`_Py_HashSecret`] from the OS once.
///
/// Called from the runtime's link anchor. The fixed compile-time value keeps
/// the symbol non-zero even if entropy is unavailable.
pub(crate) fn initialize_hash_secret() {
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};
    static INITIALIZED: AtomicBool = AtomicBool::new(false);
    if INITIALIZED.swap(true, Ordering::Relaxed) {
        return;
    }
    let mut bytes = [0u8; 24];
    let mut filled = false;
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        filled = file.read_exact(&mut bytes).is_ok();
    }
    if !filled {
        // Deterministic fallback: time, pid and this static's address.
        let address = ptr::addr_of_mut!(_Py_HashSecret) as usize as u64;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        let pid = std::process::id() as u64;
        let mut state = address ^ now.rotate_left(17) ^ pid.rotate_left(41);
        for slot in bytes.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *slot = state as u8;
        }
    }
    if bytes.iter().all(|&byte| byte == 0) {
        bytes[0] = 0x5a;
        bytes[23] = 0xa5;
    }
    // SAFETY: the runtime initialises this before any extension can read it;
    // the symbol is plain data and the write is not racing a reader.
    unsafe {
        (*ptr::addr_of_mut!(_Py_HashSecret)).bytes = bytes;
    }
}

// ---------------------------------------------------------------------------
// Link anchor
// ---------------------------------------------------------------------------

/// References every entry point and data object so the staticlib linker keeps
/// them in the executable's dynamic symbol table.
pub(crate) fn anchor() {
    initialize_hash_secret();
    let functions: &[*const ()] = &[
        _PyArg_CheckPositional as *const (),
        _PyArg_BadArgument as *const (),
        _PyArg_UnpackKeywords as *const (),
        _PyObject_MakeTpCall as *const (),
        _Py_CheckFunctionResult as *const (),
        _Py_CheckRecursiveCall as *const (),
        _PyThreadState_GetCurrent as *const (),
        _PyEval_SetProfile as *const (),
        _PyErr_ChainExceptions1 as *const (),
        _PyErr_Format as *const (),
        _PyErr_FormatFromCause as *const (),
        _PyNumber_Index as *const (),
        _PyType_LookupRef as *const (),
        _PyUnicode_EqualToASCIIString as *const (),
        _PyUnicode_AsUTF8String as *const (),
        _PySys_GetOptionalAttrString as *const (),
        _PyLong_AsByteArray as *const (),
        _PyLong_FromByteArray as *const (),
        _PyLong_Format as *const (),
        _PyTuple_Resize as *const (),
        _PyBytes_Resize as *const (),
        _PyTime_FromSeconds as *const (),
        _PyTime_FromLong as *const (),
        _PyTime_FromSecondsObject as *const (),
        _Py_Gid_Converter as *const (),
        _Py_Uid_Converter as *const (),
        _Py_RestoreSignals as *const (),
        _Py_open_noraise as *const (),
        _Py_write_noraise as *const (),
        _Py_set_inheritable_async_safe as *const (),
        _Py_hashtable_hash_ptr as *const (),
        _Py_hashtable_compare_direct as *const (),
        _Py_hashtable_new_full as *const (),
        _Py_hashtable_destroy as *const (),
        _Py_hashtable_clear as *const (),
        _Py_hashtable_set as *const (),
        _Py_hashtable_get as *const (),
        PyArg_Parse as *const (),
        PyDict_Items as *const (),
        PyDict_PopString as *const (),
        PyErr_FormatUnraisable as *const (),
        PyErr_SetFromErrnoWithFilename as *const (),
        PyImport_ImportModuleAttrString as *const (),
        PyList_Clear as *const (),
        PyList_Reverse as *const (),
        PyModule_AddObjectRef as *const (),
        PyModule_AddType as *const (),
        PyOS_AfterFork_Child as *const (),
        PyOS_AfterFork_Parent as *const (),
        PyOS_BeforeFork as *const (),
        PyStructSequence_GetItem as *const (),
        PyStructSequence_New as *const (),
        PyStructSequence_NewType as *const (),
        PyStructSequence_SetItem as *const (),
        PySys_Audit as *const (),
        PyTime_PerfCounterRaw as *const (),
        PyType_GetModuleByDef as *const (),
        PyUnicode_EncodeFSDefault as *const (),
        PyUnicode_FSConverter as *const (),
        Py_ReprEnter as *const (),
        Py_ReprLeave as *const (),
    ];
    let data: &[*const u8] = &[
        ptr::addr_of!(_Py_ctype_table) as *const u8,
        ptr::addr_of!(_Py_ctype_tolower) as *const u8,
        ptr::addr_of!(_Py_HashSecret) as *const u8,
        ptr::addr_of!(PyOS_InputHook) as *const u8,
        ptr::addr_of!(Py_hexdigits) as *const u8,
    ];
    let _ = std::hint::black_box(functions);
    let _ = std::hint::black_box(data);
}

// ---------------------------------------------------------------------------
// Additional public C API used by the pinned `lib-dynload` extensions
// ---------------------------------------------------------------------------

/// Renders an errno as CPython's `OSError` message body.
fn errno_message(errno: c_int, filename: Option<&str>) -> String {
    let detail = std::io::Error::from_raw_os_error(errno).to_string();
    let detail = detail
        .rsplit_once(" (os error ")
        .map(|(text, _)| text)
        .unwrap_or(detail.as_str());
    match filename {
        Some(filename) => format!("[Errno {errno}] {detail}: '{filename}'"),
        None => format!("[Errno {errno}] {detail}"),
    }
}

/// `PyErr_SetFromErrnoWithFilename`: the filename-qualified `OSError` builder.
#[no_mangle]
pub unsafe extern "C" fn PyErr_SetFromErrnoWithFilename(
    exception: *mut ShimObject,
    filename: *const c_char,
) -> *mut ShimObject {
    super::initialize_exception_types();
    let errno = std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO);
    let filename = if filename.is_null() {
        None
    } else {
        Some(read_c_string(filename))
    };
    let class = if exception.is_null() {
        super::PyExc_OSError
    } else {
        exception
    };
    set_error(format!(
        "{}: {}",
        exception_prefix(class),
        errno_message(errno, filename.as_deref())
    ));
    ptr::null_mut()
}

/// `PyErr_FormatUnraisable`: report the pending exception on stderr and clear
/// it, as CPython's unraisable hook does when no hook is installed.
#[no_mangle]
pub unsafe extern "C" fn PyErr_FormatUnraisable(format: *const c_char, mut args: ...) {
    let context = if format.is_null() {
        "Exception ignored".to_string()
    } else {
        let text = unsafe { format_message(format, &mut args) }.unwrap_or_default();
        if text.is_empty() {
            "Exception ignored".to_string()
        } else {
            text
        }
    };
    let pending = super::take_error();
    eprintln!("Exception ignored in: {context}");
    if let Some(message) = pending {
        eprintln!("{message}");
        // `take_error` stashes the exception object for the raise path; an
        // unraisable report consumes it here instead.
        if let Some(exception) = super::take_error_exception(&message) {
            dec_ref(exception);
        }
    }
}

/// `PyUnicode_EncodeFSDefault`: encode a string with the filesystem encoding.
///
/// The supported platforms use UTF-8 with surrogate escapes; shim strings are
/// already valid UTF-8, so the encoding is the string's UTF-8 bytes.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_EncodeFSDefault(unicode: *mut ShimObject) -> *mut ShimObject {
    let Some(text) = as_string(unicode) else {
        set_type_error("encode() argument must be str, not NULL or non-string");
        return ptr::null_mut();
    };
    PyBytes_FromStringAndSize(text.as_ptr() as *const c_char, text.len() as isize)
}

/// `Py_CLEANUP_SUPPORTED` from CPython's `getargs.h`.
const PY_CLEANUP_SUPPORTED: c_int = 0x2_0000;

/// `PyUnicode_FSConverter`: the `O&` converter used for path arguments.
///
/// Accepts `str`, `bytes` and path-like objects; the stored `PyObject *` is a
/// new reference to a `bytes` path with no embedded NUL.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_FSConverter(arg: *mut ShimObject, addr: *mut c_void) -> c_int {
    if addr.is_null() {
        super::bad_internal_call();
        return 0;
    }
    let slot = addr as *mut *mut ShimObject;
    if arg.is_null() {
        let previous = *slot;
        *slot = ptr::null_mut();
        if !previous.is_null() {
            dec_ref(previous);
        }
        return 1;
    }
    let path = match kind_ref(arg) {
        Some(ShimKind::Str { .. } | ShimKind::Bytes { .. }) => {
            inc_ref(arg);
            arg
        }
        _ => {
            let fspath = PyObject_GetAttrString(arg, c"__fspath__".as_ptr());
            if fspath.is_null() {
                if !error_pending() {
                    set_type_error(
                        "expected str, bytes or os.PathLike object, not an unsupported object",
                    );
                }
                return 0;
            }
            let result = PyObject_CallNoArgs(fspath);
            dec_ref(fspath);
            if result.is_null() {
                return 0;
            }
            result
        }
    };
    let output = match kind_ref(path) {
        Some(ShimKind::Bytes { .. }) => {
            inc_ref(path);
            path
        }
        Some(ShimKind::Str { .. }) => PyUnicode_EncodeFSDefault(path),
        _ => {
            dec_ref(path);
            set_type_error("__fspath__() must return str or bytes");
            return 0;
        }
    };
    dec_ref(path);
    if output.is_null() {
        return 0;
    }
    let has_nul = bytes_data(output)
        .map(|data| data.contains(&0))
        .unwrap_or(false);
    if has_nul {
        dec_ref(output);
        set_error("ValueError: embedded null byte");
        return 0;
    }
    let previous = *slot;
    *slot = output;
    if !previous.is_null() {
        dec_ref(previous);
    }
    PY_CLEANUP_SUPPORTED
}

/// `PyImport_ImportModuleAttrString`: import a module and read one attribute.
#[no_mangle]
pub unsafe extern "C" fn PyImport_ImportModuleAttrString(
    module_name: *const c_char,
    attribute_name: *const c_char,
) -> *mut ShimObject {
    if module_name.is_null() || attribute_name.is_null() {
        super::bad_internal_call();
        return ptr::null_mut();
    }
    let module = PyImport_ImportModule(module_name);
    if module.is_null() {
        return ptr::null_mut();
    }
    let attribute = PyObject_GetAttrString(module, attribute_name);
    dec_ref(module);
    attribute
}

/// `PyModule_AddObjectRef`: add `value` without stealing the caller's reference.
#[no_mangle]
pub unsafe extern "C" fn PyModule_AddObjectRef(
    module: *mut ShimObject,
    name: *const c_char,
    value: *mut ShimObject,
) -> c_int {
    if value.is_null() {
        set_type_error("PyModule_AddObjectRef: value is NULL");
        return -1;
    }
    inc_ref(value);
    let status = PyModule_AddObject(module, name, value);
    if status != 0 {
        // `PyModule_AddObject` leaves ownership with the caller on failure; we
        // took the extra reference above, so balance it.
        dec_ref(value);
    }
    status
}

/// `PyModule_AddType`: add a type under its short name.
#[no_mangle]
pub unsafe extern "C" fn PyModule_AddType(
    module: *mut ShimObject,
    type_object: *mut types::TypeObject,
) -> c_int {
    if type_object.is_null() {
        set_type_error("PyModule_AddType: type is NULL");
        return -1;
    }
    let name = types::type_short_name(type_object);
    let Ok(name) = CString::new(name) else {
        set_type_error("PyModule_AddType: type name contains a NUL");
        return -1;
    };
    PyModule_AddObjectRef(module, name.as_ptr(), type_object as *mut ShimObject)
}

/// `PyList_Clear`: remove every item, releasing each reference.
///
/// Returns 0 on success and -1 with an exception when `list` is not a list.
#[no_mangle]
pub unsafe extern "C" fn PyList_Clear(list: *mut ShimObject) -> c_int {
    let items = match list_slots_mut(list) {
        Ok(items) => std::mem::take(items),
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    sync_list_prefix(list);
    for item in items {
        if item != 0 {
            dec_ref(item as *mut ShimObject);
        }
    }
    0
}

/// `PyList_Reverse`: reverse the list in place.
#[no_mangle]
pub unsafe extern "C" fn PyList_Reverse(list: *mut ShimObject) -> c_int {
    match list_slots_mut(list) {
        Ok(items) => {
            items.reverse();
            sync_list_prefix(list);
            0
        }
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyDict_Items`: a list of `(key, value)` tuples, in dictionary order.
#[no_mangle]
pub unsafe extern "C" fn PyDict_Items(dict: *mut ShimObject) -> *mut ShimObject {
    let pairs = match dict_slots(dict) {
        Ok(pairs) => pairs.to_vec(),
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let list = PyList_New(0);
    if list.is_null() {
        return ptr::null_mut();
    }
    for (key, value) in pairs {
        inc_ref(key as *mut ShimObject);
        inc_ref(value as *mut ShimObject);
        let pair = new_tuple(vec![key, value]);
        let status = PyList_Append(list, pair);
        dec_ref(pair);
        if status != 0 {
            dec_ref(list);
            return ptr::null_mut();
        }
    }
    list
}

/// `PyDict_PopString`: remove `key` and hand its value to `*result`.
///
/// Returns 1 when found, 0 when absent (with `*result = NULL`), -1 on error.
#[no_mangle]
pub unsafe extern "C" fn PyDict_PopString(
    dict: *mut ShimObject,
    key: *const c_char,
    result: *mut *mut ShimObject,
) -> c_int {
    if !result.is_null() {
        *result = ptr::null_mut();
    }
    if key.is_null() {
        super::bad_internal_call();
        return -1;
    }
    let name = read_c_string(key);
    let pairs = match dict_slots_mut(dict) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    let position = pairs.iter().position(|&(existing, _)| {
        matches!(
            kind_ref(existing as *mut ShimObject),
            Some(ShimKind::Str { text, .. }) if text == &name
        )
    });
    let Some(index) = position else {
        return 0;
    };
    let (stored_key, value) = pairs.remove(index);
    dec_ref(stored_key as *mut ShimObject);
    if !result.is_null() {
        *result = value as *mut ShimObject;
    } else {
        dec_ref(value as *mut ShimObject);
    }
    1
}

// `Py_ReprEnter` recursion guard, per thread (like CPython's thread-state
// dictionary list).
std::thread_local! {
    static REPR_GUARD: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `Py_ReprEnter`: enter a recursive `repr()` guard for `object`.
#[no_mangle]
pub unsafe extern "C" fn Py_ReprEnter(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        return 0;
    }
    REPR_GUARD.with(|guard| {
        let mut guard = guard.borrow_mut();
        if guard.contains(&(object as usize)) {
            1
        } else {
            guard.push(object as usize);
            0
        }
    })
}

/// `Py_ReprLeave`: leave the recursive `repr()` guard for `object`.
#[no_mangle]
pub unsafe extern "C" fn Py_ReprLeave(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    REPR_GUARD.with(|guard| {
        let mut guard = guard.borrow_mut();
        if let Some(position) = guard.iter().rposition(|&entry| entry == object as usize) {
            guard.remove(position);
        }
    });
}

/// `PyTime_PerfCounterRaw`: monotonic-clock nanoseconds.
#[no_mangle]
pub unsafe extern "C" fn PyTime_PerfCounterRaw(result: *mut i64) -> c_int {
    if result.is_null() {
        super::bad_internal_call();
        return -1;
    }
    let mut timestamp: libc::timespec = std::mem::zeroed();
    if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) != 0 {
        let errno = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO);
        set_error(format!("OSError: {}", errno_message(errno, None)));
        return -1;
    }
    let seconds = timestamp.tv_sec as i64;
    let nanoseconds = timestamp.tv_nsec as i64;
    match seconds
        .checked_mul(SEC_TO_NS)
        .and_then(|value| value.checked_add(nanoseconds))
    {
        Some(value) => {
            *result = value;
            0
        }
        None => {
            time_overflow();
            -1
        }
    }
}

/// `PyOS_InputHook`: the readline-style input hook installed by `_tkinter`.
#[no_mangle]
pub static mut PyOS_InputHook: Option<unsafe extern "C" fn() -> c_int> = None;

/// `PyOS_BeforeFork`: quiesce other threads before `fork()`.
#[no_mangle]
pub unsafe extern "C" fn PyOS_BeforeFork() {
    gil_acquire();
}

/// `PyOS_AfterFork_Parent`: resume the parent's interpreter lock.
#[no_mangle]
pub unsafe extern "C" fn PyOS_AfterFork_Parent() {
    gil_release();
}

/// `PyOS_AfterFork_Child`: resume the GIL state in the forked child.
#[no_mangle]
pub unsafe extern "C" fn PyOS_AfterFork_Child() {
    gil_release();
}

/// `Py_hexdigits`: the lowercase-then-uppercase hexadecimal alphabet.
#[no_mangle]
pub static Py_hexdigits: [u8; 23] = *b"0123456789abcdefABCDEF\0";

/// `PySys_Audit`: CPython's audit-hook entry point.
///
/// This host has no `PySys_AddAuditHook` implementation, so no hook can be
/// registered and the call is a faithful no-op once the event name is
/// validated (CPython also returns 0 when the hook list is empty).
#[no_mangle]
pub unsafe extern "C" fn PySys_Audit(
    event: *const c_char,
    _format: *const c_char,
    _args: ...
) -> c_int {
    if event.is_null() {
        set_error("SystemError: PySys_Audit: event must not be NULL");
        return -1;
    }
    0
}

/// `PyType_GetModuleByDef`: the module associated with `type` or one of its
/// bases, when that module was created from `def`.
///
/// Returns a borrowed reference, as CPython does.
#[no_mangle]
pub unsafe extern "C" fn PyType_GetModuleByDef(
    type_object: *mut ShimObject,
    definition: *mut super::PyModuleDefMirror,
) -> *mut ShimObject {
    if type_object.is_null() || definition.is_null() {
        super::bad_internal_call();
        return ptr::null_mut();
    }
    if !types::is_type_pointer(type_object) {
        set_type_error("PyType_GetModuleByDef: argument is not a type");
        return ptr::null_mut();
    }
    let mut current = type_object as *mut types::TypeObject;
    let mut steps = 0usize;
    while !current.is_null() && steps < 64 {
        if let Some(module) = types::type_module(current) {
            if PyModule_GetDef(module) == definition {
                return module;
            }
        }
        // SAFETY: every visited pointer was validated as a type.
        current = (*current).tp_base;
        steps += 1;
    }
    set_type_error(format!(
        "PyType_GetModuleByDef: cannot find module for type '{}'",
        types::type_name(type_object as *mut types::TypeObject)
    ));
    ptr::null_mut()
}

// ---------------------------------------------------------------------------
// `PyArg_Parse` (compatibility form)
// ---------------------------------------------------------------------------

/// `PyArg_Parse`: the pre-tuple compatibility parser used by `_tkinter`.
///
/// A tuple argument is parsed like `PyArg_ParseTuple`; any other object is
/// treated as the single argument described by the first format unit, as
/// CPython's `FLAG_COMPAT` mode does. Supported units: `s`, `es`/`et`, `O`,
/// `i`, `l`, `n`, `d`, `p`, with `|`, `:` and `;` handled like the tuple
/// parser. Unsupported units raise a real `TypeError`.
#[no_mangle]
pub unsafe extern "C" fn PyArg_Parse(
    args: *mut ShimObject,
    format: *const c_char,
    mut va: ...
) -> c_int {
    if format.is_null() {
        set_type_error("PyArg_Parse: format is NULL");
        return 0;
    }
    if args.is_null() {
        set_type_error("PyArg_Parse: argument object is NULL");
        return 0;
    }
    let text = CStr::from_ptr(format).to_bytes();
    let tuple_mode = matches!(kind_ref(args), Some(ShimKind::Tuple(_)));
    if !tuple_mode && text.first() == Some(&b'(') {
        set_type_error("PyArg_Parse: format uses parentheses with a non-tuple argument");
        return 0;
    }
    let items: &'static [usize] = if tuple_mode {
        match tuple_slots(args) {
            Ok(items) => items,
            Err(error) => {
                set_error(error);
                return 0;
            }
        }
    } else {
        &[]
    };
    let mut index = 0usize;
    let mut consumed = 0usize;
    let mut optional = false;
    let mut cursor = 0usize;
    while cursor < text.len() {
        let code = text[cursor];
        cursor += 1;
        match code {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c | 0x0b => continue,
            b'|' => {
                optional = true;
                continue;
            }
            b':' | b';' => break,
            _ => {}
        }
        let Some(object) = (if tuple_mode {
            items
                .get(index)
                .map(|&item| item as *mut ShimObject)
                .filter(|&item| !item.is_null())
        } else if consumed == 0 {
            Some(args)
        } else {
            None
        }) else {
            if optional {
                break;
            }
            set_type_error("PyArg_Parse: not enough arguments");
            return 0;
        };
        if tuple_mode {
            index += 1;
        }
        consumed += 1;
        let converted = unsafe { parse_compat_unit(code, object, &mut cursor, text, &mut va) };
        if converted != 1 {
            return converted;
        }
    }
    if tuple_mode && !optional && index < items.len() {
        set_type_error("PyArg_Parse: too many arguments");
        return 0;
    }
    1
}

/// Converts one [`PyArg_Parse`] unit. Returns 1 on success, 0 with an error.
unsafe fn parse_compat_unit(
    code: u8,
    object: *mut ShimObject,
    cursor: &mut usize,
    format: &[u8],
    va: &mut core::ffi::VaList<'_>,
) -> c_int {
    match code {
        b's' => {
            let out = unsafe { va.next_arg::<*mut *const c_char>() };
            if out.is_null() {
                set_type_error("PyArg_Parse: NULL output pointer for 's'");
                return 0;
            }
            match as_c_string(object) {
                Ok(text) => {
                    unsafe { *out = text };
                    1
                }
                Err(_) => {
                    set_type_error(format!(
                        "PyArg_Parse: argument must be str, not {}",
                        object_type_name(object)
                    ));
                    0
                }
            }
        }
        b'e' => {
            // 'e' consumes an encoding name, then one of 's'/'t'.
            let encoding = unsafe { va.next_arg::<*const c_char>() };
            if encoding.is_null() {
                set_type_error("PyArg_Parse: NULL encoding for 'e'");
                return 0;
            }
            let Some(&kind) = format.get(*cursor) else {
                set_type_error("PyArg_Parse: 'e' must be followed by 's' or 't'");
                return 0;
            };
            *cursor += 1;
            if kind != b's' && kind != b't' {
                set_type_error("PyArg_Parse: 'e' must be followed by 's' or 't'");
                return 0;
            }
            let out = unsafe { va.next_arg::<*mut *mut c_char>() };
            if out.is_null() {
                set_type_error("PyArg_Parse: NULL output pointer for 'e'");
                return 0;
            }
            let size_out = if kind == b's' {
                let size_out = unsafe { va.next_arg::<*mut isize>() };
                if size_out.is_null() {
                    set_type_error("PyArg_Parse: NULL size pointer for 'es'");
                    return 0;
                }
                Some(size_out)
            } else {
                None
            };
            let data = match kind_ref(object) {
                Some(ShimKind::Bytes { data, .. }) => data.clone(),
                Some(ShimKind::Str { text, .. }) => text.as_bytes().to_vec(),
                _ => {
                    set_type_error(format!(
                        "PyArg_Parse: argument must be str or bytes, not {}",
                        object_type_name(object)
                    ));
                    return 0;
                }
            };
            let Ok(buffer) = CString::new(data.clone()) else {
                set_error("ValueError: embedded null character");
                return 0;
            };
            if let Some(size_out) = size_out {
                unsafe { *size_out = data.len() as isize };
            }
            unsafe { *out = buffer.into_raw() };
            1
        }
        b'O' => {
            let out = unsafe { va.next_arg::<*mut *mut ShimObject>() };
            if out.is_null() {
                set_type_error("PyArg_Parse: NULL output pointer for 'O'");
                return 0;
            }
            unsafe { *out = object };
            1
        }
        b'i' => {
            let Ok(value) = as_int(object).and_then(|value| {
                i32::try_from(value).map_err(|_| "integer out of range".to_string())
            }) else {
                set_type_error(format!(
                    "PyArg_Parse: argument must be an integer, not {}",
                    object_type_name(object)
                ));
                return 0;
            };
            let out = unsafe { va.next_arg::<*mut c_int>() };
            if out.is_null() {
                set_type_error("PyArg_Parse: NULL output pointer for 'i'");
                return 0;
            }
            unsafe { *out = value };
            1
        }
        b'l' => {
            let Ok(value) = as_int(object).and_then(|value| {
                c_long::try_from(value).map_err(|_| "integer out of range".to_string())
            }) else {
                set_type_error("PyArg_Parse: argument must be an integer");
                return 0;
            };
            let out = unsafe { va.next_arg::<*mut c_long>() };
            if out.is_null() {
                set_type_error("PyArg_Parse: NULL output pointer for 'l'");
                return 0;
            }
            unsafe { *out = value };
            1
        }
        b'n' => {
            let Ok(value) = as_int(object).and_then(|value| {
                isize::try_from(value).map_err(|_| "integer out of range".to_string())
            }) else {
                set_type_error("PyArg_Parse: argument must be an integer");
                return 0;
            };
            let out = unsafe { va.next_arg::<*mut isize>() };
            if out.is_null() {
                set_type_error("PyArg_Parse: NULL output pointer for 'n'");
                return 0;
            }
            unsafe { *out = value };
            1
        }
        b'p' => {
            let Ok(value) = as_predicate(object) else {
                set_type_error("PyArg_Parse: argument must be a truth value");
                return 0;
            };
            let out = unsafe { va.next_arg::<*mut c_int>() };
            if out.is_null() {
                set_type_error("PyArg_Parse: NULL output pointer for 'p'");
                return 0;
            }
            unsafe { *out = value as c_int };
            1
        }
        b'd' => {
            let value = match as_float(object) {
                Ok(value) => value,
                Err(_) => {
                    set_type_error("PyArg_Parse: argument must be a float");
                    return 0;
                }
            };
            let out = unsafe { va.next_arg::<*mut f64>() };
            if out.is_null() {
                set_type_error("PyArg_Parse: NULL output pointer for 'd'");
                return 0;
            }
            unsafe { *out = value };
            1
        }
        other => {
            set_type_error(format!(
                "PyArg_Parse: unsupported format character '{}'",
                if other.is_ascii_graphic() {
                    other as char
                } else {
                    '?'
                }
            ));
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Struct sequences
// ---------------------------------------------------------------------------

/// Mirrors `PyStructSequence_Field`.
#[repr(C)]
pub struct StructSequenceField {
    pub name: *const c_char,
    pub doc: *const c_char,
}

/// Mirrors `PyStructSequence_Desc`.
#[repr(C)]
pub struct StructSequenceDesc {
    pub name: *const c_char,
    pub doc: *const c_char,
    pub fields: *mut StructSequenceField,
    pub n_in_sequence: c_int,
}

/// Type-slot identifiers used by the struct-sequence type.
const PY_TP_DOC: c_int = 56;
const PY_TP_NEW: c_int = 65;
const PY_TP_REPR: c_int = 66;
const PY_TP_MEMBERS: c_int = 72;
/// `PyMemberDef` `T_OBJECT`.
const T_OBJECT_MEMBER: c_int = 6;
/// `PyMemberDef` read-only flag.
const READONLY_MEMBER: c_int = 1;

/// Metadata for one struct-sequence type.
#[derive(Clone)]
struct StructSequenceMeta {
    type_address: usize,
    name: String,
    fields: Vec<String>,
    n_sequence: usize,
    n_fields: usize,
}

/// Every struct-sequence type created in this process.
///
/// The registry deliberately keeps one strong reference to each type: the
/// address is used to recognise tuple-kind instances, so the type must not be
/// freed and its address reused while the registry entry exists.
static STRUCTSEQ_TYPES: LazyLock<Mutex<Vec<StructSequenceMeta>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Runs `read` with the metadata of the struct-sequence type at `type_address`.
fn with_structseq_meta<R>(
    type_address: usize,
    read: impl FnOnce(&StructSequenceMeta) -> R,
) -> Option<R> {
    let registry = STRUCTSEQ_TYPES
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    registry
        .iter()
        .find(|meta| meta.type_address == type_address)
        .map(read)
}

/// True when `type_object` is a struct-sequence type created by this module.
///
/// Used by the object-model helpers in [`super`] to locate the inline tuple
/// payload of an instance whose `ob_type` is the struct-sequence type.
pub(crate) fn is_structseq_type(type_object: *mut types::TypeObject) -> bool {
    if type_object.is_null() {
        return false;
    }
    with_structseq_meta(type_object as usize, |_| ()).is_some()
}

/// Number of fields stored in an instance of `type_object`.
unsafe fn structseq_field_count(type_object: *mut types::TypeObject) -> Option<usize> {
    with_structseq_meta(type_object as usize, |meta| meta.n_fields)
}

/// `PyStructSequence_NewType`: build a tuple-subclass type for `desc`.
#[no_mangle]
pub unsafe extern "C" fn PyStructSequence_NewType(
    desc: *mut StructSequenceDesc,
) -> *mut types::TypeObject {
    if desc.is_null() {
        super::bad_internal_call();
        return ptr::null_mut();
    }
    let desc_ref = unsafe { &*desc };
    if desc_ref.name.is_null() || desc_ref.fields.is_null() {
        super::bad_internal_call();
        return ptr::null_mut();
    }
    if desc_ref.n_in_sequence < 0 {
        set_type_error("PyStructSequence_NewType: negative n_in_sequence");
        return ptr::null_mut();
    }
    let name = read_c_string(desc_ref.name);
    let mut fields = Vec::new();
    let mut cursor = desc_ref.fields;
    loop {
        // SAFETY: the descriptor owns a NULL-terminated field array.
        let field = unsafe { &*cursor };
        if field.name.is_null() {
            break;
        }
        fields.push(read_c_string(field.name));
        // SAFETY: advancing within the caller-owned array.
        cursor = unsafe { cursor.add(1) };
        if fields.len() > 4096 {
            set_type_error("PyStructSequence_NewType: field table is not terminated");
            return ptr::null_mut();
        }
    }
    if fields.is_empty() {
        set_type_error("PyStructSequence_NewType: no fields");
        return ptr::null_mut();
    }
    if desc_ref.n_in_sequence as usize != fields.len() {
        // This host allocates struct sequences as shim tuples whose inline
        // array length is `ob_size`; hidden fields would live past that array.
        set_error(format!(
            "SystemError: PyStructSequence_NewType: '{}' has {} hidden fields, which this host \
             does not support",
            name,
            fields.len() - desc_ref.n_in_sequence as usize
        ));
        return ptr::null_mut();
    }

    let mut members: Vec<types::PyMemberDefMirror> = fields
        .iter()
        .enumerate()
        .map(|(index, _field)| types::PyMemberDefMirror {
            name: unsafe { (*desc_ref.fields.add(index)).name },
            type_: T_OBJECT_MEMBER,
            offset: (super::tuple_layout::TUPLE_ITEMS_OFFSET + index * 8) as isize,
            flags: READONLY_MEMBER,
            doc: unsafe { (*desc_ref.fields.add(index)).doc },
        })
        .collect();
    members.push(types::PyMemberDefMirror {
        name: ptr::null(),
        type_: 0,
        offset: 0,
        flags: 0,
        doc: ptr::null(),
    });
    // The table is borrowed by the type for its whole lifetime (the member
    // descriptors read it directly), so the allocation is intentionally leaked
    // alongside the registry's strong reference to the type.
    let member_pointer = members.as_mut_ptr();
    std::mem::forget(members);
    let member_table = member_pointer as *mut c_void;

    let mut slots = vec![
        super::PyTypeSlotMirror {
            slot: PY_TP_NEW,
            pfunc: structseq_new as *const () as *mut c_void,
        },
        super::PyTypeSlotMirror {
            slot: PY_TP_REPR,
            pfunc: structseq_repr as *const () as *mut c_void,
        },
        super::PyTypeSlotMirror {
            slot: PY_TP_MEMBERS,
            pfunc: member_table,
        },
        super::PyTypeSlotMirror {
            slot: 0,
            pfunc: ptr::null_mut(),
        },
    ];
    if !desc_ref.doc.is_null() {
        slots.insert(
            2,
            super::PyTypeSlotMirror {
                slot: PY_TP_DOC,
                pfunc: desc_ref.doc as *mut c_void,
            },
        );
    }
    let basicsize = super::tuple_layout::TUPLE_ITEMS_OFFSET + fields.len() * 8;
    let mut spec = super::PyTypeSpecMirror {
        name: desc_ref.name,
        basicsize: basicsize as c_int,
        itemsize: 8,
        flags: 0,
        slots: slots.as_mut_ptr(),
    };
    let created = types::PyType_FromSpecWithBases(
        &mut spec,
        ptr::addr_of_mut!(types::TUPLE_TYPE) as *mut ShimObject,
    );
    if created.is_null() {
        return ptr::null_mut();
    }
    let type_object = created as *mut types::TypeObject;
    // Instances are shim tuples; the Shim role is what lets `kind_ref`,
    // tuple APIs and the deallocator treat them as such.
    unsafe { (*type_object).role = TypeRole::Shim };

    // CPython publishes the field counts and `__match_args__` on the type.
    let dict = unsafe { (*type_object).tp_dict as *mut ShimObject };
    if !dict.is_null() {
        for (key, value) in [
            (c"n_sequence_fields".as_ptr(), desc_ref.n_in_sequence as i64),
            (c"n_fields".as_ptr(), fields.len() as i64),
            (c"n_unnamed_fields".as_ptr(), 0),
        ] {
            let number = PyLong_FromLong(value as c_long);
            if number.is_null() {
                dec_ref(created);
                return ptr::null_mut();
            }
            let status = PyDict_SetItemString(dict, key, number);
            dec_ref(number);
            if status != 0 {
                dec_ref(created);
                return ptr::null_mut();
            }
        }
        let mut names = Vec::with_capacity(fields.len());
        for field in &fields {
            names.push(new_str(field.clone()) as usize);
        }
        let match_args = new_tuple(names);
        let status = PyDict_SetItemString(dict, c"__match_args__".as_ptr(), match_args);
        dec_ref(match_args);
        if status != 0 {
            dec_ref(created);
            return ptr::null_mut();
        }
    }

    inc_ref(created);
    let field_count = fields.len();
    STRUCTSEQ_TYPES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(StructSequenceMeta {
            type_address: type_object as usize,
            name,
            fields,
            n_sequence: desc_ref.n_in_sequence as usize,
            n_fields: field_count,
        });
    type_object
}

/// Allocates an empty instance of a struct-sequence type.
unsafe fn structseq_instance(type_object: *mut types::TypeObject) -> *mut ShimObject {
    let Some(count) = (unsafe { structseq_field_count(type_object) }) else {
        set_type_error("struct sequence type is not registered");
        return ptr::null_mut();
    };
    let object = new_tuple(vec![0usize; count]);
    // SAFETY: the tuple is freshly allocated and owned by this frame.
    unsafe { (*object).head.ob_type = type_object };
    object
}

/// `PyStructSequence_New`: allocate an instance with every field unset.
#[no_mangle]
pub unsafe extern "C" fn PyStructSequence_New(
    type_object: *mut types::TypeObject,
) -> *mut ShimObject {
    if type_object.is_null() {
        super::bad_internal_call();
        return ptr::null_mut();
    }
    unsafe { structseq_instance(type_object) }
}

/// Address of one inline struct-sequence slot.
unsafe fn structseq_slot(object: *mut ShimObject, index: usize) -> *mut usize {
    (object as *mut u8).add(super::tuple_layout::TUPLE_ITEMS_OFFSET + index * 8) as *mut usize
}

/// `PyStructSequence_SetItem`: store `value` in slot `index`, stealing the
/// reference exactly as CPython's raw tuple write does.
#[no_mangle]
pub unsafe extern "C" fn PyStructSequence_SetItem(
    object: *mut ShimObject,
    index: isize,
    value: *mut ShimObject,
) {
    if object.is_null() || index < 0 {
        super::bad_internal_call();
        return;
    }
    let type_object = super::type_of(object);
    let Some(count) = (unsafe { structseq_field_count(type_object) }) else {
        super::bad_internal_call();
        return;
    };
    let index = index as usize;
    if index >= count {
        set_error(format!(
            "SystemError: PyStructSequence_SetItem: index {index} out of range"
        ));
        return;
    }
    unsafe { *structseq_slot(object, index) = value as usize };
}

/// `PyStructSequence_GetItem`: borrowed reference from slot `index`.
#[no_mangle]
pub unsafe extern "C" fn PyStructSequence_GetItem(
    object: *mut ShimObject,
    index: isize,
) -> *mut ShimObject {
    if object.is_null() || index < 0 {
        super::bad_internal_call();
        return ptr::null_mut();
    }
    let type_object = super::type_of(object);
    let Some(count) = (unsafe { structseq_field_count(type_object) }) else {
        super::bad_internal_call();
        return ptr::null_mut();
    };
    let index = index as usize;
    if index >= count {
        set_error(format!(
            "SystemError: PyStructSequence_GetItem: index {index} out of range"
        ));
        return ptr::null_mut();
    }
    unsafe { *structseq_slot(object, index) as *mut ShimObject }
}

/// `tp_new` for struct-sequence types: `Type(sequence[, dict])`.
unsafe extern "C" fn structseq_new(
    type_object: *mut types::TypeObject,
    args: *mut ShimObject,
    _keywords: *mut ShimObject,
) -> *mut ShimObject {
    let Some(meta) = with_structseq_meta(type_object as usize, |meta| meta.clone()) else {
        set_type_error("struct sequence type is not registered");
        return ptr::null_mut();
    };
    if args.is_null() || !matches!(kind_ref(args), Some(ShimKind::Tuple(_))) {
        set_type_error(format!("{} constructor requires a sequence", meta.name));
        return ptr::null_mut();
    }
    let Ok(arguments) = tuple_slots(args) else {
        return ptr::null_mut();
    };
    if arguments.is_empty() || arguments.len() > 2 {
        set_type_error(format!(
            "{}() takes a sequence and an optional dict",
            meta.name
        ));
        return ptr::null_mut();
    }
    let sequence = arguments[0] as *mut ShimObject;
    let dictionary = if arguments.len() == 2 {
        arguments[1] as *mut ShimObject
    } else {
        ptr::null_mut()
    };
    if !dictionary.is_null() && !matches!(kind_ref(dictionary), Some(ShimKind::Dict(_))) {
        set_type_error(format!("{}() takes a dict as second arg", meta.name));
        return ptr::null_mut();
    }
    let list = PySequence_Fast(sequence, c"constructor requires a sequence".as_ptr());
    if list.is_null() {
        return ptr::null_mut();
    }
    let (length, getter): (
        usize,
        extern "C" fn(*mut ShimObject, isize) -> *mut ShimObject,
    ) = match kind_ref(list) {
        Some(ShimKind::Tuple(_)) => (
            tuple_slots(list).map(<[usize]>::len).unwrap_or(0),
            PyTuple_GetItem,
        ),
        Some(ShimKind::List(_)) => (PyList_Size(list).max(0) as usize, PyList_GetItem),
        _ => {
            dec_ref(list);
            set_type_error(format!("{} constructor requires a sequence", meta.name));
            return ptr::null_mut();
        }
    };
    if length != meta.n_sequence {
        dec_ref(list);
        set_type_error(format!(
            "{}() takes a {}-sequence ({} given)",
            meta.name, meta.n_sequence, length
        ));
        return ptr::null_mut();
    }
    if !dictionary.is_null()
        && dict_slots(dictionary)
            .map(|pairs| !pairs.is_empty())
            .unwrap_or(false)
    {
        dec_ref(list);
        set_type_error(format!(
            "{}() got duplicate or unexpected field name(s)",
            meta.name
        ));
        return ptr::null_mut();
    }
    let instance = unsafe { structseq_instance(type_object) };
    if instance.is_null() {
        dec_ref(list);
        return ptr::null_mut();
    }
    for index in 0..length {
        let value = getter(list, index as isize);
        if value.is_null() {
            dec_ref(instance);
            dec_ref(list);
            return ptr::null_mut();
        }
        inc_ref(value);
        unsafe { *structseq_slot(instance, index) = value as usize };
    }
    // Hidden fields do not exist for the supported shape; fill the remaining
    // visible slots with None so an unset field can never surface as NULL.
    for index in length..meta.n_fields {
        let none = ptr::addr_of_mut!(super::_Py_NoneStruct);
        inc_ref(none);
        unsafe { *structseq_slot(instance, index) = none as usize };
    }
    dec_ref(list);
    instance
}

/// `tp_repr` for struct-sequence types: `Name(field=value, ...)`.
unsafe extern "C" fn structseq_repr(object: *mut ShimObject) -> *mut ShimObject {
    let type_object = super::type_of(object);
    let Some(meta) = with_structseq_meta(type_object as usize, |meta| meta.clone()) else {
        return super::PyObject_Repr(object);
    };
    let mut output = format!("{}(", meta.name);
    for index in 0..meta.n_sequence {
        if index > 0 {
            output.push_str(", ");
        }
        output.push_str(&meta.fields[index]);
        output.push('=');
        let value = unsafe { *structseq_slot(object, index) as *mut ShimObject };
        if value.is_null() {
            output.push_str("None");
            continue;
        }
        let rendered = super::PyObject_Repr(value);
        if rendered.is_null() {
            set_error(format!(
                "SystemError: repr of field '{}' failed",
                meta.fields[index]
            ));
            return ptr::null_mut();
        }
        if let Some(text) = as_string(rendered) {
            output.push_str(&text);
        }
        dec_ref(rendered);
    }
    output.push(')');
    new_str(output)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // Explicit imports resolve the glob ambiguity between `super::*` (which
    // re-exports the `std::ffi` aliases) and `crate::capi::*` (which uses the
    // `std::os::raw` spellings); both name the same C types.
    use std::os::raw::{c_char, c_int, c_void};

    use super::*;
    use crate::capi::*;

    fn c_string(text: &str) -> std::ffi::CString {
        std::ffi::CString::new(text).unwrap()
    }

    unsafe fn take_message() -> String {
        crate::capi::take_error().unwrap_or_default()
    }

    #[test]
    fn check_positional_accepts_and_rejects_counts() {
        let name = c_string("f");
        unsafe {
            assert_eq!(_PyArg_CheckPositional(name.as_ptr(), 2, 1, 3), 1);
            assert_eq!(_PyArg_CheckPositional(name.as_ptr(), 0, 1, 3), 0);
            let message = take_message();
            assert!(
                message.contains("f expected at least 1 argument, got 0"),
                "{message}"
            );
            assert_eq!(_PyArg_CheckPositional(name.as_ptr(), 4, 1, 3), 0);
            let message = take_message();
            assert!(
                message.contains("f expected at most 3 arguments, got 4"),
                "{message}"
            );
        }
    }

    #[test]
    fn bad_argument_names_types() {
        let fname = c_string("append");
        let display = c_string("argument 1");
        let expected = c_string("Element");
        let none = ptr::addr_of_mut!(_Py_NoneStruct);
        unsafe {
            _PyArg_BadArgument(fname.as_ptr(), display.as_ptr(), expected.as_ptr(), none);
            let message = take_message();
            assert_eq!(
                message,
                "TypeError: append() argument 1 must be Element, not None"
            );
        }
    }

    #[test]
    fn unpack_keywords_positional_and_keyword() {
        let empty = c_string("");
        let x = c_string("x");
        let y = c_string("y");
        // The keyword table is NULL-terminated, exactly like the statics that
        // clinic-generated code embeds.
        let keywords: [*const c_char; 4] = [empty.as_ptr(), x.as_ptr(), y.as_ptr(), ptr::null()];
        let mut parser = PyArgParser {
            format: ptr::null(),
            keywords: keywords.as_ptr(),
            fname: ptr::null(),
            custom_msg: ptr::null(),
            once: 0,
            is_kwtuple_owned: 0,
            pos: 0,
            min: 0,
            max: 0,
            kwtuple: ptr::null_mut(),
            next: ptr::null_mut(),
        };
        // Keep the function name C string alive for the whole test.
        let fname = c_string("f");
        parser.fname = fname.as_ptr();
        let first = new_int(10);
        let second = new_int(20);
        let first_ptr = first;
        let second_ptr = second;
        let args = [first_ptr, second_ptr];
        let mut buf = [ptr::null_mut(); 3];
        unsafe {
            let result = _PyArg_UnpackKeywords(
                args.as_ptr(),
                1,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut parser,
                1,
                2,
                0,
                0,
                buf.as_mut_ptr(),
            );
            // Fast path returns the caller vector for a lone positional arg.
            assert_eq!(result, args.as_ptr());
            assert_eq!(parser.pos, 1);

            let y_obj = new_int(30);
            // `kwnames` carries the keyword *names* (a tuple of strings); the
            // matching values follow the positional vector on the stack.
            let name_y = new_str("y".to_string());
            let mut values = [first_ptr, second_ptr, y_obj];
            let keyword_names = new_tuple(vec![name_y as usize]);
            let result = _PyArg_UnpackKeywords(
                values.as_mut_ptr(),
                2,
                ptr::null_mut(),
                keyword_names,
                &mut parser,
                1,
                2,
                0,
                0,
                buf.as_mut_ptr(),
            );
            assert_eq!(result, buf.as_ptr());
            assert_eq!(buf[0], first_ptr);
            assert_eq!(buf[1], second_ptr);
            assert_eq!(buf[2], y_obj);
            dec_ref(keyword_names);
            take_message();
            // Missing a required positional.
            let only = [first_ptr];
            let result = _PyArg_UnpackKeywords(
                only.as_ptr(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut parser,
                1,
                2,
                0,
                0,
                buf.as_mut_ptr(),
            );
            assert!(result.is_null());
            let message = take_message();
            assert!(
                message.contains("takes at least 1 positional argument"),
                "{message}"
            );
        }
        dec_ref(first);
        dec_ref(second);
    }

    #[test]
    fn unpack_keywords_dict_and_unexpected() {
        let empty = c_string("");
        let first = c_string("first");
        // NULL-terminated, like the static tables clinic code embeds.
        let keywords: [*const c_char; 3] = [empty.as_ptr(), first.as_ptr(), ptr::null()];
        let fname = c_string("f");
        let mut parser = PyArgParser {
            format: ptr::null(),
            keywords: keywords.as_ptr(),
            fname: fname.as_ptr(),
            custom_msg: ptr::null(),
            once: 0,
            is_kwtuple_owned: 0,
            pos: 0,
            min: 0,
            max: 0,
            kwtuple: ptr::null_mut(),
            next: ptr::null_mut(),
        };
        let positional = new_int(1);
        let keyword_value = new_int(2);
        let dict = PyDict_New();
        PyDict_SetItemString(dict, c"first".as_ptr(), keyword_value);
        let args = [positional];
        let mut buf = [ptr::null_mut(); 2];
        unsafe {
            let result = _PyArg_UnpackKeywords(
                args.as_ptr(),
                1,
                dict,
                ptr::null_mut(),
                &mut parser,
                1,
                2,
                0,
                0,
                buf.as_mut_ptr(),
            );
            assert_eq!(result, buf.as_ptr());
            assert_eq!(buf[0], positional);
            assert_eq!(buf[1], keyword_value);

            let bogus = new_int(3);
            let bogus_dict = PyDict_New();
            PyDict_SetItemString(bogus_dict, c"bogus".as_ptr(), bogus);
            let result = _PyArg_UnpackKeywords(
                args.as_ptr(),
                0,
                bogus_dict,
                ptr::null_mut(),
                &mut parser,
                0,
                2,
                0,
                0,
                buf.as_mut_ptr(),
            );
            assert!(result.is_null());
            let message = take_message();
            assert!(
                message.contains("unexpected keyword argument 'bogus'"),
                "{message}"
            );
            dec_ref(bogus_dict);
            dec_ref(bogus);
        }
        dec_ref(dict);
        dec_ref(positional);
        dec_ref(keyword_value);
    }

    #[test]
    fn long_byte_array_round_trip() {
        unsafe {
            let value = new_int(-2);
            let mut bytes = [0u8; 4];
            assert_eq!(
                _PyLong_AsByteArray(value, bytes.as_mut_ptr(), 4, 1, 1, 1),
                0
            );
            assert_eq!(bytes, [0xfe, 0xff, 0xff, 0xff]);
            dec_ref(value);
            let decoded = _PyLong_FromByteArray(bytes.as_ptr(), 4, 1, 1);
            assert_eq!(as_int(decoded), Ok(-2));
            dec_ref(decoded);

            let value = new_int(0x0102);
            let mut bytes = [0u8; 2];
            assert_eq!(
                _PyLong_AsByteArray(value, bytes.as_mut_ptr(), 2, 0, 0, 1),
                0
            );
            assert_eq!(bytes, [0x01, 0x02]);
            dec_ref(value);
            let decoded = _PyLong_FromByteArray(bytes.as_ptr(), 2, 0, 0);
            assert_eq!(as_int(decoded), Ok(0x0102));
            dec_ref(decoded);

            let negative = new_int(-1);
            assert_eq!(
                _PyLong_AsByteArray(negative, bytes.as_mut_ptr(), 2, 1, 0, 1),
                -1
            );
            let message = take_message();
            assert!(
                message.contains("can't convert negative int to unsigned"),
                "{message}"
            );
            dec_ref(negative);

            let too_big = new_int(0x1234);
            let mut one = [0u8; 1];
            assert_eq!(
                _PyLong_AsByteArray(too_big, one.as_mut_ptr(), 1, 1, 0, 1),
                -1
            );
            let message = take_message();
            assert!(message.contains("int too large"), "{message}");
            assert_eq!(one, [0x34]);
            dec_ref(too_big);
        }
    }

    #[test]
    fn long_format_bases() {
        unsafe {
            let value = new_int(-255);
            for (base, expected) in [
                (10, "-255"),
                (16, "-0xff"),
                (8, "-0o377"),
                (2, "-0b11111111"),
            ] {
                let text = _PyLong_Format(value, base);
                assert_eq!(as_string(text).as_deref(), Some(expected));
                dec_ref(text);
            }
            dec_ref(value);
        }
    }

    #[test]
    fn time_conversions_and_rounding() {
        assert_eq!(_PyTime_FromSeconds(3), 3_000_000_000);
        unsafe {
            let mut timestamp = 0i64;
            let value = new_int(7);
            assert_eq!(_PyTime_FromLong(&mut timestamp, value), 0);
            assert_eq!(timestamp, 7);
            dec_ref(value);

            let fraction = new_object(ShimKind::Float(1.5));
            assert_eq!(
                _PyTime_FromSecondsObject(&mut timestamp, fraction, ROUND_CEILING),
                0
            );
            assert_eq!(timestamp, 1_500_000_000);
            dec_ref(fraction);

            let nan = new_object(ShimKind::Float(f64::NAN));
            assert_eq!(
                _PyTime_FromSecondsObject(&mut timestamp, nan, ROUND_FLOOR),
                -1
            );
            let message = take_message();
            assert!(message.contains("Invalid value NaN"), "{message}");
            dec_ref(nan);

            let text = new_str("1".to_string());
            assert_eq!(
                _PyTime_FromSecondsObject(&mut timestamp, text, ROUND_FLOOR),
                -1
            );
            let message = take_message();
            assert!(
                message.contains("cannot be interpreted as an integer or float"),
                "{message}"
            );
            dec_ref(text);
        }
    }

    #[test]
    fn ctype_tables_match_cpython() {
        assert_eq!(_Py_ctype_table[b'a' as usize], PY_CTF_LOWER | PY_CTF_XDIGIT);
        assert_eq!(_Py_ctype_table[b'F' as usize], PY_CTF_UPPER | PY_CTF_XDIGIT);
        assert_eq!(_Py_ctype_table[b'7' as usize], PY_CTF_DIGIT | PY_CTF_XDIGIT);
        assert_eq!(_Py_ctype_table[b' ' as usize], PY_CTF_SPACE);
        assert_eq!(_Py_ctype_table[b'\n' as usize], PY_CTF_SPACE);
        assert_eq!(_Py_ctype_table[b'g' as usize], PY_CTF_LOWER);
        assert_eq!(_Py_ctype_tolower[b'A' as usize], b'a');
        assert_eq!(_Py_ctype_tolower[b'z' as usize], b'z');
        assert_eq!(_Py_ctype_table[0x80], 0);
    }

    #[test]
    fn hash_secret_is_nonzero_and_seeded() {
        initialize_hash_secret();
        let secret = unsafe { (*ptr::addr_of_mut!(_Py_HashSecret)).bytes };
        assert!(secret.iter().any(|&byte| byte != 0));
    }

    #[test]
    fn hashtable_stores_retrieves_and_destroys() {
        static DESTROYED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        unsafe extern "C" fn destroy_value(value: *mut c_void) {
            if !value.is_null() {
                DESTROYED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        unsafe {
            let table = _Py_hashtable_new_full(
                Some(_Py_hashtable_hash_ptr),
                Some(_Py_hashtable_compare_direct),
                None,
                Some(destroy_value),
                ptr::null_mut(),
            );
            assert!(!table.is_null());
            let first = 0x1234usize as *mut c_void;
            let second = 0x5678usize as *mut c_void;
            assert_eq!(
                _Py_hashtable_set(table, first, std::ptr::dangling_mut::<c_void>()),
                0
            );
            assert_eq!(_Py_hashtable_set(table, second, 0x2 as *mut c_void), 0);
            assert_eq!(
                _Py_hashtable_get(table, first),
                std::ptr::dangling_mut::<c_void>()
            );
            assert_eq!(_Py_hashtable_get(table, second), 0x2 as *mut c_void);
            assert!(_Py_hashtable_get(table, 0x99usize as *mut c_void).is_null());
            assert_eq!((*table).nentries, 2);
            _Py_hashtable_clear(table);
            assert_eq!((*table).nentries, 0);
            assert_eq!(DESTROYED.load(std::sync::atomic::Ordering::SeqCst), 2);
            assert_eq!(_Py_hashtable_set(table, first, 0x3 as *mut c_void), 0);
            _Py_hashtable_destroy(table);
            assert_eq!(DESTROYED.load(std::sync::atomic::Ordering::SeqCst), 3);
        }
    }

    #[test]
    fn hash_pointer_rotates_the_address() {
        let pointer = 0x1234_5678usize as *const c_void;
        unsafe {
            assert_eq!(
                _Py_hashtable_hash_ptr(pointer),
                0x1234_5678usize.rotate_right(4)
            );
            assert_eq!(_Py_hashtable_compare_direct(pointer, pointer), 1);
            assert_eq!(
                _Py_hashtable_compare_direct(pointer, 0x9usize as *const c_void),
                0
            );
        }
    }

    #[test]
    fn tuple_and_bytes_resize_move_references() {
        unsafe {
            let mut tuple = PyTuple_New(2);
            let first = new_int(1);
            let second = new_int(2);
            PyTuple_SetItem(tuple, 0, first);
            PyTuple_SetItem(tuple, 1, second);
            let pointer = ptr::addr_of_mut!(tuple);
            assert_eq!(_PyTuple_Resize(pointer, 4), 0);
            tuple = *pointer;
            assert_eq!(PyTuple_Size(tuple), 4);
            assert_eq!(PyTuple_GetItem(tuple, 0), first);
            assert_eq!(PyTuple_GetItem(tuple, 1), second);
            assert_eq!(tuple_slots(tuple).unwrap()[2], 0);
            assert_eq!(_PyTuple_Resize(pointer, 1), 0);
            tuple = *pointer;
            assert_eq!(PyTuple_Size(tuple), 1);
            dec_ref(tuple);

            let mut bytes = PyBytes_FromStringAndSize(c"abc".as_ptr(), 3);
            let pointer = ptr::addr_of_mut!(bytes);
            assert_eq!(_PyBytes_Resize(pointer, 5), 0);
            bytes = *pointer;
            assert_eq!(PyBytes_Size(bytes), 5);
            let data = PyBytes_AsString(bytes);
            assert_eq!(std::slice::from_raw_parts(data as *const u8, 5), b"abc\0\0");
            assert_eq!(_PyBytes_Resize(pointer, 1), 0);
            bytes = *pointer;
            assert_eq!(PyBytes_Size(bytes), 1);
            dec_ref(bytes);
        }
    }

    #[test]
    fn sys_optional_attr_returns_new_reference() {
        unsafe {
            let mut value: *mut ShimObject = ptr::null_mut();
            assert_eq!(
                _PySys_GetOptionalAttrString(c"platform".as_ptr(), &mut value),
                1
            );
            assert!(!value.is_null());
            assert!(matches!(kind_ref(value), Some(ShimKind::Str { .. })));
            dec_ref(value);
            assert_eq!(
                _PySys_GetOptionalAttrString(c"no_such_name".as_ptr(), &mut value),
                0
            );
            assert!(value.is_null());
        }
    }

    #[test]
    fn check_function_result_enforces_contract() {
        unsafe {
            let callable = new_str("f".to_string());
            let result = new_int(1);
            assert_eq!(
                _Py_CheckFunctionResult(ptr::null_mut(), callable, result, ptr::null()),
                result
            );
            dec_ref(result);
            assert!(_Py_CheckFunctionResult(
                ptr::null_mut(),
                callable,
                ptr::null_mut(),
                ptr::null()
            )
            .is_null());
            let message = take_message();
            assert!(
                message.contains("returned NULL without setting an exception"),
                "{message}"
            );
            let result = new_int(2);
            set_error("ValueError: boom");
            assert!(
                _Py_CheckFunctionResult(ptr::null_mut(), callable, result, ptr::null()).is_null()
            );
            let message = take_message();
            assert!(
                message.contains("returned a result with an exception set"),
                "{message}"
            );
            dec_ref(callable);
        }
    }

    #[test]
    fn err_format_handles_strings_and_integers() {
        unsafe {
            crate::capi::initialize_exception_types();
            let class = PyExc_TypeError;
            let result = _PyErr_Format(
                ptr::null_mut(),
                class,
                c"%s must be %d, not %s".as_ptr(),
                c"value".as_ptr(),
                3 as c_int,
                c"str".as_ptr(),
            );
            assert!(result.is_null());
            assert_eq!(take_message(), "TypeError: value must be 3, not str");

            // Seed the raised exception so the context chain is observable.
            PyErr_SetString(PyExc_ValueError, c"inner".as_ptr());
            let result = _PyErr_FormatFromCause(class, c"outer %s".as_ptr(), c"failure".as_ptr());
            assert!(result.is_null());
            let raised = PyErr_GetRaisedException();
            assert!(!raised.is_null());
            let kind = kind_ref(raised).unwrap();
            let ShimKind::Exception { context, .. } = kind else {
                panic!("raised object is not an exception instance");
            };
            assert_ne!(*context, 0, "the inner exception must be the context");
            let text = crate::capi::exception_text(kind);
            assert!(text.contains("outer failure"), "{text}");
            dec_ref(raised);
        }
    }

    #[test]
    fn uid_and_gid_converters() {
        unsafe {
            let value = new_int(1000);
            let mut uid: libc::uid_t = 0;
            let mut gid: libc::gid_t = 0;
            assert_eq!(_Py_Uid_Converter(value, &mut uid), 1);
            assert_eq!(uid, 1000);
            assert_eq!(_Py_Gid_Converter(value, &mut gid), 1);
            assert_eq!(gid, 1000);
            dec_ref(value);

            let negative = new_int(-2);
            assert_eq!(_Py_Uid_Converter(negative, &mut uid), 0);
            let message = take_message();
            assert!(message.contains("uid is less than minimum"), "{message}");
            dec_ref(negative);

            let minus_one = new_int(-1);
            assert_eq!(_Py_Uid_Converter(minus_one, &mut uid), 1);
            assert_eq!(uid, u32::MAX);
            dec_ref(minus_one);

            let text = new_str("x".to_string());
            assert_eq!(_Py_Uid_Converter(text, &mut uid), 0);
            let message = take_message();
            assert!(message.contains("uid should be integer"), "{message}");
            dec_ref(text);
        }
    }

    #[test]
    fn unicode_ascii_comparison_and_utf8_string() {
        unsafe {
            let text = new_str("hello".to_string());
            assert_eq!(_PyUnicode_EqualToASCIIString(text, c"hello".as_ptr()), 1);
            assert_eq!(_PyUnicode_EqualToASCIIString(text, c"hell".as_ptr()), 0);
            let raw = _PyUnicode_AsUTF8String(text);
            assert_eq!(as_string(text).as_deref(), Some("hello"));
            assert_eq!(PyBytes_Size(raw), 5);
            dec_ref(raw);
            dec_ref(text);
        }
    }

    // -- residual `lib-dynload` public API ----------------------------------

    #[test]
    fn compat_arg_parse_single_object_and_tuple() {
        unsafe {
            let text = new_str("hello".to_string());
            let mut out: *const c_char = ptr::null();
            assert_eq!(super::PyArg_Parse(text, c"s".as_ptr(), &mut out), 1);
            assert_eq!(read_c_string(out), "hello");
            dec_ref(text);

            let encoded = new_str("xyz".to_string());
            let mut buffer: *mut c_char = ptr::null_mut();
            assert_eq!(
                super::PyArg_Parse(encoded, c"et:enc".as_ptr(), c"utf-8".as_ptr(), &mut buffer,),
                1
            );
            assert_eq!(read_c_string(buffer), "xyz");
            libc::free(buffer as *mut c_void);
            dec_ref(encoded);

            let tuple = new_tuple(vec![new_int(5) as usize]);
            let mut number: c_int = 0;
            assert_eq!(super::PyArg_Parse(tuple, c"i".as_ptr(), &mut number), 1);
            assert_eq!(number, 5);
            dec_ref(tuple);

            // A missing required unit and an unsupported unit both fail loudly.
            let text = new_str("x".to_string());
            let mut second_number: c_int = 0;
            assert_eq!(
                super::PyArg_Parse(text, c"ii".as_ptr(), &mut number, &mut second_number),
                0
            );
            take_message();
            assert_eq!(super::PyArg_Parse(text, c"z".as_ptr()), 0);
            take_message();
            dec_ref(text);
        }
    }

    #[test]
    fn struct_sequence_type_round_trip() {
        unsafe {
            let filename_name = c_string("filename");
            let lineno_name = c_string("lineno");
            let filename_doc = c_string("source file");
            let lineno_doc = c_string("line number");
            let type_name = c_string("test.FrameInfo");
            let type_doc = c_string("frame information");
            let mut fields = [
                super::StructSequenceField {
                    name: filename_name.as_ptr(),
                    doc: filename_doc.as_ptr(),
                },
                super::StructSequenceField {
                    name: lineno_name.as_ptr(),
                    doc: lineno_doc.as_ptr(),
                },
                super::StructSequenceField {
                    name: ptr::null(),
                    doc: ptr::null(),
                },
            ];
            let mut desc = super::StructSequenceDesc {
                name: type_name.as_ptr(),
                doc: type_doc.as_ptr(),
                fields: fields.as_mut_ptr(),
                n_in_sequence: 2,
            };
            let type_object = super::PyStructSequence_NewType(&mut desc);
            assert!(!type_object.is_null());
            assert!(types::is_subtype(
                type_object,
                ptr::addr_of_mut!(types::TUPLE_TYPE)
            ));

            let instance = super::PyStructSequence_New(type_object);
            assert!(!instance.is_null());
            assert_eq!(crate::capi::PyTuple_Size(instance), 2);
            let file = new_str("a.py".to_string());
            let line = new_int(7);
            super::PyStructSequence_SetItem(instance, 0, file);
            super::PyStructSequence_SetItem(instance, 1, line);
            assert_eq!(super::PyStructSequence_GetItem(instance, 0), file);
            assert_eq!(super::PyStructSequence_GetItem(instance, 1), line);

            let attribute = crate::capi::PyObject_GetAttrString(instance, c"filename".as_ptr());
            assert!(!attribute.is_null());
            assert_eq!(as_string(attribute).as_deref(), Some("a.py"));
            dec_ref(attribute);
            assert!(crate::capi::PyObject_GetAttrString(instance, c"missing".as_ptr()).is_null());
            take_message();

            let rendered = crate::capi::PyObject_Repr(instance);
            let text = as_string(rendered).unwrap_or_default();
            assert!(
                text.contains("test.FrameInfo(filename='a.py', lineno=7)"),
                "{text}"
            );
            dec_ref(rendered);

            // The type is callable through its `tp_new` (as `_lsprof` uses it).
            let inner = new_tuple(vec![
                new_str("b.py".to_string()) as usize,
                new_int(9) as usize,
            ]);
            let arguments = new_tuple(vec![inner as usize]);
            let constructed = crate::capi::PyObject_Call(
                type_object as *mut ShimObject,
                arguments,
                ptr::null_mut(),
            );
            assert!(!constructed.is_null(), "struct sequence constructor failed");
            assert_eq!(crate::capi::PyTuple_Size(constructed), 2);
            assert_eq!(
                as_int(super::PyStructSequence_GetItem(constructed, 1)),
                Ok(9)
            );
            dec_ref(constructed);
            dec_ref(arguments);
            dec_ref(instance);
        }
    }

    #[test]
    fn dict_items_and_pop_string() {
        unsafe {
            let dict = PyDict_New();
            let ten = new_int(10);
            PyDict_SetItemString(dict, c"a".as_ptr(), ten);
            let items = super::PyDict_Items(dict);
            assert!(!items.is_null());
            assert_eq!(PyList_Size(items), 1);
            let pair = PyList_GetItem(items, 0);
            assert_eq!(crate::capi::PyTuple_Size(pair), 2);
            dec_ref(items);

            let mut popped: *mut ShimObject = ptr::null_mut();
            assert_eq!(super::PyDict_PopString(dict, c"a".as_ptr(), &mut popped), 1);
            assert_eq!(as_int(popped), Ok(10));
            dec_ref(popped);
            assert_eq!(super::PyDict_PopString(dict, c"a".as_ptr(), &mut popped), 0);
            assert!(popped.is_null());
            dec_ref(dict);
            dec_ref(ten);
        }
    }

    #[test]
    fn list_clear_and_reverse() {
        unsafe {
            let list = PyList_New(0);
            let first = new_int(1);
            let second = new_int(2);
            PyList_Append(list, first);
            PyList_Append(list, second);
            assert_eq!(super::PyList_Reverse(list), 0);
            assert_eq!(PyList_GetItem(list, 0), second);
            assert_eq!(PyList_GetItem(list, 1), first);
            super::PyList_Clear(list);
            assert_eq!(PyList_Size(list), 0);
            dec_ref(list);
            dec_ref(first);
            dec_ref(second);
        }
    }

    #[test]
    fn repr_guard_is_per_object_and_thread() {
        unsafe {
            let object = new_int(1);
            assert_eq!(super::Py_ReprEnter(object), 0);
            assert_eq!(super::Py_ReprEnter(object), 1);
            super::Py_ReprLeave(object);
            assert_eq!(super::Py_ReprEnter(object), 0);
            super::Py_ReprLeave(object);
            dec_ref(object);
        }
    }

    #[test]
    fn perf_counter_and_hexdigits() {
        unsafe {
            let mut value = 0i64;
            assert_eq!(super::PyTime_PerfCounterRaw(&mut value), 0);
            assert!(value > 0);
            assert_eq!(&super::Py_hexdigits[..10], b"0123456789");
            assert_eq!(&super::Py_hexdigits[10..22], b"abcdefABCDEF");
            assert_eq!(super::Py_hexdigits[22], 0);
        }
    }

    #[test]
    fn filesystem_converter_and_encoder() {
        unsafe {
            let text = new_str("path/x".to_string());
            let encoded = super::PyUnicode_EncodeFSDefault(text);
            assert_eq!(bytes_data(encoded).unwrap(), b"path/x");
            dec_ref(encoded);

            let mut converted: *mut ShimObject = ptr::null_mut();
            let status = super::PyUnicode_FSConverter(
                text,
                &mut converted as *mut *mut ShimObject as *mut c_void,
            );
            assert_ne!(status, 0);
            assert_eq!(bytes_data(converted).unwrap(), b"path/x");
            dec_ref(converted);
            dec_ref(text);

            let embedded = new_str("a\0b".to_string());
            let mut converted: *mut ShimObject = ptr::null_mut();
            let status = super::PyUnicode_FSConverter(
                embedded,
                &mut converted as *mut *mut ShimObject as *mut c_void,
            );
            assert_eq!(status, 0);
            assert!(converted.is_null());
            let message = take_message();
            assert!(message.contains("embedded null byte"), "{message}");
            dec_ref(embedded);
        }
    }

    #[test]
    fn thread_lock_acquire_and_release() {
        use crate::capi::object_api::{
            PyThread_acquire_lock, PyThread_allocate_lock, PyThread_free_lock,
            PyThread_release_lock,
        };
        unsafe {
            let lock = PyThread_allocate_lock();
            assert!(!lock.is_null());
            assert_eq!(PyThread_acquire_lock(lock, 0), 1);
            assert_eq!(PyThread_acquire_lock(lock, 0), 0);
            PyThread_release_lock(lock);
            assert_eq!(PyThread_acquire_lock(lock, 0), 1);
            PyThread_release_lock(lock);
            PyThread_free_lock(lock);
        }
    }

    #[test]
    fn sys_audit_validates_event_and_reports_without_hooks() {
        unsafe {
            assert_eq!(super::PySys_Audit(ptr::null(), ptr::null()), -1);
            take_message();
            assert_eq!(super::PySys_Audit(c"open".as_ptr(), ptr::null()), 0);
        }
    }
}
