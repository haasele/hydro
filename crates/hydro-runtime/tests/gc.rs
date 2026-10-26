//! Integration tests for the interim conservative collector (ARCHITECTURE §7).
//!
//! Conservative scanning may retain objects whose stale address remains on
//! the stack or in a register, so all assertions are tolerant: they check
//! that garbage is reclaimed and reachable values survive, never exact
//! counts. Tests run on their own threads, each with a thread-local heap.
//!
//! MIXED MODE ONLY: roots are plain Rust locals, not precise shadow frames,
//! so the suite requires the conservative scan (`HYDRO_GC_PRECISE` unset).
//! Strict-mode collector behaviour is covered by `tests/precise_gc.rs`.

use hydro_runtime::abi::*;

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
        let _ = hydro_str_new(b"garbage".as_ptr(), 7);
    }
}

#[inline(never)]
fn make_reachable_list() -> i64 {
    let items = [
        hydro_str_new(b"alpha".as_ptr(), 5),
        hydro_str_new(b"beta".as_ptr(), 4),
    ];
    hydro_list_new(items.len(), items.as_ptr())
}

#[inline(never)]
fn make_reachable_tuple() -> i64 {
    let items = [
        hydro_str_new(b"alpha".as_ptr(), 5),
        hydro_str_new(b"beta".as_ptr(), 4),
    ];
    hydro_tuple_new(items.len(), items.as_ptr())
}

#[inline(never)]
fn make_cycle() {
    let items = [hydro_none()];
    let list = hydro_list_new(1, items.as_ptr());
    // `x[0] = x`: only the cycle keeps the list reachable.
    hydro_subscript_set(list, hydro_int(0), list);
}

#[test]
fn unreachable_strings_are_reclaimed() {
    hydro_runtime_init();
    make_garbage(200);
    let before = hydro_gc_objects();
    assert!(before >= 1, "no garbage object was registered");

    scrub_stack();
    let freed = hydro_gc_collect();
    let after = hydro_gc_objects();

    // Under `HYDRO_GC_STRESS` most garbage is already gone before the forced
    // collection, so only require that progress was made.
    assert!(freed > 0, "no garbage object was collected");
    assert!(
        after < before,
        "registry did not shrink ({before} -> {after})"
    );
    assert_eq!(hydro_exc_pending(), 0);
}

#[test]
fn reachable_list_survives_collection() {
    hydro_runtime_init();
    let list = make_reachable_list();

    scrub_stack();
    let _freed = hydro_gc_collect();

    assert_eq!(hydro_len(list), hydro_int(2));
    let first = hydro_subscript(list, hydro_int(0));
    let second = hydro_subscript(list, hydro_int(1));
    // Reaching the elements through the list and reading their contents
    // proves both the list and its string children survived.
    assert_eq!(hydro_len(first), hydro_int(5));
    assert_eq!(hydro_len(second), hydro_int(4));
    assert_eq!(hydro_exc_pending(), 0);
}

#[test]
fn reachable_tuple_elements_survive_collection() {
    hydro_runtime_init();
    let tuple = make_reachable_tuple();

    scrub_stack();
    let _freed = hydro_gc_collect();

    // The tuple is a root; its string children are only reachable through
    // the tuple, proving `Tuple` children are traced.
    assert_eq!(hydro_len(tuple), hydro_int(2));
    let first = hydro_subscript(tuple, hydro_int(0));
    let second = hydro_subscript(tuple, hydro_int(1));
    assert_eq!(hydro_len(first), hydro_int(5));
    assert_eq!(hydro_len(second), hydro_int(4));
    assert_eq!(hydro_exc_pending(), 0);
}

#[test]
fn unreachable_cycle_is_collected() {
    hydro_runtime_init();
    make_cycle();
    let before = hydro_gc_objects();
    assert!(before >= 1, "cycle was not allocated");

    scrub_stack();
    let freed = hydro_gc_collect();

    assert!(freed >= 1, "self-referential list was not collected");
    assert!(
        hydro_gc_objects() < before,
        "registry still contains the garbage cycle"
    );
}

#[test]
fn pending_exception_is_a_root() {
    hydro_runtime_init();
    let message = hydro_str_new(b"boom".as_ptr(), 4);
    assert_eq!(hydro_raise(0, message), hydro_none());
    assert_eq!(hydro_exc_pending(), 1);

    scrub_stack();
    let _freed = hydro_gc_collect();

    // The tagged pending exception survived and still carries its payload.
    assert_eq!(hydro_exc_pending(), 1);
    assert_eq!(hydro_exc_tag(), 0);
    let value = hydro_exc_value();
    assert!(value & 7 == 2, "pending value is not a heap object");
    let text = hydro_to_str(value);
    assert_eq!(hydro_len(text), hydro_int(4));
    hydro_exc_clear();
}

#[test]
fn runtime_init_clears_the_registry() {
    hydro_runtime_init();
    let _s = hydro_str_new(b"value".as_ptr(), 5);
    let _f = hydro_float_new(1.5);
    assert!(hydro_gc_objects() >= 2);

    hydro_runtime_init();

    assert_eq!(hydro_gc_objects(), 0);
    assert_eq!(hydro_exc_pending(), 0);
}
