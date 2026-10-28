//! End-to-end tests for opaque extension objects.
//!
//! A small stable-ABI C fixture (`tests/fixtures/opaque_ext.c`) exports a
//! custom type, a pre-built singleton and its class. The tests compile the
//! fixture once, load it through the runtime's extension bridge and verify that
//! handles keep their identity, attributes, methods, constructor, comparison,
//! repr and truthiness, and that they can be passed back to the extension.
//!
//! The fixture is compiled at test time because it must be a shared object:
//! the host provides the CPython C API symbols when it is `dlopen`ed. When no C
//! compiler or Python headers are available the test reports a skip instead of
//! failing an unrelated environment.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_runtime::abi::*;

const OP_EQ: u32 = 7;
const OP_IS: u32 = 15;
const ATTRIBUTE_ERROR: i64 = 9;

static SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn setup() -> MutexGuard<'static, ()> {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    hydro_runtime_init();
    guard
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn int(value: i64) -> i64 {
    hydro_int(value)
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
        .join("opaque_ext.c");
    let directory = std::env::temp_dir().join(format!(
        "hydro-opaque-extension-{}-{}",
        std::process::id(),
        source.metadata().ok()?.len()
    ));
    std::fs::create_dir_all(&directory).ok()?;
    let output = directory.join("opaque_ext.abi3.so");

    let include_output = Command::new("python3-config")
        .arg("--includes")
        .output()
        .ok()?;
    if !include_output.status.success() {
        return None;
    }
    let includes = String::from_utf8_lossy(&include_output.stdout);
    let mut command = Command::new("cc");
    command.args(["-shared", "-fPIC", "-O2", "-Wall", "-Wextra"]);
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

fn load(path: &Path, member: &str) -> i64 {
    let path = path.to_str().expect("fixture path is UTF-8");
    let value = hydro_extension_get(path.as_ptr(), path.len(), member.as_ptr(), member.len());
    assert_no_error();
    value
}

fn set_attr(object: i64, name: &str, value: i64) {
    let result = hydro_setattr(object, name.as_ptr(), name.len(), value);
    assert_eq!(result, hydro_none(), "attribute assignment failed");
    assert_no_error();
}

fn get_attr(object: i64, name: &str) -> i64 {
    hydro_getattr(object, name.as_ptr(), name.len())
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

#[test]
fn opaque_handles_keep_identity_attributes_and_calls() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };

    // Importing the type and twice importing the singleton produces distinct
    // Hydro values for the same extension pointers.
    let thing_type = load(path, "Thing");
    let singleton = load(path, "SINGLETON");
    let singleton_again = load(path, "SINGLETON");

    assert_eq!(
        hydro_binop(OP_IS, singleton, singleton_again),
        hydro_true(),
        "the same extension pointer must compare identical"
    );
    assert_eq!(
        hydro_binop(OP_EQ, singleton, singleton_again),
        hydro_true(),
        "rich comparison of equal-valued instances"
    );
    assert_eq!(hydro_truthy(singleton), 1);

    // Type objects expose their name/module and remain type objects.
    assert_str(hydro_getattr(thing_type, b"__name__".as_ptr(), 8), "Thing");
    assert_str(
        hydro_getattr(thing_type, b"__module__".as_ptr(), 10),
        "opaque_ext",
    );

    // Data attribute and method dispatch both go through the extension.
    assert_eq!(
        hydro_getattr(singleton, b"value".as_ptr(), 5),
        int(11),
        "getset attribute"
    );
    let bumped = hydro_method(singleton, b"bump".as_ptr(), 4, 1, [int(4)].as_ptr());
    assert_no_error();
    assert_eq!(bumped, int(15));
    assert_eq!(
        hydro_getattr(singleton, b"value".as_ptr(), 5),
        int(15),
        "the method mutated the instance"
    );

    // Type objects construct through `call_type_object`.
    let arg = [int(7)];
    let first = hydro_call(thing_type, 1, arg.as_ptr());
    let second = hydro_call(thing_type, 1, arg.as_ptr());
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, first, second),
        hydro_false(),
        "construction allocates distinct instances"
    );
    assert_eq!(
        hydro_binop(OP_EQ, first, second),
        hydro_true(),
        "tp_richcompare compares the stored values"
    );
    assert_eq!(
        hydro_getattr(first, b"value".as_ptr(), 5),
        int(7),
        "constructed value"
    );

    // Passing a handle back to the extension returns the same pointer.
    let identity = call_fixture(path, "identity", &[first]);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, identity, first),
        hydro_true(),
        "identity(handle) must return the same handle"
    );
    assert_eq!(
        hydro_binop(OP_EQ, identity, first),
        hydro_true(),
        "the round-tripped handle has the same value"
    );
    let same = call_fixture(path, "same", &[identity, first]);
    assert_eq!(
        same,
        hydro_true(),
        "the extension sees the pointer identity"
    );
    let other = call_fixture(path, "make", &[int(7)]);
    assert_eq!(call_fixture(path, "same", &[other, first]), hydro_false());

    // `isinstance` uses the extension's type hierarchy.
    let isinstance = hydro_builtin_function(b"isinstance".as_ptr(), 10);
    assert_no_error();
    let verdict = hydro_call(isinstance, 2, [first, thing_type].as_ptr());
    assert_no_error();
    assert_eq!(verdict, hydro_true());
    let verdict = hydro_call(isinstance, 2, [int(1), thing_type].as_ptr());
    assert_no_error();
    assert_eq!(verdict, hydro_false());

    // `repr` is the extension's `tp_repr`.
    assert_str(hydro_to_repr(singleton), "<Thing 15>");

    // Missing attributes surface as `AttributeError`, not `TypeError`.
    let missing = hydro_getattr(singleton, b"nope".as_ptr(), 4);
    assert_eq!(missing, hydro_none());
    assert_eq!(
        hydro_exc_tag(),
        ATTRIBUTE_ERROR,
        "a missing extension attribute is an AttributeError"
    );
    hydro_exc_clear();
    assert_no_error();

    // Handles are ordinary heap values: collecting does not disturb them.
    hydro_gc_collect();
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, singleton, load(path, "SINGLETON")),
        hydro_true()
    );
}

#[test]
fn opaque_handles_survive_gc_and_keep_the_extension_value_alive() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };

    let instance = call_fixture(path, "make", &[int(23)]);
    assert_no_error();
    // Force a collection; the handle is rooted by the local value and the
    // extension object must stay alive across it.
    hydro_gc_collect();
    assert_no_error();
    assert_eq!(hydro_getattr(instance, b"value".as_ptr(), 5), int(23));
    assert_eq!(hydro_truthy(instance), 1);
    assert_str(hydro_to_repr(instance), "<Thing 23>");
}

#[test]
fn extension_function_and_type_metadata_is_writable() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };

    // A first-class extension function keeps its writable metadata in the
    // shim slot: `func.__module__ = module` and `__qualname__` are visible on
    // the next read.
    let identity = load(path, "identity");
    set_attr(identity, "__module__", text("numpy"));
    assert_str(get_attr(identity, "__module__"), "numpy");
    set_attr(identity, "__qualname__", text("numpy.identity"));
    assert_str(get_attr(identity, "__qualname__"), "numpy.identity");
    set_attr(identity, "__doc__", hydro_none());
    assert_eq!(get_attr(identity, "__doc__"), hydro_none());

    // Arbitrary attributes on a C function are rejected, like CPython.
    let bad = hydro_setattr(identity, b"missing".as_ptr(), 7, int(1));
    assert_eq!(bad, hydro_none());
    assert_eq!(hydro_exc_tag(), ATTRIBUTE_ERROR);
    hydro_exc_clear();

    // Type objects accept the same four slots; the override wins over the
    // `tp_name`-derived default and survives later reads.
    let thing_type = load(path, "Thing");
    assert_str(get_attr(thing_type, "__module__"), "opaque_ext");
    set_attr(thing_type, "__module__", text("numpy"));
    assert_str(get_attr(thing_type, "__module__"), "numpy");
    set_attr(thing_type, "__qualname__", text("numpy.ndarray"));
    assert_str(get_attr(thing_type, "__qualname__"), "numpy.ndarray");
    set_attr(thing_type, "__module__", text("opaque_ext"));

    // Bound extension methods are opaque handles too.
    let instance = call_fixture(path, "make", &[int(3)]);
    let method = get_attr(instance, "bump");
    set_attr(method, "__module__", text("numpy"));
    assert_str(get_attr(method, "__module__"), "numpy");
}

#[test]
fn extension_type_can_be_a_hydro_class_base() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };

    let base = load(path, "Thing");
    let class = hydro_class_new(
        b"matrix".as_ptr(),
        6,
        base,
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_no_error();

    // Construction defers to the extension base's `tp_new`/vectorcall.
    let instance = hydro_call(class, 1, [int(9)].as_ptr());
    assert_no_error();
    assert_eq!(get_attr(instance, "value"), int(9));
    assert_eq!(
        hydro_method(instance, b"bump".as_ptr(), 4, 1, [int(2)].as_ptr()),
        int(11)
    );

    // The Hydro class participates in `isinstance` through its extension base.
    let isinstance = hydro_builtin_function(b"isinstance".as_ptr(), 10);
    let verdict = hydro_call(isinstance, 2, [instance, class].as_ptr());
    assert_no_error();
    assert_eq!(verdict, hydro_true());

    // A type without a construction protocol is rejected with a clear error.
    let opaque = load(path, "Opaque");
    let rejected = hydro_class_new(
        b"broken".as_ptr(),
        6,
        opaque,
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(rejected, hydro_none());
    assert_eq!(hydro_exc_tag(), 1, "a clear TypeError");
    hydro_exc_clear();
}

#[test]
fn extension_module_dir_getattr_and_hasattr_agree() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };

    let path_text = path.to_str().expect("fixture path is UTF-8");
    let module = hydro_extension_module_value(
        b"opaque_ext".as_ptr(),
        10,
        path_text.as_ptr(),
        path_text.len(),
    );
    assert_no_error();

    let dir = hydro_builtin_function(b"dir".as_ptr(), 3);
    let names = hydro_call(dir, 1, [module].as_ptr());
    assert_no_error();
    let iterator = hydro_iter_new(names);
    let mut found = Vec::new();
    loop {
        let mut item = hydro_none();
        let status = hydro_iter_next(iterator, &mut item);
        assert_no_error();
        if status == 0 {
            break;
        }
        for candidate in ["Thing", "Opaque", "SINGLETON", "identity", "same", "make"] {
            if hydro_binop(OP_EQ, item, text(candidate)) == hydro_true() {
                found.push(candidate);
            }
        }
    }
    for expected in ["Thing", "Opaque", "SINGLETON", "identity", "same", "make"] {
        assert!(
            found.contains(&expected),
            "dir(extension) omitted {expected}: {found:?}"
        );
        assert_eq!(
            hydro_hasattr(module, expected.as_ptr(), expected.len()),
            hydro_true(),
            "dir and hasattr disagree for {expected}"
        );
        // Star-imported names resolve through the same `ShimAttr` surface as
        // explicit imports.
        let value = hydro_extension_get(
            path_text.as_ptr(),
            path_text.len(),
            expected.as_ptr(),
            expected.len(),
        );
        assert_no_error();
        assert_ne!(value, hydro_none());
    }
}

// ---------------------------------------------------------------------------
// Hydro values as real CPython objects
// ---------------------------------------------------------------------------

const TYPE_ERROR: i64 = 1;

fn float(value: f64) -> i64 {
    hydro_float_new(value)
}

/// A Hydro class with one class attribute and one instance carrying `value`.
fn widget_class_and_instance() -> (i64, i64) {
    let attribute_name = text("answer");
    let attribute_value = int(42);
    let class = hydro_class_new(
        b"Widget".as_ptr(),
        6,
        hydro_none(),
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        1,
        [attribute_name].as_ptr(),
        [attribute_value].as_ptr(),
    );
    assert_no_error();
    let instance = hydro_instance_new(class);
    assert_no_error();
    hydro_setattr(instance, b"value".as_ptr(), 5, int(7));
    assert_no_error();
    (class, instance)
}

#[test]
fn hydro_values_keep_identity_attributes_and_calls() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };
    // A previous test may have left handles in the module's static slots.
    call_fixture(path, "clear_stored", &[]);
    assert_no_error();

    let (class, instance) = widget_class_and_instance();
    let len_function = hydro_builtin_function(b"len".as_ptr(), 3);
    assert_no_error();
    let math = hydro_module_value(b"math".as_ptr(), 4);
    assert_no_error();
    // A container whose elements are themselves Hydro handles.
    let container = hydro_list_new(2, [class, instance].as_ptr());
    assert_no_error();

    for (slot, value) in [
        (0, class),
        (1, instance),
        (2, len_function),
        (3, math),
        (4, container),
    ] {
        call_fixture(path, "store", &[value, int(slot)]);
        assert_no_error();
    }

    // The extension stores the handle itself, so the value it returns is the
    // very same Hydro object.
    let stored_class = call_fixture(path, "stored", &[int(0)]);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, stored_class, class),
        hydro_true(),
        "a stored Hydro class keeps its identity"
    );
    assert_str(
        call_fixture(path, "attr", &[stored_class, text("__name__")]),
        "Widget",
    );

    // Class metadata resolves through Hydro.
    assert_str(
        call_fixture(path, "attr", &[class, text("__name__")]),
        "Widget",
    );
    assert_str(call_fixture(path, "type_name", &[class]), "Widget");
    assert_eq!(
        call_fixture(path, "type_flags", &[class]),
        hydro_true(),
        "PyType_GetFlags reports the Hydro class as a heap type"
    );

    // Instance attributes use Hydro's own lookup (instance dict and class
    // attributes).
    assert_eq!(
        call_fixture(path, "attr", &[instance, text("value")]),
        int(7)
    );
    assert_eq!(
        call_fixture(path, "attr", &[instance, text("answer")]),
        int(42)
    );
    assert_str(
        call_fixture(path, "repr_of", &[instance]),
        "<Widget object>",
    );
    assert_str(call_fixture(path, "str_of", &[instance]), "<Widget object>");
    assert_eq!(call_fixture(path, "truth", &[instance]), hydro_true());

    // `isinstance`/`issubclass` run Hydro's hierarchy.
    assert_eq!(
        call_fixture(path, "isinstance", &[instance, class]),
        hydro_true()
    );
    assert_eq!(
        call_fixture(path, "isinstance", &[int(1), class]),
        hydro_false()
    );
    assert_eq!(
        call_fixture(path, "issubclass", &[class, class]),
        hydro_true()
    );

    // Comparison runs through the Hydro value, not the wrapper pointer.
    assert_eq!(
        call_fixture(path, "equal", &[instance, instance]),
        hydro_true()
    );
    let other = hydro_instance_new(class);
    assert_no_error();
    assert_eq!(
        call_fixture(path, "equal", &[instance, other]),
        hydro_false()
    );

    // Functions call Hydro code; arguments and results round-trip.
    assert_eq!(
        call_fixture(path, "call1", &[len_function, container]),
        int(2)
    );
    assert_eq!(call_fixture(path, "length", &[container]), int(2));
    assert_eq!(call_fixture(path, "list_size", &[container]), int(2));
    let first = call_fixture(path, "list_item", &[container, int(0)]);
    assert_no_error();
    assert_str(
        call_fixture(path, "attr", &[first, text("__name__")]),
        "Widget",
    );
    let second = call_fixture(path, "list_item", &[container, int(1)]);
    assert_no_error();
    assert_eq!(
        call_fixture(path, "isinstance", &[second, class]),
        hydro_true()
    );

    // A module attribute yields a Hydro function whose call runs Hydro.
    let sqrt = call_fixture(path, "attr", &[math, text("sqrt")]);
    assert_no_error();
    let root = call_fixture(path, "call1", &[sqrt, float(16.0)]);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, root, float(4.0)),
        hydro_true(),
        "math.sqrt runs through the Hydro callable"
    );

    // Missing attributes surface as AttributeError from Hydro.
    let missing = call_fixture(path, "attr", &[instance, text("nope")]);
    assert_eq!(missing, hydro_none());
    assert_eq!(hydro_exc_tag(), ATTRIBUTE_ERROR);
    hydro_exc_clear();
    assert_no_error();

    // A value with no handle representation is rejected instead of faked.
    let range = hydro_range_new(int(0), int(3), int(1));
    assert_no_error();
    let rejected = call_fixture(path, "identity", &[range]);
    assert_eq!(rejected, hydro_none());
    assert_eq!(
        hydro_exc_tag(),
        TYPE_ERROR,
        "unsupported Hydro values still raise TypeError"
    );
    hydro_exc_clear();
    assert_no_error();

    call_fixture(path, "clear_stored", &[]);
    assert_no_error();
}

#[test]
fn hydro_handles_survive_gc_stress_and_release_their_roots() {
    let _guard = setup();
    let Some(path) = fixture_path() else {
        eprintln!("skipping: no C compiler or Python headers for the fixture");
        return;
    };
    call_fixture(path, "clear_stored", &[]);
    assert_no_error();

    let (class, instance) = widget_class_and_instance();
    let len_function = hydro_builtin_function(b"len".as_ptr(), 3);
    assert_no_error();

    for round in 0..16 {
        call_fixture(path, "store", &[class, int(0)]);
        call_fixture(path, "store", &[instance, int(1)]);
        call_fixture(path, "store", &[len_function, int(2)]);
        assert_no_error();

        // Collect repeatedly while the extension owns references. The
        // persistent roots must keep every wrapped Hydro value reachable.
        for _ in 0..8 {
            hydro_gc_collect();
            assert_no_error();
        }

        let stored_class = call_fixture(path, "stored", &[int(0)]);
        let stored_instance = call_fixture(path, "stored", &[int(1)]);
        let stored_function = call_fixture(path, "stored", &[int(2)]);
        assert_no_error();
        assert_str(
            call_fixture(path, "attr", &[stored_class, text("__name__")]),
            "Widget",
        );
        assert_eq!(
            call_fixture(path, "attr", &[stored_instance, text("value")]),
            int(7),
        );
        let arguments = [int(round), int(round + 1)];
        let list = hydro_list_new(2, arguments.as_ptr());
        assert_no_error();
        assert_eq!(
            call_fixture(path, "call1", &[stored_function, list]),
            int(2),
        );
        assert_eq!(
            call_fixture(path, "isinstance", &[stored_instance, stored_class]),
            hydro_true()
        );
    }

    // Dropping every C reference releases the persistent roots; the Hydro
    // locals stay valid and a further collection must not disturb them.
    call_fixture(path, "clear_stored", &[]);
    assert_no_error();
    for _ in 0..8 {
        hydro_gc_collect();
        assert_no_error();
    }
    assert_str(hydro_getattr(class, b"__name__".as_ptr(), 8), "Widget");
    assert_eq!(hydro_getattr(instance, b"value".as_ptr(), 5), int(7));
    assert_eq!(hydro_getattr(instance, b"answer".as_ptr(), 6), int(42));
    assert_eq!(call_fixture(path, "stored", &[int(0)]), hydro_none());
    assert_no_error();
}
