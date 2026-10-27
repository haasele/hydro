//! Integration tests for the second stdlib shim wave: `warnings`,
//! `_warnings`, `marshal`, `_tokenize`, `_collections_abc`,
//! `collections.abc`, `gc`, `atexit` and `site`.
//!
//! The tests drive the public runtime ABI exactly like generated code and pin
//! both the implemented behavior and the explicitly unsupported names.

use hydro_runtime::abi::*;

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const ATTRIBUTE_ERROR: i64 = 9;
const NOT_IMPLEMENTED_ERROR: i64 = 32;
const USER_WARNING: i64 = 43;

const OP_EQ: u32 = 7;
const OP_GE: u32 = 12;
const OP_IS: u32 = 15;

fn int(n: i64) -> i64 {
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn module(module: &str, function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn module_get(module: &str, name: &str) -> i64 {
    hydro_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
}

fn item(container: i64, index: i64) -> i64 {
    hydro_subscript(container, int(index))
}

fn class(name: &str, base: i64) -> i64 {
    hydro_class_new(
        name.as_ptr(),
        name.len(),
        base,
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    )
}

/// Creates a class with the given normal methods (`kind` 0).
fn class_with_methods(name: &str, base: i64, methods: &[(&str, i64)]) -> i64 {
    let names: Vec<i64> = methods.iter().map(|(name, _)| text(name)).collect();
    let funcs: Vec<i64> = methods.iter().map(|(_, function)| *function).collect();
    let kinds: Vec<i64> = methods.iter().map(|_| int(0)).collect();
    let (names_ptr, funcs_ptr, kinds_ptr) = if methods.is_empty() {
        (std::ptr::null(), std::ptr::null(), std::ptr::null())
    } else {
        (names.as_ptr(), funcs.as_ptr(), kinds.as_ptr())
    };
    hydro_class_new(
        name.as_ptr(),
        name.len(),
        base,
        methods.len(),
        names_ptr,
        funcs_ptr,
        kinds_ptr,
        0,
        std::ptr::null(),
        std::ptr::null(),
    )
}

fn empty_dict() -> i64 {
    hydro_dict_new(0, std::ptr::null(), std::ptr::null())
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_error_tag(expected: i64) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), expected, "wrong exception tag");
    hydro_exc_clear();
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_binop(OP_EQ, actual, int(expected)), hydro_true());
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert_eq!(hydro_binop(OP_EQ, actual, text(expected)), hydro_true());
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_true());
}

fn assert_false(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_false());
}

fn assert_same(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_binop(OP_IS, actual, expected), hydro_true());
}

fn assert_none(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_none());
}

fn assert_non_negative(actual: i64) {
    assert_no_error();
    assert_eq!(hydro_binop(OP_GE, actual, int(0)), hydro_true());
}

// -------------------------------------------------------------------------
// Module surfaces
// -------------------------------------------------------------------------

#[test]
fn shim_modules_are_importable() {
    let _guard = test_guard();
    hydro_runtime_init();

    for (module, member) in [
        ("warnings", "filters"),
        ("_warnings", "filters"),
        ("marshal", "version"),
        ("_tokenize", "TokenizerIter"),
        ("_collections_abc", "Mapping"),
        ("collections.abc", "Mapping"),
        ("gc", "collect"),
        ("atexit", "register"),
        ("site", "ENABLE_USER_SITE"),
    ] {
        let value = module_get(module, member);
        assert_no_error();
        assert_ne!(value, hydro_none(), "{module}.{member} must exist");
    }

    assert_str(module_get("collections.abc", "__name__"), "collections.abc");
    assert_str(module_get("collections", "__name__"), "collections");
    assert_str(
        module_get("_collections_abc", "__name__"),
        "_collections_abc",
    );
}

// -------------------------------------------------------------------------
// warnings / _warnings
// -------------------------------------------------------------------------

#[test]
fn warnings_filters_actions_and_formatting() {
    let _guard = test_guard();
    hydro_runtime_init();

    // The CPython 3.14 default filter set: 5 entries.
    assert_int(hydro_len(module_get("warnings", "filters")), 5);

    assert_none(module("warnings", "resetwarnings", &[]));
    assert_int(hydro_len(module_get("warnings", "filters")), 0);

    // Both spellings share one filter stack.
    assert_none(module("warnings", "simplefilter", &[text("ignore")]));
    assert_int(hydro_len(module_get("_warnings", "filters")), 1);
    assert_none(module("warnings", "warn", &[text("hidden")]));

    let user_warning = module_get("builtins", "UserWarning");
    assert_str(
        module(
            "warnings",
            "formatwarning",
            &[text("msg"), user_warning, text("f.py"), int(7)],
        ),
        "f.py:7: UserWarning: msg\n",
    );

    // `error` turns the next user warning into the category exception.
    assert_none(module("warnings", "simplefilter", &[text("error")]));
    module("warnings", "warn", &[text("boom")]);
    assert_error_tag(USER_WARNING);

    // Invalid action / category / attribute are tagged errors.
    module("warnings", "simplefilter", &[text("nope")]);
    assert_error_tag(VALUE_ERROR);
    module(
        "warnings",
        "simplefilter",
        &[text("ignore"), text("category")],
    );
    assert_error_tag(TYPE_ERROR);
    module("warnings", "warn", &[int(1)]);
    assert_error_tag(TYPE_ERROR);
    module_get("warnings", "missing");
    assert_error_tag(ATTRIBUTE_ERROR);
    module_get("_warnings", "missing");
    assert_error_tag(ATTRIBUTE_ERROR);
}

#[test]
fn warnings_registry_records_and_deduplicates() {
    let _guard = test_guard();
    hydro_runtime_init();

    assert_none(module("warnings", "resetwarnings", &[]));
    assert_none(module("warnings", "simplefilter", &[text("default")]));
    let user_warning = module_get("builtins", "UserWarning");
    let registry = empty_dict();

    let mut args = vec![
        text("registered"),
        user_warning,
        text("mod.py"),
        int(3),
        text("mod"),
        registry,
    ];
    assert_none(module("warnings", "warn_explicit", &args));
    // The registry holds its version marker plus the warning key.
    assert_int(hydro_len(registry), 2);

    // The same location/warning is suppressed by the registry.
    assert_none(module("warnings", "warn_explicit", &args));
    assert_int(hydro_len(registry), 2);

    // A caller-supplied registry must be a dict.
    args[5] = int(1);
    module("warnings", "warn_explicit", &args);
    assert_error_tag(TYPE_ERROR);
}

// -------------------------------------------------------------------------
// marshal
// -------------------------------------------------------------------------

#[test]
fn marshal_reports_the_missing_format() {
    let _guard = test_guard();
    hydro_runtime_init();

    assert_int(module_get("marshal", "version"), 5);

    module("marshal", "dump", &[text("value"), text("file")]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
    module("marshal", "dumps", &[text("value")]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
    module("marshal", "load", &[text("file")]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
    module("marshal", "loads", &[text("bytes")]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
    module_get("marshal", "loads_extra");
    assert_error_tag(ATTRIBUTE_ERROR);
}

// -------------------------------------------------------------------------
// _tokenize
// -------------------------------------------------------------------------

#[test]
fn tokenize_never_fabricates_tokens() {
    let _guard = test_guard();
    hydro_runtime_init();

    let constructor = module_get("_tokenize", "TokenizerIter");
    assert_no_error();
    assert_ne!(constructor, hydro_none());
    module("_tokenize", "TokenizerIter", &[text("x = 1\n")]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
    module_get("_tokenize", "TokenInfo");
    assert_error_tag(ATTRIBUTE_ERROR);
}

// -------------------------------------------------------------------------
// collections.abc
// -------------------------------------------------------------------------

#[test]
fn collections_abc_classes_and_inheritance_checks() {
    let _guard = test_guard();
    hydro_runtime_init();

    let mapping = module_get("collections.abc", "Mapping");
    assert_same(mapping, module_get("_collections_abc", "Mapping"));
    for name in [
        "Buffer",
        "Iterable",
        "Iterator",
        "Sized",
        "Container",
        "Collection",
        "Sequence",
        "MutableSequence",
        "Mapping",
        "MutableMapping",
        "Set",
        "MutableSet",
        "Hashable",
        "Callable",
        "Generator",
        "Coroutine",
        "Awaitable",
        "AsyncIterable",
        "AsyncIterator",
    ] {
        let class = module_get("collections.abc", name);
        assert_no_error();
        assert_ne!(class, hydro_none(), "collections.abc.{name} must exist");
    }

    let mapping = module_get("collections.abc", "Mapping");
    let sequence = module_get("collections.abc", "Sequence");

    // `collections.abc` and `collections.abc.abc` both resolve to the nested
    // module value; its attributes come from the class table.
    let from_package = module_get("collections", "abc");
    assert_same(hydro_getattr(from_package, b"Mapping".as_ptr(), 7), mapping);
    let nested_self = module_get("collections.abc", "abc");
    assert_same(hydro_getattr(nested_self, b"Mapping".as_ptr(), 7), mapping);
    assert_str(
        hydro_getattr(nested_self, b"__name__".as_ptr(), 8),
        "collections.abc",
    );

    // Explicit inheritance participates in the ordinary class chain.
    let my_map = class("MyMap", mapping);
    let instance = hydro_instance_new(my_map);
    assert_true(module("builtins", "isinstance", &[instance, mapping]));
    assert_true(module("builtins", "issubclass", &[my_map, mapping]));
    assert_false(module("builtins", "issubclass", &[my_map, sequence]));

    // Documented limit: virtual registration does not exist, so a plain dict
    // is only a Mapping when it subclasses the ABC.
    let dict = empty_dict();
    assert_false(module("builtins", "isinstance", &[dict, mapping]));
    assert_false(module(
        "builtins",
        "isinstance",
        &[hydro_list_new(0, std::ptr::null()), mapping],
    ));

    // `register` exists (as in CPython) but reports the unsupported semantics.
    let args = [dict];
    hydro_method(mapping, b"register".as_ptr(), 8, 1, args.as_ptr());
    assert_error_tag(NOT_IMPLEMENTED_ERROR);

    module_get("collections.abc", "Missing");
    assert_error_tag(ATTRIBUTE_ERROR);
}

#[test]
fn buffer_is_a_real_abc_with_a_structural_hook() {
    let _guard = test_guard();
    hydro_runtime_init();

    let buffer = module_get("collections.abc", "Buffer");
    assert_no_error();
    assert_same(buffer, module_get("_collections_abc", "Buffer"));
    assert_str(hydro_getattr(buffer, b"__name__".as_ptr(), 8), "Buffer");

    // Native exporters satisfy the structural hook, unrelated values do not.
    let bytes = hydro_bytes_new(b"abc".as_ptr(), 3);
    let bytearray = hydro_bytearray_new(b"abc".as_ptr(), 3);
    let view = hydro_memoryview_new(bytes);
    for exporter in [bytes, bytearray, view] {
        assert_true(module("builtins", "isinstance", &[exporter, buffer]));
    }
    for other in [
        int(1),
        text("x"),
        hydro_list_new(0, std::ptr::null()),
        empty_dict(),
    ] {
        assert_false(module("builtins", "isinstance", &[other, buffer]));
    }
    // Native exporter type spellings are class-like targets for Buffer.
    for name in ["bytes", "bytearray", "memoryview"] {
        let native = hydro_builtin_function(name.as_ptr(), name.len());
        assert_true(module("builtins", "issubclass", &[native, buffer]));
    }

    // Explicit inheritance uses the ordinary class chain.
    let subclass = class("MyBuffer", buffer);
    let instance = hydro_instance_new(subclass);
    assert_true(module("builtins", "isinstance", &[instance, buffer]));
    assert_true(module("builtins", "issubclass", &[subclass, buffer]));

    // Structural `__subclasshook__` behaviour: a class declaring `__buffer__`
    // matches without inheriting from Buffer, and so do its instances.
    let method = module_get("gc", "collect");
    assert_no_error();
    let duck = class_with_methods("Duck", hydro_none(), &[("__buffer__", method)]);
    assert_true(module("builtins", "issubclass", &[duck, buffer]));
    assert_true(module(
        "builtins",
        "isinstance",
        &[hydro_instance_new(duck), buffer],
    ));
    let plain = class("Plain", hydro_none());
    assert_false(module("builtins", "issubclass", &[plain, buffer]));
    assert_false(module(
        "builtins",
        "isinstance",
        &[hydro_instance_new(plain), buffer],
    ));

    // The classmethod itself reports the same answer.
    let args = [duck];
    assert_true(hydro_method(
        buffer,
        b"__subclasshook__".as_ptr(),
        16,
        1,
        args.as_ptr(),
    ));
    let args = [plain];
    assert_false(hydro_method(
        buffer,
        b"__subclasshook__".as_ptr(),
        16,
        1,
        args.as_ptr(),
    ));

    // `Buffer` is an `ABCMeta` class: `register` records a real edge, and
    // `__abstractmethods__` reports the abstract `__buffer__` hook.
    let registered = class("Registered", hydro_none());
    let args = [registered];
    let result = hydro_method(buffer, b"register".as_ptr(), 8, 1, args.as_ptr());
    assert_same(result, registered);
    assert_true(module(
        "builtins",
        "isinstance",
        &[hydro_instance_new(registered), buffer],
    ));
    assert_true(module("builtins", "issubclass", &[registered, buffer]));
    let abstract_methods = hydro_getattr(buffer, b"__abstractmethods__".as_ptr(), 19);
    assert_no_error();
    assert_int(hydro_len(abstract_methods), 1);

    // The abstract body raises instead of fabricating a buffer.
    let args = [view, int(0)];
    hydro_method(buffer, b"__buffer__".as_ptr(), 10, 2, args.as_ptr());
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
}

// -------------------------------------------------------------------------
// gc
// -------------------------------------------------------------------------

#[test]
fn gc_reports_the_real_collector() {
    let _guard = test_guard();
    hydro_runtime_init();

    assert_true(module("gc", "isenabled", &[]));
    assert_none(module("gc", "enable", &[]));
    assert_non_negative(module("gc", "collect", &[]));

    let stats = module("gc", "get_stats", &[]);
    assert_int(hydro_len(stats), 1);
    let entry = item(stats, 0);
    let collections = hydro_subscript(entry, text("collections"));
    assert_non_negative(collections);
    let collected = hydro_subscript(entry, text("collected"));
    assert_non_negative(collected);
    assert_int(hydro_subscript(entry, text("uncollectable")), 0);

    let count = module("gc", "get_count", &[]);
    assert_int(hydro_len(count), 3);
    let threshold = module("gc", "get_threshold", &[]);
    assert_int(hydro_len(threshold), 3);
    assert_non_negative(item(threshold, 0));

    module("gc", "collect", &[int(3)]);
    assert_error_tag(VALUE_ERROR);
    module("gc", "collect", &[text("x")]);
    assert_error_tag(TYPE_ERROR);

    for function in [
        "disable",
        "set_threshold",
        "freeze",
        "unfreeze",
        "get_objects",
    ] {
        if function == "set_threshold" {
            module("gc", function, &[int(1000)]);
        } else {
            module("gc", function, &[]);
        }
        assert_error_tag(NOT_IMPLEMENTED_ERROR);
    }
    module_get("gc", "garbage");
    assert_error_tag(ATTRIBUTE_ERROR);
}

// -------------------------------------------------------------------------
// atexit
// -------------------------------------------------------------------------

#[test]
fn atexit_runs_lifo_and_isolates_errors() {
    let _guard = test_guard();
    hydro_runtime_init();

    assert_none(module("warnings", "resetwarnings", &[]));
    let simplefilter = module_get("warnings", "simplefilter");
    assert_same(
        module("atexit", "register", &[simplefilter, text("error")]),
        simplefilter,
    );
    module("atexit", "register", &[simplefilter, text("ignore")]);
    assert_no_error();
    assert_none(module("atexit", "_run_exitfuncs", &[]));

    // LIFO: `ignore` was registered last, `error` first.
    let filters = module_get("warnings", "filters");
    assert_int(hydro_len(filters), 2);
    assert_str(item(item(filters, 0), 0), "error");
    assert_str(item(item(filters, 1), 0), "ignore");

    // A failing callback is reported and cleared; later callbacks still run.
    assert_none(module("warnings", "resetwarnings", &[]));
    let get_objects = module_get("gc", "get_objects");
    module("atexit", "register", &[simplefilter, text("error")]);
    module("atexit", "register", &[get_objects]);
    assert_no_error();
    assert_none(module("atexit", "_run_exitfuncs", &[]));
    assert_no_error();
    let filters = module_get("warnings", "filters");
    assert_int(hydro_len(filters), 1);
    assert_str(item(item(filters, 0), 0), "error");

    // unregister removes every registration of the callback.
    assert_none(module("warnings", "resetwarnings", &[]));
    module("atexit", "register", &[simplefilter, text("ignore")]);
    assert_none(module("atexit", "unregister", &[simplefilter]));
    assert_none(module("atexit", "_run_exitfuncs", &[]));
    assert_int(hydro_len(module_get("warnings", "filters")), 0);

    // _clear drops registrations without calling them.
    module("atexit", "register", &[simplefilter, text("ignore")]);
    assert_none(module("atexit", "_clear", &[]));
    assert_none(module("atexit", "_run_exitfuncs", &[]));
    assert_int(hydro_len(module_get("warnings", "filters")), 0);

    module("atexit", "register", &[int(1)]);
    assert_error_tag(TYPE_ERROR);
    module_get("atexit", "atexit");
    assert_error_tag(ATTRIBUTE_ERROR);
}

// -------------------------------------------------------------------------
// site
// -------------------------------------------------------------------------

#[test]
fn site_reports_truthful_values() {
    let _guard = test_guard();
    hydro_runtime_init();

    assert_false(module_get("site", "ENABLE_USER_SITE"));
    assert_none(module_get("site", "USER_SITE"));
    assert_none(module_get("site", "USER_BASE"));

    let prefixes = module_get("site", "PREFIXES");
    assert_int(hydro_len(prefixes), 2);
    assert_true(hydro_binop(
        OP_EQ,
        item(prefixes, 0),
        module_get("sys", "prefix"),
    ));
    assert_true(hydro_binop(
        OP_EQ,
        item(prefixes, 1),
        module_get("sys", "exec_prefix"),
    ));

    assert_none(module("site", "main", &[]));
    module("site", "addsitedir", &[text("/tmp")]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
    module("site", "addsitedir", &[int(1)]);
    assert_error_tag(TYPE_ERROR);
    module_get("site", "USERSITE");
    assert_error_tag(ATTRIBUTE_ERROR);
}
