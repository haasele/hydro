//! HNA v1 end-to-end fixture (`hna_rust`).
//!
//! This crate is deliberately a test binding, not a published module. It is a
//! `cdylib` that exports the three HNA v1 entry points and a static
//! [`HydroNativeModule`] descriptor. It links only against the hydro runtime's
//! C services resolved from the loading process; it never links `libpython`.
//!
//! The fixture exercises the contract surface used by `tea/tests/hna.rs`:
//!
//! - `add(a, b)` adds two Python ints (tagged `HydroValue`s),
//! - `greet(name)` concatenates a prefix with a Python `str`,
//! - `make_list(n)` builds a rooted `[0, .., n-1]` list,
//! - `fail()` raises a Python `ValueError` through `hydro_raise_msg`,
//! - `boom()` panics deliberately so the process aborts without unwinding,
//! - `MAGIC` is the constant `7`.
//!
//! `hydro_native_abi` returns [`HNA_VERSION`] and `hydro_native_abi_build`
//! returns the build counter `1`.

use core::mem::size_of;
use core::ptr;

use hydro_hna::{
    HydroConstant, HydroFunction, HydroNativeModule, HydroValue, HNA_FLAG_THREAD_SAFE, HNA_VERSION,
    RUNTIME_ABI,
};

/// Exception tags from the hydro runtime (`hydro_runtime::exc`).
const TYPE_ERROR: u32 = 1;
const VALUE_ERROR: u32 = 0;

/// `hydro_binop` operator code for `a + b` (ARCHITECTURE §3.2).
const OP_ADD: u32 = 0;

/// HNA build counter reported by `hydro_native_abi_build`.
const BUILD: u32 = 1;

// Runtime services. These are defined by the hydro runtime and resolved from
// the loading process; the fixture must not link the runtime crate itself,
// because the module is loaded into an already running runtime.
unsafe extern "C" {
    fn hydro_int(v: HydroValue) -> HydroValue;
    fn hydro_str_new(ptr: *const u8, len: usize) -> HydroValue;
    fn hydro_list_new(argc: u64, items: *const HydroValue) -> HydroValue;
    fn hydro_list_append(list: HydroValue, value: HydroValue) -> HydroValue;
    fn hydro_binop(op: u32, a: HydroValue, b: HydroValue) -> HydroValue;
    fn hydro_raise_msg(tag: u32, ptr: *const u8, len: usize) -> HydroValue;
    fn hydro_gc_scope_push(base: *const HydroValue, count: usize) -> usize;
    fn hydro_gc_scope_pop(token: usize);
}

/// Tagged immediates use the low three bits; `000` is a Python `int`.
fn is_int(value: HydroValue) -> bool {
    value & 0b111 == 0
}

/// Raises a Python `TypeError` and returns the `None` immediate.
fn type_error(message: &[u8]) -> HydroValue {
    // SAFETY: the message is a valid byte slice; `hydro_raise_msg` copies it.
    unsafe { hydro_raise_msg(TYPE_ERROR, message.as_ptr(), message.len()) }
}

/// Raises a Python `ValueError` with `message` and returns the `None`
/// immediate.
fn value_error(message: &[u8]) -> HydroValue {
    // SAFETY: the message is a valid byte slice; `hydro_raise_msg` copies it.
    unsafe { hydro_raise_msg(VALUE_ERROR, message.as_ptr(), message.len()) }
}

/// Validates the call arity defensively. The loader enforces min/max, but a
/// binding must never read past `argv` even if it is reached another way.
fn arguments<'a>(argc: u64, argv: *const HydroValue, expected: u64) -> Option<&'a [HydroValue]> {
    if argc != expected || argv.is_null() {
        return None;
    }
    // SAFETY: `argv` points at `argc` readable tagged values by contract.
    Some(unsafe { core::slice::from_raw_parts(argv, argc as usize) })
}

unsafe extern "C" fn add_call(_self: HydroValue, argc: u64, argv: *const HydroValue) -> HydroValue {
    let Some(args) = arguments(argc, argv, 2) else {
        return type_error(b"add() requires exactly two int arguments");
    };
    if !is_int(args[0]) || !is_int(args[1]) {
        return type_error(b"add() requires int arguments");
    }
    // SAFETY: both operands are tagged ints; `hydro_binop` handles promotion
    // to BigInt and raises on overflow-adjacent cases.
    unsafe { hydro_binop(OP_ADD, args[0], args[1]) }
}

unsafe extern "C" fn greet_call(
    _self: HydroValue,
    argc: u64,
    argv: *const HydroValue,
) -> HydroValue {
    let Some(args) = arguments(argc, argv, 1) else {
        return type_error(b"greet() requires exactly one str argument");
    };
    // `+` validates that `name` is a str and raises TypeError otherwise.
    // `hydro_str_new` derives a value that must stay rooted across the
    // allocating `hydro_binop` call.
    let mut roots = [0i64; 1];
    // SAFETY: `roots` outlives the scope and is not moved while pushed.
    let token = unsafe { hydro_gc_scope_push(roots.as_ptr(), roots.len()) };
    roots[0] = {
        // SAFETY: the prefix is a valid static byte slice.
        unsafe { hydro_str_new(b"hello, ".as_ptr(), 7) }
    };
    // SAFETY: both operands are Hydro values; the result is anchored on the
    // runtime heap.
    let result = unsafe { hydro_binop(OP_ADD, roots[0], args[0]) };
    // SAFETY: LIFO pop of the scope pushed above.
    unsafe { hydro_gc_scope_pop(token) };
    result
}

unsafe extern "C" fn make_list_call(
    _self: HydroValue,
    argc: u64,
    argv: *const HydroValue,
) -> HydroValue {
    let Some(args) = arguments(argc, argv, 1) else {
        return type_error(b"make_list() requires exactly one int argument");
    };
    if !is_int(args[0]) {
        return type_error(b"make_list() requires an int argument");
    }
    let count = args[0] >> 3;
    if !(0..=100_000).contains(&count) {
        return value_error(b"make_list() argument must be between 0 and 100000");
    }

    // The list is built by repeated allocating calls, so it must be a precise
    // root across `hydro_list_append`.
    let mut roots = [0i64; 1];
    // SAFETY: `roots` outlives the scope and is not moved while pushed.
    let token = unsafe { hydro_gc_scope_push(roots.as_ptr(), roots.len()) };
    // SAFETY: a zero-length list with a NULL item buffer is valid.
    roots[0] = unsafe { hydro_list_new(0, ptr::null()) };
    for index in 0..count {
        // SAFETY: `hydro_int` tags the raw small integer.
        let item = unsafe { hydro_int(index) };
        // SAFETY: `roots[0]` is a list created above.
        unsafe { hydro_list_append(roots[0], item) };
    }
    let list = roots[0];
    // SAFETY: LIFO pop of the scope pushed above.
    unsafe { hydro_gc_scope_pop(token) };
    list
}

unsafe extern "C" fn fail_call(
    _self: HydroValue,
    _argc: u64,
    _argv: *const HydroValue,
) -> HydroValue {
    value_error(b"hna_rust.fail() deliberate failure")
}

/// Deliberate panic for the NB-3 abort test. A panic must never become a
/// Python exception; the process terminates (SIGABRT) instead of unwinding
/// into Hydro.
unsafe extern "C" fn boom_call(
    _self: HydroValue,
    _argc: u64,
    _argv: *const HydroValue,
) -> HydroValue {
    panic!("hna fixture panic")
}

unsafe extern "C" fn magic_value() -> HydroValue {
    // SAFETY: `hydro_int` tags the raw small integer.
    unsafe { hydro_int(7) }
}

unsafe extern "C" fn init() -> i32 {
    0
}

/// Wrappers that make the descriptor arrays usable in a process-lifetime
/// `static`. HNA descriptors are read-only after construction and contain raw
/// pointers and function pointers, which are not `Sync` by themselves.
#[repr(transparent)]
struct SyncFunctions([HydroFunction; 5]);
// SAFETY: the array is immutable static data; the raw pointers are to static
// strings and functions, and the descriptor is never mutated.
unsafe impl Sync for SyncFunctions {}

#[repr(transparent)]
struct SyncConstants([HydroConstant; 1]);
// SAFETY: same as `SyncFunctions`.
unsafe impl Sync for SyncConstants {}

#[repr(transparent)]
struct SyncModule(HydroNativeModule);
// SAFETY: same as `SyncFunctions`; the module only points at the statics below.
unsafe impl Sync for SyncModule {}

static FUNCTIONS: SyncFunctions = SyncFunctions([
    HydroFunction {
        name: c"add".as_ptr(),
        min_args: 2,
        max_args: 2,
        flags: 0,
        params: c"a,b".as_ptr(),
        call: Some(add_call),
    },
    HydroFunction {
        name: c"greet".as_ptr(),
        min_args: 1,
        max_args: 1,
        flags: 0,
        params: c"name".as_ptr(),
        call: Some(greet_call),
    },
    HydroFunction {
        name: c"make_list".as_ptr(),
        min_args: 1,
        max_args: 1,
        flags: 0,
        params: c"n".as_ptr(),
        call: Some(make_list_call),
    },
    HydroFunction {
        name: c"fail".as_ptr(),
        min_args: 0,
        max_args: 0,
        flags: 0,
        params: ptr::null(),
        call: Some(fail_call),
    },
    HydroFunction {
        name: c"boom".as_ptr(),
        min_args: 0,
        max_args: 0,
        flags: 0,
        params: ptr::null(),
        call: Some(boom_call),
    },
]);

static CONSTANTS: SyncConstants = SyncConstants([HydroConstant {
    name: c"MAGIC".as_ptr(),
    value: Some(magic_value),
}]);

static MODULE: SyncModule = SyncModule(HydroNativeModule {
    struct_size: size_of::<HydroNativeModule>() as u32,
    hna_version: HNA_VERSION,
    runtime_abi_min: RUNTIME_ABI as u64,
    module_name: c"hna_rust".as_ptr(),
    provider: c"hydro-hna-fixture 0.1.0".as_ptr(),
    platform: c"linux-x86_64".as_ptr(),
    flags: HNA_FLAG_THREAD_SAFE,
    init: Some(init),
    function_count: 5,
    functions: &FUNCTIONS.0 as *const [HydroFunction; 5] as *const HydroFunction,
    constant_count: 1,
    constants: &CONSTANTS.0 as *const [HydroConstant; 1] as *const HydroConstant,
});

/// HNA entry point: the major version this binding implements.
#[no_mangle]
pub extern "C" fn hydro_native_abi() -> u32 {
    HNA_VERSION
}

/// HNA entry point: build counter for reports.
#[no_mangle]
pub extern "C" fn hydro_native_abi_build() -> u32 {
    BUILD
}

/// HNA entry point: the static, read-only module descriptor.
#[no_mangle]
pub extern "C" fn hydro_native_module_v1() -> *const HydroNativeModule {
    &MODULE.0 as *const HydroNativeModule
}
