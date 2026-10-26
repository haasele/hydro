//! Runtime ABI tests for closures: wrapped entry points receive captured
//! values followed by user arguments, and the collector keeps captures alive.

use hydro_runtime::abi::{
    hydro_binop, hydro_call, hydro_cell_get, hydro_cell_new, hydro_cell_set, hydro_closure_new,
    hydro_exc_clear, hydro_exc_pending, hydro_gc_collect, hydro_int, hydro_len, hydro_runtime_init,
    hydro_str_new,
};

extern "C" fn add_captured(argv: *const i64) -> i64 {
    // argv = [captured x, argument y]
    unsafe {
        let x = *argv;
        let y = *argv.add(1);
        hydro_binop(hydro_ir::BinOp::Add.code(), x, y)
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
    hydro_closure_new(
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
    hydro_runtime_init();
    let captures = [hydro_int(2)];
    let closure = new_closure(add_captured, 1, "make.add", &captures);
    assert_eq!(hydro_exc_pending(), 0);

    let args = [hydro_int(3)];
    let result = hydro_call(closure, 1, args.as_ptr());
    assert_eq!(hydro_exc_pending(), 0);
    assert_eq!(result, hydro_int(5));
}

#[test]
fn closures_capture_independent_values() {
    hydro_runtime_init();
    let first_captures = [hydro_int(2)];
    let second_captures = [hydro_int(10)];
    let first = new_closure(add_captured, 1, "make.first", &first_captures);
    let second = new_closure(add_captured, 1, "make.second", &second_captures);

    let args = [hydro_int(3)];
    assert_eq!(hydro_call(first, 1, args.as_ptr()), hydro_int(5));
    assert_eq!(hydro_call(second, 1, args.as_ptr()), hydro_int(13));
}

#[test]
fn closure_arity_is_the_user_arity() {
    hydro_runtime_init();
    let captures = [hydro_int(1), hydro_int(2)];
    let closure = new_closure(add_captured, 1, "make.add", &captures);

    // Two captured values plus no user argument: arity mismatch.
    let result = hydro_call(closure, 0, std::ptr::null());
    assert_eq!(result, hydro_runtime_none());
    assert_ne!(hydro_exc_pending(), 0);
    hydro_exc_clear();
}

#[inline(never)]
fn make_text_closure() -> i64 {
    let text = hydro_str_new(b"abc".as_ptr(), 3);
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
    hydro_runtime_init();
    // The string is only reachable through the closure's capture vector.
    let closure = make_text_closure();
    scrub_stack();
    let _ = hydro_gc_collect();

    let result = hydro_call(closure, 0, std::ptr::null());
    assert_eq!(hydro_exc_pending(), 0);
    assert_eq!(hydro_len(result), hydro_int(3));
}

#[inline(never)]
fn make_text_cell_closure() -> i64 {
    let text = hydro_str_new(b"abc".as_ptr(), 3);
    let cell = hydro_cell_new(text);
    new_closure(first_capture, 0, "cell", &[cell])
}

#[test]
fn cells_share_mutations_and_trace_their_value() {
    hydro_runtime_init();
    let cell = hydro_cell_new(hydro_int(1));
    assert_eq!(hydro_cell_get(cell), hydro_int(1));
    hydro_cell_set(cell, hydro_int(2));
    assert_eq!(hydro_cell_get(cell), hydro_int(2));

    // The string is only reachable through the closure's captured cell.
    let closure = make_text_cell_closure();
    scrub_stack();
    let _ = hydro_gc_collect();
    let cell = hydro_call(closure, 0, std::ptr::null());
    assert_eq!(hydro_exc_pending(), 0);
    let text = hydro_cell_get(cell);
    assert_eq!(hydro_len(text), hydro_int(3));
}

#[test]
fn null_capture_buffer_is_an_internal_error() {
    hydro_runtime_init();
    let result = hydro_closure_new(
        first_capture as *const u8,
        0,
        b"bad".as_ptr(),
        3,
        1,
        std::ptr::null(),
        0,
    );
    assert_eq!(result, hydro_runtime_none());
    assert_ne!(hydro_exc_pending(), 0);
    hydro_exc_clear();
}

fn hydro_runtime_none() -> i64 {
    hydro_runtime::abi::hydro_none()
}
