//! End-to-end tests for the CPython-compatible tuple/list prefix.
//!
//! The fixture (`tests/fixtures/tuple_layout.c`) is a real C extension built
//! without `Py_LIMITED_API`, so it reads tuple and list handles through the
//! inline macros (`PyTuple_GET_SIZE`/`PyTuple_GET_ITEM`/`PyTuple_SET_ITEM`,
//! `PyList_GET_SIZE`/`PyList_GET_ITEM`/`PyList_SET_ITEM`). That is the same
//! memory access pyo3-ffi performs for PyO3 extensions, which is why
//! `SchemaValidator(...)` previously reported
//! `takes from 1 to 3 positional arguments but 9223372036854775821 were given`.
//!
//! The probes verify values inside the extension and raise `AssertionError`
//! with the observed numbers on a mismatch, so failures name the exact macro.
//!
//! When no C compiler or Python headers are available the tests report a skip
//! instead of failing an unrelated environment.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_runtime::abi::*;

const OP_EQ: u32 = 7;

static SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn setup() -> MutexGuard<'static, ()> {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    hydro_runtime_init();
    guard
}

fn int(value: i64) -> i64 {
    hydro_int(value)
}

fn list(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
}

fn tuple(items: &[i64]) -> i64 {
    hydro_tuple_new(items.len(), items.as_ptr())
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn fixture_path() -> Option<&'static Path> {
    static PATH: LazyLock<Option<PathBuf>> = LazyLock::new(build_fixture);
    PATH.as_deref()
}

/// Compiles the C fixture into a shared object. `None` means the environment
/// has no compiler/headers; the tests then report a skip.
fn build_fixture() -> Option<PathBuf> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("tuple_layout.c");
    let directory = std::env::temp_dir().join(format!(
        "hydro-tuple-layout-{}-{}",
        std::process::id(),
        source.metadata().ok()?.len()
    ));
    std::fs::create_dir_all(&directory).ok()?;
    let output = directory.join("tuple_layout_probe.so");

    let include_output = Command::new("python3-config")
        .arg("--includes")
        .output()
        .ok()?;
    if !include_output.status.success() {
        return None;
    }
    let includes = String::from_utf8_lossy(&include_output.stdout);
    let mut command = Command::new("cc");
    // No `Py_LIMITED_API`: the inline macros are the point of this fixture.
    // `-DNDEBUG` disables the debug-only `index < allocated` assertion in
    // `PyList_SET_ITEM`; this host does not mirror `PyListObject.allocated`.
    command.args(["-shared", "-fPIC", "-O2", "-DNDEBUG", "-Wall", "-Wextra"]);
    for include in includes.split_whitespace() {
        command.arg(include);
    }
    command.arg(&source).arg("-o").arg(&output);
    let status = command.status().ok()?;
    if !status.success() {
        return None;
    }
    Some(output)
}

fn call_fixture(path: &Path, member: &str, args: &[i64]) -> i64 {
    let path = path.to_str().expect("fixture path is UTF-8");
    hydro_extension_call(
        path.as_ptr(),
        path.len(),
        member.as_ptr(),
        member.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, actual, int(expected)),
        hydro_true(),
        "expected the integer {expected}"
    );
}

/// PyO3 computes the positional-argument count of a `#[new]` wrapper with
/// `PyTuple_GET_SIZE(args)` and then reads the arguments with
/// `PyTuple_GET_ITEM`. Both are inline macros; this is the exact pydantic-core
/// failure path.
#[test]
fn inline_macros_see_tuple_size_and_items() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };

    // The probe checks its own `args` tuple size (the PyO3 failure), the
    // user tuple size and both inline items, then returns True.
    let probe = call_fixture(
        path,
        "tuple_probe2",
        &[tuple(&[int(10), int(20)]), int(10), int(20)],
    );
    assert_no_error();
    assert_eq!(probe, hydro_true());

    // A one-element tuple is the `SchemaValidator({'type': 'int'})` call shape.
    let single = call_fixture(path, "tuple_probe1", &[tuple(&[int(7)]), int(7)]);
    assert_no_error();
    assert_eq!(single, hydro_true());

    // An empty tuple must read as size 0, not a huge refcount-like value.
    let empty = call_fixture(path, "tuple_probe0", &[tuple(&[])]);
    assert_no_error();
    assert_eq!(empty, hydro_true());
}

/// `PyTuple_SET_ITEM` writes the inline array directly; the host must expose
/// those items through `PyTuple_Size`/`PyTuple_GetItem` and when the tuple
/// crosses back to Hydro.
#[test]
fn extension_built_tuple_is_a_real_tuple() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };

    let built = call_fixture(path, "make_tuple", &[int(7), int(8)]);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, built, tuple(&[int(7), int(8)])),
        hydro_true(),
        "extension-built tuple is not a Hydro tuple"
    );
    assert_eq!(hydro_len(built), int(2));
    assert_int(hydro_subscript(built, int(0)), 7);
    assert_int(hydro_subscript(built, int(1)), 8);
}

/// `PyList_GET_SIZE`/`PyList_GET_ITEM` use `ob_size` and `ob_item`; the
/// pointer has to be republished after `PyList_Append` reallocates the `Vec`.
///
/// The bridge copies Hydro containers on the way into an extension, so the
/// extension mutates its own copy: the probe verifies the C-side observation
/// and the original Hydro list deliberately stays unchanged.
#[test]
fn inline_macros_see_list_size_items_and_appends() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };

    let probe = list(&[int(10), int(20)]);
    let summary = call_fixture(path, "list_probe", &[probe, int(30), int(10)]);
    assert_no_error();
    assert_eq!(summary, hydro_true());
    // Documented copy semantics: appending inside the extension does not
    // reach the Hydro value that was passed in.
    assert_eq!(hydro_len(probe), int(2));
    assert_int(hydro_subscript(probe, int(1)), 20);
}

/// `PyList_SET_ITEM` after `PyList_New` fills the host's storage in place.
#[test]
fn extension_built_list_is_a_real_list() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };

    let built = call_fixture(path, "make_list", &[int(7), int(8)]);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, built, list(&[int(7), int(8)])),
        hydro_true(),
        "extension-built list is not a Hydro list"
    );
    assert_eq!(hydro_len(built), int(2));
    assert_int(hydro_subscript(built, int(0)), 7);
    assert_int(hydro_subscript(built, int(1)), 8);
}
