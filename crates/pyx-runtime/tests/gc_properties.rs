//! Deterministic property-style tests for allocation and collection.
//!
//! A SplitMix64 generator drives allocation/mutation sequences. The tests
//! assert invariants that hold for every interleaving: reachable graphs keep
//! their contents, collection never grows the registry, and garbage is
//! reclaimed. Conservative stack scanning may retain a stale value, so the
//! assertions tolerate partial reclamation exactly like `gc.rs`.

use std::sync::Mutex;

use pyx_runtime::abi::*;

/// Serialises the tests: `pyx_runtime_init` touches process-wide state.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Small deterministic PRNG (SplitMix64).
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `0..n`; `n` must be positive.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn value(&mut self) -> i64 {
        (self.next_u64() % 1001) as i64 - 500
    }
}

/// Tagged integer. `pyx_int` is intentionally not used here: it treats every
/// low-bit-zero argument as already tagged, so raw multiples of 8 would be
/// misread (e.g. `8` as index `1`).
fn int(n: i64) -> i64 {
    n << 3
}

fn decode(value: i64) -> usize {
    assert_eq!(value & 7, 0, "expected a tagged integer");
    (pyx_to_int(value) >> 3) as usize
}

/// Overwrites stack below the caller so stale tagged values from helper frames
/// are unlikely to be found by the conservative scan.
#[inline(never)]
fn scrub_stack() {
    let mut buffer = [0u8; 64 * 1024];
    std::hint::black_box(&mut buffer);
    buffer[0] = 0;
    std::hint::black_box(&buffer);
}

/// Allocates a fresh string of `len` bytes.
fn make_text(len: usize) -> i64 {
    let bytes = vec![b'x'; len];
    pyx_str_new(bytes.as_ptr(), bytes.len())
}

/// Allocates a mixed batch and returns nothing: all tagged handles die here.
#[inline(never)]
fn make_garbage(rng: &mut SplitMix64, count: usize) {
    for _ in 0..count {
        match rng.below(5) {
            0 | 1 => {
                let _ = make_text(1 + rng.below(48));
            }
            2 => {
                let items = [int(rng.value()), int(rng.value())];
                let _ = pyx_list_new(items.len(), items.as_ptr());
            }
            3 => {
                let items = [int(rng.value())];
                let _ = pyx_tuple_new(items.len(), items.as_ptr());
            }
            _ => {
                let key = int(rng.value());
                let value = int(rng.value());
                let _ = pyx_dict_new(1, &key, &value);
            }
        }
    }
}

fn check_collection(freed: i64, before: i64, after: i64) {
    assert!(freed >= 0, "collector returned a negative count");
    assert!(freed <= before, "collector freed more objects than existed");
    assert!(after <= before, "registry grew during a sweep");
}

#[derive(Clone, Copy)]
enum Elem {
    Int(i64),
    Text(usize),
}

fn assert_elem(value: i64, elem: Elem) {
    match elem {
        Elem::Int(expected) => assert_eq!(value, int(expected)),
        Elem::Text(len) => assert_eq!(decode(pyx_len(value)), len),
    }
}

#[test]
fn reachable_graph_survives_randomized_churn() {
    let _guard = guard();
    pyx_runtime_init();

    let mut total_reclaimed = 0;
    for seed in [1, 2, 3, 0x5EED] {
        let mut rng = SplitMix64::new(seed);
        let mut model: Vec<Vec<Elem>> = Vec::new();
        let mut inner = [0i64; 5];

        for slot in inner.iter_mut() {
            let mut items = [0i64; 8];
            let mut child = Vec::new();
            for item in items.iter_mut().take(3) {
                let value = rng.value();
                *item = int(value);
                child.push(Elem::Int(value));
            }
            let len = 3 + rng.below(5);
            for item in items.iter_mut().take(len).skip(3) {
                let text_len = 1 + rng.below(24);
                *item = make_text(text_len);
                child.push(Elem::Text(text_len));
            }
            *slot = pyx_list_new(len, items.as_ptr());
            model.push(child);
        }
        let root = pyx_list_new(inner.len(), inner.as_ptr());
        // Only the root keeps the inner lists alive from now on.
        inner.fill(0);
        std::hint::black_box(&inner);

        let verify = |root: i64, model: &[Vec<Elem>]| {
            assert_eq!(decode(pyx_len(root)), model.len());
            for (slot, child) in model.iter().enumerate() {
                let list = pyx_subscript(root, int(slot as i64));
                assert_eq!(decode(pyx_len(list)), child.len());
                for (index, elem) in child.iter().enumerate() {
                    assert_elem(pyx_subscript(list, int(index as i64)), *elem);
                }
                assert_eq!(pyx_exc_pending(), 0);
            }
        };

        let mut reclaimed = 0;
        for step in 0..400 {
            let slot = rng.below(model.len());
            let list = pyx_subscript(root, int(slot as i64));
            match rng.below(10) {
                0..=3 => {
                    let value = rng.value();
                    assert_eq!(pyx_list_append(list, int(value)), pyx_none());
                    model[slot].push(Elem::Int(value));
                }
                4 | 5 => {
                    if !model[slot].is_empty() {
                        let index = rng.below(model[slot].len());
                        if rng.below(2) == 0 {
                            let value = rng.value();
                            assert_eq!(
                                pyx_subscript_set(list, int(index as i64), int(value)),
                                pyx_none()
                            );
                            model[slot][index] = Elem::Int(value);
                        } else {
                            let text_len = 1 + rng.below(24);
                            assert_eq!(
                                pyx_subscript_set(list, int(index as i64), make_text(text_len)),
                                pyx_none()
                            );
                            model[slot][index] = Elem::Text(text_len);
                        }
                    }
                }
                6 => {
                    if !model[slot].is_empty() {
                        let index = rng.below(model[slot].len());
                        assert_eq!(pyx_delete_subscript(list, int(index as i64)), pyx_none());
                        model[slot].remove(index);
                    }
                }
                _ => {
                    let count = 1 + rng.below(12);
                    make_garbage(&mut rng, count);
                }
            }
            if step % 25 == 0 {
                let before = pyx_gc_objects();
                scrub_stack();
                let freed = pyx_gc_collect();
                check_collection(freed, before, pyx_gc_objects());
                reclaimed += freed;
                verify(root, &model);
            }
        }
        let before = pyx_gc_objects();
        scrub_stack();
        let freed = pyx_gc_collect();
        check_collection(freed, before, pyx_gc_objects());
        reclaimed += freed;
        verify(root, &model);
        total_reclaimed += reclaimed;
    }
    assert!(
        total_reclaimed > 0,
        "no garbage was reclaimed during the churn"
    );
}

#[test]
fn allocation_and_collection_invariants() {
    let _guard = guard();
    pyx_runtime_init();

    let mut rng = SplitMix64::new(0xA11C);
    for _ in 0..16 {
        let count = 1 + rng.below(40);
        let mut items = [0i64; 40];
        let mut lengths = [0usize; 40];
        for (slot, item) in items.iter_mut().take(count).enumerate() {
            let len = 1 + rng.below(32);
            lengths[slot] = len;
            *item = make_text(len);
        }
        let root = pyx_list_new(count, items.as_ptr());
        items.fill(0);
        std::hint::black_box(&items);
        let expected: Vec<usize> = lengths[..count].to_vec();

        let garbage_count = 8 + rng.below(24);
        make_garbage(&mut rng, garbage_count);
        let before = pyx_gc_objects();
        assert!(
            before > count as i64,
            "allocations were not registered before collection"
        );
        scrub_stack();
        let freed = pyx_gc_collect();
        let after = pyx_gc_objects();
        check_collection(freed, before, after);

        assert_eq!(decode(pyx_len(root)), count);
        for (index, len) in expected.iter().enumerate() {
            let value = pyx_subscript(root, int(index as i64));
            assert_eq!(decode(pyx_len(value)), *len);
        }
        assert_eq!(pyx_exc_pending(), 0);
    }
}

#[test]
fn nested_lists_are_traced() {
    let _guard = guard();
    pyx_runtime_init();

    let mut rng = SplitMix64::new(0x1234);
    let depth = 3 + rng.below(6);
    let mut node = {
        let items = [int(rng.value()), make_text(8)];
        pyx_list_new(items.len(), items.as_ptr())
    };
    for level in 1..depth {
        let items = [node, int(level as i64)];
        node = pyx_list_new(items.len(), items.as_ptr());
    }
    let root = node;

    let before = pyx_gc_objects();
    scrub_stack();
    let freed = pyx_gc_collect();
    check_collection(freed, before, pyx_gc_objects());

    let mut cursor = root;
    for level in (1..depth).rev() {
        assert_eq!(decode(pyx_len(cursor)), 2);
        assert_eq!(pyx_subscript(cursor, int(1)), int(level as i64));
        cursor = pyx_subscript(cursor, int(0));
    }
    assert_eq!(decode(pyx_len(cursor)), 2);
    assert_eq!(decode(pyx_len(pyx_subscript(cursor, int(1)))), 8);
    assert_eq!(pyx_exc_pending(), 0);
}

#[test]
fn unreachable_cycles_are_collected() {
    let _guard = guard();
    pyx_runtime_init();

    let before = pyx_gc_objects();
    for round in 0..48 {
        let payload = make_text(4 + round % 13);
        let items = [payload, pyx_none()];
        let cycle = pyx_list_new(items.len(), items.as_ptr());
        // `x[1] = x`: only the cycle keeps the list reachable.
        pyx_subscript_set(cycle, int(1), cycle);
    }
    let peak = pyx_gc_objects();
    assert!(peak >= before, "registry shrank during allocations");

    scrub_stack();
    let freed = pyx_gc_collect();
    let after = pyx_gc_objects();
    check_collection(freed, peak, after);
    assert!(freed >= 1, "no cycle was reclaimed");
    assert_eq!(pyx_exc_pending(), 0);
}
