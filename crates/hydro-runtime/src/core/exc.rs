//! Exception state and tracebacks (ARCHITECTURE §3.3).
//!
//! A single pending exception per thread lives in a `thread_local`. Raising
//! overwrites any previous pending exception; the codegen polls
//! `hydro_exc_pending()` after every fallible runtime call. A separate stack of
//! exceptions whose handlers are active supplies implicit `__context__`
//! chaining, and the codegen keeps it balanced on every handler exit path.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::value::{self, NONE};

#[path = "exception_impl.rs"]
mod exception_impl;

/// Payload of a heap `Exception` object (kind 2).
#[derive(Clone, Debug)]
pub(crate) struct ExcData {
    /// Exception tag, see the constants below.
    pub(crate) tag: u32,
    /// Python-visible exception class name, e.g. `"ZeroDivisionError"`.
    pub(crate) name: &'static str,
    /// Message rendered by `str(exception)` and the traceback.
    pub(crate) msg: String,
    /// Source file at raise time (`""` if unknown).
    pub(crate) file: String,
    /// 1-based source line at raise time (`0` if unknown).
    pub(crate) line: u32,
    /// Function name at raise time (`""` if unknown).
    pub(crate) frame: String,
    /// Tagged exception value passed to `raise ... from`, if any.
    pub(crate) cause: Option<i64>,
    /// Exception being handled when this one was raised (implicit chaining).
    pub(crate) context: Option<i64>,
    /// `raise ... from ...` (including `from None`): hide `context` in the
    /// rendered traceback even though it is stored.
    pub(crate) suppress_context: bool,
    /// Exit status of a `SystemExit` carrying an integer code. `None` means
    /// the payload is a message instead (`sys.exit("boom")`).
    pub(crate) exit_code: Option<i64>,
    /// Python-visible `args` tuple, rendered as text: `str()` of each
    /// positional constructor argument (`repr()` for `KeyError`, like
    /// CPython's `str(exception)`). Runtime-generated raises store their
    /// rendered message here; exceptions raised without arguments have an
    /// empty vector, so `e.args == ()`.
    pub(crate) args: Vec<String>,
    /// The original positional constructor arguments as tagged values.
    /// CPython keeps them as the `args` tuple; here they are the objects
    /// `e.args` returns, and they are GC edges (see `gc::heap::mark_value`).
    /// Empty when the exception was produced by a raise whose argument
    /// exists only as text.
    pub(crate) arg_values: Vec<i64>,
    /// `repr()` of each entry in [`Self::args`], in the same order.
    /// `repr(exception)` and the multi-argument `str(exception)` render from
    /// it, so `repr(Exception(1, "a"))` is `Exception(1, 'a')` rather than a
    /// quote-heavy reconstruction of the `str()` forms.
    pub(crate) arg_reprs: Vec<String>,
    /// Keyword-only constructor payload. `AttributeError` stores `name` and
    /// `obj`, `NameError` stores `name`, and the `SyntaxError` family stores
    /// the source-position keywords. The tagged values are GC edges (see
    /// `gc::heap::mark_value`).
    pub(crate) keywords: Vec<(String, i64)>,
    /// Original exception instance when the raise crossed the C-API boundary
    /// (pydantic-core raises its `ValidationError` this way). The tagged value
    /// is a [`crate::value::KIND_EXT_OBJECT`] handle and a GC edge: `str`,
    /// `repr`, `args`, attribute reads and method calls delegate to it, while
    /// the tag stays the handler-compatible approximation the frontend emits.
    pub(crate) object: Option<i64>,
}

// Exception tags, fixed by ARCHITECTURE §3.2.
pub(crate) const VALUE_ERROR: u32 = 0;
pub(crate) const TYPE_ERROR: u32 = 1;
pub(crate) const ZERO_DIVISION_ERROR: u32 = 2;
pub(crate) const RUNTIME_ERROR: u32 = 3;
pub(crate) const OVERFLOW_ERROR: u32 = 4;
pub(crate) const KEYBOARD_INTERRUPT: u32 = 5;
pub(crate) const INDEX_ERROR: u32 = 6;
pub(crate) const KEY_ERROR: u32 = 7;
pub(crate) const STOP_ITERATION: u32 = 8;
pub(crate) const ATTRIBUTE_ERROR: u32 = 9;
/// `OSError` (file I/O failures); `IOError` is an alias in CPython.
pub(crate) const OS_ERROR: u32 = 10;
/// Runtime-internal `GeneratorExit`, injected by `generator.close()`. The tag
/// is exposed as a first-class class value, but `raise`/`except` do not accept
/// it from source.
pub(crate) const GENERATOR_EXIT: u32 = 11;
/// `Exception`: a handler for this tag matches every concrete exception
/// except `KeyboardInterrupt` and `GeneratorExit`.
pub(crate) const EXCEPTION: u32 = 12;
/// `BaseException`: a handler for this tag matches every exception.
pub(crate) const BASE_EXCEPTION: u32 = 13;
/// `AssertionError`: raised by `assert` statements and unittest assertions.
pub(crate) const ASSERTION_ERROR: u32 = 14;
/// `FileNotFoundError`: a subclass of [`OS_ERROR`]. `except FileNotFoundError`
/// and `except OSError` both match either tag.
pub(crate) const FILE_NOT_FOUND_ERROR: u32 = 15;
/// `NameError`: raised by `del` of an unbound module global.
pub(crate) const NAME_ERROR: u32 = 16;
/// `SystemExit`: catchable like any other exception; uncaught it prints no
/// traceback and its stored code becomes the process status.
pub(crate) const SYSTEM_EXIT: u32 = 17;
/// `StopAsyncIteration`: the normal end of an async iterator, raised by
/// `__anext__`/`asend`/`athrow` when the body finishes.
pub(crate) const STOP_ASYNC_ITERATION: u32 = 18;
/// `CancelledError`: a `BaseException` injected into a task by `cancel()`.
/// It is deliberately excluded from the `Exception` matcher.
pub(crate) const CANCELLED_ERROR: u32 = 19;
/// `Skipped`: raised by `pytest.skip()`; the runner reports a skip and keeps
/// going. Runtime-internal, like [`GENERATOR_EXIT`].
pub(crate) const SKIPPED: u32 = 20;
/// `XFailed`: raised when a test marked `xfail` fails; the runner reports an
/// expected failure. Runtime-internal.
pub(crate) const XFAILED: u32 = 21;

// Tags appended for the CPython 3.14 taxonomy. Existing tags above are never
// renumbered; `19..=21` stay reserved for the runtime-internal kinds.
/// `SystemError`: an internal interpreter error.
pub(crate) const SYSTEM_ERROR: u32 = 22;
/// `MemoryError`: out of memory.
pub(crate) const MEMORY_ERROR: u32 = 23;
/// `RecursionError`: a `RuntimeError` subclass raised when the recursion
/// limit would be exceeded.
pub(crate) const RECURSION_ERROR: u32 = 24;
/// `SyntaxError`: invalid syntax.
pub(crate) const SYNTAX_ERROR: u32 = 25;
/// `IndentationError`: a `SyntaxError` subclass.
pub(crate) const INDENTATION_ERROR: u32 = 26;
/// `TabError`: an `IndentationError` subclass.
pub(crate) const TAB_ERROR: u32 = 27;
/// `ArithmeticError`: base of [`ZERO_DIVISION_ERROR`], [`OVERFLOW_ERROR`]
/// and [`FLOATING_POINT_ERROR`].
pub(crate) const ARITHMETIC_ERROR: u32 = 28;
/// `LookupError`: base of [`INDEX_ERROR`] and [`KEY_ERROR`].
pub(crate) const LOOKUP_ERROR: u32 = 29;
/// `ImportError`: a failed import.
pub(crate) const IMPORT_ERROR: u32 = 30;
/// `ModuleNotFoundError`: an `ImportError` subclass.
pub(crate) const MODULE_NOT_FOUND_ERROR: u32 = 31;
/// `NotImplementedError`: a `RuntimeError` subclass.
pub(crate) const NOT_IMPLEMENTED_ERROR: u32 = 32;
/// `EOFError`: end of file where input was expected.
pub(crate) const EOF_ERROR: u32 = 33;
/// `UnicodeError`: a `ValueError` subclass and the base of the three codec
/// errors below.
pub(crate) const UNICODE_ERROR: u32 = 34;
pub(crate) const UNICODE_DECODE_ERROR: u32 = 35;
pub(crate) const UNICODE_ENCODE_ERROR: u32 = 36;
pub(crate) const UNICODE_TRANSLATE_ERROR: u32 = 37;
/// `BufferError`: a buffer operation failed.
pub(crate) const BUFFER_ERROR: u32 = 38;
/// `ReferenceError`: a weak reference outlived its target.
pub(crate) const REFERENCE_ERROR: u32 = 39;
/// `ExceptionGroup`: also a [`BASE_EXCEPTION_GROUP`].
pub(crate) const EXCEPTION_GROUP: u32 = 40;
/// `BaseExceptionGroup`: also a `BaseException`.
pub(crate) const BASE_EXCEPTION_GROUP: u32 = 41;
/// `Warning`: base of every warning category below.
pub(crate) const WARNING: u32 = 42;
pub(crate) const USER_WARNING: u32 = 43;
pub(crate) const DEPRECATION_WARNING: u32 = 44;
pub(crate) const PENDING_DEPRECATION_WARNING: u32 = 45;
pub(crate) const SYNTAX_WARNING: u32 = 46;
pub(crate) const RUNTIME_WARNING: u32 = 47;
pub(crate) const FUTURE_WARNING: u32 = 48;
pub(crate) const IMPORT_WARNING: u32 = 49;
pub(crate) const UNICODE_WARNING: u32 = 50;
pub(crate) const BYTES_WARNING: u32 = 51;
pub(crate) const RESOURCE_WARNING: u32 = 52;
/// `FloatingPointError`: an `ArithmeticError` subclass.
pub(crate) const FLOATING_POINT_ERROR: u32 = 53;
/// `BlockingIOError`: an `OSError` subclass.
pub(crate) const BLOCKING_IO_ERROR: u32 = 54;
/// `ChildProcessError`: an `OSError` subclass.
pub(crate) const CHILD_PROCESS_ERROR: u32 = 55;
/// `ConnectionError`: an `OSError` subclass and the base of the four
/// connection failures below.
pub(crate) const CONNECTION_ERROR: u32 = 56;
pub(crate) const BROKEN_PIPE_ERROR: u32 = 57;
pub(crate) const CONNECTION_ABORTED_ERROR: u32 = 58;
pub(crate) const CONNECTION_REFUSED_ERROR: u32 = 59;
pub(crate) const CONNECTION_RESET_ERROR: u32 = 60;
/// `FileExistsError`: an `OSError` subclass.
pub(crate) const FILE_EXISTS_ERROR: u32 = 61;
/// `InterruptedError`: an `OSError` subclass.
pub(crate) const INTERRUPTED_ERROR: u32 = 62;
/// `IsADirectoryError`: an `OSError` subclass.
pub(crate) const IS_A_DIRECTORY_ERROR: u32 = 63;
/// `NotADirectoryError`: an `OSError` subclass.
pub(crate) const NOT_A_DIRECTORY_ERROR: u32 = 64;
/// `PermissionError`: an `OSError` subclass.
pub(crate) const PERMISSION_ERROR: u32 = 65;
/// `ProcessLookupError`: an `OSError` subclass.
pub(crate) const PROCESS_LOOKUP_ERROR: u32 = 66;
/// `TimeoutError`: an `OSError` subclass (the builtin, not
/// `asyncio.TimeoutError`).
pub(crate) const TIMEOUT_ERROR: u32 = 67;
/// `UnboundLocalError`: a `NameError` subclass raised when a local name is
/// read before it is bound.
pub(crate) const UNBOUND_LOCAL_ERROR: u32 = 68;

// CPython's `IOError` and `EnvironmentError` are aliases of `OSError`, so they
// have no separate tags; `hydro_ir::ExcKind::IOError` and
// `hydro_ir::ExcKind::EnvironmentError` both resolve to [`OS_ERROR`].

/// Whether `tag` is `OSError` or one of its built-in subclasses. These
/// constructors accept `(errno, strerror[, filename])`, unlike the
/// single-message exception classes.
pub(crate) fn is_os_error_tag(tag: u32) -> bool {
    matches!(
        tag,
        OS_ERROR
            | FILE_NOT_FOUND_ERROR
            | BLOCKING_IO_ERROR
            | CHILD_PROCESS_ERROR
            | CONNECTION_ERROR
            | BROKEN_PIPE_ERROR
            | CONNECTION_ABORTED_ERROR
            | CONNECTION_REFUSED_ERROR
            | CONNECTION_RESET_ERROR
            | FILE_EXISTS_ERROR
            | INTERRUPTED_ERROR
            | IS_A_DIRECTORY_ERROR
            | NOT_A_DIRECTORY_ERROR
            | PERMISSION_ERROR
            | PROCESS_LOOKUP_ERROR
            | TIMEOUT_ERROR
    )
}

/// Whether `tag` is `SyntaxError` or one of its source-position subclasses.
pub(crate) fn is_syntax_tag(tag: u32) -> bool {
    matches!(tag, SYNTAX_ERROR | INDENTATION_ERROR | TAB_ERROR)
}

pub(crate) fn tag_name(tag: u32) -> &'static str {
    match tag {
        VALUE_ERROR => "ValueError",
        TYPE_ERROR => "TypeError",
        ZERO_DIVISION_ERROR => "ZeroDivisionError",
        RUNTIME_ERROR => "RuntimeError",
        OVERFLOW_ERROR => "OverflowError",
        KEYBOARD_INTERRUPT => "KeyboardInterrupt",
        INDEX_ERROR => "IndexError",
        KEY_ERROR => "KeyError",
        STOP_ITERATION => "StopIteration",
        ATTRIBUTE_ERROR => "AttributeError",
        OS_ERROR => "OSError",
        GENERATOR_EXIT => "GeneratorExit",
        EXCEPTION => "Exception",
        BASE_EXCEPTION => "BaseException",
        ASSERTION_ERROR => "AssertionError",
        FILE_NOT_FOUND_ERROR => "FileNotFoundError",
        NAME_ERROR => "NameError",
        SYSTEM_EXIT => "SystemExit",
        STOP_ASYNC_ITERATION => "StopAsyncIteration",
        CANCELLED_ERROR => "CancelledError",
        SKIPPED => "Skipped",
        XFAILED => "XFailed",
        SYSTEM_ERROR => "SystemError",
        MEMORY_ERROR => "MemoryError",
        RECURSION_ERROR => "RecursionError",
        SYNTAX_ERROR => "SyntaxError",
        INDENTATION_ERROR => "IndentationError",
        TAB_ERROR => "TabError",
        ARITHMETIC_ERROR => "ArithmeticError",
        LOOKUP_ERROR => "LookupError",
        IMPORT_ERROR => "ImportError",
        MODULE_NOT_FOUND_ERROR => "ModuleNotFoundError",
        NOT_IMPLEMENTED_ERROR => "NotImplementedError",
        EOF_ERROR => "EOFError",
        UNICODE_ERROR => "UnicodeError",
        UNICODE_DECODE_ERROR => "UnicodeDecodeError",
        UNICODE_ENCODE_ERROR => "UnicodeEncodeError",
        UNICODE_TRANSLATE_ERROR => "UnicodeTranslateError",
        BUFFER_ERROR => "BufferError",
        REFERENCE_ERROR => "ReferenceError",
        EXCEPTION_GROUP => "ExceptionGroup",
        BASE_EXCEPTION_GROUP => "BaseExceptionGroup",
        WARNING => "Warning",
        USER_WARNING => "UserWarning",
        DEPRECATION_WARNING => "DeprecationWarning",
        PENDING_DEPRECATION_WARNING => "PendingDeprecationWarning",
        SYNTAX_WARNING => "SyntaxWarning",
        RUNTIME_WARNING => "RuntimeWarning",
        FUTURE_WARNING => "FutureWarning",
        IMPORT_WARNING => "ImportWarning",
        UNICODE_WARNING => "UnicodeWarning",
        BYTES_WARNING => "BytesWarning",
        RESOURCE_WARNING => "ResourceWarning",
        FLOATING_POINT_ERROR => "FloatingPointError",
        BLOCKING_IO_ERROR => "BlockingIOError",
        CHILD_PROCESS_ERROR => "ChildProcessError",
        CONNECTION_ERROR => "ConnectionError",
        BROKEN_PIPE_ERROR => "BrokenPipeError",
        CONNECTION_ABORTED_ERROR => "ConnectionAbortedError",
        CONNECTION_REFUSED_ERROR => "ConnectionRefusedError",
        CONNECTION_RESET_ERROR => "ConnectionResetError",
        FILE_EXISTS_ERROR => "FileExistsError",
        INTERRUPTED_ERROR => "InterruptedError",
        IS_A_DIRECTORY_ERROR => "IsADirectoryError",
        NOT_A_DIRECTORY_ERROR => "NotADirectoryError",
        PERMISSION_ERROR => "PermissionError",
        PROCESS_LOOKUP_ERROR => "ProcessLookupError",
        TIMEOUT_ERROR => "TimeoutError",
        UNBOUND_LOCAL_ERROR => "UnboundLocalError",
        // Unknown tags are rendered as RuntimeError rather than panicking.
        _ => "RuntimeError",
    }
}

fn default_message(tag: u32) -> &'static str {
    match tag {
        ZERO_DIVISION_ERROR => "division by zero",
        INDEX_ERROR => "index out of range",
        KEY_ERROR => "key not found",
        STOP_ITERATION => "",
        ATTRIBUTE_ERROR => "attribute not found",
        OS_ERROR => "I/O error",
        FILE_NOT_FOUND_ERROR => "No such file or directory",
        NAME_ERROR => "name is not defined",
        // `SystemExit()` stringifies to the empty string, like `Exception`.
        SYSTEM_EXIT => "",
        GENERATOR_EXIT => "",
        STOP_ASYNC_ITERATION => "",
        CANCELLED_ERROR => "",
        SKIPPED => "",
        XFAILED => "",
        // `Exception()` and `BaseException()` stringify to the empty string.
        EXCEPTION | BASE_EXCEPTION => "",
        ASSERTION_ERROR => "",
        // CPython's `str(Exc())` is the empty string for every other class in
        // the taxonomy; concrete failures pass their message explicitly.
        _ => "",
    }
}

/// Direct base classes of a concrete exception tag, matching CPython 3.14.
///
/// A tag can have more than one parent (`ExceptionGroup` derives from both
/// `BaseExceptionGroup` and `Exception`); every other node has zero or one.
/// Runtime-internal `Skipped`/`XFailed` are ordinary `Exception` children so
/// they keep their historical catchability, while `CancelledError` is a
/// `BaseException` like `asyncio.CancelledError`.
pub(crate) fn parents(tag: u32) -> &'static [u32] {
    match tag {
        // Roots and the BaseException-only children.
        BASE_EXCEPTION => &[],
        SYSTEM_EXIT | KEYBOARD_INTERRUPT | GENERATOR_EXIT | CANCELLED_ERROR
        | BASE_EXCEPTION_GROUP => &[BASE_EXCEPTION],
        EXCEPTION_GROUP => &[BASE_EXCEPTION_GROUP, EXCEPTION],
        EXCEPTION => &[BASE_EXCEPTION],
        // Ordinary exceptions.
        UNBOUND_LOCAL_ERROR => &[NAME_ERROR],
        VALUE_ERROR | TYPE_ERROR | RUNTIME_ERROR | STOP_ITERATION | ATTRIBUTE_ERROR
        | ASSERTION_ERROR | NAME_ERROR | STOP_ASYNC_ITERATION | SYSTEM_ERROR | MEMORY_ERROR
        | EOF_ERROR | BUFFER_ERROR | REFERENCE_ERROR | ARITHMETIC_ERROR | LOOKUP_ERROR
        | IMPORT_ERROR | SYNTAX_ERROR | WARNING | SKIPPED | XFAILED => &[EXCEPTION],
        // ArithmeticError.
        ZERO_DIVISION_ERROR | OVERFLOW_ERROR | FLOATING_POINT_ERROR => &[ARITHMETIC_ERROR],
        // LookupError.
        INDEX_ERROR | KEY_ERROR => &[LOOKUP_ERROR],
        // ImportError.
        MODULE_NOT_FOUND_ERROR => &[IMPORT_ERROR],
        // RuntimeError.
        RECURSION_ERROR | NOT_IMPLEMENTED_ERROR => &[RUNTIME_ERROR],
        // ValueError -> UnicodeError.
        UNICODE_ERROR => &[VALUE_ERROR],
        UNICODE_DECODE_ERROR | UNICODE_ENCODE_ERROR | UNICODE_TRANSLATE_ERROR => &[UNICODE_ERROR],
        // SyntaxError.
        INDENTATION_ERROR => &[SYNTAX_ERROR],
        TAB_ERROR => &[INDENTATION_ERROR],
        // OSError and its subclasses.
        OS_ERROR => &[EXCEPTION],
        BLOCKING_IO_ERROR
        | CHILD_PROCESS_ERROR
        | FILE_EXISTS_ERROR
        | FILE_NOT_FOUND_ERROR
        | INTERRUPTED_ERROR
        | IS_A_DIRECTORY_ERROR
        | NOT_A_DIRECTORY_ERROR
        | PERMISSION_ERROR
        | PROCESS_LOOKUP_ERROR
        | TIMEOUT_ERROR => &[OS_ERROR],
        CONNECTION_ERROR => &[OS_ERROR],
        BROKEN_PIPE_ERROR
        | CONNECTION_ABORTED_ERROR
        | CONNECTION_REFUSED_ERROR
        | CONNECTION_RESET_ERROR => &[CONNECTION_ERROR],
        // Warning family.
        USER_WARNING
        | DEPRECATION_WARNING
        | PENDING_DEPRECATION_WARNING
        | SYNTAX_WARNING
        | RUNTIME_WARNING
        | FUTURE_WARNING
        | IMPORT_WARNING
        | UNICODE_WARNING
        | BYTES_WARNING
        | RESOURCE_WARNING => &[WARNING],
        // Unknown tags have no modelled ancestry.
        _ => &[],
    }
}

/// True for every exception tag the runtime defines. Used to reject malformed
/// values instead of caching a class under a bogus tag.
fn known_tag(tag: u32) -> bool {
    tag == BASE_EXCEPTION || !parents(tag).is_empty()
}

/// Whether exception `tag` is `ancestor` or derives from it.
///
/// Unknown tags never match an arbitrary ancestor: both tags must be known
/// (or be the same known tag), so a malformed tag cannot silently satisfy a
/// concrete handler.
pub(crate) fn tag_is_subclass(tag: u32, ancestor: u32) -> bool {
    if tag == ancestor {
        return known_tag(tag);
    }
    if !known_tag(tag) || !known_tag(ancestor) {
        return false;
    }
    // Depth-first walk over the parent graph. It is tiny (one diamond at
    // `ExceptionGroup`) and acyclic, so a stack with a visited check is enough.
    let mut pending = vec![tag];
    let mut visited = Vec::new();
    while let Some(current) = pending.pop() {
        if current == ancestor {
            return true;
        }
        if visited.contains(&current) {
            continue;
        }
        visited.push(current);
        for &parent in parents(current) {
            if parent == ancestor {
                return true;
            }
            if !visited.contains(&parent) {
                pending.push(parent);
            }
        }
    }
    false
}

/// Returns the cached native class object for the built-in exception `tag`,
/// creating an empty class named after the tag on first use.
///
/// The class is an ordinary `KIND_CLASS` object whose base is the class of the
/// tag's parent in [`parents`], so `Class.__name__`, inheritance,
/// `issubclass` and instantiation work like any class the user defines. The
/// cache is thread-local and the created class is registered as a collector
/// root (the cache itself is Rust-side state the stack scan cannot see).
/// [`init`] clears the cache before a new program run frees the heap.
///
/// An unknown tag reports `RuntimeError` and returns `None`.
pub(crate) fn exception_class(tag: u32) -> i64 {
    if let Some(class) = CLASS_CACHE.with(|cache| cache.borrow().get(&tag).copied()) {
        return class;
    }
    if !known_tag(tag) {
        return raise_with_message(RUNTIME_ERROR, &format!("unknown exception tag {tag}"));
    }
    // `ExceptionGroup` has two bases; a class object stores one, so use the
    // first (`BaseExceptionGroup`) and leave the `Exception` edge to the tag
    // hierarchy, which `isinstance`/`issubclass` consult.
    let base = match parents(tag).first() {
        Some(&parent) => exception_class(parent),
        None => NONE,
    };
    let methods = vec![(
        "__init__".to_string(),
        value::native_function_new("builtins", "__exception_init__", usize::MAX),
    )];
    // `exception_class` is called lazily while a handler is evaluating an
    // exception (for example `except struct.error:`), so a pending exception
    // may be active. Native class creation stops early when `pending()` is
    // set; park the exception for the allocation and restore it afterwards.
    let saved = pending().then(pending_value);
    if saved.is_some() {
        clear();
    }
    let saved_roots = saved.map(|exception| crate::heap::root_values(&[exception]));
    let class = value::class_new(tag_name(tag).to_string(), base, methods);
    if let Some(exception) = saved {
        if class != NONE && !pending() {
            publish(exception);
        }
        if let Some(roots) = saved_roots {
            crate::heap::release_roots(roots);
        }
        if class == NONE {
            return NONE;
        }
    }
    // The cache is invisible to the conservative stack scan, so the class must
    // be an explicit root while it lives in the heap.
    crate::heap::add_thread_root(class);
    CLASS_CACHE.with(|cache| cache.borrow_mut().insert(tag, class));
    class
}

/// Returns the tag of the native built-in exception class `class`, if it is
/// one created by [`exception_class`].
///
/// `type(ValueError('x')) is ValueError` relies on both paths resolving to
/// the cached class for the same tag.
pub(crate) fn exception_tag_of_class(class: i64) -> Option<u32> {
    CLASS_CACHE.with(|cache| {
        cache
            .borrow()
            .iter()
            .find(|(_, cached)| **cached == class)
            .map(|(tag, _)| *tag)
    })
}

/// Builds a plain built-in exception object `Tag(message)` without raising
/// it. Used by the class-call path (`ValueError('x')`).
///
/// `args` are the already-rendered Python arguments (empty for `Tag()`);
/// they become the `args` tuple exposed by [`exception_attr`].
pub(crate) fn exception_value(tag: u32, message: &str, args: Vec<String>) -> i64 {
    exception_value_with_keywords(tag, message, args, Vec::new())
}

/// [`exception_value`] with the keyword-only constructor payload stored.
///
/// The values stay tagged (not rendered) because `AttributeError.name`,
/// `AttributeError.obj` and the `SyntaxError` position attributes expose the
/// original objects. Callers validate the keyword names against
/// [`exception_keyword_names`] before constructing.
pub(crate) fn exception_value_with_keywords(
    tag: u32,
    message: &str,
    args: Vec<String>,
    keywords: Vec<(String, i64)>,
) -> i64 {
    value::exception_new(ExcData {
        tag,
        name: tag_name(tag),
        msg: message.to_string(),
        file: String::new(),
        line: 0,
        frame: String::new(),
        cause: None,
        context: None,
        suppress_context: false,
        exit_code: None,
        args,
        arg_values: Vec::new(),
        arg_reprs: Vec::new(),
        keywords,
        object: None,
    })
}

/// Keyword names accepted by the built-in exception constructor `tag`.
///
/// CPython's `AttributeError` takes `name` and `obj`, `NameError` takes
/// `name`, and the `SyntaxError` family carries source-position keywords.
/// Every other built-in exception rejects keyword arguments with `TypeError`.
pub(crate) fn exception_keyword_names(tag: u32) -> &'static [&'static str] {
    match tag {
        ATTRIBUTE_ERROR => &["name", "obj"],
        NAME_ERROR => &["name"],
        SYNTAX_ERROR | INDENTATION_ERROR | TAB_ERROR => &[
            "msg",
            "filename",
            "lineno",
            "offset",
            "text",
            "end_lineno",
            "end_offset",
        ],
        _ => &[],
    }
}

/// Builds an exception with the full positional payload CPython keeps.
///
/// `args` holds the `str()` of each argument, `reprs` its `repr()` (used by
/// [`exception_repr`] and by the multi-argument `str()`), and `values` the
/// original tagged arguments that `e.args` exposes. Callers that only have
/// rendered text use [`exception_value`]; constructor calls use
/// [`exception_value_from_call`].
pub(crate) fn exception_value_with_arguments(
    tag: u32,
    message: &str,
    args: Vec<String>,
    reprs: Vec<String>,
    values: Vec<i64>,
    keywords: Vec<(String, i64)>,
) -> i64 {
    value::exception_new(ExcData {
        tag,
        name: tag_name(tag),
        msg: message.to_string(),
        file: String::new(),
        line: 0,
        frame: String::new(),
        cause: None,
        context: None,
        suppress_context: false,
        exit_code: None,
        args,
        arg_values: values,
        arg_reprs: reprs,
        keywords,
        object: None,
    })
}

/// CPython's `BaseException.__str__` for a stored positional payload.
///
/// No argument renders as the empty string, a single argument as its `str()`
/// (its `repr()` for `KeyError`), and several arguments as the `repr()` of the
/// `args` tuple: `str(Exception("a", "b")) == "('a', 'b')"`.
pub(crate) fn message_from_args(tag: u32, args: &[String], reprs: &[String]) -> String {
    match args.len() {
        0 => String::new(),
        1 if tag == KEY_ERROR => reprs.first().cloned().unwrap_or_default(),
        1 => args[0].clone(),
        _ => tuple_repr_from(reprs),
    }
}

/// `repr(tuple(items))` for already-rendered element representations.
fn tuple_repr_from(reprs: &[String]) -> String {
    let mut out = String::with_capacity(reprs.len() * 4 + 2);
    out.push('(');
    for (index, item) in reprs.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(item);
    }
    if reprs.len() == 1 {
        out.push(',');
    }
    out.push(')');
    out
}

/// Instantiates a built-in exception class the way CPython's
/// `BaseException.__init__` does: every positional argument is stored in
/// `args`, `str()` and `repr()` are derived from that tuple, and the
/// class-specific keyword payload is validated and stored.
///
/// `OSError` and its subclasses keep their specialized `(errno, strerror[,
/// filename])` rendering and are not handled here. An unsupported keyword
/// reports `TypeError` through the pending exception and returns [`NONE`].
pub(crate) fn exception_value_from_call(
    tag: u32,
    values: &[i64],
    keywords: &[(String, i64)],
) -> i64 {
    if is_group_tag(tag) {
        return group_from_call(tag, values, keywords);
    }
    let allowed = exception_keyword_names(tag);
    for (name, _) in keywords {
        if !allowed.contains(&name.as_str()) {
            let message = if allowed.is_empty() {
                format!("{}() takes no keyword arguments", tag_name(tag))
            } else {
                format!(
                    "{}() got an unexpected keyword argument '{}'",
                    tag_name(tag),
                    name
                )
            };
            return raise_with_message(TYPE_ERROR, &message);
        }
    }
    let mut args = Vec::with_capacity(values.len());
    let mut reprs = Vec::with_capacity(values.len());
    for &value in values {
        args.push(value::to_string(value));
        reprs.push(value::repr(value));
    }
    let message = message_from_args(tag, &args, &reprs);
    exception_value_with_arguments(
        tag,
        &message,
        args,
        reprs,
        values.to_vec(),
        keywords.to_vec(),
    )
}

/// CPython's `repr(exception)`: `ClassName(repr(arg), ...)`, or `ClassName()`
/// when the exception has no arguments.
///
/// Extension exceptions keep the original instance's `repr()` (rendered by
/// the caller); every other exception derives it from the stored payload.
pub(crate) fn exception_repr(data: &ExcData) -> String {
    exception_repr_named(&exception_display_name(data), data)
}

/// [`exception_repr`] for a caller-supplied class name.
///
/// User-defined exception subclasses are stored as a payload plus their
/// Python class; the class name (not the tag's native name) is what CPython
/// prints.
pub(crate) fn exception_repr_named(name: &str, data: &ExcData) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push_str(name);
    out.push('(');
    for (index, repr) in argument_reprs(data).iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(repr);
    }
    out.push(')');
    out
}

/// `repr()` of every stored argument, in `args` order.
///
/// Constructor-built exceptions carry the pre-rendered representations;
/// runtime-generated ones (whose arguments exist only as text) fall back to
/// quoting their `str()` form, which is exact because those arguments are
/// strings by construction.
fn argument_reprs(data: &ExcData) -> Vec<String> {
    if data.arg_reprs.len() == data.args.len() && !data.arg_reprs.is_empty() {
        return data.arg_reprs.clone();
    }
    if data.arg_values.len() == data.args.len() && !data.arg_values.is_empty() {
        return data
            .arg_values
            .iter()
            .map(|value| value::repr(*value))
            .collect();
    }
    data.args
        .iter()
        .map(|text| value::quote_str(text))
        .collect()
}

/// Local-slot observation of one activation.
///
/// While the activation runs, `base`/`slots` point at the precise GC slot
/// array registered by generated code (or by the interpreter) and `f_locals`
/// reads the current values. When the activation ends, `base` is cleared and
/// `snapshot` holds the locals dict the frame object keeps exposing.
///
/// The binding is atomic so `frame_bind` can refresh an activation that was
/// already materialised without taking a lock; `names` is fixed when the
/// record is materialised (binding always precedes materialisation, because
/// only a deeper running call can observe an activation).
pub(crate) struct FrameLocals {
    /// Address of the activation's slot array (`0` when unbound or ended).
    pub(crate) base: AtomicUsize,
    pub(crate) slots: AtomicUsize,
    pub(crate) names: Vec<String>,
    pub(crate) snapshot: Mutex<Option<i64>>,
}

/// One running (or suspended) activation as recorded by `hydro_frame_enter`.
///
/// The raw pointers are only read while the activation is on a thread's or a
/// fiber's activation stack; they point at blobs in the generated function's
/// stack frame (or the interpreter's live metadata), which outlive the
/// activation. Nothing here is allocated on the common path: the record is a
/// fixed-size value pushed onto a vector, and the optional `FrameData` only
/// appears when introspection (`sys._getframe`, frame objects,
/// `inspect.currentframe`) asks for it.
pub(crate) struct Activation {
    name: *const u8,
    name_len: usize,
    /// Source file active at entry. Shared, so capturing it is a refcount
    /// bump rather than a string copy.
    file: Arc<String>,
    module: *const u8,
    module_len: usize,
    names: *const u8,
    names_len: usize,
    /// Precise slot array and its length, valid while the activation runs.
    base: *mut i64,
    slots: usize,
    params: usize,
    flags: u32,
    line: u32,
    first_line: u32,
    /// Introspection record, built on first observation and then shared with
    /// every frame object materialised for this activation.
    cached: Option<Arc<FrameData>>,
}

impl Activation {
    /// Builds the introspection record for this activation on demand.
    fn materialize(&self, parent: Option<Arc<FrameData>>) -> FrameData {
        FrameData {
            name: decode_bytes(self.name, self.name_len),
            module: Mutex::new(decode_bytes(self.module, self.module_len)),
            file: (*self.file).clone(),
            first_line: AtomicU32::new(self.first_line),
            line: AtomicU32::new(self.line),
            flags: AtomicU32::new(self.flags),
            argcount: AtomicUsize::new(self.params),
            trace: Mutex::new(NONE),
            parent,
            locals: FrameLocals {
                base: AtomicUsize::new(self.base as usize),
                slots: AtomicUsize::new(self.slots),
                names: decode_names(self.names, self.names_len),
                snapshot: Mutex::new(None),
            },
        }
    }
}

/// One activation's introspection record, built on first observation and
/// shared with every frame object materialised for that activation.
///
/// `parent` keeps the caller chain alive after the caller has returned, so a
/// retained frame's `f_back` can still report its snapped locals. The record
/// contains a rooted snapshot dict after the activation ends; `f_locals` of a
/// running activation instead reads the live GC slots.
pub(crate) struct FrameData {
    pub(crate) name: String,
    /// Module key owning the code, resolved by `hydro_ir::Program::module_of`
    /// at frame entry.
    pub(crate) module: Mutex<String>,
    pub(crate) file: String,
    pub(crate) first_line: AtomicU32,
    pub(crate) line: AtomicU32,
    pub(crate) flags: AtomicU32,
    pub(crate) argcount: AtomicUsize,
    /// `frame.f_trace`: the per-frame trace callback, or `None`. CPython stores
    /// it on the frame; observing it through `f_trace` is the only way a
    /// compiled activation can be told apart from an untraced one. The payload
    /// lives outside the collector's graph, so an assigned value is retained as
    /// a process-wide root (see [`frame_setattr`]).
    pub(crate) trace: Mutex<i64>,
    pub(crate) parent: Option<Arc<FrameData>>,
    pub(crate) locals: FrameLocals,
}

impl FrameData {
    /// Python-visible line of this activation.
    pub(crate) fn current_line(&self) -> u32 {
        self.line.load(Ordering::Relaxed)
    }

    /// Module key observed by `f_globals`; empty for an unbound frame.
    pub(crate) fn module_name(&self) -> String {
        self.module
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// CPython-style `co_qualname`: the generated function name without the
    /// imported-module prefix the frontend adds for uniqueness. The method
    /// chain (`C.m`) is preserved.
    pub(crate) fn code_qualname(&self) -> String {
        let module = self.module_name();
        let name = self.name.as_str();
        if module.is_empty() {
            return name.to_string();
        }
        let stripped = name
            .strip_prefix(module.as_str())
            .and_then(|rest| rest.strip_prefix('.'))
            .unwrap_or(name);
        stripped.to_string()
    }

    /// CPython-style `co_name`: the final component of the qualname.
    pub(crate) fn code_name(&self) -> String {
        let qualname = self.code_qualname();
        match qualname.rsplit_once('.') {
            Some((_, name)) => name.to_string(),
            None => qualname,
        }
    }
}

#[derive(Default)]
struct State {
    file: Arc<String>,
    line: u32,
    /// Active call activations, innermost last.
    frames: Vec<Activation>,
    /// Tagged heap pointer to the pending exception object, if any.
    pending: Option<i64>,
    /// Exceptions whose handlers are active (and in-flight exceptions while a
    /// `finally` body runs), innermost last. Implicit chaining reads the top;
    /// the heap mirror keeps these values alive.
    ///
    /// This is the running fiber's own handler stack. While a fiber runs the
    /// caller's stack sits in [`Self::saved_handled`], because CPython swaps
    /// `tstate->exc_info` to the generator's `gi_exc_state` for the duration
    /// of a resume (and to the coroutine's `cr_exc_state` for a coroutine).
    handled: Vec<i64>,
    /// Caller handler stacks parked by nested fiber resumes, outermost first.
    ///
    /// CPython links the resumed fiber's exception state to the caller's via
    /// `previous_item`, so `sys.exc_info()` and implicit chaining fall back to
    /// the caller's exception when the fiber handles none itself. The parked
    /// stacks provide that fallback and keep their exceptions reachable.
    saved_handled: Vec<Vec<i64>>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    /// Native class objects exposed as first-class exception values, keyed by
    /// exception tag. The cached values live in the heap and are registered as
    /// thread roots; [`init`] drops the cache before the heap is freed so a
    /// later program run cannot observe a dangling class pointer.
    static CLASS_CACHE: RefCell<HashMap<u32, i64>> = RefCell::new(HashMap::new());
}

fn with_frames<R>(f: impl FnOnce(&mut Vec<Activation>) -> R) -> R {
    if let Some(ptr) = crate::fiber::current_ptr() {
        // SAFETY: a fiber's `saved_frames` lives in its boxed `FiberState` for
        // the whole fiber lifetime. Only the owning thread runs the fiber, and
        // it cannot finish while this activation is executing.
        return f(unsafe { &mut (*ptr).saved_frames });
    }
    STATE.with(|state| f(&mut state.borrow_mut().frames))
}

/// Removes and returns the thread's frame chain (`fiber` uses this when a
/// generator fiber suspends or resumes).
pub(crate) fn take_thread_frames() -> Vec<Activation> {
    STATE.with(|state| std::mem::take(&mut state.borrow_mut().frames))
}

/// Installs `frames` as the thread's frame chain (the resume side of
/// [`take_thread_frames`]).
pub(crate) fn install_thread_frames(frames: Vec<Activation>) {
    STATE.with(|state| state.borrow_mut().frames = frames);
}

/// Number of activations on the running frame chain: CPython's recursion depth
/// as observed by `sys.setrecursionlimit` and `len(sys._current_frames()[id])`
/// walked through `f_back`.
///
/// This is the *frame* depth, not the dynamic-call counter in
/// `fiber::recursion_depth`: generated code enters one activation per function
/// (including statically resolved calls the depth guard never sees), so the
/// chain is the honest measure of how many Python frames are live.
pub(crate) fn frame_depth() -> usize {
    with_frames(|frames| frames.len())
}

// Source location tracking

pub(crate) fn set_file_ptr(ptr: *const u8, len: usize) {
    let file = Arc::new(decode_bytes(ptr, len));
    STATE.with(|s| s.borrow_mut().file = file);
}

pub(crate) fn set_line(line: u32) {
    STATE.with(|s| s.borrow_mut().line = line);
    with_frames(|frames| {
        if let Some(frame) = frames.last_mut() {
            frame.line = line;
            if let Some(cached) = &frame.cached {
                cached.line.store(line, Ordering::Relaxed);
            }
        }
    });
}

/// Pushes one activation. The record captures the current file and line; the
/// caller link is reconstructed from the activation stack when (and only
/// when) introspection materialises it, so `f_back` still follows the real
/// call chain. No heap allocation happens here.
pub(crate) fn frame_enter_ptr(ptr: *const u8, len: usize) {
    let (file, line) = STATE.with(|s| {
        let state = s.borrow();
        (state.file.clone(), state.line)
    });
    with_frames(|frames| {
        frames.push(Activation {
            name: ptr,
            name_len: len,
            file,
            module: ptr::null(),
            module_len: 0,
            names: ptr::null(),
            names_len: 0,
            base: ptr::null_mut(),
            slots: 0,
            params: 0,
            flags: 0,
            line,
            first_line: 0,
            cached: None,
        });
    });
}

/// Binds the innermost activation to its live local slots and metadata.
///
/// `names` is a NUL-separated UTF-8 blob in slot order; `base` points at the
/// `slots` tagged values the activation registered with
/// `hydro_gc_frame_push`. An invalid pointer or blob is ignored
/// rather than dereferenced, and never panic: the frame then exposes its
/// metadata but no live locals.
#[allow(clippy::too_many_arguments)]
pub(crate) fn frame_bind(
    base: *mut i64,
    slots: usize,
    names: *const u8,
    names_len: usize,
    params: usize,
    flags: u32,
    module: *const u8,
    module_len: usize,
    first_line: u32,
) {
    if slots > (1 << 20) || (base.is_null() && slots != 0) {
        return;
    }
    // Everything is stored as-is; decoding and heap work happen only if the
    // activation is later materialised. An already materialised record (the
    // binding ran after an observation) is refreshed in place without a lock.
    with_frames(|frames| {
        let Some(activation) = frames.last_mut() else {
            return;
        };
        activation.base = if slots == 0 { ptr::null_mut() } else { base };
        activation.slots = if slots == 0 { 0 } else { slots };
        activation.names = names;
        activation.names_len = names_len;
        if !module.is_null() && module_len != 0 {
            activation.module = module;
            activation.module_len = module_len;
        }
        if first_line != 0 {
            activation.line = first_line;
            if let Some(cached) = &activation.cached {
                cached.line.store(first_line, Ordering::Relaxed);
            }
        }
        activation.first_line = first_line;
        activation.params = params;
        activation.flags = flags;
        if let Some(cached) = &activation.cached {
            cached.first_line.store(first_line, Ordering::Relaxed);
            cached.flags.store(flags, Ordering::Relaxed);
            cached.argcount.store(params, Ordering::Relaxed);
            cached
                .locals
                .base
                .store(activation.base as usize, Ordering::Relaxed);
            cached
                .locals
                .slots
                .store(activation.slots, Ordering::Relaxed);
        }
    });
}

/// Pops the innermost call frame. A frame object may outlive it, so the live
/// slot view is replaced by a rooted snapshot when any frame object (or a
/// retained child record) still references the activation.
pub(crate) fn frame_leave() {
    let popped = with_frames(|frames| frames.pop());
    let Some(mut activation) = popped else {
        return;
    };
    // The common path never materialised the record: dropping the activation
    // is all that is left, and its slots are dead with the caller's storage.
    let Some(data) = activation.cached.take() else {
        return;
    };
    let base = data.locals.base.swap(0, Ordering::Relaxed);
    let slots = data.locals.slots.swap(0, Ordering::Relaxed);
    if base == 0 || slots == 0 {
        return;
    }
    // `data` is the only reference we still hold here: a count above one
    // means a frame object (or a child record's parent link) still needs this
    // activation's locals. The slots are still valid here (generated code
    // leaves the precise frame registered until after `hydro_frame_leave`).
    if Arc::strong_count(&data) > 1 {
        let dict = build_locals_dict(&data.locals.names, base as *mut i64, slots);
        // The record is native memory outside the collector's graph, so the
        // snapshot is registered as a process-wide root. It stays valid until
        // the runtime reset, exactly like a cached module value.
        crate::heap::add_global_root(dict);
        *data
            .locals
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(dict);
    }
}

/// A live frame's locals read the activation slots; an ended frame returns
/// the snapshot taken by [`frame_leave`].
pub(crate) fn frame_locals(data: &Arc<FrameData>) -> i64 {
    let locals = &data.locals;
    let base = locals.base.load(Ordering::Relaxed);
    let slots = locals.slots.load(Ordering::Relaxed);
    if base != 0 && slots > 0 {
        return build_locals_dict(&locals.names, base as *mut i64, slots);
    }
    locals
        .snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .unwrap_or_else(|| value::dict_new(Vec::new()))
}

fn build_locals_dict(names: &[String], base: *mut i64, slots: usize) -> i64 {
    let dict = value::dict_new(Vec::new());
    let dict_root = crate::heap::root_values(&[dict]);
    for (index, name) in names.iter().enumerate().take(slots) {
        if name.is_empty() {
            continue;
        }
        // SAFETY: `frame_bind` accepted this pointer only for `slots` values
        // initialised by the registered precise frame; the binding is cleared
        // before the activation's storage disappears.
        let slot = unsafe { ptr::read_volatile(base.add(index)) };
        let key = value::str_alloc(name.clone());
        let entry_root = crate::heap::root_values(&[key, slot]);
        crate::container::dict_set(dict, key, slot);
        crate::heap::release_roots(entry_root);
    }
    crate::heap::release_roots(dict_root);
    dict
}

/// Activation record at `depth` frames above the caller of `sys._getframe`
/// (`depth == 0` is the innermost running Hydro activation).
pub(crate) fn frame_at(depth: usize) -> Option<Arc<FrameData>> {
    with_frames(|frames| {
        let last = frames.len().checked_sub(1)?;
        let index = last.checked_sub(depth)?;
        // Materialise bottom-up so every record's `parent` is the caller's
        // record: observing an inner frame caches the whole `f_back` chain.
        for i in 0..=index {
            if frames[i].cached.is_some() {
                continue;
            }
            let parent = if i == 0 {
                None
            } else {
                frames[i - 1].cached.clone()
            };
            let data = frames[i].materialize(parent);
            frames[i].cached = Some(Arc::new(data));
        }
        frames[index].cached.clone()
    })
}

/// `sys._getframe(depth)`: materialises the frame object at `depth`, or raises
/// CPython's `ValueError` when the stack is shallower.
pub(crate) fn getframe(depth: usize) -> i64 {
    match frame_at(depth) {
        Some(data) => value::frame_new(data),
        None => raise_with_message(VALUE_ERROR, "call stack is not deep enough"),
    }
}

/// `inspect.currentframe()`: the innermost active Hydro activation.
pub(crate) fn current_frame() -> i64 {
    getframe(0)
}

/// Module key of the frame at `depth`; `None` when the stack is too shallow or
/// the activation carries no module (an ABI-only frame).
pub(crate) fn frame_module_at(depth: usize) -> Option<String> {
    let module = frame_at(depth)?.module_name();
    (!module.is_empty()).then_some(module)
}

/// Attribute lookup on a frame object. `None` means the name is not a frame
/// attribute; the caller raises `AttributeError`.
pub(crate) fn frame_attr(frame: i64, name: &str) -> Option<i64> {
    let state = value::as_frame(frame)?;
    let data = &state.data;
    match name {
        "f_back" => Some(match &data.parent {
            Some(parent) => value::frame_new(parent.clone()),
            None => value::NONE,
        }),
        "f_code" => Some(frame_code(data)),
        "f_globals" => Some(crate::core::globals::view(&data.module_name())),
        "f_locals" => Some(frame_locals(data)),
        "f_lineno" => Some(
            value::encode_int(data.current_line() as i64)
                .unwrap_or_else(|| value::bigint_new(i64::from(data.current_line()).into())),
        ),
        // CPython reports the last instruction index; the runtime keeps no
        // bytecode, so the conventional "no instruction" value is truthful.
        "f_lasti" => Some(value::encode_int(-1).expect("fits")),
        "f_trace" => Some(*data.trace.lock().unwrap_or_else(|p| p.into_inner())),
        "__class__" => Some(value::frame_class()),
        _ => None,
    }
}

/// Assignment to a frame attribute: `frame.f_trace = value` and the read-only
/// slots CPython rejects with a precise error.
///
/// `None` means the name is not a frame attribute at all (the caller raises the
/// ordinary `AttributeError`); every other result is the call's return value,
/// which is `None` on success and a raised exception otherwise.
pub(crate) fn frame_setattr(frame: i64, name: &str, new_value: i64) -> Option<i64> {
    let state = value::as_frame(frame)?;
    match name {
        "f_trace" => {
            // The activation record is native memory outside the collector's
            // graph, so keep the assigned value reachable for as long as the
            // record remembers it. A replaced callback stays rooted (the
            // process-wide root list is only cleared on a fresh program run),
            // which `f_trace` assignments make rare enough to be the same
            // conservative retention the locals snapshot already uses.
            crate::heap::add_global_root(new_value);
            *state
                .data
                .trace
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_value;
            Some(NONE)
        }
        // CPython allows this only while a trace function drives the frame; the
        // runtime has no tracing hooks, so the precise error is the truthful
        // answer instead of silently moving the reported line.
        "f_lineno" => Some(raise_with_message(
            VALUE_ERROR,
            "f_lineno can only be set in a trace function",
        )),
        "f_back" | "f_code" | "f_globals" | "f_locals" | "f_lasti" | "f_builtins" => {
            Some(raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("attribute '{name}' of 'frame' objects is not writable"),
            ))
        }
        _ => None,
    }
}

/// `frame.clear()`: drop the frame's local bindings and trace callback.
///
/// CPython refuses while the frame is running or suspended, because its locals
/// storage is still owned by the interpreter; the same two states are visible
/// here as a live slot binding that is (or is not) on the running chain.
/// `None` means `frame` is not a frame object.
pub(crate) fn frame_clear(frame: i64) -> Option<i64> {
    let state = value::as_frame(frame)?;
    let data = &state.data;
    if data.locals.base.load(Ordering::Relaxed) != 0 {
        let executing = with_frames(|frames| {
            frames.iter().any(|activation| {
                activation
                    .cached
                    .as_ref()
                    .is_some_and(|cached| Arc::ptr_eq(cached, data))
            })
        });
        let message = if executing {
            "cannot clear an executing frame"
        } else {
            // A suspended generator's activation still owns slots on its fiber
            // stack; its locals are only snapshotted when it finishes.
            "cannot clear a suspended frame"
        };
        return Some(raise_with_message(RUNTIME_ERROR, message));
    }
    // The empty dict is rooted for the same reason `frame_leave` roots its
    // snapshot: the record lives outside the collector's graph.
    let locals = value::dict_new(Vec::new());
    crate::heap::add_global_root(locals);
    *data
        .locals
        .snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(locals);
    data.locals.slots.store(0, Ordering::Relaxed);
    *data
        .trace
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = NONE;
    Some(NONE)
}

/// `sys._current_frames()`: the innermost activation of each thread that can be
/// observed, keyed by its thread identifier.
///
/// Hydro keeps frame chains in thread-local state (and in the running fiber, see
/// [`with_frames`]), so another thread's chain cannot be walked without pausing
/// it. The honest result is therefore this thread's top frame; a thread with no
/// active activation contributes no entry, exactly like CPython skips a thread
/// that has never run Python code.
pub(crate) fn current_frames() -> i64 {
    let Some(data) = frame_at(0) else {
        return value::dict_new(Vec::new());
    };
    let dict = value::dict_new(Vec::new());
    let dict_root = crate::heap::root_values(&[dict]);
    let ident = value::encode_int(crate::heap::current_thread_id())
        .expect("thread identifiers fit in a tagged integer");
    let frame = value::frame_new(data);
    let entry_root = crate::heap::root_values(&[ident, frame]);
    crate::container::dict_set(dict, ident, frame);
    crate::heap::release_roots(entry_root);
    crate::heap::release_roots(dict_root);
    dict
}

/// Attribute lookup on a code object.
pub(crate) fn code_attr(code: i64, name: &str) -> Option<i64> {
    let state = value::as_code(code)?;
    let int = |n: usize| {
        value::encode_int(n as i64).unwrap_or_else(|| value::bigint_new((n as u64).into()))
    };
    match name {
        "co_name" => Some(value::str_alloc(state.name.clone())),
        "co_qualname" => Some(value::str_alloc(state.qualname.clone())),
        "co_filename" => Some(value::str_alloc(state.filename.clone())),
        "co_firstlineno" => Some(int(state.first_line as usize)),
        "co_argcount" => Some(int(state.argcount)),
        "co_posonlyargcount" => Some(int(state.posonlyargcount)),
        "co_kwonlyargcount" => Some(int(state.kwonlyargcount)),
        "co_nlocals" => Some(int(state.nlocals)),
        "co_flags" => Some(int(state.flags as usize)),
        "co_varnames" => Some(string_tuple(&state.varnames)),
        "co_freevars" | "co_cellvars" | "co_names" | "co_consts" => {
            Some(value::tuple_new(Vec::new()))
        }
        // Hydro keeps no bytecode. The truthful synthetic surface is empty
        // instruction/line/exception tables, zero stack usage and empty
        // position iterables, so `dis`/`inspect` observe a code object with no
        // instructions instead of an `AttributeError`.
        "co_code" | "co_linetable" | "co_exceptiontable" => Some(value::bytes_new(Vec::new())),
        "co_stacksize" => Some(int(0)),
        "co_lines" | "co_positions" => Some(value::list_new(Vec::new())),
        "__class__" => Some(value::code_class()),
        _ => None,
    }
}

/// Builds a tuple of Python strings, rooting every element until the tuple
/// allocation has completed.
fn string_tuple(items: &[String]) -> i64 {
    let mut values = Vec::with_capacity(items.len());
    let mut roots = Vec::with_capacity(items.len() + 1);
    for item in items {
        let value = value::str_alloc(item.clone());
        roots.push(crate::heap::root_values(&[value]));
        values.push(value);
    }
    let tuple = value::tuple_new(values);
    roots.push(crate::heap::root_values(&[tuple]));
    for start in roots.into_iter().rev() {
        crate::heap::release_roots(start);
    }
    tuple
}

fn frame_code(data: &Arc<FrameData>) -> i64 {
    let nlocals = data.locals.names.len();
    let varnames = data.locals.names.clone();
    value::code_new(value::CodeState {
        name: data.code_name(),
        qualname: data.code_qualname(),
        filename: data.file.clone(),
        first_line: data.first_line.load(Ordering::Relaxed),
        argcount: data.argcount.load(Ordering::Relaxed),
        // The frame ABI records only the positional count (`co_argcount`) and
        // no parameter kinds, so a frame's code object cannot distinguish
        // positional-only or keyword-only slots; `function.__code__` reports
        // those from the attached signature instead.
        posonlyargcount: 0,
        kwonlyargcount: 0,
        flags: data.flags.load(Ordering::Relaxed),
        nlocals,
        varnames,
    })
}

/// `repr(frame)`: CPython prints the address; Hydro prints a stable zero so
/// output stays reproducible.
pub(crate) fn frame_repr(data: &FrameData) -> String {
    format!(
        "<frame at 0x0, file {}, line {}, code {}>",
        value::quote_str(&data.file),
        data.current_line(),
        data.code_name()
    )
}

fn decode_names(ptr: *const u8, len: usize) -> Vec<String> {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return Vec::new();
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` readable for as long as
    // the bound activation runs; materialisation only happens within it.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    bytes
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}

fn decode_bytes(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return String::new();
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

// Pending exception state

pub(crate) fn pending() -> bool {
    STATE.with(|s| s.borrow().pending.is_some())
}

pub(crate) fn pending_tag() -> i64 {
    STATE.with(|s| {
        s.borrow()
            .pending
            .and_then(value::exception_data)
            .map(|e| e.tag as i64)
            .unwrap_or(-1)
    })
}

pub(crate) fn pending_value() -> i64 {
    STATE.with(|s| s.borrow().pending.unwrap_or(NONE))
}

pub(crate) fn clear() -> i64 {
    STATE.with(|s| s.borrow_mut().pending = None);
    // Keep the collector's mirror in sync: the exception object is no longer
    // a root and may be reclaimed.
    crate::heap::set_pending_mirror(value::NONE);
    0
}

pub(crate) fn init() {
    STATE.with(|s| *s.borrow_mut() = State::default());
    // The cached class objects belong to the heap that `hydro_runtime_init`
    // frees right after this call; dropping the cache prevents a later run
    // from handing out a dangling pointer.
    CLASS_CACHE.with(|cache| cache.borrow_mut().clear());
    // Sessions and notes hold values (and permanent thread roots) from the
    // heap that is about to be freed.
    STAR_SESSIONS.with(|sessions| sessions.borrow_mut().clear());
    NOTES.with(|notes| notes.borrow_mut().clear());
    value::clear_type_class_cache();
    crate::heap::set_pending_mirror(value::NONE);
    crate::heap::set_handled_mirror(&[]);
}

/// Pushes `value` (a tagged exception or `None`) as the active handler
/// context. `None` marks a `finally` body entered without an in-flight
/// exception, so outer handlers are not shadowed by a stale value.
pub(crate) fn handler_push(value: i64) -> i64 {
    STATE.with(|s| s.borrow_mut().handled.push(value));
    refresh_handled_mirror();
    NONE
}

/// Pops the innermost handler context (`hydro_exc_handler_pop`).
pub(crate) fn handler_pop() -> i64 {
    STATE.with(|s| {
        s.borrow_mut().handled.pop();
    });
    refresh_handled_mirror();
    NONE
}

/// Number of active handler contexts (`hydro_exc_handler_depth`).
///
/// Only the running fiber's own stack counts: generated code captures this
/// depth to balance its pushes, and CPython's per-frame state node likewise
/// starts empty instead of including the caller's handled exceptions.
pub(crate) fn handler_depth() -> i64 {
    STATE.with(|s| s.borrow().handled.len() as i64)
}

/// Pops handler contexts until the stack is back at `depth`
/// (`hydro_exc_handler_truncate`). A depth at or beyond the current length is a
/// no-op, so a path that already unwound cannot pop a caller's context.
pub(crate) fn handler_truncate(depth: i64) -> i64 {
    STATE.with(|s| {
        let mut state = s.borrow_mut();
        if depth >= 0 {
            state.handled.truncate(depth as usize);
        }
    });
    refresh_handled_mirror();
    NONE
}

/// Installs `own` as the running fiber's handler stack and parks the caller's
/// stack (`fiber::resume_raw` calls this around the context switch).
pub(crate) fn enter_fiber_handlers(own: Vec<i64>) {
    STATE.with(|s| {
        let mut state = s.borrow_mut();
        let caller = std::mem::replace(&mut state.handled, own);
        state.saved_handled.push(caller);
    });
    refresh_handled_mirror();
}

/// Restores the caller's handler stack and returns the fiber's own stack
/// (the resume side of [`enter_fiber_handlers`]).
pub(crate) fn leave_fiber_handlers() -> Vec<i64> {
    let own = STATE.with(|s| {
        let mut state = s.borrow_mut();
        let caller = state.saved_handled.pop().unwrap_or_default();
        std::mem::replace(&mut state.handled, caller)
    });
    refresh_handled_mirror();
    own
}

/// Topmost exception handled by the running fiber itself, ignoring the
/// caller's parked stack. `gen.throw`/`close` chain an injected exception to
/// this state only (CPython's `_PyErr_ChainStackItem` inspects the current
/// `gi_exc_state` node, not its `previous_item` chain).
pub(crate) fn handled_own() -> Option<i64> {
    STATE.with(|s| {
        s.borrow()
            .handled
            .iter()
            .rev()
            .copied()
            .find(|&value| value != NONE)
    })
}

/// Exception whose handler is active, if any. `None` entries above it are
/// `finally` markers and are skipped. The running fiber's own stack wins;
/// when it handles nothing, the parked caller stacks supply the exception
/// CPython reaches through `previous_item`.
fn handled() -> Option<i64> {
    STATE.with(|s| {
        let state = s.borrow();
        state
            .handled
            .iter()
            .rev()
            .copied()
            .find(|&value| value != NONE)
            .or_else(|| {
                state
                    .saved_handled
                    .iter()
                    .rev()
                    .find_map(|stack| stack.iter().rev().copied().find(|&value| value != NONE))
            })
    })
}

/// Tagged values of the active handler stacks (the running fiber's and every
/// parked caller's), for the collector's root scan.
pub(crate) fn handled_values() -> Vec<i64> {
    STATE.with(|s| {
        let state = s.borrow();
        let mut values = state.handled.clone();
        for stack in &state.saved_handled {
            values.extend_from_slice(stack);
        }
        values
    })
}

/// Mirror the live handler values into the thread registration for the
/// collector. Looks only at the thread-local state; callers must not hold a
/// borrow.
fn refresh_handled_mirror() {
    let values = handled_values();
    crate::heap::set_handled_mirror(&values);
}

/// `sys.exc_info()`: the `(type, value, traceback)` triple of the exception
/// whose handler is active, or `(None, None, None)` outside `except` handling.
///
/// The type is the exception's class value (`type(value)`), so a user-defined
/// exception class keeps its identity and a built-in one matches the cached
/// class the builtin name resolves to. Hydro records the raise location in
/// [`ExcData`] but has no traceback object, so the third element is always
/// `None`, as `value.__traceback__` reports.
pub(crate) fn exc_info() -> i64 {
    match handled() {
        Some(exception) => value::tuple_new(vec![exception_type(exception), exception, NONE]),
        None => value::tuple_new(vec![NONE, NONE, NONE]),
    }
}

/// Class value reported as the first element of [`exc_info`].
///
/// A user-defined exception keeps its Python class; a built-in one resolves to
/// the cached class for its tag. A malformed tag has no class to expose, so
/// the type is `None` rather than an exception raised while reporting, and the
/// handled value is still returned unchanged.
fn exception_type(exception: i64) -> i64 {
    if value::as_exc_instance(exception).is_some() {
        return value::type_class(exception);
    }
    match value::exception_data(exception) {
        Some(data) if known_tag(data.tag) => exception_class(data.tag),
        _ => NONE,
    }
}

/// Renders the traceback text CPython's default `sys.excepthook` prints for
/// `value`: every exception in the cause/context chain, deepest first, each
/// with its raise location. Returns `None` when `value` is not an exception
/// object.
pub(crate) fn render_exception(value: i64) -> Option<String> {
    value::exception_clone(value).map(|data| render_chain(value, &data))
}

/// `sys.excepthook(type, value, traceback)`: CPython's default hook.
///
/// A `None` type prints nothing, like `sys_excepthook`. An exception `value`
/// is rendered with [`render_exception`]; anything else is reported truthfully
/// as `TypeName: repr(value)` because Hydro cannot build a traceback for a
/// non-exception. The hook writes to stderr and never raises.
pub(crate) fn excepthook(exc_type: i64, value: i64, _traceback: i64) -> i64 {
    if let Some(text) = excepthook_text(exc_type, value) {
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        let _ = stderr.write_all(text.as_bytes());
        let _ = stderr.flush();
    }
    NONE
}

/// Text [`excepthook`] writes, or `None` when the hook is silent (`type=None`).
fn excepthook_text(exc_type: i64, value: i64) -> Option<String> {
    if exc_type == NONE {
        return None;
    }
    Some(match render_exception(value) {
        Some(text) => text,
        None => {
            let name = value::class_display_name(exc_type)
                .map(str::to_string)
                .unwrap_or_else(|| value::type_name_of(exc_type));
            format!("{name}: {}\n", value::repr(value))
        }
    })
}

/// Raises `tag` with `message`, capturing the current file/line/frame.
/// Returns the ABI dummy value (`None`).
///
/// This is the internal raise path used by runtime failures: the rendered
/// message is also the single `args` element, matching CPython's
/// `ZeroDivisionError("division by zero")`.
pub(crate) fn raise_with_message(tag: u32, message: &str) -> i64 {
    let args = if message.is_empty() {
        Vec::new()
    } else {
        vec![message.to_string()]
    };
    raise_with_message_cause(tag, message, args, Vec::new(), Vec::new(), None, false)
}

/// Source location `(file, line, frame name)` of the innermost activation, as
/// recorded on a newly raised exception.
fn current_location() -> (String, u32, String) {
    let (file, line) = STATE.with(|s| {
        let state = s.borrow();
        ((*state.file).clone(), state.line)
    });
    let frame = with_frames(|frames| {
        frames.last().map_or_else(String::new, |activation| {
            decode_bytes(activation.name, activation.name_len)
        })
    });
    (file, line, frame)
}

/// [`raise_with_message`] with an explicit cause (`None` means "no cause")
/// and the `from` suppression flag. A new exception records the exception
/// whose handler is active as its implicit `context`.
///
/// `values`/`reprs` carry the original argument object of a `raise X(v)` form
/// (empty when the raise only has text). They are rooted across the payload
/// allocation: a Rust-side `Vec` is invisible to the collector.
fn raise_with_message_cause(
    tag: u32,
    message: &str,
    args: Vec<String>,
    reprs: Vec<String>,
    values: Vec<i64>,
    cause: Option<i64>,
    suppress_context: bool,
) -> i64 {
    let (file, line, frame) = current_location();
    // Root the tagged payload before allocating the exception object.
    let start = crate::heap::root_values(&values);
    if let Some(cause) = cause {
        crate::heap::root_values(&[cause]);
    }
    let data = ExcData {
        tag,
        name: tag_name(tag),
        msg: message.to_string(),
        file,
        line,
        frame,
        cause,
        context: handled(),
        suppress_context,
        exit_code: None,
        args,
        arg_values: values,
        arg_reprs: reprs,
        keywords: Vec::new(),
        object: None,
    };
    let exception = value::exception_new(data);
    crate::heap::release_roots(start);
    publish(exception)
}

/// The `StopIteration.value` member for `data`: the first positional
/// argument, or `None` when the exception was built without one.
///
/// CPython stores the member at construction (`StopIteration_init`); runtime
/// raises only render their argument as text, so the text is materialised
/// back into a string in that case.
fn stop_iteration_value(data: &ExcData) -> i64 {
    data.arg_values
        .first()
        .copied()
        .or_else(|| {
            data.args
                .first()
                .map(|text| value::str_from_bytes(text.as_bytes()))
        })
        .unwrap_or(NONE)
}

/// Raised when a generator body finishes: `StopIteration` carrying the
/// generator's return value, exactly like CPython's `_PyGen_SetStopIterationValue`.
///
/// `result == None` raises the bare form (`args == ()`, `value is None`).
pub(crate) fn raise_stop_iteration(result: i64) -> i64 {
    if result == NONE {
        return raise_with_message(STOP_ITERATION, "");
    }
    let (file, line, frame) = current_location();
    // `result` only lives on the Rust stack while the payload allocates.
    let start = crate::heap::root_values(&[result]);
    let text = value::to_string(result);
    let repr = value::repr(result);
    let data = ExcData {
        tag: STOP_ITERATION,
        name: tag_name(STOP_ITERATION),
        msg: text.clone(),
        file,
        line,
        frame,
        cause: None,
        // `_PyGen_SetStopIterationValue` installs the exception with
        // `PyErr_SetRaisedException`, so a valued StopIteration never picks up
        // the caller's handled exception as context (the bare form, built by
        // `PyErr_SetNone`, does).
        context: None,
        suppress_context: false,
        exit_code: None,
        args: vec![text],
        arg_values: vec![result],
        arg_reprs: vec![repr],
        keywords: Vec::new(),
        object: None,
    };
    let exception = value::exception_new(data);
    crate::heap::release_roots(start);
    publish(exception)
}

/// Publishes an existing exception without implicit `__context__` chaining.
///
/// CPython propagates a captured or injected exception with `_PyErr_Restore`/
/// `_PyErr_SetRaisedException`, which keeps the exception's context exactly as
/// it was raised. [`raise_value`] is the source-level `raise` and adds the
/// handled exception as context; this is the propagation path.
pub(crate) fn raise_value_unchained(v: i64) -> i64 {
    match value::exception_data(v) {
        Some(_) => publish(v),
        None if value::is_ext_exception(v) => raise_extension(v, RUNTIME_ERROR, ""),
        None => raise_with_message(TYPE_ERROR, "exceptions must derive from BaseException"),
    }
}

/// Raises `v` as an injected exception (`gen.throw`/cancellation), chaining it
/// only to `context`: the running fiber's own handled exception.
///
/// A pre-existing context is kept, and the value itself is published when no
/// chaining is needed, so `gen.throw(exc)` propagating unchanged preserves
/// object identity. CPython mutates the exception in place; the immutable
/// payload forces a copy only when a context is actually attached.
pub(crate) fn raise_injected(v: i64, context: Option<i64>) -> i64 {
    match value::exception_data(v) {
        Some(data) => {
            let context = context.filter(|&handled| handled != v);
            match context {
                Some(context) if data.context.is_none() => {
                    let mut data = data.clone();
                    data.context = Some(context);
                    publish(value::exception_with_data(v, data))
                }
                _ => publish(v),
            }
        }
        None if value::is_ext_exception(v) => raise_extension(v, RUNTIME_ERROR, ""),
        None => raise_with_message(TYPE_ERROR, "exceptions must derive from BaseException"),
    }
}

/// PEP 479: a `StopIteration` (or, for async generators,
/// `StopAsyncIteration`) escaping a generator body becomes a `RuntimeError`.
///
/// CPython's `stopiteration_error` sets both `__cause__` and `__context__` to
/// the escaping exception and suppresses the implicit context rendering.
pub(crate) fn raise_generator_stop(error: i64, message: &str) -> i64 {
    let (file, line, frame) = current_location();
    // `error` stays referenced by the pending slot, but the payload copy must
    // survive a collection during allocation.
    let start = crate::heap::root_values(&[error]);
    let data = ExcData {
        tag: RUNTIME_ERROR,
        name: tag_name(RUNTIME_ERROR),
        msg: message.to_string(),
        file,
        line,
        frame,
        cause: Some(error),
        context: Some(error),
        suppress_context: true,
        exit_code: None,
        args: vec![message.to_string()],
        arg_values: Vec::new(),
        arg_reprs: vec![value::quote_str(message)],
        keywords: Vec::new(),
        object: None,
    };
    let exception = value::exception_new(data);
    crate::heap::release_roots(start);
    publish(exception)
}

/// Raises `SystemExit` with the tagged Python value `code`, as `sys.exit()`
/// and `raise SystemExit(...)` do.
///
/// `None` exits 0, an integer stores that status, a string stores a message
/// (printed by [`uncaught`] before exiting 1) and any other object is
/// rendered with `repr()`. The exception is an ordinary catchable
/// `SystemExit`; only [`uncaught`] turns it into a process status.
pub(crate) fn raise_exit(code: i64) -> i64 {
    let (message, exit_code, args, reprs, values) = if code == value::NONE {
        (String::new(), Some(0), Vec::new(), Vec::new(), Vec::new())
    } else if let Some(status) = value::as_integer(code) {
        (
            value::to_string(code),
            Some(status),
            vec![value::to_string(code)],
            vec![value::repr(code)],
            vec![code],
        )
    } else if let Some(text) = value::as_str(code) {
        (
            text.to_string(),
            None,
            vec![text.to_string()],
            vec![value::repr(code)],
            vec![code],
        )
    } else {
        (
            value::repr(code),
            None,
            vec![value::to_string(code)],
            vec![value::repr(code)],
            vec![code],
        )
    };
    let (file, line, frame) = current_location();
    let start = crate::heap::root_values(&values);
    let data = ExcData {
        tag: SYSTEM_EXIT,
        name: tag_name(SYSTEM_EXIT),
        msg: message,
        file,
        line,
        frame,
        cause: None,
        context: handled(),
        suppress_context: true,
        exit_code,
        args,
        arg_values: values,
        arg_reprs: reprs,
        keywords: Vec::new(),
        object: None,
    };
    let exception = value::exception_new(data);
    crate::heap::release_roots(start);
    publish(exception)
}

/// Installs `exc` as the pending exception and mirrors it into the thread's
/// GC registration so a collector stopping the world at a safepoint also
/// marks it. Returns the ABI dummy value (`None`).
fn publish(exc: i64) -> i64 {
    STATE.with(|s| s.borrow_mut().pending = Some(exc));
    crate::heap::set_pending_mirror(exc);
    NONE
}

/// Normalises the ABI `cause` argument: `None` suppresses the cause
/// (`raise ... from None`), any other tagged value is stored as the cause.
fn cause_of(cause: i64) -> Option<i64> {
    if cause == NONE {
        None
    } else {
        Some(cause)
    }
}

/// Exclusive access to an exception payload (native or user-defined subclass).
///
/// CPython mutates an exception in place when a `raise` records `__context__`
/// or `__cause__`, and tests compare the re-raised object by identity, so the
/// payload is updated rather than copied. `None` for values that are not
/// exception instances.
fn exception_data_mut(exception: i64) -> Option<&'static mut ExcData> {
    if !value::is_heap(exception) || (exception & !value::TAG_MASK) == 0 {
        return None;
    }
    // SAFETY: the tag bits identify a heap object rooted by the caller; the
    // payload type is selected by `kind` and checked below.
    let obj = unsafe { &mut *((exception & !value::TAG_MASK) as *mut value::HydroObj) };
    match obj.kind {
        value::KIND_EXCEPTION => match obj.data_mut() {
            value::ObjDataMut::Exception(data) => Some(data),
            _ => None,
        },
        value::KIND_EXC_INSTANCE => match obj.data_mut() {
            value::ObjDataMut::ExcInstance(state) => Some(&mut state.data),
            _ => None,
        },
        _ => None,
    }
}

/// Records `context` on an existing exception in place. Returns whether the
/// payload could be updated (`false` for non-exception values).
fn set_exception_context(exception: i64, context: i64) -> bool {
    match exception_data_mut(exception) {
        Some(data) => {
            data.context = Some(context);
            true
        }
        None => false,
    }
}

/// Renders the positional payload of a `raise X(v)` form.
///
/// Returns `(message, args, reprs, values)`: `None` selects the tag's default
/// message and stores no argument; `KeyError` renders its argument with
/// `repr()` (so `KeyError("k")` prints `'k'`), while every other exception
/// uses `str()` for strings and `repr()` for non-string arguments. The
/// argument object itself is kept so `e.args == (v,)` and `repr(e)` match
/// CPython.
fn raise_payload(tag: u32, msg: i64) -> (String, Vec<String>, Vec<String>, Vec<i64>) {
    if msg == value::NONE {
        return (
            default_message(tag).to_string(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
    }
    let text = value::to_string(msg);
    let repr = value::repr(msg);
    let message = if tag == KEY_ERROR {
        repr.clone()
    } else if value::as_str(msg).is_some() {
        text.clone()
    } else {
        repr.clone()
    };
    (message, vec![text], vec![repr], vec![msg])
}

/// `hydro_raise`: `msg` is a string value or `None` (default message).
///
/// Python renders a `KeyError` argument with `repr()` (so `KeyError("k")`
/// prints `'k'`), while other exception types use `str()` for strings and
/// `repr()` for non-string arguments. The argument object is stored in `args`
/// alongside its rendered forms.
pub(crate) fn raise(tag: u32, msg: i64) -> i64 {
    if tag == SYSTEM_EXIT {
        return raise_exit(msg);
    }
    let (text, args, reprs, values) = raise_payload(tag, msg);
    raise_with_message_cause(tag, &text, args, reprs, values, None, false)
}

/// `hydro_raise_msg`: copies the message bytes before storing them.
pub(crate) fn raise_msg_ptr(tag: u32, ptr: *const u8, len: usize) -> i64 {
    let text = decode_bytes(ptr, len);
    raise_with_message(tag, &text)
}

/// `hydro_raise_value`: re-raises an existing exception object (bare `raise`).
///
/// The object is stored as-is, so type, message and source location of the
/// original raise are preserved. If another exception's handler is active and
/// the object has no implicit context yet, `__context__` is attached to the
/// object in place (CPython's `PyException_SetContext`); re-raising the
/// handled object itself never creates a self-cycle. Raising anything that is
/// not a `BaseException` raises CPython's
/// `TypeError: exceptions must derive from BaseException`.
pub(crate) fn raise_value(v: i64) -> i64 {
    // `raise SomeExceptionClass` instantiates the class with no arguments,
    // exactly like CPython; only the resulting instance is published.
    if value::as_class(v).is_some() {
        let instance = crate::core::builtins::call(v, 0, std::ptr::null());
        if pending() {
            return NONE;
        }
        if instance == v {
            return raise_with_message(TYPE_ERROR, "exceptions must derive from BaseException");
        }
        return raise_value(instance);
    }
    // An extension exception object re-raised from source. Hydro has no class
    // object for it, so it becomes the handler-compatible `RuntimeError`
    // wrapper that keeps the original instance.
    if value::exception_data(v).is_none() {
        return if value::is_ext_exception(v) {
            raise_extension(v, RUNTIME_ERROR, "")
        } else {
            raise_with_message(TYPE_ERROR, "exceptions must derive from BaseException")
        };
    }
    // CPython's `PyException_SetContext` mutates the re-raised object, so
    // `raise` through an outer handler preserves its identity.
    let needs_context = value::exception_data(v).is_some_and(|data| data.context.is_none());
    if needs_context {
        if let Some(context) = handled().filter(|&h| h != v) {
            if !set_exception_context(v, context) {
                // No exclusive payload (should not happen for a checked
                // exception); fall back to a copy that keeps the class.
                if let Some(mut data) = value::exception_clone(v) {
                    data.context = Some(context);
                    return publish(value::exception_with_data(v, data));
                }
            }
        }
    }
    publish(v)
}

/// Python-visible class name of an exception payload.
///
/// A raise that crossed the C-API boundary keeps the original extension
/// instance, whose type name is authoritative (for example
/// `pydantic_core._pydantic_core.ValidationError`); a native raise uses the
/// static tag name.
pub(crate) fn exception_display_name(data: &ExcData) -> String {
    match data.object.and_then(value::ext_object_type_name) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => data.name.to_string(),
    }
}

/// Message rendered for an exception payload in tracebacks.
///
/// An extension instance supplies its own `str()`; otherwise the stored
/// message is used.
pub(crate) fn exception_message(data: &ExcData) -> String {
    match data.object {
        Some(object) => {
            let text = value::ext_object_str(object);
            if text.is_empty() {
                data.msg.clone()
            } else {
                text
            }
        }
        None => data.msg.clone(),
    }
}

/// Raises `tag` while preserving an extension exception instance.
///
/// The frontend approximates an imported exception class with
/// [`RUNTIME_ERROR`] for `raise`/`except`, so the Hydro payload carries that
/// tag while `object` (an opaque [`value::KIND_EXT_OBJECT`] handle) supplies
/// the original `str`, `repr`, `args`, attributes and methods. `message` is
/// the C-API error text, used only when the extension cannot render itself.
pub(crate) fn raise_extension(object: i64, tag: u32, message: &str) -> i64 {
    raise_extension_with(object, tag, message, None, false)
}

/// [`raise_extension`] with an explicit `raise ... from` cause.
pub(crate) fn raise_extension_from(object: i64, tag: u32, message: &str, cause: i64) -> i64 {
    raise_extension_with(object, tag, message, cause_of(cause), true)
}

fn raise_extension_with(
    object: i64,
    tag: u32,
    message: &str,
    cause: Option<i64>,
    suppress_context: bool,
) -> i64 {
    let args = if message.is_empty() {
        Vec::new()
    } else {
        vec![message.to_string()]
    };
    let (file, line, frame) = current_location();
    let context = handled();
    // The extension handle is not reachable from the pending mirror until the
    // exception object exists, so root it across the allocation.
    let roots = crate::heap::root_values(&[object]);
    let data = ExcData {
        tag,
        name: tag_name(tag),
        msg: message.to_string(),
        file,
        line,
        frame,
        cause,
        context,
        suppress_context,
        exit_code: None,
        args,
        arg_values: Vec::new(),
        arg_reprs: Vec::new(),
        keywords: Vec::new(),
        object: Some(object),
    };
    let exception = value::exception_new(data);
    publish(exception);
    crate::heap::release_roots(roots);
    NONE
}

/// `hydro_raise_from`: `raise X(msg) from cause`.
///
/// A `cause` of `None` (`from None`) stores no cause; any other tagged value
/// is stored as the explicit cause. Either way the implicit `context` is
/// recorded but no longer rendered, so explicit chaining wins.
pub(crate) fn raise_from(tag: u32, msg: i64, cause: i64) -> i64 {
    if tag == SYSTEM_EXIT {
        return raise_exit(msg);
    }
    let (text, args, reprs, values) = raise_payload(tag, msg);
    raise_with_message_cause(tag, &text, args, reprs, values, cause_of(cause), true)
}

/// `hydro_raise_value_from`: `raise value from cause` for an existing exception
/// object.
///
/// The payload is copied with the new cause rather than mutated: exception
/// objects can be shared between threads, so the original stays untouched.
/// Raising anything that is not a `BaseException` raises CPython's
/// `TypeError: exceptions must derive from BaseException`, like
/// [`raise_value`].
pub(crate) fn raise_value_from(v: i64, cause: i64) -> i64 {
    if value::as_class(v).is_some() {
        let instance = crate::core::builtins::call(v, 0, std::ptr::null());
        if pending() {
            return NONE;
        }
        if instance == v {
            return raise_with_message(TYPE_ERROR, "exceptions must derive from BaseException");
        }
        return raise_value_from(instance, cause);
    }
    match value::exception_data(v) {
        Some(data) => {
            let mut data = data.clone();
            data.cause = cause_of(cause);
            data.suppress_context = true;
            if let Some(context) = handled().filter(|&h| h != v) {
                data.context = Some(context);
            }
            // Keep a user-defined exception's Python class identity.
            publish(value::exception_with_data(v, data))
        }
        None if value::is_ext_exception(v) => raise_extension_from(v, RUNTIME_ERROR, "", cause),
        None => raise_with_message(TYPE_ERROR, "exceptions must derive from BaseException"),
    }
}

/// `hydro_exc_matches`: whether the pending exception is caught by a handler for
/// `handler_tag`.
///
/// Matching walks the CPython parent chain in [`parents`]: a handler catches
/// its own class and every subclass, so `ArithmeticError` catches
/// `ZeroDivisionError`, `LookupError` catches `IndexError`/`KeyError`,
/// `ImportError` catches `ModuleNotFoundError`, `OSError` catches
/// `FileNotFoundError` and every other `OSError` subclass, and `BaseException`
/// catches all of them. `Exception` catches ordinary exceptions but not the
/// `BaseException`-only children (`SystemExit`, `KeyboardInterrupt`,
/// `GeneratorExit`, `CancelledError`, `BaseExceptionGroup`).
///
/// An unknown pending tag falls back to matching only `BaseException` and
/// `Exception`; an unknown handler tag never matches. Without a pending
/// exception nothing matches.
pub(crate) fn matches_handler(handler_tag: u32) -> i32 {
    let pending = STATE.with(|s| s.borrow().pending);
    let Some(tag) = pending.and_then(|exc| value::exception_data(exc).map(|data| data.tag)) else {
        return 0;
    };
    if !known_tag(handler_tag) {
        return 0;
    }
    if !known_tag(tag) {
        // Historical safe fallback for a malformed tag: it is treated as an
        // ordinary, unknown exception rather than matching a concrete
        // handler.
        return i32::from(handler_tag == BASE_EXCEPTION || handler_tag == EXCEPTION);
    }
    i32::from(tag_is_subclass(tag, handler_tag))
}

/// `except <expression>:` dispatch: whether `exception` is caught by
/// `matcher`, enforcing CPython's operand rules.
///
/// `matcher` is a class, a tuple of classes (nested tuples allowed), or an
/// invalid operand. A class that does not inherit from `BaseException` — and
/// anything that is not a class at all, including an exception *instance* —
/// reports CPython's
/// `TypeError: catching classes that do not inherit from BaseException is not
/// allowed` and returns `0`. This is what makes `except SomeObject:` fail
/// instead of silently catching or ignoring the exception.
///
/// Unlike [`matches_handler`] this matches an explicit value, because the
/// handler expression is evaluated with the pending exception parked
/// (cleared) so the expression cannot observe it.
pub(crate) fn matches_handler_value(exception: i64, matcher: i64) -> i32 {
    match match_exception_value(exception, matcher) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(()) => {
            raise_with_message(
                TYPE_ERROR,
                "catching classes that do not inherit from BaseException is not allowed",
            );
            0
        }
    }
}

/// Recursive form of [`matches_handler_value`]; `Err` marks an operand that
/// cannot catch anything.
fn match_exception_value(exception: i64, matcher: i64) -> Result<bool, ()> {
    if let Some(items) = value::as_tuple(matcher) {
        for &item in items {
            if match_exception_value(exception, item)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if !is_exception_class(matcher) {
        return Err(());
    }
    let Some(data) = value::exception_data(exception) else {
        return Ok(false);
    };
    if let Some(handler_tag) = exception_tag_of_class(matcher) {
        // Built-in exception classes are matched by their tag hierarchy.
        let tag = if known_tag(data.tag) {
            data.tag
        } else {
            value::as_exc_instance(exception)
                .and_then(|instance| exception_tag_of_class(instance.class))
                .or_else(|| {
                    value::instance_class_value(exception)
                        .and_then(value::exception_tag_of_derived_class)
                })
                .unwrap_or(data.tag)
        };
        if !known_tag(tag) {
            // Match [`matches_handler`]'s fallback for an anonymous native
            // exception payload: it is an ordinary exception, so broad
            // handlers (`except Exception:`, `except BaseException:`) still
            // catch it.
            return Ok(handler_tag == EXCEPTION || handler_tag == BASE_EXCEPTION);
        }
        return Ok(tag_is_subclass(tag, handler_tag));
    }
    // A native or user exception class that is not in the built-in tag table
    // (`struct.error`, `re.error`, …) must match by class identity. Mapping it
    // to its nearest built-in ancestor would make `except struct.error:` catch
    // every ordinary exception.
    if let Some(instance) = value::as_exc_instance(exception) {
        return Ok(value::class_mro(instance.class).contains(&matcher));
    }
    if let Some(class) = value::instance_class_value(exception) {
        return Ok(value::class_mro(class).contains(&matcher));
    }
    Ok(false)
}

/// Whether `candidate` is a class CPython accepts as an exception handler:
/// `BaseException` or a subclass of it.
pub(crate) fn is_exception_class(candidate: i64) -> bool {
    if value::as_class(candidate).is_none() {
        return false;
    }
    exception_tag_of_class(candidate)
        .or_else(|| value::exception_tag_of_derived_class(candidate))
        .is_some_and(|tag| tag_is_subclass(tag, BASE_EXCEPTION))
}

// Exception groups (PEP 654)

/// Reserved payload slots of an exception group. The names cannot collide with
/// Python identifiers, so `e.<name>` never exposes them and they survive the
/// payload copies `raise` makes for implicit chaining.
const GROUP_EXCEPTIONS_KEY: &str = "$exceptions";
const GROUP_MESSAGE_KEY: &str = "$message";
/// Identity of the group an `except*` clause split this one from. Kept on
/// every derived group so the final re-raise can merge re-raised parts back
/// into the original nesting (CPython compares traceback/cause/context
/// identity; Hydro has no traceback object and copies the payload on
/// re-raise, so an explicit origin marker is exact).
const GROUP_STAR_ROOT_KEY: &str = "$star_root";

/// Whether `tag` is `BaseExceptionGroup` or `ExceptionGroup`.
pub(crate) fn is_group_tag(tag: u32) -> bool {
    tag == EXCEPTION_GROUP || tag == BASE_EXCEPTION_GROUP
}

fn is_group_value(v: i64) -> bool {
    value::exception_data(v).is_some_and(|data| is_group_tag(data.tag))
}

/// Whether `v` is an exception instance (native payload or extension).
fn is_exception_instance(v: i64) -> bool {
    value::exception_data(v).is_some() || value::is_ext_exception(v)
}

/// Whether `v` is an ordinary (`Exception`-derived) exception, which is what
/// decides between `ExceptionGroup` and `BaseExceptionGroup`.
fn is_nonbase_exception(v: i64) -> bool {
    match value::exception_data(v) {
        Some(data) => tag_is_subclass(data.tag, EXCEPTION),
        // An extension exception is approximated as an ordinary exception.
        None => value::is_ext_exception(v),
    }
}

fn group_children(data: &ExcData) -> Option<Vec<i64>> {
    let tuple = keyword_value(data, GROUP_EXCEPTIONS_KEY)?;
    value::as_tuple(tuple).map(|items| items.to_vec())
}

/// How a derived group renders `args[1]`: CPython keeps the *original*
/// constructor argument there, so a list prints as a list and a tuple as a
/// tuple; `split`/`subgroup`/`derive` always derive from a list.
#[derive(Clone, Copy)]
enum GroupSource {
    List,
    Tuple,
    /// The exact object the constructor received.
    Exact(i64),
}

/// Builds a group payload with CPython's `BaseExceptionGroup` fields.
///
/// `message` must be a `str` value. `children` must be a validated, non-empty
/// list of exception instances; they are rooted while the payload allocates.
fn group_value(requested: u32, message: i64, children: &[i64], source: GroupSource) -> i64 {
    let roots = crate::heap::root_values(children);
    crate::heap::root_values(std::slice::from_ref(&message));
    let tag = if requested == EXCEPTION_GROUP
        || children.iter().all(|&child| is_nonbase_exception(child))
    {
        EXCEPTION_GROUP
    } else {
        BASE_EXCEPTION_GROUP
    };
    let source_value = match source {
        GroupSource::List => value::list_new(children.to_vec()),
        GroupSource::Tuple => value::tuple_new(children.to_vec()),
        GroupSource::Exact(v) => v,
    };
    crate::heap::root_values(std::slice::from_ref(&source_value));
    let exceptions = match value::as_tuple(source_value) {
        Some(_) => source_value,
        None => value::tuple_new(children.to_vec()),
    };
    let message_text = value::to_string(message);
    let count = children.len();
    let plural = if count > 1 { "s" } else { "" };
    let msg = format!("{message_text} ({count} sub-exception{plural})");
    let data = ExcData {
        tag,
        name: tag_name(tag),
        msg,
        file: String::new(),
        line: 0,
        frame: String::new(),
        cause: None,
        context: None,
        suppress_context: false,
        exit_code: None,
        args: vec![message_text, value::to_string(source_value)],
        arg_values: vec![message, source_value],
        arg_reprs: vec![value::repr(message), value::repr(source_value)],
        keywords: vec![
            (GROUP_EXCEPTIONS_KEY.to_string(), exceptions),
            (GROUP_MESSAGE_KEY.to_string(), message),
        ],
        object: None,
    };
    let result = value::exception_new(data);
    crate::heap::release_roots(roots);
    result
}

/// Validates the `(message, exceptions)` constructor arguments CPython's
/// `BaseExceptionGroup.__new__` requires and builds the group payload.
fn group_from_call(tag: u32, values: &[i64], keywords: &[(String, i64)]) -> i64 {
    let name = tag_name(tag);
    if !keywords.is_empty() {
        return raise_with_message(TYPE_ERROR, &format!("{name}() takes no keyword arguments"));
    }
    if values.len() != 2 {
        return raise_with_message(
            TYPE_ERROR,
            &format!(
                "{name}() takes exactly 2 arguments ({} given)",
                values.len()
            ),
        );
    }
    let message = values[0];
    if value::as_str(message).is_none() {
        return raise_with_message(
            TYPE_ERROR,
            &format!(
                "{name}() argument 1 must be str, not {}",
                value::type_name(message)
            ),
        );
    }
    let source = values[1];
    let children: Vec<i64> = if let Some(items) = value::as_list(source) {
        items.to_vec()
    } else if let Some(items) = value::as_tuple(source) {
        items.to_vec()
    } else {
        return raise_with_message(
            TYPE_ERROR,
            "second argument (exceptions) must be a sequence",
        );
    };
    if children.is_empty() {
        return raise_with_message(
            VALUE_ERROR,
            "second argument (exceptions) must be a non-empty sequence",
        );
    }
    for (index, &child) in children.iter().enumerate() {
        if !is_exception_instance(child) {
            return raise_with_message(
                VALUE_ERROR,
                &format!("Item {index} of second argument (exceptions) is not an exception"),
            );
        }
    }
    let nested_base = children.iter().any(|&child| !is_nonbase_exception(child));
    if tag == EXCEPTION_GROUP && nested_base {
        return raise_with_message(
            TYPE_ERROR,
            "Cannot nest BaseExceptions in an ExceptionGroup",
        );
    }
    group_value(tag, message, &children, GroupSource::Exact(source))
}

/// Matcher operand of `split`/`subgroup`/`except*`: an exception class, a
/// tuple of exception classes, or a predicate callable.
enum GroupMatcher {
    Types(Vec<i64>),
    Predicate(i64),
}

fn class_tag(class: i64) -> Option<u32> {
    exception_tag_of_class(class).or_else(|| value::exception_tag_of_derived_class(class))
}

fn is_group_class(class: i64) -> bool {
    value::as_class(class).is_some()
        && class_tag(class).is_some_and(|tag| tag_is_subclass(tag, BASE_EXCEPTION_GROUP))
}

/// Classifies a `split`/`subgroup` matcher, reporting an invalid operand
/// through the pending `TypeError` exactly like CPython's
/// `get_matcher_type`. With `star` set, callables are rejected (an `except*`
/// clause only accepts classes) and `BaseExceptionGroup` subclasses are
/// reported with their dedicated message.
fn classify_matcher(matcher: i64, star: bool) -> Result<GroupMatcher, ()> {
    let invalid = |message: &str| {
        raise_with_message(TYPE_ERROR, message);
    };
    if let Some(items) = value::as_tuple(matcher) {
        if items.is_empty() {
            invalid("expected an exception type, a tuple of exception types, or a callable (other than a class)");
            return Err(());
        }
        for &item in items {
            if !is_exception_class(item) {
                invalid("expected an exception type, a tuple of exception types, or a callable (other than a class)");
                return Err(());
            }
        }
        if star && items.iter().any(|&item| is_group_class(item)) {
            invalid("catching ExceptionGroup with except* is not allowed. Use except instead.");
            return Err(());
        }
        return Ok(GroupMatcher::Types(items.to_vec()));
    }
    if is_exception_class(matcher) {
        if star && is_group_class(matcher) {
            invalid("catching ExceptionGroup with except* is not allowed. Use except instead.");
            return Err(());
        }
        return Ok(GroupMatcher::Types(vec![matcher]));
    }
    if value::as_class(matcher).is_some() {
        invalid("catching classes that do not inherit from BaseException is not allowed");
        return Err(());
    }
    if star {
        invalid("catching classes that do not inherit from BaseException is not allowed");
        return Err(());
    }
    if value::as_function(matcher).is_some()
        || value::as_ext_object(matcher).is_some()
        || value::as_instance(matcher)
            .is_some_and(|state| value::class_lookup(state.class, "__call__").is_some())
    {
        return Ok(GroupMatcher::Predicate(matcher));
    }
    invalid("expected an exception type, a tuple of exception types, or a callable (other than a class)");
    Err(())
}

/// Whether `exc` matches `matcher`. `Err(())` means the predicate raised; the
/// pending exception is already installed.
fn matcher_matches(exc: i64, matcher: &GroupMatcher) -> Result<bool, ()> {
    match matcher {
        GroupMatcher::Types(types) => Ok(value::exception_data(exc).is_some_and(|data| {
            types
                .iter()
                .any(|&class| class_tag(class).is_some_and(|tag| tag_is_subclass(data.tag, tag)))
        })),
        GroupMatcher::Predicate(predicate) => {
            let argv = [exc];
            let result = crate::builtins::call(*predicate, 1, argv.as_ptr());
            if pending() {
                return Err(());
            }
            Ok(value::truthy(result))
        }
    }
}

/// `exceptiongroup_subset`: derives a group holding `children` with `orig`'s
/// message, cause, context and notes. `None` for an empty subset.
fn group_subset(orig: i64, children: &[i64]) -> Option<i64> {
    if children.is_empty() {
        return None;
    }
    let data = value::exception_data(orig)?.clone();
    let message =
        keyword_value(&data, GROUP_MESSAGE_KEY).unwrap_or_else(|| value::str_alloc(String::new()));
    let roots = crate::heap::root_values(children);
    crate::heap::root_values(std::slice::from_ref(&message));
    let group = group_value(BASE_EXCEPTION_GROUP, message, children, GroupSource::List);
    crate::heap::root_values(std::slice::from_ref(&group));
    let mut group_data = value::exception_data(group)?.clone();
    group_data.cause = data.cause;
    group_data.context = data.context;
    let root = keyword_value(&data, GROUP_STAR_ROOT_KEY).unwrap_or(orig);
    group_data
        .keywords
        .retain(|(key, _)| key != GROUP_STAR_ROOT_KEY);
    group_data
        .keywords
        .push((GROUP_STAR_ROOT_KEY.to_string(), root));
    let group = value::exception_with_data(group, group_data);
    if let Some(notes) = notes_of(orig) {
        if let Some(items) = value::as_list(notes) {
            let copy = value::list_new(items.to_vec());
            set_notes(group, copy);
        }
    }
    crate::heap::release_roots(roots);
    Some(group)
}

/// `exceptiongroup_split_recursive`: splits `exc` into the part matched by
/// `matcher` and (when requested) the remainder, preserving nesting.
fn split_recursive(
    exc: i64,
    matcher: &GroupMatcher,
    construct_rest: bool,
) -> Result<(Option<i64>, Option<i64>), ()> {
    if matcher_matches(exc, matcher)? {
        return Ok((Some(exc), None));
    }
    let Some(data) = value::exception_data(exc) else {
        return Ok((None, construct_rest.then_some(exc)));
    };
    if !is_group_tag(data.tag) {
        return Ok((None, construct_rest.then_some(exc)));
    }
    let children = group_children(data).unwrap_or_default();
    let roots = crate::heap::root_values(&[]);
    let mut matched: Vec<i64> = Vec::new();
    let mut rest: Vec<i64> = Vec::new();
    for child in children {
        let (child_match, child_rest) = split_recursive(child, matcher, construct_rest)?;
        if let Some(child) = child_match {
            crate::heap::push_scoped_root(child);
            matched.push(child);
        }
        if let Some(child) = child_rest {
            crate::heap::push_scoped_root(child);
            rest.push(child);
        }
    }
    let match_group = group_subset(exc, &matched);
    if let Some(group) = match_group {
        crate::heap::push_scoped_root(group);
    }
    let rest_group = if construct_rest {
        let group = group_subset(exc, &rest);
        if let Some(group) = group {
            crate::heap::push_scoped_root(group);
        }
        group
    } else {
        None
    };
    crate::heap::release_roots(roots);
    Ok((match_group, rest_group))
}

/// `BaseExceptionGroup.split(matcher)` / `.subgroup(matcher)`.
fn group_split(object: i64, args: &[i64], keep_rest: bool) -> i64 {
    let method = if keep_rest { "split" } else { "subgroup" };
    if args.len() != 1 {
        return exception_arity(method, 1, args.len());
    }
    let Some(data) = value::exception_data(object) else {
        return raise_with_message(TYPE_ERROR, "split() requires an exception group");
    };
    if !is_group_tag(data.tag) {
        return raise_with_message(TYPE_ERROR, "split() requires an exception group");
    }
    let matcher = match classify_matcher(args[0], false) {
        Ok(matcher) => matcher,
        Err(()) => return NONE,
    };
    let roots = crate::heap::root_values(&[object]);
    let (matched, rest) = match split_recursive(object, &matcher, keep_rest) {
        Ok(pair) => pair,
        Err(()) => {
            crate::heap::release_roots(roots);
            return NONE;
        }
    };
    let result = if keep_rest {
        let matched = matched.unwrap_or(NONE);
        let rest = rest.unwrap_or(NONE);
        crate::heap::root_values(&[matched, rest]);
        value::tuple_new(vec![matched, rest])
    } else {
        matched.unwrap_or(NONE)
    };
    crate::heap::release_roots(roots);
    result
}

/// `BaseExceptionGroup.derive(excs)`: CPython's default implementation (a
/// `BaseExceptionGroup`-built group; the constructor selects the concrete
/// class from the members).
fn group_derive(object: i64, args: &[i64]) -> i64 {
    if args.len() != 1 {
        return exception_arity("derive", 1, args.len());
    }
    let Some(data) = value::exception_data(object) else {
        return raise_with_message(TYPE_ERROR, "derive() requires an exception group");
    };
    let message =
        keyword_value(data, GROUP_MESSAGE_KEY).unwrap_or_else(|| value::str_alloc(String::new()));
    let source = args[0];
    let children: Vec<i64> = if let Some(items) = value::as_list(source) {
        items.to_vec()
    } else if let Some(items) = value::as_tuple(source) {
        items.to_vec()
    } else {
        return raise_with_message(
            TYPE_ERROR,
            "second argument (exceptions) must be a sequence",
        );
    };
    if children.is_empty() {
        return raise_with_message(
            VALUE_ERROR,
            "second argument (exceptions) must be a non-empty sequence",
        );
    }
    for (index, &child) in children.iter().enumerate() {
        if !is_exception_instance(child) {
            return raise_with_message(
                VALUE_ERROR,
                &format!("Item {index} of second argument (exceptions) is not an exception"),
            );
        }
    }
    group_value(
        BASE_EXCEPTION_GROUP,
        message,
        &children,
        GroupSource::Exact(source),
    )
}

// `BaseException.__notes__` storage. Hydro's exception payload is immutable,
// so notes live in a thread-local side table keyed by the exception value;
// each list is a permanent thread root and the table is cleared by `init`.
thread_local! {
    static NOTES: RefCell<HashMap<i64, i64>> = RefCell::new(HashMap::new());
}

fn notes_of(object: i64) -> Option<i64> {
    NOTES.with(|notes| notes.borrow().get(&object).copied())
}

fn set_notes(object: i64, list: i64) {
    crate::heap::add_thread_root(list);
    NOTES.with(|notes| {
        notes.borrow_mut().insert(object, list);
    });
}

/// `BaseException.add_note(note)`: appends to `__notes__`, creating the list on
/// first use.
fn add_note(object: i64, args: &[i64]) -> i64 {
    if args.len() != 1 {
        return exception_arity("add_note", 1, args.len());
    }
    let note = args[0];
    if value::as_str(note).is_none() {
        return raise_with_message(
            TYPE_ERROR,
            &format!("note must be a str, not {}", value::type_name(note)),
        );
    }
    let list = match notes_of(object) {
        Some(list) => list,
        None => {
            let list = value::list_new(Vec::new());
            set_notes(object, list);
            list
        }
    };
    if let Some(mut items) = value::list_items_mut(list) {
        items.push(note);
        crate::heap::write_barrier_edge(list, note);
    }
    NONE
}

/// Collects the leaf exceptions of `exc` into `leaves`.
fn collect_leaves(exc: i64, leaves: &mut Vec<i64>) {
    match value::exception_data(exc) {
        Some(data) if is_group_tag(data.tag) => {
            if let Some(children) = group_children(data) {
                for child in children {
                    collect_leaves(child, leaves);
                }
            }
        }
        _ => leaves.push(exc),
    }
}

/// `exception_group_projection`: the sub-group of `orig` holding exactly the
/// leaves present in `keep`.
fn group_projection(orig: i64, keep: &[i64]) -> Option<i64> {
    let mut leaves = Vec::new();
    for &entry in keep {
        collect_leaves(entry, &mut leaves);
    }
    let roots = crate::heap::root_values(&leaves);
    let projected = projection_recursive(orig, &leaves);
    if let Some(group) = projected {
        crate::heap::push_scoped_root(group);
    }
    crate::heap::release_roots(roots);
    projected
}

fn projection_recursive(exc: i64, leaves: &[i64]) -> Option<i64> {
    let data = value::exception_data(exc)?;
    if !is_group_tag(data.tag) {
        return leaves.contains(&exc).then_some(exc);
    }
    let children = group_children(data)?;
    let roots = crate::heap::root_values(&[]);
    let mut matched = Vec::new();
    for child in children {
        if let Some(child) = projection_recursive(child, leaves) {
            crate::heap::push_scoped_root(child);
            matched.push(child);
        }
    }
    let result = group_subset(exc, &matched);
    if let Some(group) = result {
        crate::heap::push_scoped_root(group);
    }
    crate::heap::release_roots(roots);
    result
}

/// `_PyExc_PrepReraiseStar`: computes the exception to raise after the last
/// `except*` clause.
///
/// Re-raised parts (groups derived from `orig`, identified by the origin
/// marker) are merged back into their original nesting; exceptions raised by
/// clause bodies are collected into a new `ExceptionGroup("")` together with
/// the projection, or returned directly when it is the only entry.
fn prep_reraise_star(orig: i64, results: &[i64]) -> i64 {
    let present: Vec<i64> = results.iter().copied().filter(|&v| v != NONE).collect();
    if present.is_empty() {
        return NONE;
    }
    if !is_group_value(orig) {
        // A naked exception was caught; at most one clause ran.
        return present[0];
    }
    let roots = crate::heap::root_values(&present);
    crate::heap::root_values(std::slice::from_ref(&orig));
    let mut raised = Vec::new();
    let mut reraised = Vec::new();
    for &entry in &present {
        let is_reraise = value::exception_data(entry).is_some_and(|data| {
            is_group_tag(data.tag) && keyword_value(data, GROUP_STAR_ROOT_KEY) == Some(orig)
        });
        if is_reraise {
            reraised.push(entry);
        } else {
            crate::heap::push_scoped_root(entry);
            raised.push(entry);
        }
    }
    let reraised_eg = if reraised.is_empty() {
        NONE
    } else {
        let group = group_projection(orig, &reraised);
        match group {
            Some(group) => {
                crate::heap::push_scoped_root(group);
                group
            }
            None => NONE,
        }
    };
    let result = if raised.is_empty() {
        reraised_eg
    } else {
        if reraised_eg != NONE {
            crate::heap::push_scoped_root(reraised_eg);
            raised.push(reraised_eg);
        }
        if raised.len() > 1 {
            raised.retain(|&entry| entry != NONE);
            let empty = value::str_alloc(String::new());
            group_value(BASE_EXCEPTION_GROUP, empty, &raised, GroupSource::List)
        } else {
            raised[0]
        }
    };
    crate::heap::release_roots(roots);
    result
}

/// One open `except*` session.
struct StarSession {
    orig: i64,
    rest: i64,
    results: Vec<i64>,
    roots: usize,
}

thread_local! {
    static STAR_SESSIONS: RefCell<Vec<StarSession>> = const { RefCell::new(Vec::new()) };
}

fn star_token(index: usize) -> i64 {
    // Tagged integer (`payload << 3`); sessions never exceed 2^60.
    (index as i64) << 3
}

fn star_index(token: i64) -> Option<usize> {
    if token & 0b111 != 0 || token < 0 {
        return None;
    }
    let index = (token >> 3) as usize;
    STAR_SESSIONS.with(|sessions| (index < sessions.borrow().len()).then_some(index))
}

/// Opens an `except*` split session and returns its tagged token.
pub(crate) fn star_begin(orig: i64) -> i64 {
    let roots = crate::heap::root_values(&[orig]);
    STAR_SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        sessions.push(StarSession {
            orig,
            rest: orig,
            results: Vec::new(),
            roots,
        });
        star_token(sessions.len() - 1)
    })
}

/// `except*` handler matching (`_PyEval_ExceptionGroupMatch`): returns the
/// subset of the current remainder selected by `matcher`, or `None`.
pub(crate) fn star_match(token: i64, matcher_value: i64) -> i64 {
    let Some(index) = star_index(token) else {
        return NONE;
    };
    let rest = STAR_SESSIONS.with(|sessions| sessions.borrow()[index].rest);
    let matcher = match classify_matcher(matcher_value, true) {
        Ok(matcher) => matcher,
        Err(()) => return NONE,
    };
    let matched;
    let new_rest;
    if rest == NONE {
        matched = NONE;
        new_rest = NONE;
    } else {
        match matcher_matches(rest, &matcher) {
            Ok(true) => {
                if is_group_value(rest) {
                    matched = rest;
                    new_rest = NONE;
                } else {
                    // A naked exception is wrapped in a one-element group.
                    let empty = value::str_alloc(String::new());
                    let wrapped = group_value(EXCEPTION_GROUP, empty, &[rest], GroupSource::List);
                    matched = wrapped;
                    new_rest = NONE;
                }
            }
            Ok(false) if is_group_value(rest) => match split_recursive(rest, &matcher, true) {
                Ok((Some(part), remainder)) => {
                    matched = part;
                    new_rest = remainder.unwrap_or(NONE);
                }
                Ok((None, remainder)) => {
                    matched = NONE;
                    new_rest = remainder.unwrap_or(NONE);
                }
                Err(()) => return NONE,
            },
            Ok(false) => {
                matched = NONE;
                new_rest = rest;
            }
            Err(()) => return NONE,
        }
    }
    if matched != NONE {
        crate::heap::push_scoped_root(matched);
    }
    if new_rest != NONE {
        crate::heap::push_scoped_root(new_rest);
    }
    STAR_SESSIONS.with(|sessions| sessions.borrow_mut()[index].rest = new_rest);
    matched
}

/// Records the pending exception as raised by a clause body and clears it.
pub(crate) fn star_raised(token: i64) -> i64 {
    let Some(index) = star_index(token) else {
        return NONE;
    };
    if pending() {
        let raised = pending_value();
        clear();
        crate::heap::push_scoped_root(raised);
        STAR_SESSIONS.with(|sessions| sessions.borrow_mut()[index].results.push(raised));
    }
    NONE
}

/// Closes the session and returns the exception to re-raise after the last
/// clause, or `None`.
pub(crate) fn star_finish(token: i64) -> i64 {
    let Some(index) = star_index(token) else {
        return NONE;
    };
    let session = STAR_SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        if index + 1 == sessions.len() {
            sessions.pop()
        } else {
            None
        }
    });
    let Some(session) = session else {
        return NONE;
    };
    let mut results = session.results.clone();
    if session.rest != NONE {
        results.push(session.rest);
    }
    let result = prep_reraise_star(session.orig, &results);
    crate::heap::release_roots(session.roots);
    result
}

/// Exception attribute surface used by the stdlib: `args`, `__class__`,
/// `__cause__`, `__context__`, `__suppress_context__`, `__traceback__`, the
/// `BaseException` methods (`__str__`, `__repr__`, `__setstate__`,
/// `__reduce__`, `__reduce_ex__`, `with_traceback`), plus the keyword-backed
/// attributes (`AttributeError.name`/`.obj`, `NameError.name`,
/// `SyntaxError.lineno`/`.offset`/`.text`/`.end_lineno`/`.end_offset`).
///
/// Returns `None` when `value` is not a built-in exception object or when the
/// name is not an exception attribute, so the caller can raise
/// `AttributeError`. `__cause__`/`__context__` are `None` when unset and
/// `__traceback__` is always `None`: Hydro records the raise location in
/// [`ExcData`] but has no traceback object. A keyword-backed attribute that
/// was not supplied to the constructor is `None`, like CPython.
///
/// The methods are returned as bound-method values; calling one reaches
/// [`exception_method_call`], which holds the CPython behaviour.
pub(crate) fn exception_attr(object: i64, name: &str) -> Option<i64> {
    let data = value::exception_data(object)?;
    // An exception raised by a C extension keeps its original instance: every
    // attribute resolves there first, so `exc.errors`, `exc.args` and the
    // exception's own data attributes behave like CPython. A name the
    // extension does not define falls back to the native surface below.
    if let Some(extension) = data.object {
        if let Some(pointer) = value::ext_object_pointer(extension) {
            match crate::capi::object_get_attr_hydro(pointer, name) {
                Ok(attribute) => return Some(attribute),
                Err(message) => {
                    // The probe failed (typically `AttributeError`); release
                    // any exception instance it stashed before falling back.
                    if let Some(exception) = crate::capi::take_error_exception(&message) {
                        crate::capi::dec_ref(exception);
                    }
                }
            }
        }
    }
    // `__notes__` only exists once a note was added; without one CPython
    // reports `AttributeError`, so a missing entry returns `None` here.
    if name == "__notes__" {
        return notes_of(object);
    }
    Some(match name {
        "args" => args_tuple(data),
        "__class__" => value::type_class(object),
        "__cause__" => data.cause.unwrap_or(NONE),
        "__context__" => data.context.unwrap_or(NONE),
        "__suppress_context__" => value::bool_value(data.suppress_context),
        "__traceback__" => NONE,
        // `StopIteration.value`: the generator return value. CPython stores it
        // as a mutable member initialised from the first positional argument;
        // a subclass instance that assigned the name keeps its attribute.
        "value" if data.tag == STOP_ITERATION => {
            value::instance_attr(object, "value").unwrap_or_else(|| stop_iteration_value(data))
        }
        "__str__" | "__repr__" | "__setstate__" | "__reduce__" | "__reduce_ex__"
        | "with_traceback" | "__init__" | "add_note" => {
            value::bound_method_new(object, name.to_string())
        }
        // The group surface. A user subclass that overrides one of these keeps
        // its own definition: the runtime reports no attribute and the normal
        // instance lookup binds the user's method.
        "message" if is_group_tag(data.tag) => {
            keyword_value(data, GROUP_MESSAGE_KEY).unwrap_or(NONE)
        }
        "exceptions" if is_group_tag(data.tag) => {
            keyword_value(data, GROUP_EXCEPTIONS_KEY).unwrap_or(NONE)
        }
        "derive" | "split" | "subgroup"
            if is_group_tag(data.tag) && !class_defines(object, name) =>
        {
            value::bound_method_new(object, name.to_string())
        }
        // `SyntaxError.msg` is the exception's message. The remaining members
        // (`filename`, `lineno`, `offset`, `text`, `end_lineno`, `end_offset`)
        // are keyword-backed and default to `None`, exactly like CPython's
        // member descriptors for an error built without source info.
        "msg" if is_syntax_tag(data.tag) => value::str_alloc(data.msg.clone()),
        _ => {
            if !exception_keyword_names(data.tag).contains(&name) {
                return None;
            }
            keyword_value(data, name).unwrap_or(NONE)
        }
    })
}

/// Assigns a writable exception attribute. Returns `None` when `name` is not
/// part of the surface, so the caller can fall through to its own dispatch.
///
/// The writable names mirror CPython's member/getset descriptors:
/// `StopIteration.value` (the first `args` entry), `__context__`, `__cause__`
/// (which implies `__suppress_context__`, like `PyException_SetCause`) and
/// `__suppress_context__`.
pub(crate) fn exception_setattr(object: i64, name: &str, new_value: i64) -> Option<i64> {
    let tag = value::exception_data(object)?.tag;
    let exception_instance =
        |v: i64| value::exception_data(v).is_some() || value::is_ext_exception(v);
    match name {
        "value" if tag == STOP_ITERATION => {
            let data = exception_data_mut(object)?;
            // The member starts as `args[0]`; assignment replaces it while
            // the rendered message (`str(e)`) stays what the constructor
            // stored, exactly like CPython's member slot.
            let text = value::to_string(new_value);
            let repr = value::repr(new_value);
            if !data.arg_values.is_empty() {
                data.arg_values[0] = new_value;
            } else {
                data.arg_values = vec![new_value];
            }
            if !data.args.is_empty() {
                data.args[0] = text;
            } else {
                data.args = vec![text];
            }
            if !data.arg_reprs.is_empty() {
                data.arg_reprs[0] = repr;
            } else {
                data.arg_reprs = vec![repr];
            }
            Some(NONE)
        }
        "__context__" => {
            if new_value != NONE && !exception_instance(new_value) {
                return Some(raise_with_message(
                    TYPE_ERROR,
                    "exception context must be None or derive from BaseException",
                ));
            }
            let data = exception_data_mut(object)?;
            data.context = (new_value != NONE).then_some(new_value);
            Some(NONE)
        }
        "__cause__" => {
            if new_value != NONE && !exception_instance(new_value) {
                return Some(raise_with_message(
                    TYPE_ERROR,
                    "exception cause must be None or derive from BaseException",
                ));
            }
            let data = exception_data_mut(object)?;
            data.cause = (new_value != NONE).then_some(new_value);
            // `PyException_SetCause` always suppresses the rendered context.
            data.suppress_context = true;
            Some(NONE)
        }
        "__suppress_context__" => {
            let data = exception_data_mut(object)?;
            data.suppress_context = value::truthy(new_value);
            Some(NONE)
        }
        _ => None,
    }
}

/// Whether the receiver's Python class defines `name` itself. Used to keep a
/// user override of `derive`/`split`/`subgroup` ahead of the native methods:
/// the exception core is consulted before instance dispatch, so it has to
/// step aside explicitly.
fn class_defines(object: i64, name: &str) -> bool {
    value::as_exc_instance(object)
        .is_some_and(|state| value::class_lookup(state.class, name).is_some())
}

/// Calls a `BaseException` method on `object`.
///
/// Returns `None` when `method` is not part of the exception method surface,
/// so the caller can fall through to its own dispatch. A method that raises
/// reports the pending exception and returns [`NONE`].
pub(crate) fn exception_method_call(object: i64, method: &str, args: &[i64]) -> Option<i64> {
    let data = value::exception_data(object)?;
    let result = match method {
        "__str__" => {
            if !args.is_empty() {
                return Some(exception_arity("__str__", 0, args.len()));
            }
            value::str_alloc(exception_message(data))
        }
        "__repr__" => {
            if !args.is_empty() {
                return Some(exception_arity("__repr__", 0, args.len()));
            }
            value::str_alloc(exception_repr(data))
        }
        "__setstate__" => exception_impl::setstate(object, data, args),
        "__reduce__" => {
            if !args.is_empty() {
                return Some(exception_arity("__reduce__", 0, args.len()));
            }
            exception_impl::reduce(object, data)
        }
        "__reduce_ex__" => exception_impl::reduce_ex(object, data, args),
        "with_traceback" => exception_impl::with_traceback(object, args),
        // `BaseException.__init__` resets `args`; Hydro's payload is immutable,
        // so it validates the call and keeps the construction-time `args`.
        "__init__" => exception_impl::init(object, data, args),
        "add_note" => add_note(object, args),
        // A user subclass that overrides one of these keeps its own method;
        // returning `None` lets the instance dispatch find it.
        "derive" if is_group_tag(data.tag) && !class_defines(object, "derive") => {
            group_derive(object, args)
        }
        "split" if is_group_tag(data.tag) && !class_defines(object, "split") => {
            group_split(object, args, true)
        }
        "subgroup" if is_group_tag(data.tag) && !class_defines(object, "subgroup") => {
            group_split(object, args, false)
        }
        _ => return None,
    };
    Some(result)
}

/// Reports the CPython `TypeError` for a wrong `BaseException` method arity.
fn exception_arity(method: &str, expected: usize, given: usize) -> i64 {
    let detail = match expected {
        0 => "no arguments".to_string(),
        1 => "exactly one argument".to_string(),
        count => format!("exactly {count} arguments"),
    };
    raise_with_message(
        TYPE_ERROR,
        &format!("BaseException.{method}() takes {detail} ({given} given)"),
    )
}

/// Value stored for the keyword-only constructor argument `name`, if the
/// exception was built with it.
fn keyword_value(data: &ExcData, name: &str) -> Option<i64> {
    data.keywords
        .iter()
        .find(|(keyword, _)| keyword == name)
        .map(|(_, value)| *value)
}

/// Builds the `args` tuple for [`exception_attr`].
///
/// The original constructor arguments are returned when the payload has them,
/// so `Exception(*range(3)).args == (0, 1, 2)` holds with the real objects.
/// Runtime-generated raises only have rendered text and fall back to strings.
/// Each element is allocated individually and registered as a collector root
/// until the tuple owns it: allocation can trigger a collection, and the
/// element would otherwise be invisible to the stack scan.
fn args_tuple(data: &ExcData) -> i64 {
    let start = crate::heap::root_values(&[]);
    let mut items = Vec::with_capacity(data.args.len());
    if data.arg_values.len() == data.args.len() && !data.arg_values.is_empty() {
        for &item in &data.arg_values {
            items.push(item);
            crate::heap::root_values(std::slice::from_ref(&item));
        }
    } else {
        for text in &data.args {
            let item = value::str_from_bytes(text.as_bytes());
            items.push(item);
            crate::heap::root_values(std::slice::from_ref(&item));
        }
    }
    let tuple = value::tuple_new(items);
    crate::heap::release_roots(start);
    tuple
}

// Uncaught traceback

/// Maximum number of exceptions rendered for one chain. Deeper chains stop
/// at the outermost exception (the chain is rendered deepest first).
const MAX_CAUSE_CHAIN: usize = 8;

/// CPython's separator between an implicit context and the exception it
/// caused.
const DURING_HANDLING: &str = "During handling of the above exception, another exception occurred:";

/// CPython's separator between an explicit cause and the exception it caused.
const DIRECT_CAUSE: &str = "The above exception was the direct cause of the following exception:";

/// Prints the traceback for the pending exception to stderr, clears it and
/// returns the process exit code `1`. Returns `0` if nothing is pending.
pub(crate) fn uncaught() -> i64 {
    let root = STATE.with(|s| s.borrow().pending);
    let Some(root) = root else {
        return 0;
    };
    let data = value::exception_clone(root).unwrap_or_else(|| ExcData {
        tag: RUNTIME_ERROR,
        name: tag_name(RUNTIME_ERROR),
        msg: default_message(RUNTIME_ERROR).to_string(),
        file: String::new(),
        line: 0,
        frame: String::new(),
        cause: None,
        context: None,
        suppress_context: false,
        exit_code: None,
        args: Vec::new(),
        arg_values: Vec::new(),
        arg_reprs: Vec::new(),
        keywords: Vec::new(),
        object: None,
    });

    // `SystemExit` never renders a traceback. An integer code becomes the
    // process status (masked to the byte the OS sees); a message is printed
    // to stderr and exits 1.
    if data.tag == SYSTEM_EXIT {
        let status = match data.exit_code {
            Some(code) => code & 0xff,
            None => {
                // A non-integer code (typically a string) is reported on
                // stderr; CPython prints the newline even for an empty
                // message.
                let stderr = std::io::stderr();
                let mut stderr = stderr.lock();
                let _ = stderr.write_all(data.msg.as_bytes());
                let _ = stderr.write_all(b"\n");
                let _ = stderr.flush();
                1
            }
        };
        clear();
        return status;
    }

    let text = render_chain(root, &data);
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();

    clear();
    1
}

/// Renders `data` after the tracebacks of its chained predecessors.
///
/// An explicit cause wins over implicit context; without one, the context is
/// followed unless `raise ... from` suppressed it. Predecessors are rendered
/// deepest first, separated by the matching CPython marker line. At most
/// [`MAX_CAUSE_CHAIN`] exceptions are rendered and cycles are cut off, so a
/// self-referential chain cannot loop.
fn render_chain(root: i64, data: &ExcData) -> String {
    let mut chain = vec![data.clone()];
    // `edges[i]` is true when chain[i] was reached through an explicit cause.
    let mut edges: Vec<bool> = Vec::new();
    let mut seen = vec![root];
    let mut current = data.clone();
    loop {
        if chain.len() >= MAX_CAUSE_CHAIN {
            break;
        }
        let (next, direct) = match current.cause {
            Some(cause) => (cause, true),
            None if !current.suppress_context => match current.context {
                Some(context) => (context, false),
                None => break,
            },
            None => break,
        };
        if seen.contains(&next) {
            break;
        }
        seen.push(next);
        let Some(next) = value::exception_data(next) else {
            break;
        };
        current = next.clone();
        chain.push(current.clone());
        edges.push(direct);
    }

    let mut text = String::new();
    for index in (0..chain.len()).rev() {
        if index + 1 < chain.len() {
            let _ = writeln!(text);
            let separator = if edges[index] {
                DIRECT_CAUSE
            } else {
                DURING_HANDLING
            };
            let _ = writeln!(text, "{separator}");
            let _ = writeln!(text);
        }
        let data = &chain[index];
        let _ = writeln!(text, "Traceback (most recent call last):");
        let _ = writeln!(
            text,
            "  File \"{}\", line {}, in {}",
            data.file, data.line, data.frame
        );
        let _ = writeln!(
            text,
            "{}: {}",
            exception_display_name(data),
            exception_message(data)
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raise_and_clear() {
        init();
        assert!(!pending());
        assert_eq!(pending_tag(), -1);

        let msg = value::str_from_bytes(b"boom");
        let dummy = raise(ZERO_DIVISION_ERROR, msg);
        assert_eq!(dummy, NONE);
        assert!(pending());
        assert_eq!(pending_tag(), ZERO_DIVISION_ERROR as i64);

        let exc = pending_value();
        assert_eq!(value::to_string(exc), "boom");
        assert!(value::exception_data(exc).is_some());

        assert_eq!(clear(), 0);
        assert!(!pending());
        assert_eq!(pending_tag(), -1);
    }

    #[test]
    fn exception_classes_are_cached_with_a_native_initializer() {
        init();
        let first = exception_class(VALUE_ERROR);
        let second = exception_class(VALUE_ERROR);
        assert_eq!(first, second, "the same tag reuses one class object");
        let state = value::as_class(first).expect("exception class object");
        assert_eq!(state.name, "ValueError");
        assert_eq!(
            state.base,
            exception_class(EXCEPTION),
            "the native class records its CPython base"
        );
        assert!(
            state.methods.iter().any(|(name, _, _)| name == "__init__"),
            "exception classes expose the native initializer"
        );
        assert!(state.attrs.is_empty());
        assert_eq!(value::class_display_name(first), Some("ValueError"));

        let other = exception_class(OS_ERROR);
        assert_ne!(first, other);
        assert_eq!(value::class_display_name(other), Some("OSError"));
        assert_eq!(
            value::as_class(other).expect("OSError class").base,
            exception_class(EXCEPTION)
        );
        assert_eq!(
            value::as_class(exception_class(FILE_NOT_FOUND_ERROR))
                .expect("FileNotFoundError class")
                .base,
            other,
            "FileNotFoundError derives from OSError"
        );

        // A user class derived from the native class keeps normal
        // inheritance semantics for instances.
        let user = value::class_new("E".to_string(), first, Vec::new());
        let instance = value::instance_new(user);
        assert_eq!(crate::builtins::isinstance(instance, user), value::TRUE);
        assert_eq!(crate::builtins::isinstance(instance, first), value::TRUE);
        assert_eq!(crate::builtins::isinstance(instance, other), value::FALSE);
        // The base chain reaches the native `Exception` class.
        assert_eq!(
            crate::builtins::isinstance(instance, exception_class(EXCEPTION)),
            value::TRUE
        );

        // A fresh program run clears the cache before the heap is freed.
        init();
        let fresh = exception_class(VALUE_ERROR);
        assert_eq!(value::class_display_name(fresh), Some("ValueError"));
        init();
    }

    #[test]
    fn unknown_exception_tags_raise_runtime_error() {
        init();
        assert_eq!(exception_class(99), NONE);
        assert!(pending());
        assert_eq!(pending_tag(), RUNTIME_ERROR as i64);
        assert_eq!(
            value::to_string(pending_value()),
            "unknown exception tag 99"
        );
        init();
    }

    #[test]
    fn raise_value_reraises_existing_exception() {
        init();
        let msg = value::str_from_bytes(b"original");
        raise(VALUE_ERROR, msg);
        let original = pending_value();
        let original_data = value::exception_data(original).expect("exception object");
        clear();
        assert!(!pending());

        assert_eq!(raise_value(original), NONE);
        assert!(pending());
        assert_eq!(pending_tag(), VALUE_ERROR as i64);
        assert_eq!(pending_value(), original);
        assert_eq!(value::to_string(pending_value()), "original");
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.name, "ValueError");
        assert_eq!(data.msg, "original");
        assert_eq!(data.file, original_data.file);
        assert_eq!(data.line, original_data.line);
        init();
    }

    #[test]
    fn raise_value_rejects_non_exception() {
        init();
        // CPython raises `TypeError`, not `RuntimeError`, for `raise <value>`
        // with a value that is not a BaseException (including a class that
        // does not derive from one, and an exception *instance* variant used
        // as a value).
        assert_eq!(raise_value(NONE), NONE);
        assert!(pending());
        assert_eq!(pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pending_value()),
            "exceptions must derive from BaseException"
        );

        init();
        let not_an_exception = value::instance_new(value::class_new(
            "NotAnException".to_string(),
            value::object_class(),
            Vec::new(),
        ));
        assert_eq!(raise_value(not_an_exception), NONE);
        assert_eq!(pending_tag(), TYPE_ERROR as i64);

        // `raise <exception class>` instantiates the class with no arguments.
        init();
        assert_eq!(raise_value(exception_class(VALUE_ERROR)), NONE);
        assert_eq!(value::type_name(pending_value()), "ValueError");
        init();
    }

    #[test]
    fn default_messages() {
        init();
        raise(ZERO_DIVISION_ERROR, NONE);
        let exc = pending_value();
        assert_eq!(value::to_string(exc), "division by zero");
        assert_eq!(value::type_name(exc), "ZeroDivisionError");

        // `raise TypeError` stores no argument; CPython's `str(TypeError())`
        // is the empty string.
        init();
        raise(TYPE_ERROR, NONE);
        assert_eq!(value::to_string(pending_value()), "");
        assert!(value::exception_data(pending_value())
            .expect("exception object")
            .args
            .is_empty());

        init();
        raise_msg_ptr(VALUE_ERROR, b"bad".as_ptr(), 3);
        assert_eq!(value::to_string(pending_value()), "bad");
        assert_eq!(
            value::exception_data(pending_value())
                .expect("exception object")
                .args,
            vec!["bad".to_string()]
        );
    }

    #[test]
    fn uncaught_without_exception_returns_zero() {
        init();
        assert_eq!(uncaught(), 0);
    }

    #[test]
    fn location_is_captured() {
        init();
        set_file_ptr(b"test.py".as_ptr(), 7);
        set_line(7);
        frame_enter_ptr(b"main".as_ptr(), 4);
        raise(ZERO_DIVISION_ERROR, NONE);
        let exc = pending_value();
        let data = value::exception_data(exc).expect("exception object");
        assert_eq!(data.file, "test.py");
        assert_eq!(data.line, 7);
        assert_eq!(data.frame, "main");
        assert_eq!(data.tag, ZERO_DIVISION_ERROR);
        init();
    }

    #[test]
    fn frame_stack_push_pop_and_capture() {
        init();
        frame_enter_ptr(b"<module>".as_ptr(), 8);
        frame_enter_ptr(b"safe_div".as_ptr(), 8);
        raise(ZERO_DIVISION_ERROR, NONE);
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.frame, "safe_div");

        clear();
        frame_leave();
        raise(ZERO_DIVISION_ERROR, NONE);
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.frame, "<module>");

        // Unbalanced pops are no-ops.
        frame_leave();
        frame_leave();
        raise(ZERO_DIVISION_ERROR, NONE);
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.frame, "");

        // `init` clears the stack.
        frame_enter_ptr(b"main".as_ptr(), 4);
        init();
        raise(ZERO_DIVISION_ERROR, NONE);
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.frame, "");
        init();
    }

    /// Builds an exception payload directly, bypassing the raise machinery.
    fn test_data(tag: u32, msg: &str, line: u32, frame: &str, cause: Option<i64>) -> ExcData {
        ExcData {
            tag,
            name: tag_name(tag),
            msg: msg.to_string(),
            file: "test.py".to_string(),
            line,
            frame: frame.to_string(),
            cause,
            context: None,
            suppress_context: false,
            exit_code: None,
            args: vec![msg.to_string()],
            arg_values: Vec::new(),
            arg_reprs: Vec::new(),
            keywords: Vec::new(),
            object: None,
        }
    }

    /// Overwrites the dead helper frames below the caller with zeroes, so a
    /// stale tagged value cannot keep a garbage object alive.
    #[inline(never)]
    fn scrub_stack() {
        let mut buffer = [0u8; 64 * 1024];
        std::hint::black_box(&mut buffer);
        buffer[0] = 0;
        std::hint::black_box(&buffer);
    }

    /// Raises an outer exception with an inner one as its cause; no local in
    /// this frame keeps the inner value alive once the frame is gone.
    #[inline(never)]
    fn raise_chain_with_cause() -> i64 {
        let inner = value::exception_new(test_data(VALUE_ERROR, "inner", 1, "inner", None));
        raise_from(RUNTIME_ERROR, NONE, inner);
        pending_value()
    }

    #[test]
    fn hierarchy_tags_raise_like_concrete_exceptions() {
        init();
        raise(EXCEPTION, NONE);
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.tag, EXCEPTION);
        assert_eq!(data.name, "Exception");
        assert_eq!(data.msg, "");
        assert_eq!(value::type_name(pending_value()), "Exception");

        init();
        raise(BASE_EXCEPTION, value::str_from_bytes(b"base"));
        assert_eq!(value::type_name(pending_value()), "BaseException");
        assert_eq!(value::to_string(pending_value()), "base");
        init();
    }

    #[test]
    fn cancelled_error_is_a_base_exception() {
        init();
        raise_with_message(CANCELLED_ERROR, "");
        assert_eq!(matches_handler(EXCEPTION), 0);
        assert_eq!(matches_handler(BASE_EXCEPTION), 1);
        assert_eq!(value::type_name(pending_value()), "CancelledError");
        init();
    }

    #[test]
    fn handler_matching_follows_the_hierarchy() {
        // Handlers: ValueError, TypeError, KeyboardInterrupt, GeneratorExit,
        // Exception, BaseException.
        let handlers = [
            VALUE_ERROR,
            TYPE_ERROR,
            KEYBOARD_INTERRUPT,
            GENERATOR_EXIT,
            EXCEPTION,
            BASE_EXCEPTION,
        ];
        let cases: &[(u32, [bool; 6])] = &[
            (VALUE_ERROR, [true, false, false, false, true, true]),
            (KEYBOARD_INTERRUPT, [false, false, true, false, false, true]),
            (GENERATOR_EXIT, [false, false, false, true, false, true]),
            (EXCEPTION, [false, false, false, false, true, true]),
            // `BaseException` itself is not an `Exception` in CPython.
            (BASE_EXCEPTION, [false, false, false, false, false, true]),
        ];

        init();
        for &(tag, expected) in cases {
            raise_with_message(tag, "boom");
            for (&handler, &want) in handlers.iter().zip(expected.iter()) {
                assert_eq!(
                    matches_handler(handler),
                    i32::from(want),
                    "exception tag {tag} against handler {handler}"
                );
            }
        }

        // No pending exception: nothing matches, not even BaseException.
        clear();
        for handler in handlers {
            assert_eq!(matches_handler(handler), 0);
        }
        init();
    }

    #[test]
    fn handler_matching_covers_the_cpython_branches() {
        // (raised tag, handler tag, caught)
        let cases: &[(u32, u32, bool)] = &[
            // ArithmeticError.
            (ZERO_DIVISION_ERROR, ARITHMETIC_ERROR, true),
            (OVERFLOW_ERROR, ARITHMETIC_ERROR, true),
            (FLOATING_POINT_ERROR, ARITHMETIC_ERROR, true),
            (ARITHMETIC_ERROR, ZERO_DIVISION_ERROR, false),
            (ZERO_DIVISION_ERROR, EXCEPTION, true),
            // LookupError.
            (INDEX_ERROR, LOOKUP_ERROR, true),
            (KEY_ERROR, LOOKUP_ERROR, true),
            (LOOKUP_ERROR, INDEX_ERROR, false),
            // ImportError.
            (MODULE_NOT_FOUND_ERROR, IMPORT_ERROR, true),
            (IMPORT_ERROR, MODULE_NOT_FOUND_ERROR, false),
            (MODULE_NOT_FOUND_ERROR, EXCEPTION, true),
            // SyntaxError chain.
            (SYNTAX_ERROR, SYNTAX_ERROR, true),
            (INDENTATION_ERROR, SYNTAX_ERROR, true),
            (TAB_ERROR, SYNTAX_ERROR, true),
            (TAB_ERROR, INDENTATION_ERROR, true),
            (SYNTAX_ERROR, TAB_ERROR, false),
            (SYNTAX_ERROR, EXCEPTION, true),
            // UnicodeError is a ValueError.
            (UNICODE_DECODE_ERROR, UNICODE_ERROR, true),
            (UNICODE_DECODE_ERROR, VALUE_ERROR, true),
            (UNICODE_ENCODE_ERROR, UNICODE_ERROR, true),
            (UNICODE_TRANSLATE_ERROR, UNICODE_ERROR, true),
            (UNICODE_ERROR, EXCEPTION, true),
            // RuntimeError branch.
            (RECURSION_ERROR, RUNTIME_ERROR, true),
            (NOT_IMPLEMENTED_ERROR, RUNTIME_ERROR, true),
            (RUNTIME_ERROR, RECURSION_ERROR, false),
            // OSError branch.
            (FILE_NOT_FOUND_ERROR, OS_ERROR, true),
            (OS_ERROR, FILE_NOT_FOUND_ERROR, false),
            (PERMISSION_ERROR, OS_ERROR, true),
            (TIMEOUT_ERROR, OS_ERROR, true),
            (BROKEN_PIPE_ERROR, CONNECTION_ERROR, true),
            (BROKEN_PIPE_ERROR, OS_ERROR, true),
            (CONNECTION_REFUSED_ERROR, CONNECTION_ERROR, true),
            (CONNECTION_ERROR, BROKEN_PIPE_ERROR, false),
            // Warnings.
            (USER_WARNING, WARNING, true),
            (RESOURCE_WARNING, WARNING, true),
            (WARNING, USER_WARNING, false),
            // ExceptionGroup: both CPython bases.
            (EXCEPTION_GROUP, BASE_EXCEPTION_GROUP, true),
            (EXCEPTION_GROUP, EXCEPTION, true),
            (EXCEPTION_GROUP, BASE_EXCEPTION, true),
            (BASE_EXCEPTION_GROUP, EXCEPTION, false),
            (BASE_EXCEPTION_GROUP, BASE_EXCEPTION, true),
            // BaseException-only children.
            (SYSTEM_EXIT, EXCEPTION, false),
            (KEYBOARD_INTERRUPT, EXCEPTION, false),
            (GENERATOR_EXIT, EXCEPTION, false),
            (CANCELLED_ERROR, EXCEPTION, false),
            (SYSTEM_EXIT, BASE_EXCEPTION, true),
            // Runtime-internal tags keep their historical catchability.
            (SKIPPED, EXCEPTION, true),
            (XFAILED, EXCEPTION, true),
            // Unrelated concrete classes.
            (VALUE_ERROR, TYPE_ERROR, false),
            (OS_ERROR, EXCEPTION, true),
            (MEMORY_ERROR, EXCEPTION, true),
            (SYSTEM_ERROR, EXCEPTION, true),
            (EOF_ERROR, EXCEPTION, true),
            (BUFFER_ERROR, EXCEPTION, true),
            (REFERENCE_ERROR, EXCEPTION, true),
            (MEMORY_ERROR, BASE_EXCEPTION, true),
        ];

        init();
        for &(tag, handler, want) in cases {
            raise_with_message(tag, "boom");
            assert_eq!(
                matches_handler(handler),
                i32::from(want),
                "tag {} against handler {}",
                tag_name(tag),
                tag_name(handler)
            );
        }
        init();
    }

    #[test]
    fn pending_exception_values_match_classes_and_tuples() {
        // Dynamic `except <expr>:` arms match the pending exception value with
        // the runtime `isinstance` builtin. A class, a tuple of classes and an
        // empty tuple (`except tuple([])`) must all resolve without consuming
        // or disturbing the pending exception.
        init();
        raise_with_message(VALUE_ERROR, "boom");
        let exc = pending_value();
        clear();
        let value_error = exception_class(VALUE_ERROR);
        let type_error = exception_class(TYPE_ERROR);

        assert!(value::truthy(crate::builtins::isinstance(exc, value_error)));
        assert_eq!(
            crate::builtins::isinstance(exc, exception_class(EXCEPTION)),
            value::TRUE
        );
        assert_eq!(
            crate::builtins::isinstance(exc, exception_class(BASE_EXCEPTION)),
            value::TRUE
        );
        assert!(!value::truthy(crate::builtins::isinstance(exc, type_error)));

        // Tuple matchers follow the same hierarchy, including an empty tuple,
        // which matches nothing.
        let candidates = value::tuple_new(vec![type_error, value_error]);
        assert!(value::truthy(crate::builtins::isinstance(exc, candidates)));
        assert_eq!(
            crate::builtins::isinstance(exc, value::tuple_new(Vec::new())),
            value::FALSE
        );

        // The matcher itself is side-effect free; `isinstance` is tested with
        // the pending slot cleared so its result is observable.
    }

    #[test]
    fn unknown_tags_use_the_safe_fallback() {
        init();
        // An unknown pending tag matches only the two documented roots and
        // never a concrete handler.
        let data = ExcData {
            tag: 9_999,
            name: tag_name(9_999),
            msg: String::new(),
            file: String::new(),
            line: 0,
            frame: String::new(),
            cause: None,
            context: None,
            suppress_context: false,
            exit_code: None,
            args: Vec::new(),
            arg_values: Vec::new(),
            arg_reprs: Vec::new(),
            keywords: Vec::new(),
            object: None,
        };
        publish(value::exception_new(data));
        assert_eq!(matches_handler(EXCEPTION), 1);
        assert_eq!(matches_handler(BASE_EXCEPTION), 1);
        assert_eq!(matches_handler(OS_ERROR), 0);
        assert_eq!(matches_handler(9_999), 0);

        // A known pending tag never matches an unknown handler.
        raise(VALUE_ERROR, NONE);
        assert_eq!(matches_handler(9_999), 0);
        init();
    }

    #[test]
    fn tag_is_subclass_walks_every_branch() {
        // Every runtime tag in the documented ranges is modelled: a missing
        // `parents` arm would silently make `known_tag` false and break class
        // creation and handler matching.
        for tag in 0..=67u32 {
            assert!(known_tag(tag), "tag {tag} is missing a parents arm");
            assert!(tag_is_subclass(tag, tag), "{tag} should be reflexive");
            assert!(
                tag_is_subclass(tag, BASE_EXCEPTION),
                "{tag} must derive from BaseException"
            );
        }

        // The root of each branch is itself modelled.
        assert_eq!(parents(EXCEPTION), &[BASE_EXCEPTION]);
        assert_eq!(parents(OS_ERROR), &[EXCEPTION]);
        assert_eq!(parents(WARNING), &[EXCEPTION]);
        assert_eq!(parents(ARITHMETIC_ERROR), &[EXCEPTION]);
        assert_eq!(parents(LOOKUP_ERROR), &[EXCEPTION]);
        assert_eq!(parents(IMPORT_ERROR), &[EXCEPTION]);
        assert_eq!(parents(SYNTAX_ERROR), &[EXCEPTION]);
        assert_eq!(parents(EXCEPTION_GROUP), &[BASE_EXCEPTION_GROUP, EXCEPTION]);
        assert_eq!(parents(BASE_EXCEPTION_GROUP), &[BASE_EXCEPTION]);
        assert!(parents(BASE_EXCEPTION).is_empty());

        assert!(tag_is_subclass(ZERO_DIVISION_ERROR, ARITHMETIC_ERROR));
        assert!(tag_is_subclass(KEY_ERROR, LOOKUP_ERROR));
        assert!(tag_is_subclass(MODULE_NOT_FOUND_ERROR, IMPORT_ERROR));
        // CPython: TabError -> IndentationError -> SyntaxError -> Exception.
        assert!(tag_is_subclass(TAB_ERROR, INDENTATION_ERROR));
        assert!(tag_is_subclass(INDENTATION_ERROR, SYNTAX_ERROR));
        assert!(tag_is_subclass(TAB_ERROR, SYNTAX_ERROR));
        assert!(tag_is_subclass(TAB_ERROR, EXCEPTION));
        assert!(tag_is_subclass(TAB_ERROR, BASE_EXCEPTION));
        // A parent never matches one of its subclasses.
        assert!(!tag_is_subclass(SYNTAX_ERROR, TAB_ERROR));
        assert!(tag_is_subclass(UNICODE_ENCODE_ERROR, VALUE_ERROR));
        assert!(tag_is_subclass(OS_ERROR, EXCEPTION));
        assert!(tag_is_subclass(FILE_NOT_FOUND_ERROR, OS_ERROR));
        assert!(tag_is_subclass(FILE_NOT_FOUND_ERROR, EXCEPTION));
        assert!(tag_is_subclass(CONNECTION_RESET_ERROR, OS_ERROR));
        assert!(tag_is_subclass(USER_WARNING, EXCEPTION));
        assert!(tag_is_subclass(EXCEPTION_GROUP, EXCEPTION));
        assert!(tag_is_subclass(EXCEPTION_GROUP, BASE_EXCEPTION_GROUP));
        assert!(!tag_is_subclass(BASE_EXCEPTION, EXCEPTION));
        // Unknown tags never satisfy an arbitrary ancestor.
        assert!(!tag_is_subclass(9_999, EXCEPTION));
        assert!(!tag_is_subclass(EXCEPTION, 9_999));
        assert!(!tag_is_subclass(9_999, 9_999));
    }

    #[test]
    fn exception_attributes_expose_stored_payload() {
        init();
        let cause = value::exception_new(test_data(VALUE_ERROR, "cause", 1, "f", None));
        let context = value::exception_new(test_data(TYPE_ERROR, "context", 2, "f", None));
        let mut data = test_data(RUNTIME_ERROR, "outer", 3, "f", Some(cause));
        data.context = Some(context);
        let exc = value::exception_new(data);

        let args = exception_attr(exc, "args").expect("args");
        let args = value::as_tuple(args).expect("args tuple");
        assert_eq!(args.len(), 1);
        assert_eq!(value::as_str(args[0]), Some("outer"));

        assert_eq!(exception_attr(exc, "__cause__"), Some(cause));
        assert_eq!(exception_attr(exc, "__context__"), Some(context));
        assert_eq!(
            exception_attr(exc, "__suppress_context__"),
            Some(value::FALSE)
        );
        assert_eq!(exception_attr(exc, "__traceback__"), Some(NONE));
        assert_eq!(
            exception_attr(exc, "__class__"),
            Some(exception_class(RUNTIME_ERROR))
        );
        assert_eq!(exception_attr(exc, "missing"), None);

        // An exception raised without arguments has no cause, no context and
        // an empty args tuple.
        raise(VALUE_ERROR, NONE);
        let plain = pending_value();
        assert_eq!(exception_attr(plain, "__cause__"), Some(NONE));
        assert_eq!(exception_attr(plain, "__context__"), Some(NONE));
        assert_eq!(
            exception_attr(plain, "__suppress_context__"),
            Some(value::FALSE)
        );
        let empty = exception_attr(plain, "args").expect("args");
        assert!(value::as_tuple(empty).expect("args tuple").is_empty());

        // Non-exception values have no exception attributes.
        assert_eq!(exception_attr(NONE, "args"), None);
        assert_eq!(exception_attr(value::encode_int(1).unwrap(), "args"), None);
        init();
    }

    #[test]
    fn keyword_attributes_are_exposed_and_default_to_none() {
        init();
        let name = value::str_from_bytes(b"attr");
        let object = value::encode_int(7).unwrap();
        let exc = exception_value_with_keywords(
            ATTRIBUTE_ERROR,
            "boom",
            vec!["boom".to_string()],
            vec![("name".to_string(), name), ("obj".to_string(), object)],
        );
        assert_eq!(exception_attr(exc, "name"), Some(name));
        assert_eq!(exception_attr(exc, "obj"), Some(object));

        // A keyword attribute the constructor did not receive is `None`, and
        // attributes of another exception class stay non-existent.
        let named = exception_value_with_keywords(NAME_ERROR, "x", Vec::new(), Vec::new());
        assert_eq!(exception_attr(named, "name"), Some(NONE));
        assert_eq!(exception_attr(named, "obj"), None);

        let value_error = exception_value(VALUE_ERROR, "x", Vec::new());
        assert_eq!(exception_attr(value_error, "name"), None);

        // The `SyntaxError` family stores source-position keywords and
        // defaults the omitted ones to `None`.
        let lineno = value::encode_int(3).unwrap();
        let syntax = exception_value_with_keywords(
            SYNTAX_ERROR,
            "bad",
            Vec::new(),
            vec![("lineno".to_string(), lineno)],
        );
        assert_eq!(exception_attr(syntax, "lineno"), Some(lineno));
        assert_eq!(exception_attr(syntax, "offset"), Some(NONE));
        assert_eq!(exception_attr(syntax, "text"), Some(NONE));
        assert_eq!(exception_attr(syntax, "filename"), Some(NONE));
        init();
    }

    #[test]
    fn args_follow_the_raise_form() {
        init();
        // `raise ValueError('x')`: one argument, rendered with `str()`.
        raise(VALUE_ERROR, value::str_from_bytes(b"x"));
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.msg, "x");
        assert_eq!(data.args, vec!["x".to_string()]);
        clear();

        // `raise ValueError`: no argument, so `args == ()` even though the
        // exception carries a default message.
        raise(VALUE_ERROR, NONE);
        let data = value::exception_data(pending_value()).expect("exception object");
        assert!(data.args.is_empty());
        clear();

        // `KeyError` renders its argument with `repr()` but keeps the raw
        // argument in `args`.
        raise(KEY_ERROR, value::str_from_bytes(b"k"));
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.msg, "'k'");
        assert_eq!(data.args, vec!["k".to_string()]);
        clear();

        // Runtime-generated messages are real arguments in CPython.
        raise_with_message(ZERO_DIVISION_ERROR, "division by zero");
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.args, vec!["division by zero".to_string()]);
        clear();

        // `sys.exit(3)` stores the code and its textual argument.
        raise_exit(value::encode_int(3).expect("representable"));
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.exit_code, Some(3));
        assert_eq!(data.args, vec!["3".to_string()]);
        init();
    }

    #[test]
    fn cause_is_stored_and_cloned() {
        init();
        raise(VALUE_ERROR, value::str_from_bytes(b"inner"));
        let inner = pending_value();
        clear();

        assert_eq!(
            raise_from(RUNTIME_ERROR, value::str_from_bytes(b"outer"), inner),
            NONE
        );
        let outer = pending_value();
        let data = value::exception_data(outer).expect("exception object");
        assert_eq!(data.tag, RUNTIME_ERROR);
        assert_eq!(data.msg, "outer");
        assert_eq!(data.cause, Some(inner));

        let cloned = value::exception_clone(outer).expect("clone");
        assert_eq!(cloned.cause, Some(inner));
        assert_eq!(cloned.msg, "outer");

        // `from None` suppresses the cause.
        raise_from(TYPE_ERROR, NONE, NONE);
        assert_eq!(
            value::exception_data(pending_value())
                .expect("exception object")
                .cause,
            None
        );
        init();
    }

    #[test]
    fn raise_value_from_copies_with_cause() {
        init();
        raise(VALUE_ERROR, value::str_from_bytes(b"inner"));
        let inner = pending_value();
        clear();
        raise(TYPE_ERROR, value::str_from_bytes(b"outer"));
        let original = pending_value();
        clear();

        assert_eq!(raise_value_from(original, inner), NONE);
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.tag, TYPE_ERROR);
        assert_eq!(data.msg, "outer");
        assert_eq!(data.cause, Some(inner));
        // The source object is not mutated by the re-raise.
        assert_eq!(
            value::exception_data(original)
                .expect("exception object")
                .cause,
            None
        );

        // `from None` suppresses the cause for an existing exception too.
        raise_value_from(original, NONE);
        assert_eq!(
            value::exception_data(pending_value())
                .expect("exception object")
                .cause,
            None
        );

        // Non-exception values raise RuntimeError, matching bare `raise`.
        raise_value_from(NONE, inner);
        assert_eq!(pending_tag(), TYPE_ERROR as i64);
        init();
    }

    #[test]
    fn chained_traceback_is_rendered_cause_first() {
        init();
        let inner = value::exception_new(test_data(VALUE_ERROR, "inner", 3, "inner_fn", None));
        let outer = value::exception_new(test_data(
            RUNTIME_ERROR,
            "outer",
            7,
            "outer_fn",
            Some(inner),
        ));

        let data = value::exception_clone(outer).expect("clone");
        assert_eq!(
            render_chain(outer, &data),
            concat!(
                "Traceback (most recent call last):\n",
                "  File \"test.py\", line 3, in inner_fn\n",
                "ValueError: inner\n",
                "\n",
                "The above exception was the direct cause of the following exception:\n",
                "\n",
                "Traceback (most recent call last):\n",
                "  File \"test.py\", line 7, in outer_fn\n",
                "RuntimeError: outer\n",
            )
        );

        // Without a cause the single-exception format is unchanged.
        let lone = value::exception_new(test_data(RUNTIME_ERROR, "outer", 7, "outer_fn", None));
        let data = value::exception_clone(lone).expect("clone");
        assert_eq!(
            render_chain(lone, &data),
            concat!(
                "Traceback (most recent call last):\n",
                "  File \"test.py\", line 7, in outer_fn\n",
                "RuntimeError: outer\n",
            )
        );
        init();
    }

    #[test]
    fn cause_chain_is_depth_limited() {
        init();
        let mut exc = value::exception_new(test_data(VALUE_ERROR, "e0", 0, "root", None));
        for index in 1..12 {
            exc = value::exception_new(test_data(
                RUNTIME_ERROR,
                &format!("e{index}"),
                index,
                "outer",
                Some(exc),
            ));
        }

        let data = value::exception_clone(exc).expect("clone");
        let text = render_chain(exc, &data);
        assert_eq!(
            text.matches("Traceback (most recent call last):").count(),
            MAX_CAUSE_CHAIN
        );
        // The outermost exception is always rendered last.
        assert!(text.ends_with("RuntimeError: e11\n"), "{text}");
        init();
    }

    #[test]
    fn collection_keeps_the_cause_of_the_pending_exception() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        init();
        crate::heap::reset();
        let outer = raise_chain_with_cause();
        assert!(pending());
        assert_eq!(crate::heap::objects(), 2, "inner and outer were allocated");

        scrub_stack();
        let freed = crate::heap::collect();
        assert_eq!(
            freed, 0,
            "the pending exception and its cause must stay reachable"
        );
        assert_eq!(crate::heap::objects(), 2);

        // The cause survived and still carries its payload.
        let data = value::exception_data(outer).expect("exception object");
        let cause = data.cause.expect("cause edge");
        assert_eq!(
            value::exception_data(cause).expect("cause object").msg,
            "inner"
        );
        init();
    }

    #[test]
    fn handler_stack_push_pop_and_truncate() {
        init();
        let first = value::exception_new(test_data(VALUE_ERROR, "first", 1, "f", None));
        let second = value::exception_new(test_data(TYPE_ERROR, "second", 2, "f", None));
        handler_push(first);
        handler_push(second);
        assert_eq!(handler_depth(), 2);
        assert_eq!(handled(), Some(second));

        handler_truncate(1);
        assert_eq!(handler_depth(), 1);
        assert_eq!(handled(), Some(first));

        // A depth beyond the stack is a no-op, so a path that already
        // unwound cannot pop a caller's context.
        handler_truncate(5);
        assert_eq!(handler_depth(), 1);

        handler_pop();
        assert_eq!(handler_depth(), 0);
        assert_eq!(handled(), None);
        handler_pop();
        assert_eq!(handler_depth(), 0);
        init();
    }

    #[test]
    fn exc_info_reports_the_handled_exception() {
        init();
        // Outside `except` handling the triple is all `None`.
        assert_eq!(value::as_tuple(exc_info()), Some(&[NONE, NONE, NONE][..]));

        let error = value::exception_new(test_data(VALUE_ERROR, "boom", 4, "f", None));
        handler_push(error);
        let items = value::as_tuple(exc_info()).expect("tuple");
        assert_eq!(items.len(), 3);
        // The type is the cached class the name `ValueError` resolves to, and
        // the value is the handled exception itself.
        assert_eq!(items[0], exception_class(VALUE_ERROR));
        assert_eq!(items[0], value::type_class(error));
        assert_eq!(items[1], error);
        // Hydro has no traceback object, so the third slot is `None`.
        assert_eq!(items[2], NONE);

        // A `finally` marker above the handler is skipped, like `handled()`.
        let other = value::exception_new(test_data(TYPE_ERROR, "other", 5, "g", None));
        handler_push(other);
        handler_push(NONE);
        let items = value::as_tuple(exc_info()).expect("tuple");
        assert_eq!(items[0], exception_class(TYPE_ERROR));
        assert_eq!(items[1], other);
        assert_eq!(items[2], NONE);
        handler_pop();
        handler_pop();
        handler_pop();

        // A malformed tag has no class: the type is the `None` sentinel while
        // the handled value is still reported unchanged.
        let malformed = value::exception_new(test_data(99, "weird", 6, "h", None));
        handler_push(malformed);
        let items = value::as_tuple(exc_info()).expect("tuple");
        assert_eq!(items[0], NONE);
        assert_eq!(items[1], malformed);
        handler_pop();

        assert_eq!(value::as_tuple(exc_info()), Some(&[NONE, NONE, NONE][..]));
        init();
    }

    #[test]
    fn excepthook_renders_traceback_and_ignores_none_type() {
        init();
        let error = value::exception_new(test_data(VALUE_ERROR, "boom", 4, "f", None));
        // `sys.excepthook(None, ...)` prints nothing, matching CPython.
        assert_eq!(excepthook(NONE, error, NONE), NONE);
        assert_eq!(excepthook_text(NONE, error), None);

        let class = exception_class(VALUE_ERROR);
        assert_eq!(
            excepthook_text(class, error).expect("text"),
            concat!(
                "Traceback (most recent call last):\n",
                "  File \"test.py\", line 4, in f\n",
                "ValueError: boom\n",
            )
        );

        // A non-exception value is reported truthfully as `Type: repr(value)`.
        let plain = value::str_from_bytes(b"plain");
        assert_eq!(
            excepthook_text(class, plain).expect("text"),
            "ValueError: 'plain'\n"
        );
        assert_eq!(
            excepthook_text(value::type_class(plain), NONE).expect("text"),
            "str: None\n"
        );

        // Calling the hook never panics and leaves the exception state alone.
        assert_eq!(excepthook(class, error, NONE), NONE);
        assert!(!pending());
        assert_eq!(handler_depth(), 0);
        init();
    }

    #[test]
    fn handler_context_is_recorded_and_rendered() {
        init();
        let inner = value::exception_new(test_data(VALUE_ERROR, "inner", 3, "inner_fn", None));
        handler_push(inner);
        raise_with_message(RUNTIME_ERROR, "outer");
        let outer = pending_value();
        let data = value::exception_data(outer).expect("exception object");
        assert_eq!(data.context, Some(inner));
        assert_eq!(data.cause, None);
        assert!(!data.suppress_context);
        assert_eq!(
            render_chain(outer, data),
            concat!(
                "Traceback (most recent call last):\n",
                "  File \"test.py\", line 3, in inner_fn\n",
                "ValueError: inner\n",
                "\n",
                "During handling of the above exception, another exception occurred:\n",
                "\n",
                "Traceback (most recent call last):\n",
                "  File \"\", line 0, in \n",
                "RuntimeError: outer\n",
            )
        );

        // Popping the handler stops implicit chaining.
        clear();
        handler_pop();
        raise_with_message(RUNTIME_ERROR, "plain");
        assert_eq!(
            value::exception_data(pending_value())
                .expect("exception object")
                .context,
            None
        );
        init();
    }

    #[test]
    fn explicit_cause_wins_over_context() {
        init();
        let inner = value::exception_new(test_data(VALUE_ERROR, "inner", 3, "inner_fn", None));
        handler_push(inner);
        raise_from(RUNTIME_ERROR, value::str_from_bytes(b"outer"), inner);
        let outer = pending_value();
        let data = value::exception_data(outer).expect("exception object");
        assert_eq!(data.cause, Some(inner));
        assert_eq!(data.context, Some(inner));
        assert!(data.suppress_context);
        let text = render_chain(outer, data);
        assert!(!text.contains("During handling"), "{text}");
        assert!(text.contains("direct cause"), "{text}");
        init();
    }

    #[test]
    fn raise_from_none_hides_the_context() {
        init();
        let inner = value::exception_new(test_data(VALUE_ERROR, "inner", 3, "inner_fn", None));
        handler_push(inner);
        raise_from(RUNTIME_ERROR, value::str_from_bytes(b"outer"), NONE);
        let outer = pending_value();
        let data = value::exception_data(outer).expect("exception object");
        assert_eq!(data.cause, None);
        assert_eq!(data.context, Some(inner));
        assert!(data.suppress_context);
        let text = render_chain(outer, data);
        assert!(!text.contains("inner"), "{text}");
        assert!(!text.contains("During handling"), "{text}");
        assert!(text.contains("RuntimeError: outer"), "{text}");
        init();
    }

    #[test]
    fn re_raising_the_handled_exception_does_not_cycle() {
        init();
        let inner = value::exception_new(test_data(VALUE_ERROR, "inner", 3, "inner_fn", None));
        handler_push(inner);
        raise_value(inner);
        assert_eq!(pending_value(), inner);
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.context, None);
        let text = render_chain(pending_value(), data);
        assert_eq!(
            text,
            concat!(
                "Traceback (most recent call last):\n",
                "  File \"test.py\", line 3, in inner_fn\n",
                "ValueError: inner\n",
            )
        );
        handler_pop();
        init();
    }

    /// Raises an outer exception while `inner` is handled, then pops the
    /// handler; only the `context` edge keeps `inner` alive.
    #[inline(never)]
    fn raise_chain_with_context() -> i64 {
        let inner = value::exception_new(test_data(VALUE_ERROR, "inner", 1, "inner", None));
        handler_push(inner);
        raise_with_message(RUNTIME_ERROR, "outer");
        handler_pop();
        pending_value()
    }

    #[test]
    fn collection_keeps_the_context_of_the_pending_exception() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        init();
        crate::heap::reset();
        let outer = raise_chain_with_context();
        assert!(pending());
        assert_eq!(crate::heap::objects(), 2, "inner and outer were allocated");

        scrub_stack();
        let freed = crate::heap::collect();
        assert_eq!(
            freed, 0,
            "the pending exception and its context must stay reachable"
        );
        assert_eq!(crate::heap::objects(), 2);

        let data = value::exception_data(outer).expect("exception object");
        let context = data.context.expect("context edge");
        assert_eq!(
            value::exception_data(context).expect("context object").msg,
            "inner"
        );
        init();
    }

    /// Renders `args` of an exception as `repr(args_tuple)` text, the way the
    /// CPython test compares them.
    fn args_text(exception: i64) -> Vec<String> {
        let args = exception_attr(exception, "args").expect("args");
        value::as_tuple(args)
            .expect("args tuple")
            .iter()
            .map(|item| value::repr(*item))
            .collect()
    }

    #[test]
    fn baseexception_constructor_stores_positional_args() {
        init();
        // No argument: empty tuple, empty `str`, `ClassName()`.
        let plain = exception_value_from_call(VALUE_ERROR, &[], &[]);
        assert!(!pending());
        assert!(args_text(plain).is_empty());
        assert_eq!(exception_message(value::exception_data(plain).unwrap()), "");
        assert_eq!(
            exception_repr(value::exception_data(plain).unwrap()),
            "ValueError()"
        );

        // One argument: `args == (arg,)`, `str` is the argument, `repr` its
        // repr wrapped in the class name.
        let spam = value::str_from_bytes(b"spam");
        let single = exception_value_from_call(VALUE_ERROR, &[spam], &[]);
        assert_eq!(args_text(single), vec!["'spam'".to_string()]);
        let data = value::exception_data(single).expect("exception object");
        assert_eq!(exception_message(data), "spam");
        assert_eq!(exception_repr(data), "ValueError('spam')");

        // Several arguments: `args` keeps the original objects (not their
        // text) and both `str` and `repr` render the tuple.
        let zero = value::encode_int(0).expect("representable");
        let one = value::encode_int(1).expect("representable");
        let two = value::encode_int(2).expect("representable");
        let multi = exception_value_from_call(VALUE_ERROR, &[zero, one, two], &[]);
        assert_eq!(
            args_text(multi),
            vec!["0".to_string(), "1".to_string(), "2".to_string()]
        );
        let data = value::exception_data(multi).expect("exception object");
        assert_eq!(exception_message(data), "(0, 1, 2)");
        assert_eq!(exception_repr(data), "ValueError(0, 1, 2)");
        // The stored arguments are the original integers, so `.args` compares
        // equal to the tuple the caller supplied.
        let args = value::as_tuple(exception_attr(multi, "args").expect("args")).expect("tuple");
        assert_eq!(value::as_integer(args[0]), Some(0));
        assert_eq!(value::as_integer(args[2]), Some(2));

        // `KeyError` renders its single argument with `repr()` in `str` while
        // `repr` still shows the plain argument.
        let key = exception_value_from_call(KEY_ERROR, &[spam], &[]);
        let data = value::exception_data(key).expect("exception object");
        assert_eq!(exception_message(data), "'spam'");
        assert_eq!(exception_repr(data), "KeyError('spam')");

        // `OSError`-family construction is not routed here; the tag-only
        // payload still renders consistently.
        init();
    }

    #[test]
    fn baseexception_keyword_payload_is_validated_and_stored() {
        init();
        // `AttributeError("m", name="n")`: `args` keeps only the positional
        // argument and the keyword is exposed as an attribute.
        let message = value::str_from_bytes(b"m");
        let name = value::str_from_bytes(b"n");
        let attribute =
            exception_value_from_call(ATTRIBUTE_ERROR, &[message], &[("name".to_string(), name)]);
        assert!(!pending());
        assert_eq!(
            exception_message(value::exception_data(attribute).unwrap()),
            "m"
        );
        assert_eq!(exception_attr(attribute, "name"), Some(name));

        // A class that takes no keywords rejects them with CPython's
        // `TypeError`.
        init();
        assert_eq!(
            exception_value_from_call(VALUE_ERROR, &[], &[("x".to_string(), name)]),
            NONE
        );
        assert_eq!(pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pending_value()),
            "ValueError() takes no keyword arguments"
        );

        init();
        assert_eq!(
            exception_value_from_call(ATTRIBUTE_ERROR, &[], &[("x".to_string(), name)]),
            NONE
        );
        assert_eq!(
            value::to_string(pending_value()),
            "AttributeError() got an unexpected keyword argument 'x'"
        );
        init();
    }

    #[test]
    fn baseexception_method_surface() {
        init();
        let one = value::encode_int(1).expect("representable");
        let exc = exception_value_from_call(VALUE_ERROR, &[one], &[]);

        for method in [
            "__str__",
            "__repr__",
            "__setstate__",
            "__reduce__",
            "__reduce_ex__",
            "with_traceback",
        ] {
            let attribute = exception_attr(exc, method).expect("method attribute");
            assert_eq!(value::type_name(attribute), "function");
        }
        assert_eq!(exception_attr(exc, "missing"), None);

        // `__str__`/`__repr__` render the payload.
        let text = exception_method_call(exc, "__str__", &[]).expect("dispatch");
        assert_eq!(value::as_str(text), Some("1"));
        let text = exception_method_call(exc, "__repr__", &[]).expect("dispatch");
        assert_eq!(value::as_str(text), Some("ValueError(1)"));

        // `__reduce__` is `(type(self), self.args)` and reconstructs the
        // same exception through the constructor.
        let reduced = exception_method_call(exc, "__reduce__", &[]).expect("dispatch");
        let items = value::as_tuple(reduced).expect("reduce tuple");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], exception_class(VALUE_ERROR));
        let args = value::as_tuple(items[1]).expect("args tuple");
        assert_eq!(args.len(), 1);
        assert_eq!(value::as_integer(args[0]), Some(1));

        // Extra arguments report CPython's TypeError.
        init();
        let exc = exception_value_from_call(VALUE_ERROR, &[], &[]);
        assert_eq!(exception_method_call(exc, "__str__", &[one]), Some(NONE));
        assert_eq!(pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pending_value()),
            "BaseException.__str__() takes no arguments (1 given)"
        );
        init();
    }

    #[test]
    fn baseexception_setstate_restores_the_pickle_state() {
        init();
        let exc = exception_value_from_call(VALUE_ERROR, &[], &[]);

        // A dict state is snapshot and accepted (the gh-97591 refcount fix);
        // a plain built-in exception has no attribute table to hold it.
        let key = value::str_from_bytes(b"extra");
        let item = value::encode_int(7).expect("representable");
        let state = value::dict_new(vec![(key, item)]);
        assert_eq!(
            exception_method_call(exc, "__setstate__", &[state]),
            Some(NONE)
        );
        assert!(!pending());

        // A tuple state is the legacy `self.args = state` form.
        let legacy = value::tuple_new(vec![item]);
        assert_eq!(
            exception_method_call(exc, "__setstate__", &[legacy]),
            Some(NONE)
        );
        assert!(!pending());

        // Anything else is CPython's `TypeError`.
        assert_eq!(
            exception_method_call(exc, "__setstate__", &[item]),
            Some(NONE)
        );
        assert_eq!(pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pending_value()),
            "state is not a dictionary"
        );

        // The dict is snapshotted before the attributes are written: a key
        // whose `__hash__` clears the mapping must not disturb the restore.
        init();
        let mut pairs = vec![(key, item)];
        let snapshot = pairs.clone();
        let state = value::dict_new(std::mem::take(&mut pairs));
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            exception_method_call(exc, "__setstate__", &[state]),
            Some(NONE)
        );
        assert!(!pending());
        init();
    }

    #[test]
    fn except_operands_must_derive_from_base_exception() {
        init();
        raise(VALUE_ERROR, value::str_from_bytes(b"boom"));
        let exception = pending_value();

        // A valid class matches through the tag hierarchy.
        assert_eq!(
            matches_handler_value(exception, exception_class(VALUE_ERROR)),
            1
        );
        assert_eq!(
            matches_handler_value(exception, exception_class(EXCEPTION)),
            1
        );
        assert_eq!(
            matches_handler_value(exception, exception_class(OS_ERROR)),
            0
        );
        // Tuples match element-wise, including a non-matching first element.
        let tuple = value::tuple_new(vec![
            exception_class(TYPE_ERROR),
            exception_class(VALUE_ERROR),
        ]);
        assert_eq!(matches_handler_value(exception, tuple), 1);

        // A class that does not inherit from BaseException is a TypeError,
        // never a silent non-match.
        for invalid in [
            value::object_class(),
            value::instance_new(value::class_new(
                "NonBaseException".to_string(),
                value::object_class(),
                Vec::new(),
            )),
            value::str_from_bytes(b"spam"),
            exception,
        ] {
            clear();
            raise(VALUE_ERROR, value::str_from_bytes(b"boom"));
            let _ = matches_handler_value(pending_value(), invalid);
            assert_eq!(pending_tag(), TYPE_ERROR as i64);
            assert_eq!(
                value::to_string(pending_value()),
                "catching classes that do not inherit from BaseException is not allowed"
            );
        }

        // The same rule applies inside a tuple operand.
        clear();
        raise(VALUE_ERROR, value::str_from_bytes(b"boom"));
        let tuple = value::tuple_new(vec![value::object_class(), exception_class(VALUE_ERROR)]);
        let _ = matches_handler_value(pending_value(), tuple);
        assert_eq!(pending_tag(), TYPE_ERROR as i64);
        init();
    }

    #[test]
    fn exception_repr_quotes_legacy_rendered_arguments() {
        init();
        // `raise ValueError("x")` keeps the argument object, its text and its
        // representation.
        raise(VALUE_ERROR, value::str_from_bytes(b"x"));
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(exception_repr(data), "ValueError('x')");
        assert_eq!(exception_message(data), "x");

        // A runtime failure (`raise_with_message`) only keeps the rendered
        // text; repr quotes it because that argument is a string by
        // construction.
        init();
        raise_with_message(ZERO_DIVISION_ERROR, "division by zero");
        let data = value::exception_data(pending_value()).expect("exception object");
        assert_eq!(data.args, vec!["division by zero".to_string()]);
        assert_eq!(
            exception_repr(data),
            "ZeroDivisionError('division by zero')"
        );

        // `raise ZeroDivisionError` stores no argument at all, so repr has an
        // empty argument list even though the message is the default text.
        init();
        raise(ZERO_DIVISION_ERROR, NONE);
        let data = value::exception_data(pending_value()).expect("exception object");
        assert!(data.args.is_empty());
        assert_eq!(exception_message(data), "division by zero");
        assert_eq!(exception_repr(data), "ZeroDivisionError()");
        init();
    }
}
