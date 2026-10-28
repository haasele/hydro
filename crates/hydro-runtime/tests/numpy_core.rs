//! Regression fixtures for `numpy._core` import resolution.
//!
//! NumPy imports its `_core` package from a module-level `if`/`else` block and
//! rebinds the whole public surface there. The loader must link that package,
//! and — if any binding is still deferred — the runtime registry must serve the
//! already-initialized package instead of silently reporting it missing.
//!
//! These tests drive the public ABI exactly like generated code: module
//! globals through `hydro_global_set`, deferred import bindings through
//! `hydro_deferred_import`, and attribute reads through `hydro_getattr`.

use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_runtime::abi::*;

const MODULE_NOT_FOUND_ERROR: i64 = 31;

const OP_EQ: u32 = 7;
const OP_IS: u32 = 15;

/// The shared heap and the thread registry are process-global.
static SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn setup() -> MutexGuard<'static, ()> {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    hydro_runtime_init();
    guard
}

fn int(n: i64) -> i64 {
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn global_set(module: &str, name: &str, value: i64) {
    hydro_global_set(
        module.as_ptr(),
        module.len(),
        name.as_ptr(),
        name.len(),
        value,
    );
    assert_no_error();
}

fn deferred(module: &str, member: Option<&str>) -> i64 {
    let (ptr, len) = match member {
        Some(member) => (member.as_ptr(), member.len()),
        None => (std::ptr::null(), 0),
    };
    hydro_deferred_import(module.as_ptr(), module.len(), ptr, len, std::ptr::null(), 0)
}

fn getattr(obj: i64, name: &str) -> i64 {
    hydro_getattr(obj, name.as_ptr(), name.len())
}

fn module_value(module: &str) -> i64 {
    hydro_module_value(module.as_ptr(), module.len())
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, actual, text(expected)),
        hydro_true(),
        "wrong string value"
    );
}

/// Compares the rendered pending exception against `expected` and clears it.
///
/// `assert_str` cannot be used here: it asserts that no exception is pending,
/// which is exactly the state under test.
fn assert_pending_message(expected: &str) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    let message = hydro_to_str(hydro_exc_value());
    assert_eq!(
        hydro_binop(OP_EQ, message, text(expected)),
        hydro_true(),
        "wrong exception message"
    );
    hydro_exc_clear();
}

#[test]
fn preloaded_core_package_resolves_instead_of_raising_module_not_found() {
    let _guard = setup();
    // A compiled `numpy._core` initializer mirrors its public names into the
    // module global table; `sys.modules` then exposes the package under its
    // dotted name.
    global_set("numpy", "__version__", text("2.5.0"));
    global_set("numpy._core", "__name__", text("numpy._core"));
    global_set("numpy._core", "ndarray", int(11));

    // `import numpy._core` / `from . import _core` as a deferred binding.
    let core = deferred("numpy._core", None);
    assert_no_error();
    assert_str(getattr(core, "__name__"), "numpy._core");
    assert_eq!(
        hydro_to_int(getattr(core, "ndarray")),
        hydro_to_int(int(11))
    );

    // `from numpy import _core` walks the package then the submodule.
    let nested = deferred("numpy", Some("_core"));
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, nested, core),
        hydro_true(),
        "both spellings must resolve the same registered module"
    );

    // The registry is the `sys.modules` mapping, so the package is the exact
    // object an importer sees.
    let sys = module_value("sys");
    let modules = getattr(sys, "modules");
    let entry = hydro_subscript(modules, text("numpy._core"));
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, entry, core),
        hydro_true(),
        "sys.modules['numpy._core'] must be the module value"
    );
}

#[test]
fn unpreloaded_core_reports_the_exact_dotted_name() {
    let _guard = setup();
    // NumPy's `_core` handler keys off the reported name, so a genuinely
    // missing package must raise ModuleNotFoundError naming the dotted module
    // rather than a truncated parent.
    global_set("numpy", "__version__", text("2.5.0"));

    let _ = deferred("numpy._core", None);
    assert_eq!(hydro_exc_pending(), 1, "missing package must raise");
    assert_eq!(hydro_exc_tag(), MODULE_NOT_FOUND_ERROR);
    assert_pending_message("No module named 'numpy._core'");

    // The `from numpy import _core` spelling reports the same dotted name.
    let _ = deferred("numpy", Some("_core"));
    assert_eq!(hydro_exc_pending(), 1, "missing submodule must raise");
    assert_eq!(hydro_exc_tag(), MODULE_NOT_FOUND_ERROR);
    assert_pending_message("No module named 'numpy._core'");
}

#[test]
fn registering_core_after_numpy_keeps_both_entries_distinct() {
    let _guard = setup();
    // Registering a package and a submodule must not collapse them into one
    // registry entry, and `numpy._core` must not shadow `numpy`.
    global_set("numpy", "__name__", text("numpy"));
    global_set("numpy", "__version__", text("2.5.0"));
    global_set("numpy._core", "__name__", text("numpy._core"));

    let package = deferred("numpy", None);
    assert_no_error();
    assert_str(getattr(package, "__name__"), "numpy");

    let core = deferred("numpy._core", None);
    assert_no_error();
    assert_str(getattr(core, "__name__"), "numpy._core");
    assert_eq!(
        hydro_binop(OP_IS, package, core),
        hydro_false(),
        "package and submodule are distinct module values"
    );
}
