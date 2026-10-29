//! Hydro Native ABI (HNA) v1 — public contract for compiled bindings.
//!
//! The C mirror of this module is `include/hydro_native.h`. Both describe the
//! same structures, in the same field order, with the same names; the
//! normative specification is `docs/NATIVE_BINDINGS.md` §3 and the tagged
//! value encoding is `docs/ARCHITECTURE.md` §3.1.
//!
//! This crate is the contract only. It is dependency-free, `no_std` and
//! defines no exported function: a binding implements the entry points
//! ([`NativeAbiFn`], [`NativeAbiBuildFn`], [`NativeModuleV1Fn`]) and exports
//! them from its own `cdylib`.
//!
//! # Runtime-owned services
//!
//! `hydro_hna_runtime_abi`, `hydro_root_hold` and `hydro_root_release` are
//! HNA service symbols implemented by the hydro runtime. The **runtime
//! linker owns those implementations**: bindings only call them, and this
//! crate deliberately provides no shim, no re-export and no definition. The
//! type aliases [`HnaRuntimeAbiFn`], [`RootHoldFn`] and [`RootReleaseFn`]
//! document the call signatures for bindings that declare the symbols
//! themselves.
//!
//! Descriptors are static, read-only and valid for the process lifetime. The
//! runtime caches the module object and its constants, roots them
//! persistently and never calls `init` twice.
//!
//! # Layout
//!
//! All descriptor structures are `#[repr(C)]`. Function-pointer fields are
//! `Option<unsafe extern "C" fn(..)>`, which is FFI-safe and has the same
//! representation as a nullable C function pointer. The structs are `Copy`
//! but not `Send`/`Sync`: they contain raw pointers, so those auto traits are
//! intentionally not asserted.

#![no_std]
#![forbid(unsafe_code)]

use core::ffi::c_char;

/// Tagged value, encoded as `docs/ARCHITECTURE.md` §3.1 describes.
///
/// Heap layout is private; bindings never dereference a value and use the
/// runtime services declared in `include/hydro_native.h` instead.
pub type HydroValue = i64;

/// HNA major version. One integer major; only additive changes within it.
pub const HNA_VERSION: u32 = 1;

/// Numeric revision of the hydro runtime ABI this contract was written
/// against. A descriptor's `runtime_abi_min` is checked against the
/// runtime-supported range by the loader.
pub const RUNTIME_ABI: u32 = 1;

/// `HydroNativeModule::flags` bit 0: reentrant/concurrent calls are allowed.
pub const HNA_FLAG_THREAD_SAFE: u32 = 0x0000_0001;

/// Reserved `HydroNativeModule::flags` bits; must be zero.
pub const HNA_FLAG_RESERVED: u32 = 0xFFFF_FFFE;

/// `int32_t (*init)(void)` — optional module initializer. `0` is success;
/// failure sets a pending exception.
pub type HydroInitFn = unsafe extern "C" fn() -> i32;

/// `HydroValue (*call)(HydroValue self, uint64_t argc, const HydroValue* argv)`
///
/// The `self` argument is the module object's tagged value. The call pointer
/// always receives positional arguments in Python order; keyword calls are
/// resolved by the runtime against [`HydroFunction::params`].
pub type HydroCallFn =
    unsafe extern "C" fn(self_: HydroValue, argc: u64, argv: *const HydroValue) -> HydroValue;

/// `HydroValue (*value)(void)` — a constant's thunk, evaluated once by the
/// runtime and then cached.
pub type HydroValueFn = unsafe extern "C" fn() -> HydroValue;

/// HNA module descriptor (`HydroNativeModule` in the C header).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HydroNativeModule {
    /// Size of this struct as built, so tail fields can be appended.
    pub struct_size: u32,
    /// [`HNA_VERSION`].
    pub hna_version: u32,
    /// Minimum hydro runtime ABI revision.
    pub runtime_abi_min: u64,
    /// Exact dotted import name, UTF-8, NUL-terminated.
    pub module_name: *const c_char,
    /// `"distribution version"`, for diagnostics.
    pub provider: *const c_char,
    /// `"linux-x86_64"`, checked by the loader.
    pub platform: *const c_char,
    /// [`HNA_FLAG_THREAD_SAFE`] or `0`; reserved bits must be zero.
    pub flags: u32,
    /// Optional initializer; `None` means NULL.
    pub init: Option<HydroInitFn>,
    /// Number of entries in [`Self::functions`].
    pub function_count: u32,
    /// Static array of `function_count` functions.
    pub functions: *const HydroFunction,
    /// Number of entries in [`Self::constants`].
    pub constant_count: u32,
    /// Static array of `constant_count` constants.
    pub constants: *const HydroConstant,
}

/// One exported function (`HydroFunction` in the C header).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HydroFunction {
    /// Python-visible function name, NUL-terminated.
    pub name: *const c_char,
    /// Minimum positional argument count.
    pub min_args: u32,
    /// Maximum positional argument count.
    pub max_args: u32,
    /// Reserved, must be zero.
    pub flags: u32,
    /// Comma-separated keyword names, or NULL.
    pub params: *const c_char,
    /// Entry point; `None` means NULL.
    pub call: Option<HydroCallFn>,
}

/// One exported constant (`HydroConstant` in the C header).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HydroConstant {
    /// Python-visible constant name, NUL-terminated.
    pub name: *const c_char,
    /// Value thunk, evaluated once and cached; `None` means NULL.
    pub value: Option<HydroValueFn>,
}

/// `uint32_t hydro_native_abi(void)` — binding entry point; returns
/// [`HNA_VERSION`].
pub type NativeAbiFn = unsafe extern "C" fn() -> u32;

/// `uint32_t hydro_native_abi_build(void)` — binding entry point; build
/// counter for reports.
pub type NativeAbiBuildFn = unsafe extern "C" fn() -> u32;

/// `const HydroNativeModule *hydro_native_module_v1(void)` — binding entry
/// point returning the static descriptor.
pub type NativeModuleV1Fn = unsafe extern "C" fn() -> *const HydroNativeModule;

/// `uint32_t hydro_hna_runtime_abi(void)` — runtime service, implemented and
/// exported by the hydro runtime (runtime linker owns the implementation).
pub type HnaRuntimeAbiFn = unsafe extern "C" fn() -> u32;

/// `uint64_t hydro_root_hold(HydroValue)` — runtime service, implemented and
/// exported by the hydro runtime. Returns a persistent root token.
pub type RootHoldFn = unsafe extern "C" fn(value: HydroValue) -> u64;

/// `void hydro_root_release(uint64_t)` — runtime service, implemented and
/// exported by the hydro runtime.
pub type RootReleaseFn = unsafe extern "C" fn(token: u64);

const _: () = assert!(HNA_VERSION == 1);
const _: () = assert!(RUNTIME_ABI == 1);
const _: () = assert!(HNA_FLAG_THREAD_SAFE & HNA_FLAG_RESERVED == 0);
const _: () = assert!(core::mem::size_of::<HydroValue>() == 8);
