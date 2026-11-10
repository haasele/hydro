//! `_imp`: loader queries used by import machinery on top of the runtime
//! registry.
//!
//! Only the queries this runtime can answer truthfully are implemented:
//! built-in module detection (the modules listed by
//! [`hydro_ir::stdlib`]), frozen-module predicates (always false, because no
//! frozen modules exist) and the platform extension suffixes. Everything that
//! would require bytecode, frozen module data or a native extension method
//! table raises an explicit `ImportError`/`NotImplementedError`; none of these
//! paths returns a fabricated success value.
//!
//! `is_builtin` reports every module declared by the standard-library spec
//! table, which is the runtime's equivalent of `sys.builtin_module_names` in
//! the sense that these modules are provided by the runtime itself.

use hydro_ir::{stdlib, PYTHON_ABI_TAG};
use std::sync::atomic::{AtomicI64, Ordering};

use crate::exc::{self, IMPORT_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR};
use crate::value::{self, FALSE, NONE};

/// Materialises an `_imp` module constant.
///
/// `pyc_magic_number_token` is the single definition shared with
/// `importlib.util.MAGIC_NUMBER`, so a `.pyc` header written from either name
/// agrees. The frozen-module list is the one this runtime actually has (no
/// frozen modules), and `check_hash_based_pycs` reports the CPython default
/// mode.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "pyc_magic_number_token" => {
            crate::ops::int_result(crate::modules::importlib_util::PYC_MAGIC_NUMBER_TOKEN as i64)
        }
        "check_hash_based_pycs" => value::str_alloc("default".to_string()),
        "_frozen_module_names" => value::list_new(Vec::new()),
        _ => exc::raise_with_message(
            crate::exc::ATTRIBUTE_ERROR,
            &format!("module '_imp' has no attribute '{name}'"),
        ),
    }
}

static FROZEN_OVERRIDE: AtomicI64 = AtomicI64::new(-1);
static MULTI_INTERP_OVERRIDE: AtomicI64 = AtomicI64::new(-1);

fn override_state(state: &AtomicI64, args: &[i64]) -> i64 {
    let value = args
        .first()
        .and_then(|arg| value::as_integer(*arg))
        .unwrap_or(-1);
    value::encode_int(state.swap(value, Ordering::SeqCst)).unwrap_or(NONE)
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "acquire_lock" | "release_lock" => NONE,
        // The import lock is process-wide and is never held while user code
        // runs, so this thread's answer is always `False`.
        "lock_held" => FALSE,
        "init_frozen" => {
            exc::raise_with_message(IMPORT_ERROR, "frozen modules are not available in Hydro")
        }
        "is_builtin" => match module_name(function, args[0]) {
            Some(name) => value::bool_value(stdlib::module(name).is_some()),
            None => NONE,
        },
        "is_frozen" | "is_frozen_package" => match module_name(function, args[0]) {
            Some(_) => FALSE,
            None => NONE,
        },
        "extension_suffixes" => extension_suffixes(),
        "create_builtin" => exc::raise_with_message(
            IMPORT_ERROR,
            "cannot create a built-in module: Hydro has no method-table ABI for builtins",
        ),
        "exec_builtin" => exc::raise_with_message(
            IMPORT_ERROR,
            "cannot exec a built-in module: Hydro has no module execution ABI",
        ),
        "find_frozen" => {
            exc::raise_with_message(IMPORT_ERROR, "frozen modules are not available in Hydro")
        }
        "get_frozen_object" => {
            exc::raise_with_message(IMPORT_ERROR, "frozen modules are not available in Hydro")
        }
        "create_dynamic" => exc::raise_with_message(
            IMPORT_ERROR,
            "cannot create a dynamic module: Hydro has no dynamic loader spec ABI",
        ),
        "exec_dynamic" => exc::raise_with_message(
            IMPORT_ERROR,
            "cannot exec a dynamic module: Hydro has no dynamic loader ABI",
        ),
        "_fix_co_filename" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "_imp._fix_co_filename() is not supported: Hydro has no code object",
        ),
        "_override_frozen_modules_for_tests" => override_state(&FROZEN_OVERRIDE, args),
        "_override_multi_interp_extensions_check" => override_state(&MULTI_INTERP_OVERRIDE, args),
        "source_hash" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "_imp.source_hash() is not supported: Hydro has no code object",
        ),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module '_imp' has no attribute '{function}'"),
        ),
    }
}

/// Type-checks a module-name argument.
fn module_name(function: &str, arg: i64) -> Option<&'static str> {
    match value::as_str(arg) {
        Some(name) => Some(name),
        None => {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{function}() argument must be str, not {}",
                    value::type_name(arg)
                ),
            );
            None
        }
    }
}

/// Platform extension-module suffixes, in import-preference order.
///
/// The ABI-tagged suffix is derived from [`PYTHON_ABI_TAG`] and the build
/// target, matching the names the extension loader accepts. A fresh list is
/// returned on every call; CPython callers are allowed to mutate it.
fn extension_suffixes() -> i64 {
    #[cfg(target_os = "windows")]
    let suffixes: Vec<String> = vec![".pyd".to_string()];
    #[cfg(target_os = "macos")]
    let suffixes: Vec<String> = {
        let abi = format!(".{PYTHON_ABI_TAG}-darwin.so");
        vec![abi, ".abi3.so".to_string(), ".so".to_string()]
    };
    #[cfg(all(unix, target_os = "linux"))]
    let suffixes: Vec<String> = {
        let libc = if cfg!(target_env = "musl") {
            "linux-musl"
        } else {
            "linux-gnu"
        };
        let arch = std::env::consts::ARCH;
        let abi = format!(".{PYTHON_ABI_TAG}-{arch}-{libc}.so");
        vec![abi, ".abi3.so".to_string(), ".so".to_string()]
    };
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
    let suffixes: Vec<String> = vec![".abi3.so".to_string(), ".so".to_string()];

    let list = value::list_new(Vec::new());
    for suffix in suffixes {
        let item = value::str_alloc(suffix);
        let roots = crate::heap::root_values(&[item]);
        crate::container::list_append(list, item);
        crate::heap::release_roots(roots);
    }
    list
}
