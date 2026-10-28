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
const OVERFLOW_ERROR: i64 = 4;
const KEY_ERROR: i64 = 7;
const ATTRIBUTE_ERROR: i64 = 9;
const LOOKUP_ERROR: i64 = 29;
const IMPORT_ERROR: i64 = 30;
const NOT_IMPLEMENTED_ERROR: i64 = 32;
const UNICODE_DECODE_ERROR: i64 = 35;
const UNICODE_ENCODE_ERROR: i64 = 36;

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

/// Asserts a tagged pending exception with CPython's message text.
fn assert_error(actual: i64, tag: i64, message: &str) {
    assert_eq!(actual, hydro_none(), "a failing call must return None");
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), tag, "wrong exception tag");
    assert_eq!(
        hydro_binop(OP_EQ, hydro_to_str(hydro_exc_value()), text(message)),
        hydro_true(),
        "message mismatch: expected {message:?}"
    );
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

    // `deque` is a placeholder class shared by both module spellings.
    assert_same(
        module_get("_collections", "deque"),
        module_get("collections", "deque"),
    );

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

    // ASCII and Latin-1 are real codecs, including their aliases.
    assert_bytes(
        module("_codecs", "encode", &[text("abc"), text("ascii")]),
        b"abc",
    );
    assert_str(
        module("_codecs", "decode", &[bytes(b"abc"), text("us-ascii")]),
        "abc",
    );
    assert_bytes(
        module("_codecs", "encode", &[text("é"), text("latin-1")]),
        &[0xe9],
    );
    assert_str(
        module("_codecs", "decode", &[bytes(&[0xff]), text("latin_1")]),
        "ÿ",
    );
    assert_str(
        module("_codecs", "decode", &[bytes(&[0xff]), text("iso-8859-1")]),
        "ÿ",
    );

    // Unrepresentable characters: strict raises, ignore/replace recover.
    let _ = module("_codecs", "encode", &[text("é"), text("ascii")]);
    assert_error_tag(UNICODE_ENCODE_ERROR);
    assert_bytes(
        module(
            "_codecs",
            "encode",
            &[text("aéé"), text("ascii"), text("replace")],
        ),
        b"a??",
    );
    assert_bytes(
        module(
            "_codecs",
            "encode",
            &[text("aé"), text("ascii"), text("ignore")],
        ),
        b"a",
    );
    let _ = module("_codecs", "decode", &[bytes(b"a\xff"), text("ascii")]);
    assert_error_tag(UNICODE_DECODE_ERROR);
    assert_str(
        module(
            "_codecs",
            "decode",
            &[bytes(b"a\xff"), text("ascii"), text("replace")],
        ),
        "a\u{fffd}",
    );

    // The registry answers every built-in codec and its aliases, and rejects
    // unknown names instead of guessing.
    for alias in [
        "utf-8",
        "ascii",
        "us-ascii",
        "latin-1",
        "latin_1",
        "iso-8859-1",
    ] {
        let info = module("_codecs", "lookup", &[text(alias)]);
        assert_no_error();
        assert_int(hydro_len(info), 4);
    }
    let _ = module("_codecs", "lookup", &[text("cp1252")]);
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

// -------------------------------------------------------------------------
// int class and instance methods
// -------------------------------------------------------------------------

fn call_method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn call_method_kw(obj: i64, name: &str, args: &[i64], keywords: &[(&str, i64)]) -> i64 {
    let mut pairs = Vec::with_capacity(keywords.len() * 2);
    for (keyword, value) in keywords {
        pairs.push(text(keyword));
        pairs.push(*value);
    }
    hydro_method_kw(
        obj,
        name.as_ptr(),
        name.len(),
        args.len(),
        args.as_ptr(),
        keywords.len(),
        pairs.as_ptr(),
    )
}

/// The `int` builtin type value, as generated code passes it to `hydro_method`.
fn int_type() -> i64 {
    hydro_builtin_function(b"int".as_ptr(), 3)
}

/// `int.from_bytes(data, *args)` through the class-level dispatch.
fn from_bytes(data: i64, args: &[i64]) -> i64 {
    let mut full = Vec::with_capacity(args.len() + 1);
    full.push(data);
    full.extend_from_slice(args);
    call_method(int_type(), "from_bytes", &full)
}

/// `data.from_bytes(..., signed=...)` with `byteorder` positional.
fn from_bytes_signed(data: i64, byteorder: &str, signed: i64) -> i64 {
    call_method_kw(
        int_type(),
        "from_bytes",
        &[data, text(byteorder)],
        &[("signed", signed)],
    )
}

/// `value.to_bytes(length, byteorder, signed=True)`.
fn to_bytes_signed(value: i64, length: i64, byteorder: &str) -> i64 {
    call_method_kw(
        value,
        "to_bytes",
        &[int(length), text(byteorder)],
        &[("signed", hydro_true())],
    )
}

fn builtin(name: &str) -> i64 {
    hydro_builtin_function(name.as_ptr(), name.len())
}

#[test]
fn int_from_bytes_decodes_both_byte_orders() {
    let _guard = test_guard();
    hydro_runtime_init();
    let be = bytes(b"\x01\x02");
    assert_int(from_bytes(be, &[text("big")]), 258);
    assert_int(from_bytes(be, &[text("little")]), 513);
    // `byteorder` defaults to 'big'.
    assert_int(from_bytes(be, &[]), 258);
    // Empty input decodes to zero.
    assert_int(from_bytes(bytes(b""), &[]), 0);
    assert_int(from_bytes(bytes(b""), &[text("little")]), 0);

    // `byteorder` as a keyword behaves like the positional spelling.
    assert_int(
        call_method_kw(
            int_type(),
            "from_bytes",
            &[be],
            &[("byteorder", text("little"))],
        ),
        513,
    );
    // `bytes` may also arrive by keyword.
    assert_int(
        call_method_kw(int_type(), "from_bytes", &[], &[("bytes", be)]),
        258,
    );

    // `bytearray` and `memoryview` follow the buffer protocol.
    let buffer = hydro_call(builtin("bytearray"), 1, [be].as_ptr());
    assert_no_error();
    assert_int(from_bytes(buffer, &[text("big")]), 258);
    let view = hydro_call(builtin("memoryview"), 1, [be].as_ptr());
    assert_no_error();
    assert_int(from_bytes(view, &[text("little")]), 513);

    // The class attribute is a callable value, like CPython's type objects.
    let bound = hydro_getattr(int_type(), b"from_bytes".as_ptr(), 10);
    let result = hydro_call(bound, 2, [be, text("little")].as_ptr());
    assert_int(result, 513);
}

#[test]
fn int_from_bytes_honours_signed() {
    let _guard = test_guard();
    hydro_runtime_init();
    assert_int(from_bytes_signed(bytes(b"\xff"), "big", hydro_true()), -1);
    assert_int(from_bytes_signed(bytes(b"\xff"), "big", hydro_false()), 255);
    assert_int(
        from_bytes_signed(bytes(b"\xff\xfe"), "big", hydro_true()),
        -2,
    );
    assert_int(
        from_bytes_signed(bytes(b"\xff\xfe"), "little", hydro_true()),
        -257,
    );
    assert_int(from_bytes_signed(bytes(b"\x80"), "big", hydro_true()), -128);
    assert_int(from_bytes_signed(bytes(b"\x80"), "big", hydro_false()), 128);
    assert_int(from_bytes_signed(bytes(b""), "big", hydro_true()), 0);
    // `signed` is only tested for truthiness, as in CPython.
    assert_int(from_bytes_signed(bytes(b"\xff"), "big", int(1)), -1);
    assert_int(from_bytes_signed(bytes(b"\xff"), "big", int(0)), 255);
}

#[test]
fn int_from_bytes_rejects_invalid_arguments() {
    let _guard = test_guard();
    hydro_runtime_init();
    let _ = from_bytes(bytes(b"\x00"), &[text("middle")]);
    assert_error(
        hydro_none(),
        VALUE_ERROR,
        "byteorder must be either 'little' or 'big'",
    );
    let _ = from_bytes(bytes(b"\x00"), &[int(5)]);
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "from_bytes() argument 'byteorder' must be str, not int",
    );
    let _ = from_bytes(text("ab"), &[]);
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "cannot convert 'str' object to bytes",
    );
    let _ = call_method(int_type(), "from_bytes", &[]);
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "from_bytes() missing required argument 'bytes' (pos 1)",
    );
    // `signed` is keyword-only.
    let _ = from_bytes(bytes(b"\xff"), &[text("big"), hydro_true()]);
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "from_bytes() takes at most 2 positional arguments (3 given)",
    );
    let _ = call_method_kw(
        int_type(),
        "from_bytes",
        &[bytes(b"\x00")],
        &[("signed", hydro_true()), ("signed", hydro_false())],
    );
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "from_bytes() got multiple values for argument 'signed'",
    );
    let _ = call_method_kw(
        int_type(),
        "from_bytes",
        &[bytes(b"\x00")],
        &[("order", hydro_true())],
    );
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "from_bytes() got an unexpected keyword argument 'order'",
    );
}

#[test]
fn int_to_bytes_encodes_both_byte_orders() {
    let _guard = test_guard();
    hydro_runtime_init();
    assert_bytes(
        call_method(int(258), "to_bytes", &[int(2), text("big")]),
        b"\x01\x02",
    );
    assert_bytes(
        call_method(int(258), "to_bytes", &[int(2), text("little")]),
        b"\x02\x01",
    );
    // `length` defaults to 1 and `byteorder` to 'big'.
    assert_bytes(call_method(int(255), "to_bytes", &[]), b"\xff");
    assert_bytes(
        call_method_kw(int(255), "to_bytes", &[], &[("length", int(2))]),
        b"\x00\xff",
    );
    // Zero fits any length, including zero.
    assert_bytes(call_method(int(0), "to_bytes", &[int(0)]), b"");
    assert_bytes(call_method(int(0), "to_bytes", &[int(3)]), b"\x00\x00\x00");
    // The unbound class-level form takes the receiver first.
    assert_bytes(
        call_method(int_type(), "to_bytes", &[int(258), int(2), text("big")]),
        b"\x01\x02",
    );
    // The bound attribute form works like the direct call.
    let bound = hydro_getattr(int(258), b"to_bytes".as_ptr(), 8);
    let encoded = hydro_call(bound, 2, [int(2), text("little")].as_ptr());
    assert_bytes(encoded, b"\x02\x01");
}

#[test]
fn int_to_bytes_honours_signed_range() {
    let _guard = test_guard();
    hydro_runtime_init();
    assert_bytes(to_bytes_signed(int(-1), 1, "big"), b"\xff");
    assert_bytes(to_bytes_signed(int(-2), 2, "little"), b"\xfe\xff");
    assert_bytes(to_bytes_signed(int(-256), 2, "big"), b"\xff\x00");
    // The most negative representable value uses the sign bit alone.
    assert_bytes(to_bytes_signed(int(-32768), 2, "big"), b"\x80\x00");
    assert_bytes(to_bytes_signed(int(255), 2, "big"), b"\x00\xff");
    assert_bytes(to_bytes_signed(int(127), 1, "big"), b"\x7f");
    assert_bytes(to_bytes_signed(int(0), 2, "big"), b"\x00\x00");
}

#[test]
fn int_to_bytes_overflow_and_length_errors() {
    let _guard = test_guard();
    hydro_runtime_init();
    let _ = call_method(int(256), "to_bytes", &[int(1), text("big")]);
    assert_error(hydro_none(), OVERFLOW_ERROR, "int too big to convert");
    let _ = to_bytes_signed(int(128), 1, "big");
    assert_error(hydro_none(), OVERFLOW_ERROR, "int too big to convert");
    let _ = to_bytes_signed(int(-129), 1, "big");
    assert_error(hydro_none(), OVERFLOW_ERROR, "int too big to convert");
    let _ = call_method(int(-1), "to_bytes", &[int(1), text("big")]);
    assert_error(
        hydro_none(),
        OVERFLOW_ERROR,
        "can't convert negative int to unsigned",
    );
    let _ = call_method(int(1), "to_bytes", &[int(-1)]);
    assert_error(
        hydro_none(),
        VALUE_ERROR,
        "length argument must be non-negative",
    );
    let _ = call_method(int(1), "to_bytes", &[int(1), text("middle")]);
    assert_error(
        hydro_none(),
        VALUE_ERROR,
        "byteorder must be either 'little' or 'big'",
    );
}

#[test]
fn int_bit_length_matches_cpython() {
    let _guard = test_guard();
    hydro_runtime_init();
    assert_int(call_method(int(0), "bit_length", &[]), 0);
    assert_int(call_method(int(1), "bit_length", &[]), 1);
    assert_int(call_method(int(2), "bit_length", &[]), 2);
    assert_int(call_method(int(255), "bit_length", &[]), 8);
    assert_int(call_method(int(256), "bit_length", &[]), 9);
    // The count uses the absolute value.
    assert_int(call_method(int(-1), "bit_length", &[]), 1);
    assert_int(call_method(int(-255), "bit_length", &[]), 8);
    assert_int(call_method(int(-256), "bit_length", &[]), 9);

    // Arbitrary-precision values keep the exact bit count.
    let big = hydro_bigint_from_str(b"1267650600228229401496703205376".as_ptr(), 31);
    assert_int(call_method(big, "bit_length", &[]), 101);
    let negative_big = hydro_bigint_from_str(b"-1267650600228229401496703205376".as_ptr(), 32);
    assert_int(call_method(negative_big, "bit_length", &[]), 101);

    // Bound attribute and unbound class-level forms.
    let bound = hydro_getattr(int(255), b"bit_length".as_ptr(), 10);
    assert_int(hydro_call(bound, 0, std::ptr::null()), 8);
    assert_int(call_method(int_type(), "bit_length", &[int(256)]), 9);

    let _ = call_method(int(1), "bit_length", &[int(1)]);
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "int.bit_length() takes no arguments (1 given)",
    );
    let _ = call_method_kw(int(1), "bit_length", &[], &[("n", int(1))]);
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "bit_length() takes no keyword arguments",
    );
}

#[test]
fn int_bytes_round_trip_preserves_arbitrary_precision() {
    let _guard = test_guard();
    hydro_runtime_init();
    let big = hydro_bigint_from_str(b"1267650600228229401496703205376".as_ptr(), 31);
    // 2**100 is `0x10` followed by twelve zero bytes.
    let mut expected = [0u8; 13];
    expected[0] = 0x10;
    let encoded = call_method(big, "to_bytes", &[int(13), text("big")]);
    assert_bytes(encoded, &expected);
    let decoded = from_bytes(encoded, &[text("big")]);
    assert_no_error();
    assert_eq!(hydro_binop(OP_EQ, decoded, big), hydro_true());

    let mut expected_little = expected;
    expected_little.reverse();
    let encoded_little = call_method(big, "to_bytes", &[int(13), text("little")]);
    assert_bytes(encoded_little, &expected_little);
    let decoded_little = from_bytes(encoded_little, &[text("little")]);
    assert_no_error();
    assert_eq!(hydro_binop(OP_EQ, decoded_little, big), hydro_true());

    // Signed two's-complement round trip through the smallest byte lengths.
    let negative = from_bytes_signed(bytes(b"\x80"), "big", hydro_true());
    assert_int(negative, -128);
    assert_bytes(to_bytes_signed(negative, 1, "big"), b"\x80");
    assert_int(call_method(negative, "bit_length", &[]), 8);
}

#[test]
fn int_class_method_table_is_not_over_eager() {
    let _guard = test_guard();
    hydro_runtime_init();
    // Unknown class-level names still report AttributeError.
    let _ = call_method(int_type(), "fromhex", &[text("ff")]);
    assert_error_tag(ATTRIBUTE_ERROR);
    // Unknown instance methods too.
    let _ = call_method(int(1), "fromhex", &[]);
    assert_error_tag(ATTRIBUTE_ERROR);
    // Unrelated builtin functions keep their plain function surface.
    let len = builtin("len");
    let _ = call_method(len, "from_bytes", &[bytes(b"")]);
    assert_error_tag(ATTRIBUTE_ERROR);
}

// -------------------------------------------------------------------------
// str(bytes, encoding[, errors])
// -------------------------------------------------------------------------

fn call_builtin_kw(name: &str, args: &[i64], keywords: &[(&str, i64)]) -> i64 {
    let mut pairs = Vec::with_capacity(keywords.len() * 2);
    for (keyword, value) in keywords {
        pairs.push(text(keyword));
        pairs.push(*value);
    }
    hydro_call_kw(
        builtin(name),
        args.len(),
        args.as_ptr(),
        keywords.len(),
        pairs.as_ptr(),
    )
}

fn bytearray(value: &[u8]) -> i64 {
    let result = hydro_call(builtin("bytearray"), 1, [bytes(value)].as_ptr());
    assert_no_error();
    result
}

fn memoryview(value: &[u8]) -> i64 {
    let result = hydro_call(builtin("memoryview"), 1, [bytes(value)].as_ptr());
    assert_no_error();
    result
}

#[test]
fn str_constructor_keeps_the_single_argument_forms() {
    let _guard = test_guard();
    hydro_runtime_init();
    let function = builtin("str");
    assert_str(hydro_call(function, 0, std::ptr::null()), "");
    assert_str(hydro_call(function, 1, [int(42)].as_ptr()), "42");
    // `str(bytes)` is still the repr; only an explicit encoding decodes.
    assert_str(hydro_call(function, 1, [bytes(b"hi")].as_ptr()), "b'hi'");
    // Four arguments is still an arity error.
    let _ = hydro_call(
        function,
        4,
        [int(1), text("utf-8"), text("strict"), int(1)].as_ptr(),
    );
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "str() takes at most 3 argument(s) (4 given)",
    );
}

#[test]
fn str_decodes_bytes_like_objects() {
    let _guard = test_guard();
    hydro_runtime_init();
    let function = builtin("str");
    assert_str(
        hydro_call(function, 2, [bytes(&[0xc3, 0xa9]), text("utf-8")].as_ptr()),
        "é",
    );
    // `bytes`-compatible values: bytearray and memoryview are copied, not
    // reinterpreted.
    assert_str(
        hydro_call(function, 2, [bytearray(b"hi"), text("utf-8")].as_ptr()),
        "hi",
    );
    assert_str(
        hydro_call(
            function,
            2,
            [memoryview(&[0xc3, 0xa9]), text("utf-8")].as_ptr(),
        ),
        "é",
    );
    // Error handlers travel to the codec: recover, drop and reject.
    assert_str(
        hydro_call(
            function,
            3,
            [bytes(b"a\xff"), text("utf-8"), text("replace")].as_ptr(),
        ),
        "a\u{fffd}",
    );
    assert_str(
        hydro_call(
            function,
            3,
            [bytes(b"a\xff"), text("utf-8"), text("ignore")].as_ptr(),
        ),
        "a",
    );
    let _ = hydro_call(function, 2, [bytes(b"a\xff"), text("utf-8")].as_ptr());
    assert_error_tag(UNICODE_DECODE_ERROR);
    // ASCII and Latin-1 decode for real, and their aliases resolve.
    assert_str(
        hydro_call(function, 2, [bytes(b"abc"), text("ascii")].as_ptr()),
        "abc",
    );
    assert_str(
        hydro_call(function, 2, [bytes(b"abc"), text("us-ascii")].as_ptr()),
        "abc",
    );
    assert_str(
        hydro_call(function, 2, [bytes(&[0xff]), text("latin-1")].as_ptr()),
        "ÿ",
    );
    assert_str(
        hydro_call(function, 2, [bytes(&[0xff]), text("iso-8859-1")].as_ptr()),
        "ÿ",
    );
    // Non-ASCII bytes follow the selected error handler.
    let strict = hydro_call(function, 2, [bytes(&[0xff]), text("ascii")].as_ptr());
    assert_error(
        strict,
        UNICODE_DECODE_ERROR,
        "'ascii' codec can't decode byte 0xff in position 0: ordinal not in range(128)",
    );
    assert_str(
        hydro_call(
            function,
            3,
            [bytes(b"a\xff"), text("ascii"), text("replace")].as_ptr(),
        ),
        "a\u{fffd}",
    );
    let unknown = hydro_call(function, 2, [bytes(b"a"), text("cp1252")].as_ptr());
    assert_error(unknown, LOOKUP_ERROR, "unknown encoding: cp1252");
    let handler = hydro_call(
        function,
        3,
        [bytes(b"a"), text("utf-8"), text("bogus")].as_ptr(),
    );
    assert_error(handler, LOOKUP_ERROR, "unknown error handler name 'bogus'");
}

#[test]
fn str_decoding_rejects_non_bytes_and_bad_codec_arguments() {
    let _guard = test_guard();
    hydro_runtime_init();
    let function = builtin("str");
    // Records are rejected after the bytes-like check, like CPython.
    let _ = hydro_call(function, 2, [text("x"), text("utf-8")].as_ptr());
    assert_error(hydro_none(), TYPE_ERROR, "decoding str is not supported");
    let _ = hydro_call(function, 2, [int(7), text("utf-8")].as_ptr());
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "decoding to str: need a bytes-like object, int found",
    );
    let _ = hydro_call(function, 2, [bytes(b"hi"), int(1)].as_ptr());
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "str() argument 'encoding' must be str, not int",
    );
    let _ = hydro_call(
        function,
        3,
        [bytes(b"hi"), text("utf-8"), hydro_none()].as_ptr(),
    );
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "str() argument 'errors' must be str, not NoneType",
    );
}

#[test]
fn str_accepts_named_object_encoding_and_errors() {
    let _guard = test_guard();
    hydro_runtime_init();
    assert_str(
        call_builtin_kw(
            "str",
            &[bytes(&[0xc3, 0xa9])],
            &[("encoding", text("utf-8")), ("errors", text("strict"))],
        ),
        "é",
    );
    assert_str(
        call_builtin_kw(
            "str",
            &[],
            &[("object", bytes(b"hi")), ("encoding", text("utf-8"))],
        ),
        "hi",
    );
    // `object=` alone keeps the repr form.
    assert_str(call_builtin_kw("str", &[], &[("object", int(42))]), "42");
    // `errors=` without `encoding=` decodes as UTF-8, and no object at all is
    // the empty string even when codec keywords are present.
    assert_str(
        call_builtin_kw("str", &[bytes(b"a\xff")], &[("errors", text("ignore"))]),
        "a",
    );
    assert_str(
        call_builtin_kw("str", &[], &[("encoding", text("utf-8"))]),
        "",
    );
    let _ = call_builtin_kw("str", &[], &[("encoding", int(1))]);
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "str() argument 'encoding' must be str, not int",
    );
    let _ = call_builtin_kw("str", &[bytes(b"hi")], &[("nope", int(1))]);
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "str() got an unexpected keyword argument 'nope'",
    );
    let _ = call_builtin_kw("str", &[bytes(b"hi")], &[("object", bytes(b"yo"))]);
    assert_error(
        hydro_none(),
        TYPE_ERROR,
        "str() got multiple values for argument 'object'",
    );
}
