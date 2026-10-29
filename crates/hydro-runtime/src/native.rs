//! HNA v1 loader: Hydro Native ABI compiled modules (`*.hna.so`).
//!
//! The public contract is [`hydro_hna`] (mirrored by `include/hydro_native.h`)
//! and `docs/NATIVE_BINDINGS.md` §3-4. This module owns the runtime side:
//!
//! - opening the artifact with `dlopen(RTLD_NOW | RTLD_LOCAL)` and resolving
//!   the three entry points (`hydro_native_abi`, `hydro_native_abi_build`,
//!   `hydro_native_module_v1`),
//! - validating the descriptor and every function/constant entry (ABI version,
//!   runtime ABI range, platform, import name, reserved flags, counts,
//!   parameter names), naming both sides in every diagnostic,
//! - cross-checking an enclosing `hydro-manifest.toml` (when one exists)
//!   against the descriptor before the module is initialized
//!   ([`crate::native_manifest`]),
//! - the process-wide registry (one entry per canonical artifact path) that
//!   owns the `dlopen` handle, the module tagged value, the cached constants,
//!   the single `init` result and the per-module call lock,
//! - call dispatch, including keyword binding against the descriptor's
//!   `params`, argument rooting and panic containment,
//! - the process-wide persistent-root token table behind `hydro_root_hold` /
//!   `hydro_root_release`.
//!
//! A `.hna.so` path never reaches the CPython extension bridge: every
//! extension entry point (`hydro_extension_module_value`, `extension::get`,
//! `extension::call`, `extension::import_member`) routes it here first.
//!
//! # Errors and aborts
//!
//! Load and call failures raise a pending Hydro exception and return the `None`
//! immediate; no user input can panic. A panic raised by the binding itself
//! (in `init` or in a function pointer) is caught with
//! [`std::panic::catch_unwind`], reported on stderr and aborts the process: it
//! is an internal error and never unwinds into Hydro or becomes a catchable
//! Python exception (`docs/NATIVE_BINDINGS.md` §3.4).
//!
//! # Threading
//!
//! HNA has no GIL. A `thread_safe` descriptor is called directly; otherwise
//! calls are serialized per module through [`ModuleEntry::call_lock`]. Root
//! tokens created by `hydro_root_hold` belong to the persistent-root
//! registration of the calling Hydro thread, so hold and release must be
//! issued from the same thread (single-thread HNA); the token table itself is
//! process-wide. Module state (the module value and the cached constants) is
//! rooted in the same persistent registration, so a module is imported on the
//! thread that runs the program and keeps serving it; a worker thread that
//! imports a module and then exits does not take the module state with it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use hydro_hna::{
    HydroInitFn, HydroNativeModule, HydroValueFn, NativeAbiBuildFn, NativeAbiFn, NativeModuleV1Fn,
    HNA_FLAG_RESERVED, HNA_FLAG_THREAD_SAFE, HNA_VERSION, RUNTIME_ABI,
};

use crate::exc::{self, ATTRIBUTE_ERROR, IMPORT_ERROR, RUNTIME_ERROR, TYPE_ERROR};
use crate::value;

/// The only platform HNA v1 claims (`docs/NATIVE_BINDINGS.md` §3.1).
pub(crate) const PLATFORM: &str = "linux-x86_64";

/// Upper bound for a descriptor's function or constant count. Anything above
/// this is treated as a corrupt descriptor rather than a huge module.
const MAX_DESCRIPTOR_ENTRIES: u32 = 1_000_000;

/// Suffix of an HNA artifact; the loader is only reached for these paths.
const HNA_SUFFIX: &str = ".hna.so";

/// Whether `path` names an HNA artifact.
pub(crate) fn is_hna_path(path: &str) -> bool {
    path.ends_with(HNA_SUFFIX)
}

/// A validated function entry: everything the call path needs, owned so the
/// registry never dereferences descriptor storage after validation.
#[derive(Debug)]
struct NativeFunction {
    name: String,
    min_args: u32,
    max_args: u32,
    /// Keyword-bindable parameter names in positional order (empty when the
    /// descriptor declares no `params`).
    params: Vec<String>,
    call: hydro_hna::HydroCallFn,
}

/// A validated constant thunk (pre-init) or its cached value (post-init).
#[derive(Debug)]
struct ConstantThunk {
    name: String,
    value: HydroValueFn,
}

#[derive(Debug)]
struct NativeConstant {
    name: String,
    /// Evaluated exactly once at load time and rooted persistently.
    value: i64,
}

/// A descriptor that passed validation, before `init` and constant evaluation.
#[derive(Debug)]
struct ValidatedModule {
    module_name: String,
    hna_version: u32,
    platform: String,
    provider: String,
    runtime_abi_min: u64,
    thread_safe: bool,
    init: Option<HydroInitFn>,
    functions: Vec<NativeFunction>,
    constants: Vec<ConstantThunk>,
}

/// Load-time failure. `Pending` means a pending Hydro exception is already set
/// and must be propagated untouched; `Diagnostic` is a stable message that is
/// cached by artifact path and replayed by later loads; `NameMismatch` names a
/// descriptor that belongs to a different import and is deliberately not
/// cached, so importing the artifact under its real name can still succeed.
#[derive(Debug)]
enum LoadFailure {
    Pending,
    Diagnostic(String),
    NameMismatch(String),
}

/// One loaded HNA module.
struct ModuleEntry {
    /// Registry id embedded in [`value::FunctionBody::Hna`].
    id: u64,
    /// Descriptor `module_name` (the exact dotted import name).
    name: String,
    /// `dlopen` handle, pinned for the process lifetime. Never `dlclose`d:
    /// descriptor storage is static and function pointers may outlive a load
    /// attempt in a binding's own state.
    _handle: usize,
    /// Static descriptor pointer (read-only, valid while the library is
    /// loaded). Kept for diagnostics and future tail fields.
    _descriptor: *const HydroNativeModule,
    /// `hydro_native_abi_build()` value, resolved for diagnostics.
    _build: u32,
    /// `provider` from the descriptor, named by init-failure diagnostics.
    provider: String,
    thread_safe: bool,
    functions: Vec<NativeFunction>,
    constants: Vec<NativeConstant>,
    /// The single `init` outcome (`Ok(())` when the module has no `init`).
    init_result: Result<(), String>,
    /// Published module tagged value (also the `self` argument of calls).
    module_value: i64,
    /// Persistent root slots for the module value and every cached constant.
    /// They live until the collector reset clears the registry.
    _roots: Vec<usize>,
    /// Serializes calls for a module that did not declare `thread_safe`.
    call_lock: Mutex<()>,
}

// SAFETY: the descriptor and the entry points point at static read-only
// storage owned by the loaded shared library, which is never unloaded. The
// cached tagged values are treated as opaque `i64`s; Rust-side mutation is
// limited to what the registry lock or `call_lock` serializes (and for
// `thread_safe` modules the binding promises concurrent calls are safe).
unsafe impl Send for ModuleEntry {}
unsafe impl Sync for ModuleEntry {}

#[derive(Default)]
struct Registry {
    next_id: u64,
    by_id: HashMap<u64, Arc<ModuleEntry>>,
    by_path: HashMap<PathBuf, Arc<ModuleEntry>>,
    /// Stable load diagnostics keyed by canonical artifact path. A failed load
    /// (including a failed `init`) never retries in the same process, so a
    /// second import replays the same message instead of running `init` twice.
    failures: HashMap<PathBuf, String>,
    /// One loader mutex per canonical artifact path: two threads importing the
    /// same artifact serialize, while imports of different artifacts (and
    /// nested imports from `init`) stay independent.
    load_locks: HashMap<PathBuf, Arc<Mutex<()>>>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

fn lock_registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

thread_local! {
    /// Canonical paths currently being loaded on this thread, so an artifact
    /// that (directly or indirectly) imports itself reports a circular import
    /// instead of deadlocking on its own loader mutex.
    static LOADING: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

fn loading_this_thread(path: &Path) -> bool {
    LOADING.with(|loading| loading.borrow().iter().any(|candidate| candidate == path))
}

fn push_loading(path: &Path) {
    LOADING.with(|loading| loading.borrow_mut().push(path.to_path_buf()));
}

fn pop_loading(path: &Path) {
    LOADING.with(|loading| {
        let mut loading = loading.borrow_mut();
        debug_assert_eq!(loading.last().map(PathBuf::as_path), Some(path));
        loading.pop();
    });
}

/// Drops the roots pushed by [`crate::heap::root_values`] on scope exit, so
/// every early return from a loader step releases them.
struct ThreadRoots(usize);

impl Drop for ThreadRoots {
    fn drop(&mut self) {
        crate::heap::release_roots(self.0);
    }
}

// ---------------------------------------------------------------------------
// Descriptor validation
// ---------------------------------------------------------------------------

/// Borrows a NUL-terminated descriptor string.
///
/// # Safety
/// `ptr` is either NULL or a valid NUL-terminated string that stays alive for
/// the process lifetime (the descriptor contract).
unsafe fn descriptor_str(ptr: *const c_char, field: &str, module: &str) -> Result<String, String> {
    if ptr.is_null() {
        return Err(format!("hna module '{module}': descriptor {field} is NULL"));
    }
    // SAFETY: the caller guarantees a NUL-terminated, readable string.
    match unsafe { CStr::from_ptr(ptr) }.to_str() {
        Ok(text) => Ok(text.to_string()),
        Err(_) => Err(format!(
            "hna module '{module}': descriptor {field} is not valid UTF-8"
        )),
    }
}

/// Parses the comma-separated keyword names of one function.
///
/// `None` (NULL `params`) and an empty string both mean "no keyword names".
/// Names are used verbatim: empty entries and duplicates are rejected, and the
/// count may not exceed `max_args`.
fn parse_params(
    raw: Option<&str>,
    max_args: u32,
    index: usize,
    module: &str,
) -> Result<Vec<String>, String> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for (position, name) in raw.split(',').enumerate() {
        if name.is_empty() {
            return Err(format!(
                "hna module '{module}': function {index} has an empty keyword name at position {position}"
            ));
        }
        if !seen.insert(name) {
            return Err(format!(
                "hna module '{module}': function {index} has duplicate keyword name '{name}'"
            ));
        }
        names.push(name.to_string());
    }
    if names.len() > max_args as usize {
        return Err(format!(
            "hna module '{module}': function {index} declares {} keyword names, more than max_args {max_args}",
            names.len()
        ));
    }
    Ok(names)
}

/// Validates a descriptor and copies everything the runtime needs.
///
/// `expected` is the exact dotted import name when the caller knows it. The
/// path-only extension entry points (`hydro_extension_get`/`_call`) have no
/// import name, so they load with `None`; such an entry can only be created
/// when the module value was never materialised under a name, and a later
/// named load re-checks the cached entry. Diagnostics use the requested name
/// when present and the descriptor's own `module_name` otherwise, so a
/// version/platform failure still names the module.
fn validate_descriptor(
    expected: Option<&str>,
    descriptor: &HydroNativeModule,
) -> Result<ValidatedModule, String> {
    let runtime_size = std::mem::size_of::<HydroNativeModule>() as u32;
    if descriptor.struct_size < runtime_size {
        return Err(format!(
            "hna module '{}': descriptor struct_size {}, runtime expects at least {runtime_size}",
            expected.unwrap_or("<artifact>"),
            descriptor.struct_size
        ));
    }
    // Diagnostic module name: the requested import name when the caller knows
    // it, otherwise the descriptor's own `module_name` (borrowed safely; a
    // NULL, empty or non-UTF-8 pointer falls back to a neutral label so the
    // later `module_name` check reports the real problem).
    let import = match expected {
        Some(expected) => expected.to_string(),
        None => {
            match unsafe { descriptor_str(descriptor.module_name, "module_name", "<artifact>") } {
                Ok(name) if !name.is_empty() => name,
                _ => "<artifact>".to_string(),
            }
        }
    };
    let provider = unsafe { descriptor_str(descriptor.provider, "provider", &import) }?;

    if descriptor.hna_version != HNA_VERSION {
        return Err(format!(
            "hna module '{import}' (provider '{provider}'): descriptor hna_version {}, runtime supports {HNA_VERSION}",
            descriptor.hna_version
        ));
    }
    let supported_abi = 1..=u64::from(RUNTIME_ABI);
    if !supported_abi.contains(&descriptor.runtime_abi_min) {
        return Err(format!(
            "hna module '{import}' (provider '{provider}'): descriptor runtime_abi_min {}, runtime supports {RUNTIME_ABI}",
            descriptor.runtime_abi_min
        ));
    }
    let platform = unsafe { descriptor_str(descriptor.platform, "platform", &import) }?;
    if platform != PLATFORM {
        return Err(format!(
            "hna module '{import}' (provider '{provider}'): descriptor platform '{platform}', runtime platform '{PLATFORM}'"
        ));
    }
    let module_name = unsafe { descriptor_str(descriptor.module_name, "module_name", &import) }?;
    if module_name.is_empty() {
        return Err(format!(
            "hna module '{import}': descriptor module_name is empty"
        ));
    }
    if let Some(expected) = expected {
        if module_name != expected {
            return Err(module_name_mismatch(expected, &module_name));
        }
    }
    if descriptor.flags & HNA_FLAG_RESERVED != 0 {
        return Err(format!(
            "hna module '{import}' (provider '{provider}'): descriptor flags 0x{:x} set reserved bits 0x{HNA_FLAG_RESERVED:x}",
            descriptor.flags
        ));
    }
    let thread_safe = descriptor.flags & HNA_FLAG_THREAD_SAFE != 0;

    let mut seen_names: HashSet<String> = HashSet::new();
    let mut functions = Vec::new();
    let function_count = descriptor.function_count;
    if function_count > MAX_DESCRIPTOR_ENTRIES {
        return Err(format!(
            "hna module '{import}': descriptor functions count {function_count} exceeds {MAX_DESCRIPTOR_ENTRIES}"
        ));
    }
    if function_count > 0 && descriptor.functions.is_null() {
        return Err(format!(
            "hna module '{import}': descriptor functions is NULL with count {function_count}"
        ));
    }
    if function_count > 0 {
        // SAFETY: the descriptor contract promises a static array of exactly
        // `function_count` entries when the pointer is non-NULL.
        let entries =
            unsafe { std::slice::from_raw_parts(descriptor.functions, function_count as usize) };
        for (index, function) in entries.iter().enumerate() {
            let context = format!("hna module '{import}': function {index}");
            let name = unsafe { descriptor_str(function.name, "name", &import) }?;
            if name.is_empty() {
                return Err(format!("{context} has an empty name"));
            }
            if !seen_names.insert(name.clone()) {
                return Err(format!("{context} duplicate name '{name}'"));
            }
            if function.flags != 0 {
                return Err(format!(
                    "{context} has reserved flags 0x{:x}",
                    function.flags
                ));
            }
            if function.min_args > function.max_args {
                return Err(format!(
                    "{context} min_args {} exceeds max_args {}",
                    function.min_args, function.max_args
                ));
            }
            let params = if function.params.is_null() {
                None
            } else {
                // SAFETY: the descriptor contract promises a NUL-terminated
                // static string when the pointer is non-NULL.
                Some(
                    unsafe { CStr::from_ptr(function.params) }
                        .to_str()
                        .map_err(|_| format!("{context} params is not valid UTF-8"))?,
                )
            };
            let params = parse_params(params, function.max_args, index, &import)?;
            let Some(call) = function.call else {
                return Err(format!("{context} has a NULL call pointer"));
            };
            functions.push(NativeFunction {
                name,
                min_args: function.min_args,
                max_args: function.max_args,
                params,
                call,
            });
        }
    }

    let mut constants = Vec::new();
    let constant_count = descriptor.constant_count;
    if constant_count > MAX_DESCRIPTOR_ENTRIES {
        return Err(format!(
            "hna module '{import}': descriptor constants count {constant_count} exceeds {MAX_DESCRIPTOR_ENTRIES}"
        ));
    }
    if constant_count > 0 && descriptor.constants.is_null() {
        return Err(format!(
            "hna module '{import}': descriptor constants is NULL with count {constant_count}"
        ));
    }
    if constant_count > 0 {
        // SAFETY: as for the function array.
        let entries =
            unsafe { std::slice::from_raw_parts(descriptor.constants, constant_count as usize) };
        for (index, constant) in entries.iter().enumerate() {
            let context = format!("hna module '{import}': constant {index}");
            let name = unsafe { descriptor_str(constant.name, "name", &import) }?;
            if name.is_empty() {
                return Err(format!("{context} has an empty name"));
            }
            if !seen_names.insert(name.clone()) {
                return Err(format!("{context} duplicate name '{name}'"));
            }
            let Some(value) = constant.value else {
                return Err(format!("{context} has a NULL value pointer"));
            };
            constants.push(ConstantThunk { name, value });
        }
    }

    Ok(ValidatedModule {
        module_name,
        hna_version: descriptor.hna_version,
        platform,
        provider,
        runtime_abi_min: descriptor.runtime_abi_min,
        thread_safe,
        init: descriptor.init,
        functions,
        constants,
    })
}

fn module_name_mismatch(expected: &str, found: &str) -> String {
    format!(
        "hna module '{expected}': descriptor module_name '{found}' does not match import name '{expected}'"
    )
}

/// Leaf module name of an HNA artifact: `hna_c` for `…/hna_c.hna.so`,
/// `_fast` for `…/pkg/_fast.hna.so`.
///
/// The HNA contract keeps the artifact stem a single component, so a stem that
/// is empty or still contains a dot yields `None` (no leaf check).
fn artifact_leaf(canonical: &Path) -> Option<String> {
    let file = canonical.file_name()?.to_str()?;
    let stem = file.strip_suffix(HNA_SUFFIX)?;
    if stem.is_empty() || stem.contains('.') {
        return None;
    }
    Some(stem.to_string())
}

/// Whether `provided` satisfies the artifact leaf `leaf`: the bare leaf
/// (`hna_c`) or a dotted module whose last component is the leaf
/// (`pkg._fast` for `pkg/_fast.hna.so`).
fn descriptor_leaf_matches(leaf: &str, provided: &str) -> bool {
    provided == leaf || provided.ends_with(&format!(".{leaf}"))
}

/// Checks the descriptor's module name before the rest of validation.
///
/// With an exact import name (`expected = Some(..)`) the descriptor must
/// provide exactly that name. Path-only entry points (`None`) derive the
/// artifact leaf instead and accept the bare leaf or a dotted package ending in
/// `.<leaf>`. A mismatch is [`LoadFailure::NameMismatch`], which is never cached
/// by path: the artifact can still load under its real dotted name later.
fn check_descriptor_name(
    expected: Option<&str>,
    canonical: &Path,
    descriptor: &HydroNativeModule,
) -> Result<(), LoadFailure> {
    let label = expected.unwrap_or("<artifact>");
    let provided = unsafe { descriptor_str(descriptor.module_name, "module_name", label) }
        .map_err(LoadFailure::Diagnostic)?;
    match expected {
        Some(expected) => {
            if provided != expected {
                return Err(LoadFailure::NameMismatch(module_name_mismatch(
                    expected, &provided,
                )));
            }
        }
        None => {
            // `None` means the caller only has the artifact path; an artifact
            // whose stem is not a single component has no derivable leaf.
            let Some(leaf) = artifact_leaf(canonical) else {
                return Ok(());
            };
            if !descriptor_leaf_matches(&leaf, &provided) {
                return Err(LoadFailure::NameMismatch(module_name_mismatch(
                    &leaf, &provided,
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Artifact loading
// ---------------------------------------------------------------------------

/// Canonicalizes an artifact path for the load cache. A path that cannot be
/// canonicalized (a synthetic path in a unit test) is used verbatim.
fn canonical_path(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Reads the dynamic-loader error message, if any.
fn dl_error() -> String {
    // SAFETY: `dlerror` returns NULL or a NUL-terminated static string.
    let ptr = unsafe { libc::dlerror() };
    if ptr.is_null() {
        return "unknown dynamic-loader error".to_string();
    }
    // SAFETY: non-NULL NUL-terminated string owned by the loader.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Opens the artifact and resolves the three HNA entry points.
fn open_artifact(path: &Path) -> Result<(usize, *const HydroNativeModule, u32), LoadFailure> {
    let Some(text) = path.to_str() else {
        return Err(LoadFailure::Diagnostic(format!(
            "hna artifact '{}' path is not valid UTF-8",
            path.display()
        )));
    };
    let Ok(c_path) = CString::new(text) else {
        return Err(LoadFailure::Diagnostic(format!(
            "hna artifact '{}' path contains a NUL byte",
            path.display()
        )));
    };
    // SAFETY: `c_path` is a valid NUL-terminated path. RTLD_LOCAL keeps the
    // artifact's symbols out of the process namespace and RTLD_NOW resolves
    // every relocation before the first call.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(LoadFailure::Diagnostic(format!(
            "cannot load hna artifact '{}': {}",
            path.display(),
            dl_error()
        )));
    }
    let handle = handle as usize;

    // SAFETY: the symbol names are static NUL-terminated strings and `handle`
    // is a live `dlopen` handle.
    let abi_ptr = unsafe { resolve_symbol(handle, b"hydro_native_abi\0", path)? };
    // SAFETY: as above.
    let build_ptr = unsafe { resolve_symbol(handle, b"hydro_native_abi_build\0", path)? };
    // SAFETY: as above.
    let module_ptr = unsafe { resolve_symbol(handle, b"hydro_native_module_v1\0", path)? };

    // SAFETY: the descriptor contract fixes these signatures; the symbols were
    // resolved from a library that exports them under the HNA names.
    let abi: NativeAbiFn = unsafe { std::mem::transmute::<*mut c_void, NativeAbiFn>(abi_ptr) };
    // SAFETY: as above.
    let abi_build: NativeAbiBuildFn =
        unsafe { std::mem::transmute::<*mut c_void, NativeAbiBuildFn>(build_ptr) };
    // SAFETY: as above.
    let module_fn: NativeModuleV1Fn =
        unsafe { std::mem::transmute::<*mut c_void, NativeModuleV1Fn>(module_ptr) };

    // SAFETY: the entry points are plain `extern "C"` functions with no
    // arguments; calling them runs only the library's own code.
    let reported = unsafe { abi() };
    if reported != HNA_VERSION {
        return Err(LoadFailure::Diagnostic(format!(
            "hna artifact '{}': hydro_native_abi returned {reported}, runtime supports {HNA_VERSION}",
            path.display()
        )));
    }
    // SAFETY: as for `abi`.
    let build = unsafe { abi_build() };
    // SAFETY: as for `abi`.
    let descriptor = unsafe { module_fn() };
    if descriptor.is_null() {
        return Err(LoadFailure::Diagnostic(format!(
            "hna artifact '{}': hydro_native_module_v1 returned NULL",
            path.display()
        )));
    }
    Ok((handle, descriptor, build))
}

/// Resolves one required export from a live handle.
///
/// # Safety
/// `handle` must come from `dlopen` and `name` must be NUL-terminated.
unsafe fn resolve_symbol(
    handle: usize,
    name: &[u8],
    path: &Path,
) -> Result<*mut c_void, LoadFailure> {
    // Clear a stale error so a NULL result is attributed to this lookup.
    // SAFETY: `dlerror` accepts no arguments.
    unsafe { libc::dlerror() };
    // SAFETY: `name` is NUL-terminated; `handle` is live.
    let ptr = unsafe { libc::dlsym(handle as *mut c_void, name.as_ptr() as *const c_char) };
    if ptr.is_null() {
        let symbol = name.strip_suffix(&[0]).unwrap_or(name);
        let symbol = String::from_utf8_lossy(symbol);
        return Err(LoadFailure::Diagnostic(format!(
            "hna artifact '{}' does not export '{symbol}'",
            path.display()
        )));
    }
    Ok(ptr)
}

/// Runs a module initializer, exactly once per load.
///
/// `init == 0` is success. A non-zero return with no pending exception becomes
/// an `ImportError` naming the module and provider; a pending exception is
/// propagated untouched. A panic aborts the process.
fn run_init(name: &str, provider: &str, init: Option<HydroInitFn>) -> Result<(), LoadFailure> {
    let Some(init) = init else {
        return Ok(());
    };
    // SAFETY: the descriptor contract fixes the signature; the loader calls it
    // once, before the module value is published.
    let outcome = catch_unwind(AssertUnwindSafe(|| unsafe { init() }));
    match outcome {
        Ok(0) => {
            if exc::pending() {
                Err(LoadFailure::Pending)
            } else {
                Ok(())
            }
        }
        Ok(code) => {
            if exc::pending() {
                Err(LoadFailure::Pending)
            } else {
                Err(LoadFailure::Diagnostic(format!(
                    "hna module '{name}' (provider '{provider}'): init returned {code} and set no exception"
                )))
            }
        }
        Err(payload) => abort_panic(name, None, payload),
    }
}

/// Reports a panic raised inside a binding and aborts.
///
/// The panic is an internal error (`docs/NATIVE_BINDINGS.md` §3.4): it is never
/// converted into a Python exception and never unwinds into Hydro, so state
/// cannot be observed half-updated.
fn abort_panic(module: &str, function: Option<&str>, payload: Box<dyn std::any::Any + Send>) -> ! {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_string());
    match function {
        Some(function) => {
            eprintln!("hydro: panic in HNA module '{module}' function '{function}': {detail}")
        }
        None => eprintln!("hydro: panic in HNA module '{module}' during init: {detail}"),
    }
    std::process::abort()
}

/// Loads (or returns the cached entry for) one artifact.
fn load_module(expected: Option<&str>, path: &str) -> Result<Arc<ModuleEntry>, LoadFailure> {
    let canonical = canonical_path(path);
    match cached_lookup(&canonical, expected) {
        Lookup::Ready(entry) => return Ok(entry),
        Lookup::Failed(message) => return Err(LoadFailure::Diagnostic(message)),
        Lookup::Miss => {}
    }

    let lock = load_lock(&canonical);
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match cached_lookup(&canonical, expected) {
        Lookup::Ready(entry) => return Ok(entry),
        Lookup::Failed(message) => return Err(LoadFailure::Diagnostic(message)),
        Lookup::Miss => {}
    }

    let label = match expected {
        Some(name) => name.to_string(),
        None => canonical.display().to_string(),
    };
    if loading_this_thread(&canonical) {
        return Err(LoadFailure::Diagnostic(format!(
            "circular import of hna module '{label}' from '{}'",
            canonical.display()
        )));
    }

    push_loading(&canonical);
    let result = load_new(expected, &canonical);
    pop_loading(&canonical);

    match result {
        Ok(entry) => {
            let mut registry = lock_registry();
            registry.by_id.insert(entry.id, Arc::clone(&entry));
            registry.by_path.insert(canonical, entry.clone());
            Ok(entry)
        }
        Err(LoadFailure::Diagnostic(message)) => {
            record_failure(&canonical, &message);
            Err(LoadFailure::Diagnostic(message))
        }
        Err(LoadFailure::NameMismatch(message)) => Err(LoadFailure::NameMismatch(message)),
        Err(LoadFailure::Pending) => {
            // The pending exception is the user-visible error; the cached
            // diagnostic keeps later loads stable without re-running `init`.
            let message = format!(
                "hna module '{label}': initialization raised an exception and the module is unavailable"
            );
            record_failure(&canonical, &message);
            Err(LoadFailure::Pending)
        }
    }
}

enum Lookup {
    Ready(Arc<ModuleEntry>),
    Failed(String),
    Miss,
}

fn cached_lookup(canonical: &Path, expected: Option<&str>) -> Lookup {
    let registry = lock_registry();
    if let Some(message) = registry.failures.get(canonical).cloned() {
        return Lookup::Failed(message);
    }
    let entry = registry.by_path.get(canonical).cloned();
    let Some(entry) = entry else {
        return Lookup::Miss;
    };
    if let Some(expected) = expected {
        if entry.name != expected {
            // The artifact is fine; this import name is not the one it
            // provides. Recomputed each time instead of cached: the message
            // names both sides, so it is specific to the requested name.
            let message = module_name_mismatch(expected, &entry.name);
            return Lookup::Failed(message);
        }
    }
    Lookup::Ready(entry)
}

fn load_lock(canonical: &Path) -> Arc<Mutex<()>> {
    let mut registry = lock_registry();
    Arc::clone(
        registry
            .load_locks
            .entry(canonical.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

fn record_failure(canonical: &Path, message: &str) {
    let mut registry = lock_registry();
    registry
        .failures
        .insert(canonical.to_path_buf(), message.to_string());
}

/// Performs the actual load: manifest validation, `dlopen`, validation, `init`,
/// constants, publish.
fn load_new(expected: Option<&str>, canonical: &Path) -> Result<Arc<ModuleEntry>, LoadFailure> {
    // The package manifest is read before `dlopen`: a malformed manifest is a
    // load error even when the artifact would otherwise load. A distribution
    // without a manifest keeps the pre-manifest behaviour.
    let manifest =
        crate::native_manifest::find_for_artifact(canonical).map_err(LoadFailure::Diagnostic)?;
    let (handle, descriptor_ptr, build) = open_artifact(canonical)?;
    // SAFETY: `open_artifact` rejected NULL and the descriptor contract
    // promises static read-only storage valid for the process lifetime.
    let descriptor = unsafe { &*descriptor_ptr };
    // A descriptor that names a different module is a wrong-name import of a
    // valid artifact, not a broken artifact: report it without caching so the
    // correct import name can still load this path. Path-only entry points
    // (`get`/`call`/`import_member` with no expected name) compare against the
    // artifact's leaf component instead.
    check_descriptor_name(expected, canonical, descriptor)?;
    let validated = validate_descriptor(expected, descriptor).map_err(LoadFailure::Diagnostic)?;
    if let Some(found) = &manifest {
        let facts = crate::native_manifest::DescriptorFacts {
            module_name: &validated.module_name,
            hna_version: validated.hna_version,
            platform: &validated.platform,
            provider: &validated.provider,
            thread_safe: validated.thread_safe,
            runtime_abi_min: validated.runtime_abi_min,
        };
        crate::native_manifest::check_descriptor(found, &facts).map_err(LoadFailure::Diagnostic)?;
    }
    let name = validated.module_name.clone();
    let path_string = canonical.to_string_lossy().into_owned();

    // The module value must exist and stay reachable while `init` runs: the
    // binding may stash it, and the constant thunks may allocate.
    let module_value = value::extension_module_value(name.clone(), path_string);
    let _guard = ThreadRoots(crate::heap::root_values(&[module_value]));

    run_init(&name, &validated.provider, validated.init)?;

    let mut roots = Vec::new();
    let mut constants = Vec::with_capacity(validated.constants.len());
    for constant in &validated.constants {
        // SAFETY: the descriptor contract fixes the thunk signature.
        let cached = catch_unwind(AssertUnwindSafe(|| unsafe { (constant.value)() }));
        let cached = match cached {
            Ok(value) => value,
            Err(payload) => abort_panic(&name, None, payload),
        };
        if exc::pending() {
            for slot in roots {
                crate::heap::remove_persistent_root(slot);
            }
            return Err(LoadFailure::Pending);
        }
        let slot = crate::heap::add_persistent_root(cached);
        // Outside a collector registration nothing can collect, so a missing
        // slot is not an error; it simply needs no cleanup.
        if slot != usize::MAX {
            roots.push(slot);
        }
        constants.push(NativeConstant {
            name: constant.name.clone(),
            value: cached,
        });
    }
    let module_slot = crate::heap::add_persistent_root(module_value);
    if module_slot != usize::MAX {
        roots.push(module_slot);
    }

    let id = {
        let mut registry = lock_registry();
        let id = registry.next_id;
        registry.next_id = registry.next_id.saturating_add(1);
        id
    };

    let entry = Arc::new(ModuleEntry {
        id,
        name: name.clone(),
        _handle: handle,
        _descriptor: descriptor_ptr,
        _build: build,
        provider: validated.provider,
        thread_safe: validated.thread_safe,
        functions: validated.functions,
        constants,
        init_result: Ok(()),
        module_value,
        _roots: roots,
        call_lock: Mutex::new(()),
    });

    // Publish only after `init` and constant evaluation succeeded, so a failed
    // load never leaves a `sys.modules` entry this registry does not own.
    crate::modules::publish_module(&name, module_value);
    Ok(entry)
}

// ---------------------------------------------------------------------------
// Extension entry points
// ---------------------------------------------------------------------------

/// Materialises the module value for `<name>.hna.so` (`import ext`).
pub(crate) fn extension_module_value(name: &str, path: &str) -> i64 {
    match load_module(Some(name), path) {
        Ok(entry) => entry.module_value,
        Err(failure) => raise_load_failure(failure),
    }
}

/// `ext.member` when the import name is known (module attribute access).
pub(crate) fn get_named(import_name: &str, path: &str, member: &str) -> i64 {
    match load_module(Some(import_name), path) {
        Ok(entry) => get_from(&entry, member),
        Err(failure) => raise_load_failure(failure),
    }
}

/// `ext.member` for the path-only extension entry points.
pub(crate) fn get(path: &str, member: &str) -> i64 {
    match load_module(None, path) {
        Ok(entry) => get_from(&entry, member),
        Err(failure) => raise_load_failure(failure),
    }
}

fn get_from(entry: &ModuleEntry, member: &str) -> i64 {
    if let Some(index) = entry.functions.iter().position(|f| f.name == member) {
        return function_value(entry, index);
    }
    if let Some(constant) = entry.constants.iter().find(|c| c.name == member) {
        return constant.value;
    }
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!(
            "extension module '{}' has no attribute '{member}'",
            entry.name
        ),
    )
}

/// Allocates the first-class function value for `entry.functions[index]`.
fn function_value(entry: &ModuleEntry, index: usize) -> i64 {
    let function = &entry.functions[index];
    value::hna_function_new(
        entry.id,
        index as u32,
        function.name.clone(),
        function.params.clone(),
        function.min_args as usize,
    )
}

/// `from ext import member`: a failed load or a missing name is `ImportError`.
pub(crate) fn import_member(module: &str, path: &str, member: &str) -> i64 {
    let entry = match load_module(Some(module), path) {
        Ok(entry) => entry,
        Err(failure) => return raise_load_failure(failure),
    };
    if let Some(index) = entry.functions.iter().position(|f| f.name == member) {
        return function_value(&entry, index);
    }
    if let Some(constant) = entry.constants.iter().find(|c| c.name == member) {
        return constant.value;
    }
    crate::builtins::cannot_import_name(member, module)
}

/// `extension.call(path, member, args...)` for an HNA function.
pub(crate) fn call_member(path: &str, member: &str, argc: usize, argv: *const i64) -> i64 {
    let entry = match load_module(None, path) {
        Ok(entry) => entry,
        Err(failure) => return raise_load_failure(failure),
    };
    if let Some(index) = entry.functions.iter().position(|f| f.name == member) {
        return call_function(entry.id, index as u32, argc, argv);
    }
    if let Some(constant) = entry.constants.iter().find(|c| c.name == member) {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "'{}' object is not callable",
                value::type_name_of(constant.value)
            ),
        );
    }
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!(
            "extension module '{}' has no attribute '{member}'",
            entry.name
        ),
    )
}

/// Attribute names exported by the descriptor, for `dir(module)`.
///
/// The `Err` text carries an `ImportError: ` prefix because `dir()` routes it
/// back through the extension shim's message classifier.
pub(crate) fn attribute_names(name: Option<&str>, path: &str) -> Result<Vec<String>, String> {
    match load_module(name, path) {
        Ok(entry) => {
            let mut names = Vec::with_capacity(entry.functions.len() + entry.constants.len());
            names.extend(entry.functions.iter().map(|function| function.name.clone()));
            names.extend(entry.constants.iter().map(|constant| constant.name.clone()));
            Ok(names)
        }
        Err(LoadFailure::Pending) => {
            Err("ImportError: hna module initialization raised an exception".to_string())
        }
        Err(LoadFailure::Diagnostic(message) | LoadFailure::NameMismatch(message)) => {
            Err(format!("ImportError: {message}"))
        }
    }
}

fn raise_load_failure(failure: LoadFailure) -> i64 {
    match failure {
        LoadFailure::Pending => value::NONE,
        LoadFailure::Diagnostic(message) | LoadFailure::NameMismatch(message) => {
            exc::raise_with_message(crate::exc::IMPORT_ERROR, &message)
        }
    }
}

// ---------------------------------------------------------------------------
// Call dispatch
// ---------------------------------------------------------------------------

/// Positional call of an HNA function value (`FunctionBody::Hna`).
pub(crate) fn call_function(module: u64, index: u32, argc: usize, argv: *const i64) -> i64 {
    let entry = match entry_by_id(module) {
        Some(entry) => entry,
        None => {
            return exc::raise_with_message(
                RUNTIME_ERROR,
                &format!("hna module registry id {module} is unknown"),
            )
        }
    };
    if let Some(error) = module_init_error(&entry) {
        return error;
    }
    let Some(function) = entry.functions.get(index as usize) else {
        return exc::raise_with_message(
            RUNTIME_ERROR,
            &format!("hna module '{}' has no function index {index}", entry.name),
        );
    };
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{}() received {argc} argument(s) without an argument buffer",
                function.name
            ),
        );
    }
    if argc > function.max_args as usize {
        return too_many_positional(function, argc);
    }
    if argc < function.min_args as usize {
        return too_few_positional(function, argc);
    }
    let args: &[i64] = if argc == 0 {
        &[]
    } else {
        // SAFETY: NULL with argc > 0 was rejected above; the caller guarantees
        // `argc` readable tagged values.
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    invoke(&entry, function, args)
}

/// Keyword call of an HNA function value.
///
/// The descriptor's `params` name the keyword-bindable positions; every named
/// parameter is required (HNA v1 has no defaults), so a hole is reported
/// exactly like CPython's missing-argument error.
pub(crate) fn call_function_kw(
    module: u64,
    index: u32,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    let entry = match entry_by_id(module) {
        Some(entry) => entry,
        None => {
            return exc::raise_with_message(
                RUNTIME_ERROR,
                &format!("hna module registry id {module} is unknown"),
            )
        }
    };
    if let Some(error) = module_init_error(&entry) {
        return error;
    }
    let Some(function) = entry.functions.get(index as usize) else {
        return exc::raise_with_message(
            RUNTIME_ERROR,
            &format!("hna module '{}' has no function index {index}", entry.name),
        );
    };
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(TYPE_ERROR, "missing argument buffer");
    }
    if kwargc > 0
        && (kw_argv.is_null() || kwargc > isize::MAX as usize / (2 * std::mem::size_of::<i64>()))
    {
        return exc::raise_with_message(
            TYPE_ERROR,
            "keyword arguments were passed without an argument buffer",
        );
    }
    if argc > function.max_args as usize {
        return too_many_positional(function, argc);
    }

    let name = function.name.clone();
    let slots_len = argc.max(function.params.len());
    let mut slots: Vec<Option<i64>> = vec![None; slots_len];
    if argc > 0 {
        // SAFETY: NULL with argc > 0 was rejected above.
        let positional = unsafe { std::slice::from_raw_parts(argv, argc) };
        for (slot, argument) in slots.iter_mut().zip(positional.iter()) {
            *slot = Some(*argument);
        }
    }
    if kwargc > 0 {
        // SAFETY: the buffer length was checked above.
        let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
        for pair in pairs.as_chunks::<2>().0 {
            let Some(keyword) = value::as_str(pair[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "keyword argument name must be str, not '{}'",
                        value::type_name_of(pair[0])
                    ),
                );
            };
            let Some(position) = function.params.iter().position(|param| param == keyword) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{name}() got an unexpected keyword argument '{keyword}'"),
                );
            };
            if position < argc {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{name}() got multiple values for argument '{keyword}'"),
                );
            }
            if slots[position].is_some() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{name}() got multiple values for keyword argument '{keyword}'"),
                );
            }
            slots[position] = Some(pair[1]);
        }
    }
    if let Some(position) = slots.iter().position(Option::is_none) {
        let message = match function.params.get(position) {
            Some(param) => format!("{name}() missing required argument '{param}'"),
            None => format!(
                "{name}() takes at least {} positional argument(s) ({} given)",
                function.min_args,
                slots.len()
            ),
        };
        return exc::raise_with_message(TYPE_ERROR, &message);
    }
    let bound: Vec<i64> = slots
        .into_iter()
        .map(|slot| slot.unwrap_or(value::NONE))
        .collect();
    if bound.len() < function.min_args as usize {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{name}() takes at least {} positional argument(s) ({} given)",
                function.min_args,
                bound.len()
            ),
        );
    }
    invoke(&entry, function, &bound)
}

/// Raises `ImportError` naming the module and provider when a call reaches a
/// module whose single `init` failed. The loader never publishes such a
/// module, so this is a defensive guard for a stale function value.
fn module_init_error(entry: &ModuleEntry) -> Option<i64> {
    let Err(message) = &entry.init_result else {
        return None;
    };
    let text = if message.is_empty() {
        format!(
            "hna module '{}' (provider '{}'): init failed",
            entry.name, entry.provider
        )
    } else {
        message.clone()
    };
    Some(exc::raise_with_message(IMPORT_ERROR, &text))
}

fn entry_by_id(id: u64) -> Option<Arc<ModuleEntry>> {
    lock_registry().by_id.get(&id).cloned()
}

fn too_many_positional(function: &NativeFunction, argc: usize) -> i64 {
    let message = if function.min_args == function.max_args {
        format!(
            "{}() takes {} positional argument(s) ({argc} given)",
            function.name, function.min_args
        )
    } else {
        format!(
            "{}() takes at most {} positional argument(s) ({argc} given)",
            function.name, function.max_args
        )
    };
    exc::raise_with_message(TYPE_ERROR, &message)
}

fn too_few_positional(function: &NativeFunction, argc: usize) -> i64 {
    let message = match function.params.get(argc) {
        Some(param) => format!("{}() missing required argument '{param}'", function.name),
        None if function.min_args == function.max_args => format!(
            "{}() takes {} positional argument(s) ({argc} given)",
            function.name, function.min_args
        ),
        None => format!(
            "{}() takes at least {} positional argument(s) ({argc} given)",
            function.name, function.min_args
        ),
    };
    exc::raise_with_message(TYPE_ERROR, &message)
}

/// Calls the binding's function pointer with every argument rooted.
///
/// `self` is the module tagged value. The arguments (and `self`) stay
/// reachable for the whole call; the result is returned as the binding left
/// it, or replaced by the `None` immediate when the binding set a pending
/// exception.
fn invoke(entry: &ModuleEntry, function: &NativeFunction, args: &[i64]) -> i64 {
    let module_value = entry.module_value;
    let mut held: Vec<i64> = Vec::with_capacity(args.len() + 1);
    held.push(module_value);
    held.extend_from_slice(args);
    let roots = crate::heap::root_values(&held);
    let call = function.call;
    let argc = args.len() as u64;
    // Pass the runtime-rooted copy, never the caller's buffer: the caller's
    // buffer may live in Rust heap memory the conservative collector cannot
    // scan, while `held` is explicitly registered above.
    let argv = if args.is_empty() {
        std::ptr::null()
    } else {
        // SAFETY: `held` has `args.len() + 1 >= 1` elements and is not
        // modified for the rest of the call.
        unsafe { held.as_ptr().add(1) }
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `call` is the descriptor's entry point for this function and
        // receives exactly `argc` readable tagged values.
        if entry.thread_safe {
            unsafe { call(module_value, argc, argv) }
        } else {
            let _guard = entry
                .call_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            unsafe { call(module_value, argc, argv) }
        }
    }));
    crate::heap::release_roots(roots);
    match outcome {
        Ok(result) => {
            if exc::pending() {
                value::NONE
            } else {
                result
            }
        }
        Err(payload) => abort_panic(&entry.name, Some(&function.name), payload),
    }
}

// ---------------------------------------------------------------------------
// Persistent root tokens
// ---------------------------------------------------------------------------

struct Hold {
    /// Persistent-root slot in the calling thread's collector registration, or
    /// `usize::MAX` when no collector is registered (nothing can collect).
    slot: usize,
    value: i64,
}

struct HoldTable {
    next_token: u64,
    holds: HashMap<u64, Hold>,
}

impl Default for HoldTable {
    fn default() -> HoldTable {
        HoldTable {
            next_token: 1,
            holds: HashMap::new(),
        }
    }
}

static HOLDS: LazyLock<Mutex<HoldTable>> = LazyLock::new(|| Mutex::new(HoldTable::default()));

fn lock_holds() -> MutexGuard<'static, HoldTable> {
    HOLDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `hydro_root_hold`: roots `value` for the process lifetime of the token.
///
/// The persistent root belongs to the calling thread's collector
/// registration; HNA is single-threaded per binding, so hold and release must
/// come from the same Hydro thread. The token table itself is process-wide and
/// a release from another thread is ignored rather than corrupting a slot.
pub(crate) fn root_hold(value: i64) -> u64 {
    let slot = crate::heap::add_persistent_root(value);
    let mut table = lock_holds();
    let token = table.next_token;
    table.next_token = table.next_token.checked_add(1).unwrap_or(1);
    table.holds.insert(token, Hold { slot, value });
    token
}

/// `hydro_root_release`: drops a hold token. Unknown, already released or
/// zero tokens are ignored.
pub(crate) fn root_release(token: u64) {
    if token == 0 {
        return;
    }
    let hold = lock_holds().holds.remove(&token);
    if let Some(hold) = hold {
        if hold.slot != usize::MAX {
            crate::heap::release_persistent_root(hold.slot, hold.value);
        }
    }
}

/// Clears the module registry and the hold tokens for a fresh program run.
///
/// Called by the collector reset (`heap::reset`) together with the other
/// process-wide registries: the tagged values, cached constants and persistent
/// roots of the previous run are all gone.
pub(crate) fn reset() {
    {
        let mut registry = lock_registry();
        registry.next_id = 0;
        registry.by_id.clear();
        registry.by_path.clear();
        registry.failures.clear();
        registry.load_locks.clear();
    }
    lock_holds().holds.clear();
}

/// Keeps the HNA service symbols in an AOT static archive.
///
/// A staticlib linker keeps a definition only when something references it.
/// Generated Hydro code references the runtime entry points it lowers, but an
/// HNA binding resolves its full service set at `dlopen` time (value
/// constructors, root holds, typed projections and raise helpers), so without
/// an explicit reference the linker drops those definitions and loading a
/// binding fails with `undefined symbol: hydro_raise_type_error` (and
/// friends).
///
/// `hydro_runtime_init` calls this next to `capi::anchor()`, exactly like the
/// CPython shim anchor: taking every address here keeps the definitions in the
/// linked image, and Tea AOT's `--export-dynamic` then puts them in the
/// executable's dynamic symbol table for the binding to resolve.
pub(crate) fn anchor() {
    let services: &[*const ()] = &[
        // HNA v1 runtime services.
        crate::abi::hydro_hna_runtime_abi as *const (),
        crate::abi::hydro_root_hold as *const (),
        crate::abi::hydro_root_release as *const (),
        // Tagged-value services (`hydro_runtime::abi`).
        crate::abi::hydro_none as *const (),
        crate::abi::hydro_true as *const (),
        crate::abi::hydro_false as *const (),
        crate::abi::hydro_bool as *const (),
        crate::abi::hydro_int as *const (),
        crate::abi::hydro_float_new as *const (),
        crate::abi::hydro_str_new as *const (),
        crate::abi::hydro_bytes_new as *const (),
        crate::abi::hydro_list_new as *const (),
        crate::abi::hydro_tuple_new as *const (),
        crate::abi::hydro_dict_new as *const (),
        crate::abi::hydro_list_append as *const (),
        crate::abi::hydro_dict_set as *const (),
        crate::abi::hydro_getattr as *const (),
        crate::abi::hydro_setattr as *const (),
        crate::abi::hydro_call as *const (),
        crate::abi::hydro_to_str as *const (),
        crate::abi::hydro_truthy as *const (),
        crate::abi::hydro_binop as *const (),
        crate::abi::hydro_exc_pending as *const (),
        crate::abi::hydro_exc_tag as *const (),
        crate::abi::hydro_raise_msg as *const (),
        crate::abi::hydro_gc_scope_push as *const (),
        crate::abi::hydro_gc_scope_pop as *const (),
        // Typed projections and raise helpers (HNA v1.0 service set).
        crate::abi::hydro_type_tag as *const (),
        crate::abi::hydro_int_value as *const (),
        crate::abi::hydro_float_value as *const (),
        crate::abi::hydro_bool_value as *const (),
        crate::abi::hydro_str_copy as *const (),
        crate::abi::hydro_bytes_copy as *const (),
        crate::abi::hydro_raise_type_error as *const (),
        crate::abi::hydro_raise_value_error as *const (),
        crate::abi::hydro_raise_overflow_error as *const (),
    ];
    let _ = std::hint::black_box(services);
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydro_hna::{HydroConstant, HydroFunction};

    /// Serializes the tests that touch process-wide state (the root token
    /// table). Descriptor validation is pure and does not allocate Hydro
    /// objects, so those tests run in parallel.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    unsafe extern "C" fn test_call(_self: i64, argc: u64, _argv: *const i64) -> i64 {
        argc as i64
    }

    unsafe extern "C" fn test_value() -> i64 {
        42
    }

    fn c(text: &str) -> CString {
        CString::new(text).expect("test string has no NUL")
    }

    fn base_descriptor(
        name: &CString,
        provider: &CString,
        platform: &CString,
    ) -> HydroNativeModule {
        HydroNativeModule {
            struct_size: std::mem::size_of::<HydroNativeModule>() as u32,
            hna_version: HNA_VERSION,
            runtime_abi_min: 1,
            module_name: name.as_ptr(),
            provider: provider.as_ptr(),
            platform: platform.as_ptr(),
            flags: 0,
            init: None,
            function_count: 0,
            functions: std::ptr::null(),
            constant_count: 0,
            constants: std::ptr::null(),
        }
    }

    #[test]
    fn accepts_a_minimal_descriptor() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let descriptor = base_descriptor(&name, &provider, &platform);
        let validated = validate_descriptor(Some("demo._fast"), &descriptor)
            .expect("minimal descriptor is valid");
        assert_eq!(validated.module_name, "demo._fast");
        assert_eq!(validated.provider, "demo-native 0.1.0");
        assert!(!validated.thread_safe);
        assert!(validated.functions.is_empty());
        assert!(validated.constants.is_empty());
    }

    #[test]
    fn rejects_hna_version_mismatch() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.hna_version = 2;
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("descriptor hna_version 2, runtime supports 1"),
            "{message}"
        );
        assert!(
            message.contains("provider 'demo-native 0.1.0'"),
            "{message}"
        );
        assert!(message.contains("hna module 'demo._fast'"), "{message}");

        // Path-only extension loads pass no import name; the diagnostic still
        // names the descriptor's module rather than a placeholder.
        let message = validate_descriptor(None, &descriptor).unwrap_err();
        assert!(message.contains("hna module 'demo._fast'"), "{message}");
        assert!(!message.contains("<artifact>"), "{message}");

        // A NULL module_name falls back to the neutral label, and the real
        // problem is still reported by the module_name check afterwards.
        let mut anonymous = base_descriptor(&name, &provider, &platform);
        anonymous.module_name = std::ptr::null();
        let message = validate_descriptor(None, &anonymous).unwrap_err();
        assert!(
            message.contains("hna module '<artifact>': descriptor module_name is NULL"),
            "{message}"
        );
    }

    #[test]
    fn rejects_runtime_abi_min_outside_the_supported_range() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.runtime_abi_min = 2;
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("descriptor runtime_abi_min 2, runtime supports 1"),
            "{message}"
        );

        descriptor.runtime_abi_min = 0;
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("descriptor runtime_abi_min 0, runtime supports 1"),
            "{message}"
        );
    }

    #[test]
    fn rejects_platform_mismatch() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c("darwin-arm64");
        let descriptor = base_descriptor(&name, &provider, &platform);
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("descriptor platform 'darwin-arm64', runtime platform 'linux-x86_64'"),
            "{message}"
        );
    }

    #[test]
    fn rejects_module_name_mismatch() {
        let name = c("demo.fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let descriptor = base_descriptor(&name, &provider, &platform);
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert_eq!(
            message,
            "hna module 'demo._fast': descriptor module_name 'demo.fast' does not match import name 'demo._fast'"
        );
    }

    #[test]
    fn path_only_loads_derive_the_artifact_leaf() {
        assert_eq!(
            artifact_leaf(Path::new("/tmp/tea-hna-1/hna_c.hna.so")).as_deref(),
            Some("hna_c")
        );
        assert_eq!(
            artifact_leaf(Path::new("/tmp/tea-hna-1/pkg/_fast.hna.so")).as_deref(),
            Some("_fast")
        );
        assert_eq!(
            artifact_leaf(Path::new("/tmp/pkg/not-an-artifact.so")),
            None
        );
        assert_eq!(artifact_leaf(Path::new("/tmp/pkg/a.b.hna.so")), None);
    }

    #[test]
    fn path_only_name_check_accepts_leaf_or_dotted_suffix() {
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let artifact = Path::new("/tmp/tea-hna-1/hna_c.hna.so");

        let name = c("hna_c");
        let descriptor = base_descriptor(&name, &provider, &platform);
        assert!(check_descriptor_name(None, artifact, &descriptor).is_ok());

        // The file is `hna_c.hna.so` but the descriptor claims another module.
        let wrong = c("hna_wrong");
        let wrong_descriptor = base_descriptor(&wrong, &provider, &platform);
        match check_descriptor_name(None, artifact, &wrong_descriptor).unwrap_err() {
            LoadFailure::NameMismatch(message) => {
                assert!(message.contains("hna module 'hna_c'"), "{message}");
                assert!(
                    message.contains("descriptor module_name 'hna_wrong'"),
                    "{message}"
                );
            }
            other => panic!("expected a name mismatch, got {other:?}"),
        }

        // `pkg/_fast.hna.so` accepts both `_fast` and a dotted `*. _fast`.
        let dotted = Path::new("/tmp/tea-hna-1/pkg/_fast.hna.so");
        let package = c("pkg._fast");
        let package_descriptor = base_descriptor(&package, &provider, &platform);
        assert!(check_descriptor_name(None, dotted, &package_descriptor).is_ok());

        let bare = c("_fast");
        let bare_descriptor = base_descriptor(&bare, &provider, &platform);
        assert!(check_descriptor_name(None, dotted, &bare_descriptor).is_ok());

        // An exact expected name stays exact: `_fast` is not `pkg._fast`.
        match check_descriptor_name(Some("pkg._fast"), dotted, &bare_descriptor).unwrap_err() {
            LoadFailure::NameMismatch(message) => {
                assert!(message.contains("hna module 'pkg._fast'"), "{message}");
                assert!(
                    message.contains("descriptor module_name '_fast'"),
                    "{message}"
                );
            }
            other => panic!("expected a name mismatch, got {other:?}"),
        }
    }

    #[test]
    fn rejects_reserved_module_flags() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.flags = HNA_FLAG_THREAD_SAFE | 0x2;
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("descriptor flags 0x3 set reserved bits 0xfffffffe"),
            "{message}"
        );

        descriptor.flags = HNA_FLAG_THREAD_SAFE;
        let validated = validate_descriptor(Some("demo._fast"), &descriptor)
            .expect("thread_safe alone is valid");
        assert!(validated.thread_safe);
    }

    #[test]
    fn rejects_short_struct_size() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.struct_size = 32;
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        let expected = std::mem::size_of::<HydroNativeModule>();
        assert!(
            message.contains(&format!(
                "descriptor struct_size 32, runtime expects at least {expected}"
            )),
            "{message}"
        );
    }

    #[test]
    fn rejects_function_reserved_flags() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let fname = c("add");
        let functions = [HydroFunction {
            name: fname.as_ptr(),
            min_args: 2,
            max_args: 2,
            flags: 1,
            params: std::ptr::null(),
            call: Some(test_call),
        }];
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.function_count = 1;
        descriptor.functions = functions.as_ptr();
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("function 0 has reserved flags 0x1"),
            "{message}"
        );
    }

    #[test]
    fn rejects_null_function_call() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let fname = c("add");
        let functions = [HydroFunction {
            name: fname.as_ptr(),
            min_args: 2,
            max_args: 2,
            flags: 0,
            params: std::ptr::null(),
            call: None,
        }];
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.function_count = 1;
        descriptor.functions = functions.as_ptr();
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("function 0 has a NULL call pointer"),
            "{message}"
        );
    }

    #[test]
    fn rejects_min_args_above_max_args() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let fname = c("add");
        let functions = [HydroFunction {
            name: fname.as_ptr(),
            min_args: 3,
            max_args: 2,
            flags: 0,
            params: std::ptr::null(),
            call: Some(test_call),
        }];
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.function_count = 1;
        descriptor.functions = functions.as_ptr();
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("function 0 min_args 3 exceeds max_args 2"),
            "{message}"
        );
    }

    #[test]
    fn parses_comma_separated_keyword_names() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let fname = c("add");
        let params = c("a,b");
        let functions = [HydroFunction {
            name: fname.as_ptr(),
            min_args: 2,
            max_args: 2,
            flags: 0,
            params: params.as_ptr(),
            call: Some(test_call),
        }];
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.function_count = 1;
        descriptor.functions = functions.as_ptr();
        let validated = validate_descriptor(Some("demo._fast"), &descriptor).expect("valid params");
        assert_eq!(validated.functions[0].params, vec!["a", "b"]);
        assert_eq!(validated.functions[0].min_args, 2);
        assert_eq!(validated.functions[0].max_args, 2);
    }

    #[test]
    fn rejects_empty_duplicate_and_excess_keyword_names() {
        assert_eq!(
            parse_params(Some("a,,b"), 3, 0, "demo._fast").unwrap_err(),
            "hna module 'demo._fast': function 0 has an empty keyword name at position 1"
        );
        assert_eq!(
            parse_params(Some("a,a"), 3, 0, "demo._fast").unwrap_err(),
            "hna module 'demo._fast': function 0 has duplicate keyword name 'a'"
        );
        assert_eq!(
            parse_params(Some("a,b"), 1, 0, "demo._fast").unwrap_err(),
            "hna module 'demo._fast': function 0 declares 2 keyword names, more than max_args 1"
        );
        assert!(parse_params(None, 0, 0, "demo._fast").unwrap().is_empty());
        assert!(parse_params(Some(""), 0, 0, "demo._fast")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rejects_duplicate_names_across_functions_and_constants() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let fname = c("answer");
        let cname = c("answer");
        let functions = [HydroFunction {
            name: fname.as_ptr(),
            min_args: 0,
            max_args: 0,
            flags: 0,
            params: std::ptr::null(),
            call: Some(test_call),
        }];
        let constants = [HydroConstant {
            name: cname.as_ptr(),
            value: Some(test_value),
        }];
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.function_count = 1;
        descriptor.functions = functions.as_ptr();
        descriptor.constant_count = 1;
        descriptor.constants = constants.as_ptr();
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("constant 0 duplicate name 'answer'"),
            "{message}"
        );
    }

    #[test]
    fn rejects_null_arrays_with_a_nonzero_count() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.function_count = 2;
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("descriptor functions is NULL with count 2"),
            "{message}"
        );

        descriptor.function_count = 0;
        descriptor.constant_count = 2;
        let message = validate_descriptor(Some("demo._fast"), &descriptor).unwrap_err();
        assert!(
            message.contains("descriptor constants is NULL with count 2"),
            "{message}"
        );
    }

    #[test]
    fn hna_runtime_abi_is_the_supported_revision() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(crate::abi::hydro_hna_runtime_abi(), 1);
    }

    #[test]
    fn root_hold_and_release_round_trip() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let first = crate::abi::hydro_root_hold(42);
        let second = crate::abi::hydro_root_hold(value::NONE);
        // A concurrent test may reset the runtime between the two holds; a
        // reset invalidates old tokens, but a token is never zero.
        assert_ne!(first, 0);
        assert_ne!(second, 0);
        // Releasing is idempotent: a stale token is ignored, not a crash.
        crate::abi::hydro_root_release(first);
        crate::abi::hydro_root_release(first);
        crate::abi::hydro_root_release(second);
        crate::abi::hydro_root_release(0);
        crate::abi::hydro_root_release(u64::MAX);
    }

    #[test]
    fn descriptor_function_signature_is_copied() {
        let name = c("demo._fast");
        let provider = c("demo-native 0.1.0");
        let platform = c(PLATFORM);
        let fname = c("add");
        let params = c("a,b");
        let functions = [HydroFunction {
            name: fname.as_ptr(),
            min_args: 2,
            max_args: 2,
            flags: 0,
            params: params.as_ptr(),
            call: Some(test_call),
        }];
        let mut descriptor = base_descriptor(&name, &provider, &platform);
        descriptor.function_count = 1;
        descriptor.functions = functions.as_ptr();
        let validated =
            validate_descriptor(Some("demo._fast"), &descriptor).expect("valid descriptor");
        let function = &validated.functions[0];
        assert_eq!(function.name, "add");
        assert_eq!(function.params, vec!["a", "b"]);
        assert_eq!(function.min_args as usize, 2);
    }
}
