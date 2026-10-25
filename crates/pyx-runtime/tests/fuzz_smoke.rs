//! Dependency-free fuzz-smoke test for the runtime C ABI.
//!
//! A SplitMix64 generator drives random allocation/mutation/operator sequences
//! built only from values produced by the ABI itself (never raw integers that
//! could alias a heap pointer). Invariants checked after every step:
//! the runtime does not panic or abort, every pending exception exposes a
//! usable tag/value, and `pyx_exc_clear` always succeeds. Fixed seeds keep the
//! sequence reproducible; iteration counts stay inside a second.

use std::hint::black_box;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// Small integer in `-40..=40`, boxed as a tagged value.
    fn small(&mut self) -> i64 {
        tagged(self.below(81) as i64 - 40)
    }

    /// Uniform in `0..=0x110000`, the `chr` code-point domain.
    fn codepoint(&mut self) -> i64 {
        tagged((self.next_u64() % 0x0011_0000) as i64)
    }
}

/// Tagged integer built directly: `pyx_int` treats low-bit-zero arguments as
/// already tagged, so raw multiples of 8 would be misread.
fn tagged(n: i64) -> i64 {
    n << 3
}

/// UTF-8 view of a static name expected by the byte-pointer ABI.
fn bytes(text: &str) -> (*const u8, usize) {
    (text.as_ptr(), text.len())
}

/// Clears a pending exception and checks its observable state first.
/// Returns `true` when an exception was actually cleared.
fn drain(step: usize) -> bool {
    if pyx_exc_pending() == 0 {
        return false;
    }
    let tag = pyx_exc_tag();
    assert!(tag >= 0, "pending exception without a tag at step {step}");
    assert_ne!(
        pyx_exc_value(),
        0,
        "pending exception without a value at step {step}"
    );
    assert_eq!(
        pyx_exc_clear(),
        0,
        "pyx_exc_clear must return 0 at step {step}"
    );
    assert_eq!(
        pyx_exc_pending(),
        0,
        "exception still pending after clear at step {step}"
    );
    true
}

/// Asserts no exception remains pending after a clear.
fn assert_no_exception(step: usize) {
    assert_eq!(pyx_exc_pending(), 0, "unexpected exception at step {step}");
}

/// Builds the initial value pool. Values live in a stack array so the
/// conservative collector can see them across allocations.
fn pool(rng: &mut SplitMix64) -> [i64; 12] {
    let bigint = "123456789012345678901234567890";
    let text = "héllo 😀\u{200b}";
    let raw = [0u8, 0xff, b'd', b'a', b't', b'a'];
    [
        pyx_none(),
        pyx_true(),
        pyx_false(),
        tagged(7),
        tagged(-3),
        pyx_float_new(2.5),
        pyx_float_new(f64::from_bits(rng.next_u64())),
        pyx_str_new(text.as_ptr(), text.len()),
        pyx_bytes_new(raw.as_ptr(), raw.len()),
        pyx_bigint_from_str(bigint.as_ptr(), bigint.len()),
        pyx_range_new(tagged(0), tagged(9), pyx_none()),
        pyx_str_new(bytes("").0, bytes("").1),
    ]
}

/// Builds a small container of `count` pool elements.
fn container(rng: &mut SplitMix64, pool: &[i64], count: usize) -> i64 {
    let mut items = [0i64; 6];
    for slot in items.iter_mut().take(count) {
        *slot = pool[rng.below(pool.len())];
    }
    pyx_list_new(count, items.as_ptr())
}

#[test]
fn random_abi_sequences_keep_exception_state_clearable() {
    let _guard = guard();
    pyx_runtime_init();
    let start = Instant::now();

    let names = [
        "append",
        "add",
        "get",
        "pop",
        "keys",
        "items",
        "values",
        "count",
        "index",
        "upper",
        "encode",
        "decode",
        "split",
        "join",
        "replace",
        "strip",
        "startswith",
        "missing",
    ];
    let builtin_names = ["len", "abs", "str", "repr", "hash", "missing-builtin"];

    for seed in [1u64, 2, 3, 0x5EED, 0xC0FFEE] {
        let mut rng = SplitMix64::new(seed);
        let values = pool(&mut rng);
        let list = container(&mut rng, &values, 3);
        let dict = pyx_dict_new(0, std::ptr::null(), std::ptr::null());
        let set = pyx_set_new(0, std::ptr::null());
        let mut slots = [pyx_none(); 3];
        let mut cleared = 0usize;

        for step in 0..250 {
            // Keep the pool materialised on the stack so a collection in any
            // ABI call cannot reclaim the objects it references.
            black_box(&values);
            black_box(&list);
            black_box(&dict);
            black_box(&set);
            let pick = values[rng.below(values.len())];
            let other = values[rng.below(values.len())];
            let small = rng.small();

            match rng.below(26) {
                0 => {
                    pyx_list_append(list, pick);
                }
                1 => {
                    pyx_set_add(set, pick);
                }
                2 => {
                    pyx_dict_set(dict, pick, other);
                    pyx_dict_merge(dict, pick);
                }
                3 => {
                    pyx_subscript(pick, small);
                    pyx_subscript_set(list, small, pick);
                }
                4 => {
                    pyx_delete_subscript(list, small);
                    pyx_slice(list, pyx_none(), small, pyx_none());
                }
                5 => {
                    pyx_slice_set(list, pyx_none(), small, pyx_none(), pick);
                    pyx_delete_slice(list, pyx_none(), small, pyx_none());
                }
                6 => {
                    let name = rng.below(names.len());
                    let mut args = [pick, other];
                    let argc = rng.below(3);
                    pyx_method(
                        pick,
                        names[name].as_ptr(),
                        names[name].len(),
                        argc,
                        args.as_mut_ptr(),
                    );
                }
                7 => {
                    let name = rng.below(names.len());
                    let mut args = [small, pick];
                    pyx_method(
                        list,
                        names[name].as_ptr(),
                        names[name].len(),
                        1 + rng.below(2),
                        args.as_mut_ptr(),
                    );
                }
                8 => {
                    let name = rng.below(names.len());
                    pyx_getattr(pick, names[name].as_ptr(), names[name].len());
                    pyx_setattr(pick, names[name].as_ptr(), names[name].len(), other);
                    pyx_delete_attr(pick, names[name].as_ptr(), names[name].len());
                }
                9 => {
                    // Unknown op codes exercise the fallback error path.
                    let op = rng.below(35) as u32;
                    pyx_binop(op, small, rng.small());
                }
                10 => {
                    let op = rng.below(22) as u32;
                    pyx_binop(op, pick, small);
                    pyx_unop(rng.below(9) as u32, pick);
                }
                11 => {
                    pyx_len(pick);
                    pyx_truthy(pick);
                    pyx_hash(pick);
                }
                12 => {
                    pyx_to_str(pick);
                    pyx_to_repr(pick);
                    pyx_to_int(pick);
                    pyx_to_float(pick);
                }
                13 => {
                    pyx_ord(pick);
                    pyx_chr(rng.codepoint());
                    pyx_abs(pick);
                }
                14 => {
                    pyx_format_value(pick, std::ptr::null(), 0, 0);
                    pyx_format_value(pick, bytes("08.2f").0, 5, 0);
                    pyx_format_value(pick, bytes("%08.2f").0, 6, rng.below(5) as u8);
                }
                15 => {
                    let iter = pyx_iter_new(pick);
                    for _ in 0..3 {
                        let mut out = 0;
                        pyx_iter_next(iter, &mut out);
                    }
                    pyx_next(iter, other, 1);
                    pyx_iter(pick);
                }
                16 => {
                    pyx_unpack(pick, rng.below(4));
                    pyx_unpack_seq(pick, rng.below(4));
                    pyx_unpack_star(pick, rng.below(3), rng.below(3));
                }
                17 => {
                    let tag = rng.below(24) as u32;
                    pyx_raise(tag, pick);
                }
                18 => {
                    let tag = rng.below(24) as u32;
                    let text = "fuzz message \u{00e9}\u{1f600}";
                    pyx_raise_msg(tag, text.as_ptr(), text.len());
                }
                19 => {
                    pyx_raise_value(pick);
                    pyx_raise_value_from(pick, pyx_none());
                    pyx_raise_from(rng.below(24) as u32, pick, other);
                }
                20 => {
                    let depth = pyx_exc_handler_depth();
                    pyx_exc_handler_push(pyx_none());
                    pyx_exc_handler_pop();
                    pyx_exc_handler_truncate(depth);
                }
                21 => {
                    let name = rng.below(builtin_names.len());
                    let function = pyx_builtin_function(
                        builtin_names[name].as_ptr(),
                        builtin_names[name].len(),
                    );
                    // Unknown names leave an exception; a call would only
                    // re-raise through a dummy `None` receiver.
                    drain(step);
                    slots[0] = pick;
                    let argc = rng.below(2);
                    pyx_call(function, argc, slots.as_ptr());
                    pyx_call_star(function, argc, slots.as_ptr(), list);
                }
                22 => {
                    pyx_all(pick);
                    pyx_list_from(pick);
                    pyx_tuple_from(pick);
                    pyx_set_from(pick);
                    pyx_dict_from(pick);
                    pyx_sum(pick, small);
                }
                23 => {
                    pyx_enumerate(list, small);
                    let zip_args = [list, values[0]];
                    pyx_zip(2, zip_args.as_ptr());
                    pyx_map(pick, list);
                    pyx_filter(pick, list);
                }
                24 => {
                    pyx_range_new(small, rng.small(), pyx_none());
                    pyx_exception_class(rng.below(24) as u32);
                    pyx_exc_matches(rng.below(16) as u32);
                }
                _ => {
                    let file = bytes("fuzz.py");
                    let frame = bytes("fuzz");
                    pyx_set_file(file.0, file.1);
                    pyx_set_line(rng.below(10_000) as u32);
                    pyx_frame_enter(frame.0, frame.1);
                    pyx_frame_leave();
                    pyx_safepoint();
                    if step % 97 == 0 {
                        assert!(pyx_gc_collect() >= 0);
                    }
                }
            }
            if drain(step) {
                cleared += 1;
            }
            assert_no_exception(step);
        }
        assert!(
            cleared > 0,
            "seed {seed} never raised: the exception paths were not exercised"
        );
        // Objects accumulated during the sequence must be reclaimable.
        let before = pyx_gc_objects();
        assert!(before >= 0);
        assert!(pyx_gc_collect() >= 0);
        assert!(pyx_gc_objects() <= before);
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "runtime fuzz took {elapsed:?}, over the 2 s smoke budget"
    );
}

/// Random raise/clear cycles (including unknown tags) always unwind cleanly.
#[test]
fn random_raise_and_clear_cycles_are_balanced() {
    let _guard = guard();
    pyx_runtime_init();
    let start = Instant::now();

    let mut rng = SplitMix64::new(0xA11CE);
    let values = pool(&mut rng);
    for step in 0..600 {
        black_box(&values);
        let tag = rng.below(32) as u32;
        let value = values[rng.below(values.len())];
        match rng.below(3) {
            0 => {
                pyx_raise(tag, value);
            }
            1 => {
                let text = "\u{1f600} fuzz";
                pyx_raise_msg(tag, text.as_ptr(), text.len());
            }
            _ => {
                pyx_raise_from(tag, pyx_str_new(bytes("cause").0, 6), value);
            }
        }
        assert_eq!(pyx_exc_pending(), 1, "raise did not publish an exception");
        assert!(drain(step), "drain did not clear the raised exception");
        assert_no_exception(step);
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "raise cycles took {elapsed:?}, over the 2 s smoke budget"
    );
}
