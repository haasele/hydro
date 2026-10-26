//! Shared fuzz harness bodies for Hydro.
//!
//! The same functions are used by the `cargo-fuzz` targets
//! (`../fuzz_targets/*.rs`) and by the stable fallback runner
//! (`../stable/src/main.rs`), so the bounded smoke run and the long campaign
//! execute identical code.
//!
//! Both harnesses are deterministic and bounded:
//!
//! * [`frontend_pipeline`] truncates its input, then runs parse -> lower ->
//!   type-check -> native-code generation in memory. Programs are never
//!   executed, so no input can loop forever, touch the filesystem or open a
//!   socket.
//! * [`runtime_gc`] decodes a fixed number of operations from the input and
//!   drives the C ABI with values the runtime produced itself (never raw
//!   integers that could alias a heap pointer). It never spawns a thread,
//!   process or pool: those need a program shell to be meaningful and are
//!   covered by `crates/hydro-runtime/tests/shutdown.rs`.
//!
//! Invariants checked by every step:
//!
//! * the runtime/codegen must not panic or abort,
//! * a pending exception always exposes a usable tag and value and is always
//!   clearable,
//! * a collection always succeeds and never leaves a negative object count.
//!
//! The operation mix mirrors the dependency-free smoke tests in
//! `crates/hydro-frontend/tests/fuzz_smoke.rs` and
//! `crates/hydro-runtime/tests/fuzz_smoke.rs`, extended with cycles, fibers and
//! forced collections.

use std::hint::black_box;

use hydro_codegen::compile_object;
use hydro_frontend::check_source;
use hydro_runtime::abi::*;

/// Largest input the harnesses look at, in bytes.
///
/// capping the input keeps a single libFuzzer iteration bounded even when the
/// corpus grows a multi-megabyte candidate.
pub const MAX_INPUT: usize = 16 * 1024;

/// Largest number of ABI operations decoded from one input.
const MAX_STEPS: usize = 192;

/// Number of value slots kept on the stack as collector roots.
const SLOTS: usize = 24;

// -------------------------------------------------------------------------
// Parser / lowering / codegen
// -------------------------------------------------------------------------

/// Parses, lowers, type-checks and code-generates one candidate program.
///
/// The filename is a fixed ASCII string: the frontend never resolves imports
/// for `check_source`, so the harness stays independent of the filesystem.
pub fn frontend_pipeline(data: &[u8]) {
    let data = &data[..data.len().min(MAX_INPUT)];
    let source = String::from_utf8_lossy(data);
    match check_source(&source, "fuzz.py") {
        Ok(program) => {
            // Codegen reports internal failures as `CodegenError`; it must
            // never panic. The emitted object is opaque to the harness, but a
            // successful compile always produces a non-empty ELF object.
            match compile_object(&program) {
                Ok(object) => assert!(!object.is_empty(), "codegen emitted an empty object"),
                Err(error) => {
                    black_box(error.to_string());
                }
            }
        }
        Err(diagnostics) => {
            // The documented contract: a failed check reports at least one
            // diagnostic instead of silently returning a broken program.
            assert!(
                !diagnostics.is_empty(),
                "frontend rejected a program without a diagnostic"
            );
        }
    }
}

// -------------------------------------------------------------------------
// Runtime GC / object operations
// -------------------------------------------------------------------------

/// A fiber body with the generated-wrapper signature. It allocates once so
/// the collector runs with a live fiber stack, then returns.
extern "C" fn fiber_body(_argv: *const i64) -> i64 {
    let _ = hydro_str_new(b"fiber".as_ptr(), 5);
    hydro_none()
}

/// Drives allocation, containers, cycles, fibers and collection from `data`.
pub fn runtime_gc(data: &[u8]) {
    let data = &data[..data.len().min(MAX_INPUT)];
    // Each candidate starts from a clean heap: the target is single-threaded
    // and `hydro_runtime_init` frees everything the previous iteration left.
    hydro_runtime_init();

    let mut rng = SplitMix64::new(seed(data));
    let mut slots = initial_slots(&mut rng);
    drain();

    let limit = data.len().min(MAX_STEPS * 2);
    let mut offset = 0usize;
    while offset < limit {
        let op = data[offset] % 28;
        let operand = data.get(offset + 1).copied().unwrap_or(0) as usize;
        let step = offset / 2;
        offset += 2;

        // Keep every slot materialised on the stack: a collection triggered by
        // any call below must see the objects the harness still holds.
        black_box(&slots);
        let a = slots[operand % SLOTS];
        let b = slots[(operand >> 3) % SLOTS];
        let slot = operand % SLOTS;
        if std::env::var_os("FUZZ_TRACE").is_some() {
            eprintln!("step {step}: op {op} operand {operand} a={a:#x} b={b:#x}");
        }

        match op {
            0 => {
                let items = [a, b, slots[(operand >> 1) % SLOTS]];
                slots[slot] = hydro_list_new(1 + operand % 3, items.as_ptr());
            }
            1 => {
                black_box(&slots);
                hydro_list_append(a, b);
            }
            2 => {
                hydro_subscript(a, tagged(operand as i64 % 5));
            }
            3 => {
                // `x[i] = x` builds a cycle that only the collector can break.
                hydro_subscript_set(a, tagged(0), slots[(operand >> 2) % SLOTS]);
            }
            4 => {
                hydro_delete_subscript(a, tagged(0));
            }
            5 => {
                let stop = tagged(operand as i64 % 4);
                hydro_slice(a, hydro_none(), stop, hydro_none());
                hydro_slice_set(a, hydro_none(), stop, hydro_none(), b);
                hydro_delete_slice(a, hydro_none(), stop, hydro_none());
            }
            6 => {
                let keys = [a, b];
                let values = [b, a];
                slots[slot] = hydro_dict_new(2, keys.as_ptr(), values.as_ptr());
                hydro_dict_set(slots[slot], a, b);
                hydro_dict_merge(slots[slot], a);
            }
            7 => {
                let items = [a, b];
                slots[slot] = hydro_set_new(2, items.as_ptr());
                hydro_set_add(slots[slot], slots[(operand >> 4) % SLOTS]);
            }
            8 => {
                let items = [a, b];
                slots[slot] = hydro_tuple_new(2, items.as_ptr());
            }
            9 => {
                hydro_len(a);
                hydro_truthy(b);
                hydro_hash(a);
            }
            10 => {
                hydro_binop((operand % 22) as u32, a, b);
                hydro_unop((operand % 9) as u32, b);
            }
            11 => {
                hydro_to_str(a);
                hydro_to_repr(b);
                hydro_to_int(a);
                hydro_to_float(b);
                hydro_abs(a);
            }
            12 => {
                let names = [
                    "append",
                    "add",
                    "count",
                    "index",
                    "upper",
                    "encode",
                    "decode",
                    "split",
                    "join",
                    "keys",
                    "items",
                    "get",
                    "pop",
                    "strip",
                    "replace",
                    "startswith",
                    "missing",
                ];
                let name = names[operand % names.len()];
                let args = [a, b];
                let argc = operand % 3;
                hydro_method(a, name.as_ptr(), name.len(), argc, args.as_ptr());
            }
            13 => {
                hydro_getattr(a, b"missing".as_ptr(), 7);
                hydro_setattr(a, b"missing".as_ptr(), 7, b);
                hydro_delete_attr(a, b"missing".as_ptr(), 7);
            }
            14 => {
                let iterator = hydro_iter_new(a);
                for _ in 0..3 {
                    let mut out = hydro_none();
                    hydro_iter_next(iterator, &mut out);
                }
                hydro_next(iterator, b, (operand % 2) as i64);
                hydro_iter(a);
            }
            15 => {
                hydro_unpack(a, operand % 4);
                hydro_unpack_seq(b, operand % 3);
                hydro_unpack_star(a, operand % 3, (operand >> 2) % 3);
            }
            16 => {
                hydro_raise((operand % 24) as u32, a);
                drain();
                hydro_raise_value(b);
                drain();
            }
            17 => {
                let text = "fuzz \u{e9}\u{1f600}";
                hydro_raise_msg((operand % 24) as u32, text.as_ptr(), text.len());
                drain();
                hydro_raise_from((operand % 24) as u32, a, b);
                drain();
            }
            18 => {
                hydro_exception_class((operand % 24) as u32);
                hydro_exc_matches((operand % 16) as u32);
                hydro_exc_matches((operand % 8) as u32);
            }
            19 => {
                hydro_list_from(a);
                hydro_tuple_from(b);
                hydro_dict_from(a);
                hydro_set_from(b);
                hydro_all(a);
                hydro_sum(a, tagged(0));
            }
            20 => {
                hydro_enumerate(a, tagged(operand as i64 % 3));
                let zip_args = [a, b];
                hydro_zip(2, zip_args.as_ptr());
                hydro_map(a, b);
                hydro_filter(a, b);
            }
            21 => {
                hydro_format_value(a, std::ptr::null(), 0, 0);
                hydro_format_value(a, b"08.2f".as_ptr(), 5, 0);
            }
            22 => {
                let cell = hydro_cell_new(a);
                hydro_cell_get(cell);
                hydro_cell_set(cell, b);
                slots[slot] = cell;
            }
            23 => {
                assert!(hydro_gc_collect() >= 0, "collection failed at step {step}");
                assert!(
                    hydro_gc_objects() >= 0,
                    "negative object count at step {step}"
                );
            }
            24 => {
                hydro_set_file(b"fuzz.py".as_ptr(), 7);
                hydro_set_line((operand % 10_000) as u32);
                hydro_frame_enter(b"fuzz".as_ptr(), 5);
                hydro_frame_leave();
                hydro_safepoint();
            }
            25 => {
                let name = ["len", "abs", "str", "repr", "hash", "missing-builtin"][operand % 6];
                let function = hydro_builtin_function(name.as_ptr(), name.len());
                drain();
                let args = [a];
                hydro_call(function, 1, args.as_ptr());
            }
            26 => {
                // A fresh fiber every time: the generator/coroutine stack is
                // registered as a collector root and freed by `FiberState`'s
                // drop when the value becomes unreachable. `fiber_body` never
                // yields, so a fiber is always either unstarted or finished
                // when the next candidate resets the heap.
                let args = [a];
                let argc = operand % 2;
                let argv = if argc == 0 {
                    std::ptr::null()
                } else {
                    args.as_ptr()
                };
                let body: extern "C" fn(*const i64) -> i64 = fiber_body;
                let entry = body as *const u8;
                if operand % 2 == 0 {
                    slots[slot] = hydro_generator_new(entry, argc, argv);
                } else {
                    slots[slot] = hydro_coroutine_new(entry, argc, argv);
                }
            }
            27 => {
                // Driving a fiber and misusing the yield protocol both have to
                // raise instead of corrupting the caller's stack.
                hydro_await(a);
                hydro_yield(b);
                hydro_yield_from(a);
            }
            _ => unreachable!(),
        }
        drain();
    }

    assert!(hydro_gc_collect() >= 0, "final collection failed");
    assert!(hydro_gc_objects() >= 0, "negative final object count");
}

/// Tagged small integer (`hydro_int` is the identity on tagged immediates).
fn tagged(n: i64) -> i64 {
    n << 3
}

/// Builds the initial value pool, rooted in the caller's stack frame.
fn initial_slots(rng: &mut SplitMix64) -> [i64; SLOTS] {
    let text = "h\u{e9}llo \u{1f600}\u{200b}";
    let raw = [0u8, 0xff, b'a'];
    let bigint = "123456789012345678901234567890";
    let mut slots = [hydro_none(); SLOTS];
    slots[0] = hydro_none();
    slots[1] = hydro_true();
    slots[2] = hydro_false();
    slots[3] = tagged(7);
    slots[4] = tagged(-3);
    slots[5] = hydro_float_new(2.5);
    slots[6] = hydro_float_new(f64::from_bits(rng.next_u64()));
    slots[7] = hydro_str_new(text.as_ptr(), text.len());
    slots[8] = hydro_bytes_new(raw.as_ptr(), raw.len());
    slots[9] = hydro_bigint_from_str(bigint.as_ptr(), bigint.len());
    slots[10] = hydro_range_new(tagged(0), tagged(9), hydro_none());
    slots[11] = hydro_str_new(b"".as_ptr(), 0);
    slots[12] = hydro_list_new(0, std::ptr::null());
    slots[13] = hydro_dict_new(0, std::ptr::null(), std::ptr::null());
    slots[14] = hydro_set_new(0, std::ptr::null());
    slots[15] = hydro_cell_new(hydro_none());
    slots[16] = hydro_str_new(b"fuzz".as_ptr(), 4);
    let one = [tagged(1)];
    slots[17] = hydro_list_new(1, one.as_ptr());
    slots
}

/// Validates and clears a pending exception, if any.
fn drain() {
    if hydro_exc_pending() == 0 {
        return;
    }
    assert!(hydro_exc_tag() >= 0, "pending exception without a tag");
    assert_ne!(hydro_exc_value(), 0, "pending exception without a value");
    assert_eq!(hydro_exc_clear(), 0, "hydro_exc_clear must return 0");
    assert_eq!(hydro_exc_pending(), 0, "exception still pending after clear");
}

// -------------------------------------------------------------------------
// Stable fallback input generation
// -------------------------------------------------------------------------

/// Small deterministic PRNG (SplitMix64).
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Derives the per-input PRNG seed; identical inputs always behave the same.
fn seed(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in data.iter().take(256) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// Text fragments that stress the lexer and parser without forming a program.
const FRAGMENTS: &[&str] = &[
    "def ", "class ", "return ", "if ", "elif ", "else:", "for ", "in ", "while ", "match ",
    "case ", "try:", "except ", "finally:", "raise ", "import ", "from ", "as ", "lambda ",
    "yield ", "await ", "async ", "with ", "pass", "break", "continue", "\n", "\n    ", "\n\t",
    "  ", "\t", "(", ")", "[", "]", "{", "}", ":", ",", ".", "=", "==", "!=", "<=", ">=", "+",
    "-", "*", "/", "//", "%", "**", "|", "&", "^", "~", "<<", ">>", ":=", "->", "@", ";", "...",
    "'", "\"", "'''", "\"\"\"", "f\"{", "}\"", "r\"", "b\"", "0x1f", "1_000", "1.5e-3", "1j",
    "None", "True", "False", "self", "x", "_x", "\\", "\\n", "#c\n", "é", "😀", "\u{200b}",
    "\u{202e}", "\u{fffd}",
];

/// Generates a deterministic source candidate for the frontend target.
pub fn generate_frontend_input(rng: &mut SplitMix64) -> Vec<u8> {
    let parts = 1 + rng.below(32);
    let mut source = String::new();
    for _ in 0..parts {
        source.push_str(FRAGMENTS[rng.below(FRAGMENTS.len())]);
        match rng.below(4) {
            0 => source.push(' '),
            1 => source.push('\n'),
            _ => {}
        }
    }
    source.into_bytes()
}

/// Generates a deterministic operation stream for the runtime target.
pub fn generate_runtime_input(rng: &mut SplitMix64) -> Vec<u8> {
    let len = 2 + rng.below(MAX_STEPS * 2);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}
