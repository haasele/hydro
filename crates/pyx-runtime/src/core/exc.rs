//! Exception state and tracebacks (ARCHITECTURE §3.3).
//!
//! Phase 1 keeps a single pending exception per thread in a `thread_local`.
//! Raising overwrites any previous pending exception; the codegen polls
//! `pyx_exc_pending()` after every fallible runtime call.

use std::cell::RefCell;
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
/// Runtime-internal `GeneratorExit`, injected by `generator.close()`. It is
/// not reachable from Python source (PyX has no `GeneratorExit` class yet).
pub(crate) const GENERATOR_EXIT: u32 = 11;

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
        GENERATOR_EXIT => "",
        _ => "unknown error",
    }
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
    crate::heap::set_pending_mirror(value::NONE);
}

/// Raises `tag` with `message`, capturing the current file/line/frame.
/// Returns the ABI dummy value (`None`).
pub(crate) fn raise_with_message(tag: u32, message: &str) -> i64 {
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
    };
    let exc = value::exception_new(data);
    STATE.with(|s| s.borrow_mut().pending = Some(exc));
    // Mirrored into the thread's GC registration so a collector stopping the
    // world at a safepoint also marks this exception object.
    crate::heap::set_pending_mirror(exc);
    NONE
}

/// `pyx_raise`: `msg` is a string value or `None` (default message).
///
/// Python renders a `KeyError` argument with `repr()` (so `KeyError("k")`
/// prints `'k'`), while other exception types use `str()` for strings and
/// `repr()` for non-string arguments.
pub(crate) fn raise(tag: u32, msg: i64) -> i64 {
    let text = if msg == value::NONE {
        default_message(tag).to_string()
    } else if tag == KEY_ERROR {
        value::repr(msg)
    } else if let Some(s) = value::as_str(msg) {
        s.to_string()
    } else {
        value::repr(msg)
    };
    raise_with_message(tag, &text)
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
        STATE.with(|s| s.borrow_mut().pending = Some(v));
        crate::heap::set_pending_mirror(v);
        NONE
    } else {
        raise_with_message(RUNTIME_ERROR, "exceptions must derive from BaseException")
    }
}

// -------------------------------------------------------------------------
// Uncaught traceback
// -------------------------------------------------------------------------

/// Prints the traceback for the pending exception to stderr, clears it and
/// returns the process exit code `1`. Returns `0` if nothing is pending.
pub(crate) fn uncaught() -> i64 {
    let (has_pending, data) = STATE.with(|s| {
        let st = s.borrow();
        (
            st.pending.is_some(),
            st.pending.and_then(value::exception_clone),
        )
    });
    if !has_pending {
        return 0;
    }
    let data = data.unwrap_or_else(|| ExcData {
        tag: RUNTIME_ERROR,
        name: tag_name(RUNTIME_ERROR),
        msg: default_message(RUNTIME_ERROR).to_string(),
        file: String::new(),
        line: 0,
        frame: String::new(),
    });

    let mut text = String::new();
    let _ = writeln!(text, "Traceback (most recent call last):");
    let _ = writeln!(
        text,
        "  File \"{}\", line {}, in {}",
        data.file, data.line, data.frame
    );
    let _ = writeln!(text, "{}: {}", data.name, data.msg);

    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();

    clear();
    1
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
}
