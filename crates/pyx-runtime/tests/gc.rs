//! Integration tests for the interim conservative collector (ARCHITECTURE §7).
//!
//! Conservative scanning may retain objects whose stale address remains on
//! the stack or in a register, so all assertions are tolerant: they check
//! that garbage is reclaimed and reachable values survive, never exact
//! counts. Tests run on their own threads, each with a thread-local heap.
//!
//! MIXED MODE ONLY: roots are plain Rust locals, not precise shadow frames,
//! so the suite requires the conservative scan (`PYX_GC_PRECISE` unset).
//! Strict-mode collector behaviour is covered by `tests/precise_gc.rs`.

use pyx_runtime::abi::*;

/// Overwrites a large portion of the stack below the caller with zeroes, so
/// stale tagged values from helper frames are unlikely to be found by the
/// conservative scan. The helper is `inline(never)` so its frame really is
/// materialized.
#[inline(never)]
fn scrub_stack() {
    let mut buffer = [0u8; 64 * 1024];
    std::hint::black_box(&mut buffer);
    buffer[0] = 0;
    std::hint::black_box(&buffer);
}

#[inline(never)]
fn make_garbage(count: usize) {
    for _ in 0..count {
        let _ = pyx_str_new(b"garbage".as_ptr(), 7);
    }
}

#[inline(never)]
fn make_reachable_list() -> i64 {
    let items = [
        pyx_str_new(b"alpha".as_ptr(), 5),
        pyx_str_new(b"beta".as_ptr(), 4),
    ];
    pyx_list_new(items.len(), items.as_ptr())
}

#[inline(never)]
fn make_reachable_tuple() -> i64 {
    let items = [
        pyx_str_new(b"alpha".as_ptr(), 5),
        pyx_str_new(b"beta".as_ptr(), 4),
    ];
    pyx_tuple_new(items.len(), items.as_ptr())
}

#[inline(never)]
fn make_cycle() {
    let items = [pyx_none()];
    let list = pyx_list_new(1, items.as_ptr());
    // `x[0] = x`: only the cycle keeps the list reachable.
    pyx_subscript_set(list, pyx_int(0), list);
}

#[test]
fn unreachable_strings_are_reclaimed() {
    pyx_runtime_init();
    make_garbage(200);
    let before = pyx_gc_objects();
    assert!(before >= 1, "no garbage object was registered");

    scrub_stack();
    let freed = pyx_gc_collect();
    let after = pyx_gc_objects();

    // Under `PYX_GC_STRESS` most garbage is already gone before the forced
    // collection, so only require that progress was made.
    assert!(freed > 0, "no garbage object was collected");
    assert!(
        after < before,
        "registry did not shrink ({before} -> {after})"
    );
    assert_eq!(pyx_exc_pending(), 0);
}

#[test]
fn reachable_list_survives_collection() {
    pyx_runtime_init();
    let list = make_reachable_list();

    scrub_stack();
    let _freed = pyx_gc_collect();

    assert_eq!(pyx_len(list), pyx_int(2));
    let first = pyx_subscript(list, pyx_int(0));
    let second = pyx_subscript(list, pyx_int(1));
    // Reaching the elements through the list and reading their contents
    // proves both the list and its string children survived.
    assert_eq!(pyx_len(first), pyx_int(5));
    assert_eq!(pyx_len(second), pyx_int(4));
    assert_eq!(pyx_exc_pending(), 0);
}

#[test]
fn reachable_tuple_elements_survive_collection() {
    pyx_runtime_init();
    let tuple = make_reachable_tuple();

    scrub_stack();
    let _freed = pyx_gc_collect();

    // The tuple is a root; its string children are only reachable through
    // the tuple, proving `Tuple` children are traced.
    assert_eq!(pyx_len(tuple), pyx_int(2));
    let first = pyx_subscript(tuple, pyx_int(0));
    let second = pyx_subscript(tuple, pyx_int(1));
    assert_eq!(pyx_len(first), pyx_int(5));
    assert_eq!(pyx_len(second), pyx_int(4));
    assert_eq!(pyx_exc_pending(), 0);
}

#[test]
fn unreachable_cycle_is_collected() {
    pyx_runtime_init();
    make_cycle();
    let before = pyx_gc_objects();
    assert!(before >= 1, "cycle was not allocated");

    scrub_stack();
    let freed = pyx_gc_collect();

    assert!(freed >= 1, "self-referential list was not collected");
    assert!(
        pyx_gc_objects() < before,
        "registry still contains the garbage cycle"
    );
}

#[test]
fn pending_exception_is_a_root() {
    pyx_runtime_init();
    let message = pyx_str_new(b"boom".as_ptr(), 4);
    assert_eq!(pyx_raise(0, message), pyx_none());
    assert_eq!(pyx_exc_pending(), 1);

    scrub_stack();
    let _freed = pyx_gc_collect();

    // The tagged pending exception survived and still carries its payload.
    assert_eq!(pyx_exc_pending(), 1);
    assert_eq!(pyx_exc_tag(), 0);
    let value = pyx_exc_value();
    assert!(value & 7 == 2, "pending value is not a heap object");
    let text = pyx_to_str(value);
    assert_eq!(pyx_len(text), pyx_int(4));
    pyx_exc_clear();
}

#[test]
fn runtime_init_clears_the_registry() {
    pyx_runtime_init();
    let _s = pyx_str_new(b"value".as_ptr(), 5);
    let _f = pyx_float_new(1.5);
    assert!(pyx_gc_objects() >= 2);

    pyx_runtime_init();

    assert_eq!(pyx_gc_objects(), 0);
    assert_eq!(pyx_exc_pending(), 0);
}
