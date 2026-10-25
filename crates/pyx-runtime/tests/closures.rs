//! Runtime ABI tests for closures: wrapped entry points receive captured
//! values followed by user arguments, and the collector keeps captures alive.

use pyx_runtime::abi::{
    pyx_binop, pyx_call, pyx_cell_get, pyx_cell_new, pyx_cell_set, pyx_closure_new, pyx_exc_clear,
    pyx_exc_pending, pyx_gc_collect, pyx_int, pyx_len, pyx_runtime_init, pyx_str_new,
};

extern "C" fn add_captured(argv: *const i64) -> i64 {
    // argv = [captured x, argument y]
    unsafe {
        let x = *argv;
        let y = *argv.add(1);
        pyx_binop(pyx_ir::BinOp::Add.code(), x, y)
    }
}

extern "C" fn first_capture(argv: *const i64) -> i64 {
    unsafe { *argv }
}

fn new_closure(
    wrapper: extern "C" fn(*const i64) -> i64,
    arity: usize,
    name: &str,
    captures: &[i64],
) -> i64 {
    pyx_closure_new(
        wrapper as *const u8,
        arity,
        name.as_ptr(),
        name.len(),
        captures.len(),
        if captures.is_empty() {
            std::ptr::null()
        } else {
            captures.as_ptr()
        },
        0,
    )
}

#[test]
fn closure_receives_captures_before_arguments() {
    pyx_runtime_init();
    let captures = [pyx_int(2)];
    let closure = new_closure(add_captured, 1, "make.add", &captures);
    assert_eq!(pyx_exc_pending(), 0);

    let args = [pyx_int(3)];
    let result = pyx_call(closure, 1, args.as_ptr());
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(result, pyx_int(5));
}

#[test]
fn closures_capture_independent_values() {
    pyx_runtime_init();
    let first_captures = [pyx_int(2)];
    let second_captures = [pyx_int(10)];
    let first = new_closure(add_captured, 1, "make.first", &first_captures);
    let second = new_closure(add_captured, 1, "make.second", &second_captures);

    let args = [pyx_int(3)];
    assert_eq!(pyx_call(first, 1, args.as_ptr()), pyx_int(5));
    assert_eq!(pyx_call(second, 1, args.as_ptr()), pyx_int(13));
}

#[test]
fn closure_arity_is_the_user_arity() {
    pyx_runtime_init();
    let captures = [pyx_int(1), pyx_int(2)];
    let closure = new_closure(add_captured, 1, "make.add", &captures);

    // Two captured values plus no user argument: arity mismatch.
    let result = pyx_call(closure, 0, std::ptr::null());
    assert_eq!(result, pyx_runtime_none());
    assert_ne!(pyx_exc_pending(), 0);
    pyx_exc_clear();
}

#[inline(never)]
fn make_text_closure() -> i64 {
    let text = pyx_str_new(b"abc".as_ptr(), 3);
    new_closure(first_capture, 0, "make.text", &[text])
}

fn scrub_stack() {
    let mut buffer = [0u8; 64 * 1024];
    std::hint::black_box(&mut buffer);
    buffer[0] = 0;
    std::hint::black_box(&buffer);
}

#[test]
fn collector_traces_captured_heap_values() {
    pyx_runtime_init();
    // The string is only reachable through the closure's capture vector.
    let closure = make_text_closure();
    scrub_stack();
    let _ = pyx_gc_collect();

    let result = pyx_call(closure, 0, std::ptr::null());
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(pyx_len(result), pyx_int(3));
}

#[inline(never)]
fn make_text_cell_closure() -> i64 {
    let text = pyx_str_new(b"abc".as_ptr(), 3);
    let cell = pyx_cell_new(text);
    new_closure(first_capture, 0, "cell", &[cell])
}

#[test]
fn cells_share_mutations_and_trace_their_value() {
    pyx_runtime_init();
    let cell = pyx_cell_new(pyx_int(1));
    assert_eq!(pyx_cell_get(cell), pyx_int(1));
    pyx_cell_set(cell, pyx_int(2));
    assert_eq!(pyx_cell_get(cell), pyx_int(2));

    // The string is only reachable through the closure's captured cell.
    let closure = make_text_cell_closure();
    scrub_stack();
    let _ = pyx_gc_collect();
    let cell = pyx_call(closure, 0, std::ptr::null());
    assert_eq!(pyx_exc_pending(), 0);
    let text = pyx_cell_get(cell);
    assert_eq!(pyx_len(text), pyx_int(3));
}

#[test]
fn null_capture_buffer_is_an_internal_error() {
    pyx_runtime_init();
    let result = pyx_closure_new(
        first_capture as *const u8,
        0,
        b"bad".as_ptr(),
        3,
        1,
        std::ptr::null(),
        0,
    );
    assert_eq!(result, pyx_runtime_none());
    assert_ne!(pyx_exc_pending(), 0);
    pyx_exc_clear();
}

fn pyx_runtime_none() -> i64 {
    pyx_runtime::abi::pyx_none()
}
