//! Dependency-free fuzz-smoke test for the runtime C ABI.
//!
//! A SplitMix64 generator drives random allocation/mutation/operator sequences
//! built only from values produced by the ABI itself (never raw integers that
//! could alias a heap pointer). Invariants checked after every step:
//! the runtime does not panic or abort, every pending exception exposes a
//! usable tag/value, and `hydro_exc_clear` always succeeds. Fixed seeds keep the
//! sequence reproducible; iteration counts stay inside a second.

use std::hint::black_box;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hydro_runtime::abi::*;

/// Serialises the tests: `hydro_runtime_init` touches process-wide state.
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

/// Tagged integer built directly: `hydro_int` treats low-bit-zero arguments as
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
    if hydro_exc_pending() == 0 {
        return false;
    }
    let tag = hydro_exc_tag();
    assert!(tag >= 0, "pending exception without a tag at step {step}");
    assert_ne!(
        hydro_exc_value(),
        0,
        "pending exception without a value at step {step}"
    );
    assert_eq!(
        hydro_exc_clear(),
        0,
        "hydro_exc_clear must return 0 at step {step}"
    );
    assert_eq!(
        hydro_exc_pending(),
        0,
        "exception still pending after clear at step {step}"
    );
    true
}

/// Asserts no exception remains pending after a clear.
fn assert_no_exception(step: usize) {
    assert_eq!(
        hydro_exc_pending(),
        0,
        "unexpected exception at step {step}"
    );
}

/// Builds the initial value pool. Values live in a stack array so the
/// conservative collector can see them across allocations.
fn pool(rng: &mut SplitMix64) -> [i64; 12] {
    let bigint = "123456789012345678901234567890";
    let text = "héllo 😀\u{200b}";
    let raw = [0u8, 0xff, b'd', b'a', b't', b'a'];
    [
        hydro_none(),
        hydro_true(),
        hydro_false(),
        tagged(7),
        tagged(-3),
        hydro_float_new(2.5),
        hydro_float_new(f64::from_bits(rng.next_u64())),
        hydro_str_new(text.as_ptr(), text.len()),
        hydro_bytes_new(raw.as_ptr(), raw.len()),
        hydro_bigint_from_str(bigint.as_ptr(), bigint.len()),
        hydro_range_new(tagged(0), tagged(9), hydro_none()),
        hydro_str_new(bytes("").0, bytes("").1),
    ]
}

/// Builds a small container of `count` pool elements.
fn container(rng: &mut SplitMix64, pool: &[i64], count: usize) -> i64 {
    let mut items = [0i64; 6];
    for slot in items.iter_mut().take(count) {
        *slot = pool[rng.below(pool.len())];
    }
    hydro_list_new(count, items.as_ptr())
}

#[test]
fn random_abi_sequences_keep_exception_state_clearable() {
    let _guard = guard();
    hydro_runtime_init();
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
        let dict = hydro_dict_new(0, std::ptr::null(), std::ptr::null());
        let set = hydro_set_new(0, std::ptr::null());
        let mut slots = [hydro_none(); 3];
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
                    hydro_list_append(list, pick);
                }
                1 => {
                    hydro_set_add(set, pick);
                }
                2 => {
                    hydro_dict_set(dict, pick, other);
                    hydro_dict_merge(dict, pick);
                }
                3 => {
                    hydro_subscript(pick, small);
                    hydro_subscript_set(list, small, pick);
                }
                4 => {
                    hydro_delete_subscript(list, small);
                    hydro_slice(list, hydro_none(), small, hydro_none());
                }
                5 => {
                    hydro_slice_set(list, hydro_none(), small, hydro_none(), pick);
                    hydro_delete_slice(list, hydro_none(), small, hydro_none());
                }
                6 => {
                    let name = rng.below(names.len());
                    let mut args = [pick, other];
                    let argc = rng.below(3);
                    hydro_method(
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
                    hydro_method(
                        list,
                        names[name].as_ptr(),
                        names[name].len(),
                        1 + rng.below(2),
                        args.as_mut_ptr(),
                    );
                }
                8 => {
                    let name = rng.below(names.len());
                    hydro_getattr(pick, names[name].as_ptr(), names[name].len());
                    hydro_setattr(pick, names[name].as_ptr(), names[name].len(), other);
                    hydro_delete_attr(pick, names[name].as_ptr(), names[name].len());
                }
                9 => {
                    // Unknown op codes exercise the fallback error path.
                    let op = rng.below(35) as u32;
                    hydro_binop(op, small, rng.small());
                }
                10 => {
                    let op = rng.below(22) as u32;
                    hydro_binop(op, pick, small);
                    hydro_unop(rng.below(9) as u32, pick);
                }
                11 => {
                    hydro_len(pick);
                    hydro_truthy(pick);
                    hydro_hash(pick);
                }
                12 => {
                    hydro_to_str(pick);
                    hydro_to_repr(pick);
                    hydro_to_int(pick);
                    hydro_to_float(pick);
                }
                13 => {
                    hydro_ord(pick);
                    hydro_chr(rng.codepoint());
                    hydro_abs(pick);
                }
                14 => {
                    hydro_format_value(pick, std::ptr::null(), 0, 0);
                    hydro_format_value(pick, bytes("08.2f").0, 5, 0);
                    hydro_format_value(pick, bytes("%08.2f").0, 6, rng.below(5) as u8);
                }
                15 => {
                    let iter = hydro_iter_new(pick);
                    for _ in 0..3 {
                        let mut out = 0;
                        hydro_iter_next(iter, &mut out);
                    }
                    hydro_next(iter, other, 1);
                    hydro_iter(pick);
                }
                16 => {
                    hydro_unpack(pick, rng.below(4));
                    hydro_unpack_seq(pick, rng.below(4));
                    hydro_unpack_star(pick, rng.below(3), rng.below(3));
                }
                17 => {
                    let tag = rng.below(24) as u32;
                    hydro_raise(tag, pick);
                }
                18 => {
                    let tag = rng.below(24) as u32;
                    let text = "fuzz message \u{00e9}\u{1f600}";
                    hydro_raise_msg(tag, text.as_ptr(), text.len());
                }
                19 => {
                    hydro_raise_value(pick);
                    hydro_raise_value_from(pick, hydro_none());
                    hydro_raise_from(rng.below(24) as u32, pick, other);
                }
                20 => {
                    let depth = hydro_exc_handler_depth();
                    hydro_exc_handler_push(hydro_none());
                    hydro_exc_handler_pop();
                    hydro_exc_handler_truncate(depth);
                }
                21 => {
                    let name = rng.below(builtin_names.len());
                    let function = hydro_builtin_function(
                        builtin_names[name].as_ptr(),
                        builtin_names[name].len(),
                    );
                    // Unknown names leave an exception; a call would only
                    // re-raise through a dummy `None` receiver.
                    drain(step);
                    slots[0] = pick;
                    let argc = rng.below(2);
                    hydro_call(function, argc, slots.as_ptr());
                    hydro_call_star(function, argc, slots.as_ptr(), list);
                }
                22 => {
                    hydro_all(pick);
                    hydro_list_from(pick);
                    hydro_tuple_from(pick);
                    hydro_set_from(pick);
                    hydro_dict_from(pick);
                    hydro_sum(pick, small);
                }
                23 => {
                    hydro_enumerate(list, small);
                    let zip_args = [list, values[0]];
                    hydro_zip(2, zip_args.as_ptr());
                    hydro_map(pick, list);
                    hydro_filter(pick, list);
                }
                24 => {
                    hydro_range_new(small, rng.small(), hydro_none());
                    hydro_exception_class(rng.below(24) as u32);
                    hydro_exc_matches(rng.below(16) as u32);
                }
                _ => {
                    let file = bytes("fuzz.py");
                    let frame = bytes("fuzz");
                    hydro_set_file(file.0, file.1);
                    hydro_set_line(rng.below(10_000) as u32);
                    hydro_frame_enter(frame.0, frame.1);
                    hydro_frame_leave();
                    hydro_safepoint();
                    if step % 97 == 0 {
                        assert!(hydro_gc_collect() >= 0);
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
        let before = hydro_gc_objects();
        assert!(before >= 0);
        assert!(hydro_gc_collect() >= 0);
        assert!(hydro_gc_objects() <= before);
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
    hydro_runtime_init();
    let start = Instant::now();

    let mut rng = SplitMix64::new(0xA11CE);
    let values = pool(&mut rng);
    for step in 0..600 {
        black_box(&values);
        let tag = rng.below(32) as u32;
        let value = values[rng.below(values.len())];
        match rng.below(3) {
            0 => {
                hydro_raise(tag, value);
            }
            1 => {
                let text = "\u{1f600} fuzz";
                hydro_raise_msg(tag, text.as_ptr(), text.len());
            }
            _ => {
                hydro_raise_from(tag, hydro_str_new(bytes("cause").0, 6), value);
            }
        }
        assert_eq!(hydro_exc_pending(), 1, "raise did not publish an exception");
        assert!(drain(step), "drain did not clear the raised exception");
        assert_no_exception(step);
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "raise cycles took {elapsed:?}, over the 2 s smoke budget"
    );
}
