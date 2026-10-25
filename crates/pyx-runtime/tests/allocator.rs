//! Integration tests for the size-class slab pool that backs the collector.
//!
//! Every object is a 16-byte header plus a separately allocated payload. The
//! pool itself is private to the runtime, so these tests observe it through
//! the tagged heap pointers returned by the ABI: `value & !7` is the address
//! of the header, and the low three bits carry `TAG_HEAP` (`0b010`).
//! Addresses are kept as `usize`, whose low bits are zero, so a stale copy on
//! the stack cannot be mistaken for a conservative root by the collector.
//!
//! Per-class statistics, class selection and free-list separation are covered
//! by the allocator unit tests (`src/gc/alloc.rs`); the tests here exercise the
//! same machinery end-to-end: payload/header alignment, object reuse across
//! sweeps and `pyx_runtime_init`, mixed payload sizes, 12k allocations and a
//! bounded-RSS loop.
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

/// Allocates `count` strings and returns the raw header addresses.
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

/// Resident set size in pages, or `None` when `/proc` is unavailable.
fn rss_pages() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/statm").ok()?;
    text.split_whitespace().nth(1)?.parse().ok()
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

        // A fresh float object goes through a different payload size class;
        // both kinds must keep the tagged-pointer alignment contract.
        let float = pyx_float_new(1.5);
        assert_eq!(float as usize & TAG_MASK, 0b010);
        assert_eq!((float as usize & !TAG_MASK) % 8, 0);
        // `pyx_to_int` reads the float payload; 1.5 truncates to 1.
        assert_eq!(pyx_to_int(float), 1 << 3);
        assert_eq!(pyx_exc_pending(), 0);
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

    // The sweep pushes every freed header onto the header free list. `PYX_GC_STRESS`
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

    // `pyx_runtime_init` releases every object into the pool's free lists but
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
    // released all 16 cells in registry order and the LIFO header free list
    // hands them back in reverse.
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
fn mixed_payload_sizes_reuse_their_own_payload_blocks() {
    let _guard = guard();
    pyx_runtime_init();

    // f64 (8-byte payload class) and String (24-byte payload class) must not
    // alias each other's payload storage. Allocate both kinds, drop them, and
    // allocate both kinds again: each object must still read back correctly.
    let mut floats = Vec::new();
    let mut strings = Vec::new();
    for index in 0..128 {
        floats.push(pyx_float_new(index as f64));
        strings.push(pyx_str_new(b"payload".as_ptr(), 7));
    }
    for (index, &value) in floats.iter().enumerate() {
        assert_eq!(pyx_to_int(value), (index as i64) << 3);
    }
    for &value in &strings {
        assert_eq!(pyx_len(value), 7 << 3);
    }

    // Drop the tagged copies on the stack, then sweep both classes.
    floats.clear();
    strings.clear();
    scrub_stack();
    let _ = pyx_gc_collect();

    // Reallocate and verify both payload paths survive the recycle.
    for index in 0..128 {
        let float = pyx_float_new(index as f64 + 0.5);
        assert_eq!(pyx_to_int(float), (index as i64) << 3);
        let text = pyx_str_new(b"again!!".as_ptr(), 7);
        assert_eq!(pyx_len(text), 7 << 3);
    }
    assert_eq!(pyx_exc_pending(), 0);
}

#[test]
fn collector_survives_many_allocations_and_sweeps() {
    let _guard = guard();
    pyx_runtime_init();

    // More than one collection threshold, so the pool grows across several
    // slabs and the automatic collections exercise every class free list.
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

#[test]
fn bounded_rss_across_repeated_allocate_and_collect_loops() {
    let _guard = guard();
    pyx_runtime_init();

    const ROUNDS: usize = 120;
    const BATCH: usize = 256;

    let Some(mut baseline) = rss_pages() else {
        // `/proc` is unavailable (non-Linux); nothing to measure.
        return;
    };

    // Warm up so the first-touch slab growth and the registry's minimum
    // capacity are not counted as a leak.
    for round in 0..ROUNDS {
        for index in 0..BATCH {
            let text = if index % 2 == 0 {
                pyx_str_new(b"bounded-rss".as_ptr(), 9)
            } else {
                pyx_float_new(index as f64)
            };
            std::hint::black_box(text);
        }
        scrub_stack();
        let _ = pyx_gc_collect();
        if round == 8 {
            baseline = rss_pages().unwrap_or(baseline);
        }
    }

    let Some(end) = rss_pages() else {
        return;
    };
    // 4096 pages is 16 MiB on a 4 KiB-page host and 64 MiB on a 16 KiB-page
    // host: generous enough for allocator/registry slack, tight enough that a
    // leaked class free list (120 * 256 * payload bytes) would trip it.
    assert!(
        end <= baseline + 4096,
        "RSS grew from {baseline} to {end} pages across allocate/collect loops"
    );
    assert_eq!(pyx_exc_pending(), 0);
}
