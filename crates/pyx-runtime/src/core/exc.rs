//! Exception state and tracebacks (ARCHITECTURE §3.3).
//!
//! Phase 1 keeps a single pending exception per thread in a `thread_local`.
//! Raising overwrites any previous pending exception; the codegen polls
//! `pyx_exc_pending()` after every fallible runtime call.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;

use crate::value::{self, NONE};

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
    /// Tagged exception value that caused this one, or `None` for no explicit
    /// cause (including `raise ... from None`).
    pub(crate) cause: Option<i64>,
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
        GENERATOR_EXIT => "",
        // `Exception()` and `BaseException()` stringify to the empty string.
        EXCEPTION | BASE_EXCEPTION => "",
        ASSERTION_ERROR => "",
        _ => "unknown error",
    }
}

/// True for every exception tag the runtime defines. Used to reject malformed
/// values instead of caching a class under a bogus tag.
fn known_tag(tag: u32) -> bool {
    matches!(
        tag,
        VALUE_ERROR
            | TYPE_ERROR
            | ZERO_DIVISION_ERROR
            | RUNTIME_ERROR
            | OVERFLOW_ERROR
            | KEYBOARD_INTERRUPT
            | INDEX_ERROR
            | KEY_ERROR
            | STOP_ITERATION
            | ATTRIBUTE_ERROR
            | OS_ERROR
            | GENERATOR_EXIT
            | EXCEPTION
            | BASE_EXCEPTION
            | ASSERTION_ERROR
            | FILE_NOT_FOUND_ERROR
    )
}

/// Returns the cached native class object for the built-in exception `tag`,
/// creating an empty class named after the tag on first use.
///
/// The class is an ordinary `KIND_CLASS` object, so `Class.__name__`,
/// inheritance and instantiation work like any class the user defines. The
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
    let class = value::class_new(tag_name(tag).to_string(), NONE, Vec::new());
    // The cache is invisible to the conservative stack scan, so the class must
    // be an explicit root while it lives in the heap.
    crate::heap::add_thread_root(class);
    CLASS_CACHE.with(|cache| cache.borrow_mut().insert(tag, class));
    class
}

#[derive(Default)]
struct State {
    file: String,
    line: u32,
    /// Active call frames, innermost last.
    frames: Vec<String>,
    /// Tagged heap pointer to the pending exception object, if any.
    pending: Option<i64>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    /// Native class objects exposed as first-class exception values, keyed by
    /// exception tag. The cached values live in the heap and are registered as
    /// thread roots; [`init`] drops the cache before the heap is freed so a
    /// later program run cannot observe a dangling class pointer.
    static CLASS_CACHE: RefCell<HashMap<u32, i64>> = RefCell::new(HashMap::new());
}

// -------------------------------------------------------------------------
// Source location tracking
// -------------------------------------------------------------------------

pub(crate) fn set_file_ptr(ptr: *const u8, len: usize) {
    let file = decode_bytes(ptr, len);
    STATE.with(|s| s.borrow_mut().file = file);
}

pub(crate) fn set_line(line: u32) {
    STATE.with(|s| s.borrow_mut().line = line);
}

pub(crate) fn frame_enter_ptr(ptr: *const u8, len: usize) {
    let frame = decode_bytes(ptr, len);
    STATE.with(|s| s.borrow_mut().frames.push(frame));
}

/// Pops the innermost call frame; a no-op when the stack is empty.
pub(crate) fn frame_leave() {
    STATE.with(|s| {
        s.borrow_mut().frames.pop();
    });
}

/// Name of the innermost active frame (`""` when no frame is active).
fn current_frame() -> String {
    STATE.with(|s| s.borrow().frames.last().cloned().unwrap_or_default())
}

fn decode_bytes(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return String::new();
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

// -------------------------------------------------------------------------
// Pending exception state
// -------------------------------------------------------------------------

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
    // The cached class objects belong to the heap that `pyx_runtime_init`
    // frees right after this call; dropping the cache prevents a later run
    // from handing out a dangling pointer.
    CLASS_CACHE.with(|cache| cache.borrow_mut().clear());
    value::clear_type_class_cache();
    crate::heap::set_pending_mirror(value::NONE);
}

/// Raises `tag` with `message`, capturing the current file/line/frame.
/// Returns the ABI dummy value (`None`).
pub(crate) fn raise_with_message(tag: u32, message: &str) -> i64 {
    raise_with_message_cause(tag, message, None)
}

/// [`raise_with_message`] with an explicit cause (already filtered: `None`
/// means "no cause").
fn raise_with_message_cause(tag: u32, message: &str, cause: Option<i64>) -> i64 {
    let (file, line) = STATE.with(|s| {
        let st = s.borrow();
        (st.file.clone(), st.line)
    });
    let frame = current_frame();
    let data = ExcData {
        tag,
        name: tag_name(tag),
        msg: message.to_string(),
        file,
        line,
        frame,
        cause,
    };
    publish(value::exception_new(data))
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

/// Renders the `msg` argument of a raise: `None` selects the default text,
/// `KeyError` and non-string arguments use `repr()` and strings `str()`.
fn rendered_message(tag: u32, msg: i64) -> String {
    if msg == value::NONE {
        default_message(tag).to_string()
    } else if tag == KEY_ERROR {
        value::repr(msg)
    } else if let Some(s) = value::as_str(msg) {
        s.to_string()
    } else {
        value::repr(msg)
    }
}

/// `pyx_raise`: `msg` is a string value or `None` (default message).
///
/// Python renders a `KeyError` argument with `repr()` (so `KeyError("k")`
/// prints `'k'`), while other exception types use `str()` for strings and
/// `repr()` for non-string arguments.
pub(crate) fn raise(tag: u32, msg: i64) -> i64 {
    raise_with_message(tag, &rendered_message(tag, msg))
}

/// `pyx_raise_msg`: copies the message bytes before storing them.
pub(crate) fn raise_msg_ptr(tag: u32, ptr: *const u8, len: usize) -> i64 {
    let text = decode_bytes(ptr, len);
    raise_with_message(tag, &text)
}

/// `pyx_raise_value`: re-raises an existing exception object (bare `raise`).
///
/// The object is stored as-is, so type, message and source location of the
/// original raise are preserved. Non-exception values raise `RuntimeError`,
/// matching CPython.
pub(crate) fn raise_value(v: i64) -> i64 {
    if value::exception_data(v).is_some() {
        publish(v)
    } else {
        raise_with_message(RUNTIME_ERROR, "exceptions must derive from BaseException")
    }
}

/// `pyx_raise_from`: `raise X(msg) from cause`.
///
/// A `cause` of `None` (`from None`) suppresses the cause entirely; any other
/// tagged value is stored as the explicit cause so the chained traceback and
/// the collector can follow it.
pub(crate) fn raise_from(tag: u32, msg: i64, cause: i64) -> i64 {
    let text = rendered_message(tag, msg);
    raise_with_message_cause(tag, &text, cause_of(cause))
}

/// `pyx_raise_value_from`: `raise value from cause` for an existing exception
/// object.
///
/// The payload is copied with the new cause rather than mutated: exception
/// objects can be shared between threads, so the original stays untouched.
/// Non-exception values raise `RuntimeError`, matching [`raise_value`].
pub(crate) fn raise_value_from(v: i64, cause: i64) -> i64 {
    match value::exception_data(v) {
        Some(data) => {
            let mut data = data.clone();
            data.cause = cause_of(cause);
            publish(value::exception_new(data))
        }
        None => raise_with_message(RUNTIME_ERROR, "exceptions must derive from BaseException"),
    }
}

/// `pyx_exc_matches`: whether the pending exception is caught by a handler for
/// `handler_tag`.
///
/// `BaseException` matches every tag, `Exception` matches everything except
/// `KeyboardInterrupt` and the internal `GeneratorExit`, `OSError` and
/// `FileNotFoundError` match each other (the subclass relationship), and every
/// other handler tag matches only an exception with the same tag. Without a
/// pending exception nothing matches.
pub(crate) fn matches_handler(handler_tag: u32) -> i32 {
    let pending = STATE.with(|s| s.borrow().pending);
    let Some(tag) = pending.and_then(|exc| value::exception_data(exc).map(|data| data.tag)) else {
        return 0;
    };
    let matched = match handler_tag {
        BASE_EXCEPTION => true,
        EXCEPTION => tag != KEYBOARD_INTERRUPT && tag != GENERATOR_EXIT,
        // `FileNotFoundError` is an `OSError` subclass: a handler for either
        // kind catches both tags.
        OS_ERROR | FILE_NOT_FOUND_ERROR => tag == OS_ERROR || tag == FILE_NOT_FOUND_ERROR,
        _ => handler_tag == tag,
    };
    i32::from(matched)
}

// -------------------------------------------------------------------------
// Uncaught traceback
// -------------------------------------------------------------------------

/// Maximum number of exceptions rendered for one cause chain. Deeper chains
/// stop at the outermost exception (the chain is rendered deepest first).
const MAX_CAUSE_CHAIN: usize = 8;

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
    });

    let text = render_chain(root, &data);
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();

    clear();
    1
}

/// Renders `data` after the tracebacks of its explicit causes.
///
/// The chain is followed through `ExcData::cause` only (implicit context is
/// not tracked), deepest cause first, separated by the CPython marker line.
/// At most [`MAX_CAUSE_CHAIN`] exceptions are rendered and cycles are cut off,
/// so a self-referential chain cannot loop.
fn render_chain(root: i64, data: &ExcData) -> String {
    let mut chain = vec![data.clone()];
    let mut seen = vec![root];
    let mut cause = data.cause;
    while let Some(handle) = cause {
        if chain.len() >= MAX_CAUSE_CHAIN || seen.contains(&handle) {
            break;
        }
        seen.push(handle);
        let Some(next) = value::exception_data(handle) else {
            break;
        };
        cause = next.cause;
        chain.push(next.clone());
    }

    let mut text = String::new();
    for (index, data) in chain.iter().rev().enumerate() {
        if index > 0 {
            let _ = writeln!(text);
            let _ = writeln!(
                text,
                "The above exception was the direct cause of the following exception:"
            );
            let _ = writeln!(text);
        }
        let _ = writeln!(text, "Traceback (most recent call last):");
        let _ = writeln!(
            text,
            "  File \"{}\", line {}, in {}",
            data.file, data.line, data.frame
        );
        let _ = writeln!(text, "{}: {}", data.name, data.msg);
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
    fn exception_classes_are_cached_empty_classes() {
        init();
        let first = exception_class(VALUE_ERROR);
        let second = exception_class(VALUE_ERROR);
        assert_eq!(first, second, "the same tag reuses one class object");
        let state = value::as_class(first).expect("exception class object");
        assert_eq!(state.name, "ValueError");
        assert_eq!(state.base, NONE);
        assert!(state.methods.is_empty());
        assert!(state.attrs.is_empty());
        assert_eq!(value::class_display_name(first), Some("ValueError"));

        let other = exception_class(OS_ERROR);
        assert_ne!(first, other);
        assert_eq!(value::class_display_name(other), Some("OSError"));

        // A user class derived from the native class keeps normal
        // inheritance semantics for instances.
        let user = value::class_new("E".to_string(), first, Vec::new());
        let instance = value::instance_new(user);
        assert_eq!(crate::builtins::isinstance(instance, user), value::TRUE);
        assert_eq!(crate::builtins::isinstance(instance, first), value::TRUE);
        assert_eq!(crate::builtins::isinstance(instance, other), value::FALSE);

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
        assert_eq!(raise_value(NONE), NONE);
        assert!(pending());
        assert_eq!(pending_tag(), RUNTIME_ERROR as i64);
        assert_eq!(
            value::to_string(pending_value()),
            "exceptions must derive from BaseException"
        );
        init();
    }

    #[test]
    fn default_messages() {
        init();
        raise(ZERO_DIVISION_ERROR, NONE);
        let exc = pending_value();
        assert_eq!(value::to_string(exc), "division by zero");
        assert_eq!(value::type_name(exc), "ZeroDivisionError");

        init();
        raise(TYPE_ERROR, NONE);
        assert_eq!(value::to_string(pending_value()), "unknown error");

        init();
        raise_msg_ptr(VALUE_ERROR, b"bad".as_ptr(), 3);
        assert_eq!(value::to_string(pending_value()), "bad");
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
            (BASE_EXCEPTION, [false, false, false, false, true, true]),
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
        assert_eq!(pending_tag(), RUNTIME_ERROR as i64);
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
}
