//! Integration tests for the slab pool that backs the interim collector.
//!
//! The pool itself is private to the runtime, so these tests observe it
//! through the tagged heap pointers returned by the ABI: `value & !7` is the
//! address of the `PyxObj` cell, and the low three bits carry `TAG_HEAP`
//! (`0b010`). Addresses are kept as `usize`, whose low bits are zero, so a
//! stale copy on the stack cannot be mistaken for a conservative root by the
//! collector.
//!
//! Each test runs on its own thread (and therefore on its own thread-local
//! heap). `TEST_LOCK` serialises the runtime-global setup steps
//! (`pyx_runtime_init`) so the tests cannot reset each other's state.

use std::collections::HashSet;
use std::sync::Mutex;

use pyx_runtime::abi::*;

/// Tag bits of a heap value; a valid heap pointer has `value & TAG_MASK == 2`.
const TAG_MASK: usize = 0b111;

/// Serialises the tests: `pyx_runtime_init` touches process-wide state.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Overwrites a large portion of the stack below the caller, so stale tagged
/// values from earlier helper frames are unlikely to be found by the
/// conservative scan. `inline(never)` keeps the frame materialised.
#[inline(never)]
fn scrub_stack() {
    let mut buffer = [0u8; 64 * 1024];
    std::hint::black_box(&mut buffer);
    buffer[0] = 0;
    std::hint::black_box(&buffer);
}

/// Allocates `count` strings and returns the raw cell addresses.
///
/// Only untagged addresses leave this frame: the tagged values are
/// temporaries, and the returned `Vec` lives on the Rust heap (which the
/// collector does not scan).
#[inline(never)]
fn allocate_addresses(count: usize, text: &[u8]) -> Vec<usize> {
    let mut addresses = Vec::with_capacity(count);
    for _ in 0..count {
        let value = pyx_str_new(text.as_ptr(), text.len());
        addresses.push((value as usize) & !TAG_MASK);
    }
    addresses
}

/// Allocates `count` strings and returns their tagged values.
#[inline(never)]
fn allocate_values(count: usize, text: &[u8]) -> Vec<i64> {
    (0..count)
        .map(|_| pyx_str_new(text.as_ptr(), text.len()))
        .collect()
}

/// Builds a reachable list of `count` immediates.
#[inline(never)]
fn build_list(count: usize) -> i64 {
    let items: Vec<i64> = (0..count).map(|index| (index as i64) << 3).collect();
    pyx_list_new(items.len(), items.as_ptr())
}

#[test]
fn pool_cells_are_eight_byte_aligned() {
    let _guard = guard();
    pyx_runtime_init();

    for _ in 0..256 {
        let value = pyx_str_new(b"aligned".as_ptr(), 7);
        assert_eq!(
            value as usize & TAG_MASK,
            0b010,
            "string value is not a tagged heap pointer"
        );
        let address = (value as usize) & !TAG_MASK;
        assert_ne!(address, 0, "allocator returned a null cell");
        assert_eq!(
            address % 8,
            0,
            "pool cell {address:#x} violates the collector's 8-byte alignment"
        );
        assert_eq!(pyx_len(value), 7 << 3);
    }
    assert_eq!(pyx_exc_pending(), 0);
}

#[test]
fn sweep_returns_cells_to_the_free_list() {
    let _guard = guard();
    pyx_runtime_init();

    let old = allocate_addresses(48, b"old");
    let before = pyx_gc_objects();
    assert!(before > 0, "no object was allocated");

    scrub_stack();
    let freed = pyx_gc_collect();
    assert!(freed <= before, "collector freed more objects than existed");
    assert!(pyx_gc_objects() <= before, "registry grew during a sweep");

    // The sweep pushes every freed cell onto the free list. `PYX_GC_STRESS`
    // may already have reclaimed most of the batch, so the reuse assertion is
    // tied to the cells this explicit collection actually freed.
    let old_set: HashSet<usize> = old.iter().copied().collect();
    if freed > 0 {
        let recycled = allocate_addresses(freed as usize, b"new");
        assert_eq!(recycled.len(), freed as usize);
        for address in &recycled {
            assert!(
                old_set.contains(address),
                "allocation {address:#x} did not reuse a swept cell"
            );
        }
        let unique: HashSet<usize> = recycled.iter().copied().collect();
        assert_eq!(
            unique.len(),
            recycled.len(),
            "pool handed out the same cell twice"
        );
    }

    // A recycled cell must be fully reinitialised, not leak its old payload.
    for value in allocate_values(16, b"recycled-payload") {
        assert_eq!(
            value as usize & TAG_MASK,
            0b010,
            "string value is not a tagged heap pointer"
        );
        assert_eq!(
            (value as usize & !TAG_MASK) % 8,
            0,
            "recycled cell is not 8-byte aligned"
        );
        assert_eq!(pyx_len(value), 16 << 3);
    }
    assert_eq!(pyx_exc_pending(), 0);
}

#[test]
fn runtime_init_recycles_cells_without_returning_slabs() {
    let _guard = guard();
    pyx_runtime_init();

    let old = allocate_addresses(16, b"before");
    let before = pyx_gc_objects() as usize;
    assert!(before > 0, "no object was allocated");

    // `pyx_runtime_init` releases every object into the pool's free list but
    // keeps its slabs for the next program run in this process.
    pyx_runtime_init();
    assert_eq!(pyx_gc_objects(), 0);

    let after = allocate_addresses(16, b"after");
    assert_eq!(after.len(), old.len());
    let old_set: HashSet<usize> = old.iter().copied().collect();
    for address in &after {
        assert!(
            old_set.contains(address),
            "free_all did not recycle cell {address:#x}"
        );
    }

    // Without PYX_GC_STRESS no automatic collection ran, so `free_all`
    // released all 16 cells in registry order and the LIFO free list hands
    // them back in reverse.
    if before == old.len() {
        for (index, address) in after.iter().enumerate() {
            assert_eq!(
                *address,
                old[old.len() - 1 - index],
                "free_all did not recycle cell {index} in LIFO order"
            );
        }
    }

    // Slab memory must still be usable for payloads after the reset.
    for value in allocate_values(8, b"after-reset") {
        assert_eq!(pyx_len(value), 11 << 3);
    }
    assert_eq!(pyx_exc_pending(), 0);
}

#[test]
fn collector_survives_many_allocations_and_sweeps() {
    let _guard = guard();
    pyx_runtime_init();

    // More than one collection threshold, so the pool grows across several
    // slabs and the automatic collections exercise the free list.
    let addresses = allocate_addresses(12_000, b"bulk");
    assert_eq!(addresses.len(), 12_000);

    let reachable = build_list(200);
    let before = pyx_gc_objects();
    assert!(before > 0, "no object was allocated");

    scrub_stack();
    let freed = pyx_gc_collect();
    assert!(freed <= before, "collector freed more objects than existed");
    assert!(pyx_gc_objects() <= before, "registry grew during the sweep");

    // The live list and its elements survived the sweep.
    assert_eq!(pyx_len(reachable), 200 << 3);
    assert_eq!(pyx_subscript(reachable, 0 << 3), 0 << 3);
    assert_eq!(pyx_subscript(reachable, 199 << 3), 199 << 3);

    // The pool still serves new objects correctly after the large sweep.
    for value in allocate_values(64, b"after-sweep") {
        assert_eq!(
            (value as usize & !TAG_MASK) % 8,
            0,
            "post-sweep cell is not 8-byte aligned"
        );
        assert_eq!(pyx_len(value), 11 << 3);
    }
    assert_eq!(pyx_exc_pending(), 0);
}
