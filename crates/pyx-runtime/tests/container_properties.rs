//! Deterministic property-style tests for the container ABI.
//!
//! A SplitMix64 generator drives random operation sequences on lists, dicts,
//! sets and tuples. Every runtime operation is mirrored by a Rust model and the
//! observable state (`len`, subscripts, membership, iteration) is compared
//! after each step. No external crates or randomized test framework.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use pyx_runtime::abi::*;

/// Serialises the tests: `pyx_runtime_init` touches process-wide state.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Small deterministic PRNG (SplitMix64); the same seed reproduces every op.
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

    /// Small integer in `-500..=500`.
    fn value(&mut self) -> i64 {
        (self.next_u64() % 1001) as i64 - 500
    }
}

/// `in` opcode from the frozen ABI (docs/ARCHITECTURE §3).
const OP_IN: u32 = 13;

/// Tagged integer. `pyx_int` is intentionally not used here: it treats every
/// low-bit-zero argument as already tagged, so raw multiples of 8 would be
/// misread (e.g. `8` as index `1`).
fn int(n: i64) -> i64 {
    n << 3
}

fn decode_len(value: i64) -> usize {
    assert_eq!(value & 7, 0, "len must return a tagged integer");
    (pyx_to_int(value) >> 3) as usize
}

/// Sequence contents as tagged values.
fn sequence_values(sequence: i64) -> Vec<i64> {
    let len = decode_len(pyx_len(sequence));
    (0..len)
        .map(|index| pyx_subscript(sequence, int(index as i64)))
        .collect()
}

/// Set contents via the public iterator protocol.
fn set_values(set: i64) -> HashSet<i64> {
    let iterator = pyx_iter_new(set);
    let mut values = HashSet::new();
    loop {
        let mut value = 0;
        if pyx_iter_next(iterator, &mut value) == 0 {
            break;
        }
        assert!(values.insert(value), "iterator yielded a duplicate");
    }
    values
}

fn assert_list(list: i64, model: &[i64]) {
    assert_eq!(decode_len(pyx_len(list)), model.len());
    for (index, expected) in model.iter().enumerate() {
        assert_eq!(
            pyx_subscript(list, int(index as i64)),
            *expected,
            "list[{index}]"
        );
    }
    if let Some(last) = model.last() {
        assert_eq!(pyx_subscript(list, int(-1)), *last, "list[-1]");
    }
}

fn assert_dict(dict: i64, model: &HashMap<i64, i64>) {
    assert_eq!(decode_len(pyx_len(dict)), model.len());
    for (key, value) in model {
        assert_eq!(pyx_subscript(dict, *key), *value, "dict[{key}]");
    }
}

fn assert_set(set: i64, model: &HashSet<i64>) {
    assert_eq!(decode_len(pyx_len(set)), model.len());
    assert_eq!(set_values(set), *model);
    for key in -12..=12 {
        let tagged = int(key);
        let expected = model.contains(&tagged);
        assert_eq!(
            pyx_binop(OP_IN, tagged, set),
            if expected { pyx_true() } else { pyx_false() },
            "membership of {key}"
        );
    }
}

#[test]
fn list_operations_match_vec_model() {
    let _guard = guard();
    pyx_runtime_init();

    for seed in [1, 2, 3, 0x5EED, 0xDEAD_BEEF] {
        let mut rng = SplitMix64::new(seed);
        let mut model: Vec<i64> = Vec::new();
        let mut items = [0i64; 12];
        let initial = rng.below(13);
        for slot in items.iter_mut().take(initial) {
            let value = int(rng.value());
            *slot = value;
            model.push(value);
        }
        let list = pyx_list_new(model.len(), items.as_ptr());
        assert_list(list, &model);

        for step in 0..600 {
            match rng.below(10) {
                0..=3 => {
                    let value = int(rng.value());
                    assert_eq!(pyx_list_append(list, value), pyx_none());
                    model.push(value);
                }
                4 | 5 => {
                    if !model.is_empty() {
                        let index = rng.below(model.len());
                        let value = int(rng.value());
                        assert_eq!(
                            pyx_subscript_set(list, int(index as i64), value),
                            pyx_none()
                        );
                        model[index] = value;
                    }
                }
                6 | 7 => {
                    if !model.is_empty() {
                        let index = rng.below(model.len());
                        assert_eq!(pyx_delete_subscript(list, int(index as i64)), pyx_none());
                        model.remove(index);
                    }
                }
                8 => {
                    if !model.is_empty() {
                        let index = rng.below(model.len());
                        assert_eq!(
                            pyx_subscript(list, int(-(index as i64) - 1)),
                            model[model.len() - index - 1]
                        );
                    }
                }
                _ => {
                    let start = rng.below(model.len() + 1);
                    let stop = start + rng.below(model.len() - start + 1);
                    let sliced = pyx_slice(list, int(start as i64), int(stop as i64), pyx_none());
                    assert_eq!(sequence_values(sliced), model[start..stop].to_vec());
                }
            }
            if step % 29 == 0 {
                assert_list(list, &model);
                assert_eq!(pyx_exc_pending(), 0);
            }
        }
        assert_list(list, &model);
        assert_eq!(pyx_exc_pending(), 0);
    }
}

#[test]
fn dict_operations_match_hashmap_model() {
    let _guard = guard();
    pyx_runtime_init();

    for seed in [4, 5, 6, 0xC0FFEE] {
        let mut rng = SplitMix64::new(seed);
        let mut model: HashMap<i64, i64> = HashMap::new();
        let mut keys = [0i64; 12];
        let mut values = [0i64; 12];
        let initial = rng.below(13);
        for (key_slot, value_slot) in keys.iter_mut().zip(values.iter_mut()).take(initial) {
            let key = int(rng.below(8) as i64);
            let value = int(rng.value());
            *key_slot = key;
            *value_slot = value;
            model.insert(key, value);
        }
        // Duplicate literal keys keep the last value; the model does the same.
        let dict = pyx_dict_new(initial, keys.as_ptr(), values.as_ptr());
        assert_dict(dict, &model);

        for step in 0..500 {
            let key = int(rng.below(12) as i64);
            match rng.below(5) {
                0..=2 => {
                    let value = int(rng.value());
                    assert_eq!(pyx_dict_set(dict, key, value), pyx_none());
                    model.insert(key, value);
                }
                3 => {
                    if model.remove(&key).is_some() {
                        assert_eq!(pyx_delete_subscript(dict, key), pyx_none());
                        assert_eq!(pyx_exc_pending(), 0);
                    } else {
                        assert_eq!(pyx_delete_subscript(dict, key), pyx_none());
                        assert_eq!(pyx_exc_pending(), 1, "missing key must raise");
                        let _ = pyx_exc_clear();
                    }
                }
                _ => {
                    let expected = model.contains_key(&key);
                    assert_eq!(
                        pyx_binop(OP_IN, key, dict),
                        if expected { pyx_true() } else { pyx_false() }
                    );
                    if let Some(value) = model.get(&key) {
                        assert_eq!(pyx_subscript(dict, key), *value);
                    }
                }
            }
            if step % 37 == 0 {
                assert_dict(dict, &model);
                assert_eq!(pyx_exc_pending(), 0);
            }
        }
        assert_dict(dict, &model);
        assert_eq!(pyx_exc_pending(), 0);
    }
}

#[test]
fn set_operations_match_hashset_model() {
    let _guard = guard();
    pyx_runtime_init();

    for seed in [7, 8, 9, 0x0BAD_C0DE] {
        let mut rng = SplitMix64::new(seed);
        let mut model: HashSet<i64> = HashSet::new();
        let mut items = [0i64; 16];
        let initial = rng.below(17);
        for slot in items.iter_mut().take(initial) {
            let value = int(rng.below(10) as i64);
            *slot = value;
            model.insert(value);
        }
        let set = pyx_set_new(initial, items.as_ptr());
        let literal_model = model.clone();
        assert_set(set, &model);

        for step in 0..400 {
            let value = int(rng.below(10) as i64);
            match rng.below(4) {
                0 | 1 => {
                    assert_eq!(pyx_set_add(set, value), pyx_none());
                    model.insert(value);
                }
                _ => {
                    let name = b"discard";
                    assert_eq!(
                        pyx_method(set, name.as_ptr(), name.len(), 1, &value as *const i64),
                        pyx_none()
                    );
                    model.remove(&value);
                }
            }
            if step % 31 == 0 {
                assert_set(set, &model);
            }
        }
        assert_set(set, &model);
        assert_eq!(pyx_exc_pending(), 0);

        // `set(list)` must agree with the model built from the same buffer.
        let list = pyx_list_new(initial, items.as_ptr());
        assert_eq!(set_values(pyx_set_from(list)), literal_model);
    }
}

#[test]
fn tuple_operations_match_vec_model() {
    let _guard = guard();
    pyx_runtime_init();

    for seed in [10, 11, 12, 0xABBA] {
        let mut rng = SplitMix64::new(seed);
        let mut model: Vec<i64> = Vec::new();
        let mut items = [0i64; 12];
        let len = rng.below(13);
        for slot in items.iter_mut().take(len) {
            let value = int(rng.value());
            *slot = value;
            model.push(value);
        }
        let tuple = pyx_tuple_new(len, items.as_ptr());
        assert_eq!(sequence_values(tuple), model);
        if let Some(last) = model.last() {
            assert_eq!(pyx_subscript(tuple, int(-1)), *last);
        }

        let start = rng.below(len + 1);
        let stop = start + rng.below(len - start + 1);
        let sliced = pyx_slice(tuple, int(start as i64), int(stop as i64), pyx_none());
        assert_eq!(sequence_values(sliced), model[start..stop].to_vec());

        // Tuples are immutable; the failed assignment must not change them.
        assert_eq!(pyx_subscript_set(tuple, int(0), int(99)), pyx_none());
        assert_eq!(pyx_exc_pending(), 1);
        let _ = pyx_exc_clear();
        assert_eq!(sequence_values(tuple), model);
        assert_eq!(pyx_exc_pending(), 0);

        // Constructors copy their input buffer.
        items[0] = int(1234);
        assert_eq!(items[0], int(1234));
        assert_eq!(sequence_values(tuple), model);
    }
}

#[test]
fn literals_copy_input_buffers() {
    let _guard = guard();
    pyx_runtime_init();

    let mut items = [int(1), int(2), int(3)];
    let list = pyx_list_new(items.len(), items.as_ptr());
    let tuple = pyx_tuple_new(items.len(), items.as_ptr());
    items[0] = int(99);
    items[2] = int(-99);
    assert_eq!(items[0], int(99));
    assert_eq!(items[2], int(-99));
    assert_eq!(sequence_values(list), vec![int(1), int(2), int(3)]);
    assert_eq!(sequence_values(tuple), vec![int(1), int(2), int(3)]);
    assert_eq!(pyx_exc_pending(), 0);

    // Empty literals accept a null buffer (defensive ABI contract).
    assert_eq!(decode_len(pyx_len(pyx_list_new(0, std::ptr::null()))), 0);
    assert_eq!(decode_len(pyx_len(pyx_tuple_new(0, std::ptr::null()))), 0);
    assert_eq!(
        decode_len(pyx_len(pyx_dict_new(0, std::ptr::null(), std::ptr::null()))),
        0
    );
    assert_eq!(decode_len(pyx_len(pyx_set_new(0, std::ptr::null()))), 0);
}
