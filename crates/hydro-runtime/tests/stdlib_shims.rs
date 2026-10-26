//! Integration tests for the Phase 2 shim modules: `_imp`, `_opcode`,
//! `_collections`, `_codecs` and `_string`.
//!
//! The tests drive the public runtime ABI exactly like generated code and
//! pin both the implemented behavior and the explicitly unsupported names.

use hydro_runtime::abi::*;

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const KEY_ERROR: i64 = 7;
const ATTRIBUTE_ERROR: i64 = 9;
const LOOKUP_ERROR: i64 = 29;
const IMPORT_ERROR: i64 = 30;
const NOT_IMPLEMENTED_ERROR: i64 = 32;
const UNICODE_DECODE_ERROR: i64 = 35;

const OP_EQ: u32 = 7;
const OP_IS: u32 = 15;

fn int(n: i64) -> i64 {
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn bytes(value: &[u8]) -> i64 {
    hydro_bytes_new(value.as_ptr(), value.len())
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

fn assert_bytes(actual: i64, expected: &[u8]) {
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, actual, bytes(expected)),
        hydro_true(),
        "bytes mismatch"
    );
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

// -------------------------------------------------------------------------
// _imp
// -------------------------------------------------------------------------

#[test]
fn imp_loader_queries_are_truthful() {
    let _guard = test_guard();
    hydro_runtime_init();
    assert_true(module("_imp", "is_builtin", &[text("math")]));
    assert_false(module("_imp", "is_builtin", &[text("not_a_module")]));
    assert_false(module("_imp", "is_frozen", &[text("math")]));
    assert_false(module("_imp", "is_frozen_package", &[text("math")]));

    let suffixes = module("_imp", "extension_suffixes", &[]);
    assert_no_error();
    let suffixes = hydro_list_from(suffixes);
    assert_no_error();
    assert!(hydro_to_int(hydro_len(suffixes)) > 0);

    assert_none(module("_imp", "acquire_lock", &[]));
    assert_none(module("_imp", "release_lock", &[]));

    let _ = module("_imp", "is_builtin", &[int(1)]);
    assert_error_tag(TYPE_ERROR);
}

#[test]
fn imp_unsupported_operations_raise_tagged_errors() {
    let _guard = test_guard();
    hydro_runtime_init();
    // None of these may return a fabricated success value.
    let _ = module("_imp", "create_builtin", &[hydro_none()]);
    assert_error_tag(IMPORT_ERROR);
    let _ = module("_imp", "exec_builtin", &[hydro_none()]);
    assert_error_tag(IMPORT_ERROR);
    let _ = module("_imp", "find_frozen", &[text("x")]);
    assert_error_tag(IMPORT_ERROR);
    let _ = module("_imp", "get_frozen_object", &[text("x")]);
    assert_error_tag(IMPORT_ERROR);
    let _ = module("_imp", "create_dynamic", &[hydro_none()]);
    assert_error_tag(IMPORT_ERROR);
    let _ = module("_imp", "exec_dynamic", &[hydro_none()]);
    assert_error_tag(IMPORT_ERROR);

    let _ = module("_imp", "_fix_co_filename", &[hydro_none(), text("f.py")]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
    let _ = module("_imp", "source_hash", &[text("k"), bytes(b"x")]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
}

// -------------------------------------------------------------------------
// _opcode
// -------------------------------------------------------------------------

#[test]
fn opcode_exposes_only_the_specialization_flags() {
    let _guard = test_guard();
    hydro_runtime_init();
    assert_int(module_get("_opcode", "ENABLE_SPECIALIZATION"), 0);
    assert_int(module_get("_opcode", "ENABLE_SPECIALIZATION_FT"), 0);

    // `stack_effect` exists for CPython 3.14's `opcode.py` import surface but
    // Hydro has no bytecode, so calling it must fail explicitly.
    let stack_effect = module_get("_opcode", "stack_effect");
    assert_no_error();
    assert!(hydro_binop(OP_IS, stack_effect, hydro_none()) == hydro_false());
    let _ = module("_opcode", "stack_effect", &[int(1)]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
}

// -------------------------------------------------------------------------
// _collections
// -------------------------------------------------------------------------

#[test]
fn collections_defaultdict_is_a_real_alias() {
    let _guard = test_guard();
    hydro_runtime_init();
    let accelerated = module_get("_collections", "defaultdict");
    let pure = module_get("collections", "defaultdict");
    assert_same(accelerated, pure);

    // `deque` is not implemented and is not present.
    let _ = module_get("_collections", "deque");
    assert_error_tag(ATTRIBUTE_ERROR);

    // The alias is the working defaultdict, not a stand-in.
    let factory = hydro_builtin_function("list".as_ptr(), "list".len());
    let dd = module("_collections", "defaultdict", &[factory]);
    assert_no_error();
    let value = hydro_subscript(dd, text("missing"));
    assert_no_error();
    assert_int(hydro_len(value), 0);
    let again = hydro_subscript(dd, text("missing"));
    assert_same(value, again);

    // A `None` factory raises KeyError for a missing key.
    let dd = module("_collections", "defaultdict", &[hydro_none()]);
    assert_no_error();
    let _ = hydro_subscript(dd, text("missing"));
    assert_error_tag(KEY_ERROR);
}

// -------------------------------------------------------------------------
// _codecs
// -------------------------------------------------------------------------

#[test]
fn codecs_utf8_encode_and_decode() {
    let _guard = test_guard();
    hydro_runtime_init();
    assert_bytes(module("_codecs", "encode", &[text("é")]), &[0xc3, 0xa9]);

    let encoded = module("_codecs", "utf_8_encode", &[text("é")]);
    assert_no_error();
    assert_bytes(item(encoded, 0), &[0xc3, 0xa9]);
    assert_int(item(encoded, 1), 1);

    let decoded = module("_codecs", "utf_8_decode", &[bytes(&[0xc3, 0xa9])]);
    assert_no_error();
    assert_str(item(decoded, 0), "é");
    assert_int(item(decoded, 1), 2);

    assert_str(module("_codecs", "decode", &[bytes(&[0xc3, 0xa9])]), "é");

    // Incomplete trailing sequences are left unconsumed unless `final` is set.
    let partial = module("_codecs", "utf_8_decode", &[bytes(&[0xc3])]);
    assert_no_error();
    assert_str(item(partial, 0), "");
    assert_int(item(partial, 1), 0);
    let _ = module(
        "_codecs",
        "utf_8_decode",
        &[bytes(&[0xc3]), text("strict"), hydro_true()],
    );
    assert_error_tag(UNICODE_DECODE_ERROR);
}

#[test]
fn codecs_error_handling_and_lookup_errors() {
    let _guard = test_guard();
    hydro_runtime_init();
    // Malformed UTF-8 raises the taxonomy's UnicodeDecodeError.
    let _ = module("_codecs", "decode", &[bytes(&[0xff])]);
    assert_error_tag(UNICODE_DECODE_ERROR);

    assert_str(
        module(
            "_codecs",
            "decode",
            &[bytes(b"a\xff"), text("utf-8"), text("ignore")],
        ),
        "a",
    );
    assert_str(
        module(
            "_codecs",
            "decode",
            &[bytes(b"a\xff"), text("utf-8"), text("replace")],
        ),
        "a\u{fffd}",
    );
    let _ = module(
        "_codecs",
        "decode",
        &[bytes(b"a\xff"), text("utf-8"), text("bogus")],
    );
    assert_error_tag(LOOKUP_ERROR);

    // Non-UTF-8 encoding names are rejected, never silently treated as UTF-8.
    let _ = module("_codecs", "encode", &[text("é"), text("ascii")]);
    assert_error_tag(LOOKUP_ERROR);
    let _ = module("_codecs", "decode", &[bytes(b"a"), text("latin-1")]);
    assert_error_tag(LOOKUP_ERROR);

    // The registry answers the UTF-8 lookup and rejects unknown codecs.
    let info = module("_codecs", "lookup", &[text("utf-8")]);
    assert_no_error();
    assert_int(hydro_len(info), 4);
    let _ = module("_codecs", "lookup", &[text("ascii")]);
    assert_error_tag(LOOKUP_ERROR);

    // Error-handler registration is real: lookup returns the registered
    // callable, and unknown handler names are LookupError.
    let handler = hydro_builtin_function("len".as_ptr(), "len".len());
    assert_none(module(
        "_codecs",
        "register_error",
        &[text("mine"), handler],
    ));
    let looked_up = module("_codecs", "lookup_error", &[text("mine")]);
    assert_same(looked_up, handler);
    let _ = module("_codecs", "lookup_error", &[text("bogus")]);
    assert_error_tag(LOOKUP_ERROR);
    let _ = module("_codecs", "register_error", &[text("x"), int(1)]);
    assert_error_tag(TYPE_ERROR);
    let _ = module("_codecs", "register", &[int(1)]);
    assert_error_tag(TYPE_ERROR);
    assert_none(module("_codecs", "unregister", &[int(1)]));
}

#[test]
fn codecs_escape_codec() {
    let _guard = test_guard();
    hydro_runtime_init();
    let encoded = module("_codecs", "escape_encode", &[bytes(b"a'b\n")]);
    assert_no_error();
    assert_bytes(item(encoded, 0), b"a\\'b\\n");
    assert_int(item(encoded, 1), 4);

    let decoded = module("_codecs", "escape_decode", &[bytes(b"a\\nb")]);
    assert_no_error();
    assert_bytes(item(decoded, 0), b"a\nb");
    assert_int(item(decoded, 1), 4);

    let ignored = module(
        "_codecs",
        "escape_decode",
        &[bytes(b"\\xzz"), text("ignore")],
    );
    assert_no_error();
    assert_bytes(item(ignored, 0), b"zz");

    let _ = module("_codecs", "escape_decode", &[bytes(b"\\x4")]);
    assert_error_tag(VALUE_ERROR);
    let _ = module("_codecs", "escape_decode", &[bytes(b"a\\")]);
    assert_error_tag(VALUE_ERROR);
}

// -------------------------------------------------------------------------
// _string
// -------------------------------------------------------------------------

fn parsed(source: &str) -> i64 {
    let result = module("_string", "formatter_parser", &[text(source)]);
    assert_no_error();
    let result = hydro_list_from(result);
    assert_no_error();
    result
}

#[test]
fn formatter_parser_matches_cpython_examples() {
    let _guard = test_guard();
    hydro_runtime_init();
    // 'abc{0!r:>{width}}def'
    let fields = parsed("abc{0!r:>{width}}def");
    assert_int(hydro_len(fields), 2);
    let first = item(fields, 0);
    assert_str(item(first, 0), "abc");
    assert_str(item(first, 1), "0");
    assert_str(item(first, 2), ">{width}");
    assert_str(item(first, 3), "r");
    let second = item(fields, 1);
    assert_str(item(second, 0), "def");
    assert_same(item(second, 1), hydro_none());

    // Doubled braces are literal text; each escaped brace ends a chunk.
    let literal = parsed("{{}}");
    assert_int(hydro_len(literal), 2);
    assert_str(item(item(literal, 0), 0), "{");
    assert_same(item(item(literal, 0), 1), hydro_none());
    assert_str(item(item(literal, 1), 0), "}");

    // Nested format specs are returned literally.
    let nested = parsed("{a:{b:{c}}}");
    assert_int(hydro_len(nested), 1);
    assert_str(item(item(nested, 0), 1), "a");
    assert_str(item(item(nested, 0), 2), "{b:{c}}");

    // Empty field names and specs.
    let empty = parsed("{}");
    assert_int(hydro_len(empty), 1);
    assert_str(item(item(empty, 0), 1), "");
    assert_str(item(item(empty, 0), 2), "");

    // Malformed inputs are ValueError.
    let _ = module("_string", "formatter_parser", &[text("a}}}b")]);
    assert_error_tag(VALUE_ERROR);
    let _ = module("_string", "formatter_parser", &[text("{")]);
    assert_error_tag(VALUE_ERROR);
    let _ = module("_string", "formatter_parser", &[text("{a!}")]);
    assert_error_tag(VALUE_ERROR);
    let _ = module("_string", "formatter_parser", &[text("{a{b}}")]);
    assert_error_tag(VALUE_ERROR);
    let _ = module("_string", "formatter_parser", &[int(1)]);
    assert_error_tag(TYPE_ERROR);
}

#[test]
fn formatter_field_name_split_matches_cpython() {
    let _guard = test_guard();
    hydro_runtime_init();
    let split = module(
        "_string",
        "formatter_field_name_split",
        &[text("a[0].b[1]")],
    );
    assert_no_error();
    assert_str(item(split, 0), "a");
    let rest = hydro_list_from(item(split, 1));
    assert_no_error();
    assert_int(hydro_len(rest), 3);

    let first = item(rest, 0);
    assert_same(item(first, 0), hydro_false());
    assert_int(item(first, 1), 0);
    let second = item(rest, 1);
    assert_same(item(second, 0), hydro_true());
    assert_str(item(second, 1), "b");
    let third = item(rest, 2);
    assert_same(item(third, 0), hydro_false());
    assert_int(item(third, 1), 1);

    // Plain names have no rest.
    let plain = module("_string", "formatter_field_name_split", &[text("x")]);
    assert_no_error();
    assert_str(item(plain, 0), "x");
    let rest = hydro_list_from(item(plain, 1));
    assert_no_error();
    assert_int(hydro_len(rest), 0);

    // Auto-numbered field names and numeric first components.
    let auto = module("_string", "formatter_field_name_split", &[text("")]);
    assert_no_error();
    assert_str(item(auto, 0), "");
    let number = module("_string", "formatter_field_name_split", &[text("0")]);
    assert_no_error();
    assert_int(item(number, 0), 0);

    // Malformed field names are ValueError.
    let _ = module("_string", "formatter_field_name_split", &[text("x[")]);
    assert_error_tag(VALUE_ERROR);
    let _ = module("_string", "formatter_field_name_split", &[text("x[]")]);
    assert_error_tag(VALUE_ERROR);
    let _ = module("_string", "formatter_field_name_split", &[text("x.y.")]);
    assert_error_tag(VALUE_ERROR);
    let _ = module("_string", "formatter_field_name_split", &[text("x[0]y")]);
    assert_error_tag(VALUE_ERROR);
}

// -------------------------------------------------------------------------
// _weakref
// -------------------------------------------------------------------------

#[test]
fn weakref_module_is_native_and_truthful() {
    let _guard = test_guard();
    hydro_runtime_init();
    let reference_type = module_get("_weakref", "ReferenceType");
    let ref_constant = module_get("_weakref", "ref");
    // `ref` is the class value itself, not a wrapper function.
    assert_same(ref_constant, reference_type);

    let proxy_type = module_get("_weakref", "ProxyType");
    let callable_proxy_type = module_get("_weakref", "CallableProxyType");
    let proxy_types = module_get("_weakref", "ProxyTypes");
    assert_no_error();
    assert_int(hydro_len(proxy_types), 2);
    assert_same(item(proxy_types, 0), proxy_type);
    assert_same(item(proxy_types, 1), callable_proxy_type);

    // A live reference resolves to its target and is registered.
    let object = module_get("builtins", "object");
    let target = hydro_instance_new(object);
    // `ref` is a constant (the ReferenceType class), so call it as a value.
    let reference_args = [target];
    let reference = hydro_call(ref_constant, 1, reference_args.as_ptr());
    assert_no_error();
    assert_same(
        hydro_getattr(reference, b"__class__".as_ptr(), b"__class__".len()),
        reference_type,
    );
    let resolved = hydro_call(reference, 0, std::ptr::null());
    assert_same(resolved, target);

    assert_int(module("_weakref", "getweakrefcount", &[target]), 1);
    let references = module("_weakref", "getweakrefs", &[target]);
    assert_int(hydro_len(references), 1);
    assert_same(hydro_subscript(references, int(0)), reference);

    // Callbacks are rejected explicitly instead of being ignored.
    let callback_args = [target, hydro_true()];
    let _ = hydro_call(ref_constant, 2, callback_args.as_ptr());
    assert_error_tag(TYPE_ERROR);

    // A proxy around a non-callable target raises instead of pretending.
    let plain = hydro_instance_new(object);
    let _ = module("_weakref", "proxy", &[plain]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);

    // A callable proxy forwards calls to its target.
    let target_function = hydro_builtin_function("list".as_ptr(), "list".len());
    let proxy = module("_weakref", "proxy", &[target_function]);
    assert_no_error();
    let empty = hydro_call(proxy, 0, std::ptr::null());
    assert_no_error();
    assert_int(hydro_len(empty), 0);
}
