//! Integration tests for `bytes` (constructor ABI, indexing, iteration,
//! operators, methods, hashing, serialization and GC safety).
//!
//! The tests drive only the public runtime ABI, exactly like generated code.

use pyx_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const INDEX_ERROR: i64 = 6;

// `pyx_binop` op codes (see `ops.rs`).
const OP_ADD: u32 = 0;
const OP_MUL: u32 = 2;
const OP_EQ: u32 = 7;
const OP_NE: u32 = 8;
const OP_LT: u32 = 9;
const OP_LE: u32 = 10;
const OP_GT: u32 = 11;
const OP_GE: u32 = 12;
const OP_IN: u32 = 13;
const OP_NOT_IN: u32 = 14;

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
}

fn bytes(value: &[u8]) -> i64 {
    pyx_bytes_new(value.as_ptr(), value.len())
}

fn build(args: &[i64]) -> i64 {
    pyx_bytes_build(args.len(), args.as_ptr())
}

fn int(n: i64) -> i64 {
    // Test helpers pass untagged numbers; `pyx_int` treats a low-bit-zero
    // value as already tagged (8 would become integer 1).
    n << 3
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    pyx_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn module(module: &str, function: &str, args: &[i64]) -> i64 {
    pyx_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn eq(a: i64, b: i64) -> bool {
    pyx_binop(OP_EQ, a, b) == pyx_true()
}

fn assert_no_error() {
    assert_eq!(pyx_exc_pending(), 0, "unexpected pending exception");
}

fn assert_bytes(actual: i64, expected: &[u8]) {
    assert_no_error();
    assert!(
        eq(actual, bytes(expected)),
        "bytes mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        eq(actual, text(expected)),
        "string mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(actual, int(expected), "integer mismatch");
}

fn expect_error(tag: i64) {
    assert_eq!(pyx_exc_pending(), 1, "expected a pending exception");
    assert_eq!(pyx_exc_tag(), tag, "wrong exception tag");
    pyx_exc_clear();
}

/// Overwrites a large portion of the stack below the caller with zeroes so
/// the conservative collector does not keep dead locals alive.
#[inline(never)]
fn scrub_stack() {
    let mut buffer = [0u8; 64 * 1024];
    std::hint::black_box(&mut buffer);
    buffer[0] = 0;
    std::hint::black_box(&buffer);
}

#[inline(never)]
fn make_garbage(count: usize) {
    for index in 0..count {
        let payload = [index as u8, 1, 2, 3];
        let _ = pyx_bytes_new(payload.as_ptr(), payload.len());
    }
}

// -------------------------------------------------------------------------
// Literal ABI and construction
// -------------------------------------------------------------------------

#[test]
fn literal_roundtrip_and_basic_protocol() {
    pyx_runtime_init();
    let value = bytes(b"hello\x00world");
    assert_bytes(value, b"hello\x00world");
    assert_int(pyx_len(value), 11);
    assert_eq!(pyx_truthy(value), 1);
    assert_eq!(pyx_truthy(bytes(b"")), 0);
    assert_str(pyx_to_str(value), "b'hello\\x00world'");
    assert_no_error();
}

#[test]
fn null_and_empty_literal_buffers() {
    pyx_runtime_init();
    let null_ptr = std::ptr::null();
    let empty = pyx_bytes_new(null_ptr, 0);
    assert_bytes(empty, b"");
    assert_int(pyx_len(empty), 0);
    assert_no_error();
}

#[test]
fn constructor_no_arguments() {
    pyx_runtime_init();
    let empty = pyx_bytes_build(0, std::ptr::null());
    assert_bytes(empty, b"");
    assert_no_error();
}

#[test]
fn constructor_copies_bytes() {
    pyx_runtime_init();
    let source = bytes(b"copy");
    let copy = build(&[source]);
    assert_bytes(copy, b"copy");
    // A fresh object, not the same heap allocation.
    assert_ne!(source, copy);
    assert_no_error();
}

#[test]
fn constructor_from_integer_zero_fills() {
    pyx_runtime_init();
    assert_bytes(build(&[int(0)]), b"");
    assert_bytes(build(&[int(4)]), b"\x00\x00\x00\x00");
    build(&[int(-1)]);
    expect_error(VALUE_ERROR);
    build(&[int(64 * 1024 * 1024 + 1)]);
    expect_error(VALUE_ERROR);
}

#[test]
fn constructor_from_sequence() {
    pyx_runtime_init();
    let items = [int(0), int(65), int(255)];
    let list = pyx_list_new(items.len(), items.as_ptr());
    assert_bytes(build(&[list]), b"\x00A\xff");

    let tuple = pyx_tuple_new(items.len(), items.as_ptr());
    assert_bytes(build(&[tuple]), b"\x00A\xff");

    let set = pyx_set_new(items.len(), items.as_ptr());
    assert_bytes(build(&[set]), b"\x00A\xff");

    let too_large = [int(256)];
    let bad = pyx_list_new(too_large.len(), too_large.as_ptr());
    build(&[bad]);
    expect_error(VALUE_ERROR);

    let negative = [int(-3)];
    let bad = pyx_list_new(negative.len(), negative.as_ptr());
    build(&[bad]);
    expect_error(VALUE_ERROR);

    let wrong = [text("x")];
    let bad = pyx_list_new(wrong.len(), wrong.as_ptr());
    build(&[bad]);
    expect_error(TYPE_ERROR);
}

#[test]
fn constructor_with_encoding() {
    pyx_runtime_init();
    assert_bytes(build(&[text("h\u{e9}"), text("utf-8")]), "hé".as_bytes());
    assert_bytes(build(&[text("hi"), text("UTF8")]), b"hi");
    // A str without an encoding is an error.
    build(&[text("hi")]);
    expect_error(TYPE_ERROR);
    // Unknown encodings are rejected.
    build(&[text("hi"), text("latin-1")]);
    expect_error(VALUE_ERROR);
    // Encoding without a string.
    build(&[int(3), text("utf-8")]);
    expect_error(TYPE_ERROR);
    // Non-str encoding argument.
    build(&[text("hi"), int(1)]);
    expect_error(TYPE_ERROR);
    // Too many arguments.
    build(&[text("hi"), text("utf-8"), text("strict")]);
    expect_error(TYPE_ERROR);
    // Unsupported source type.
    build(&[pyx_none()]);
    expect_error(TYPE_ERROR);
}

// -------------------------------------------------------------------------
// Indexing, slicing and iteration
// -------------------------------------------------------------------------

#[test]
fn indexing_yields_integers() {
    pyx_runtime_init();
    let value = bytes(b"ab");
    assert_int(pyx_subscript(value, int(0)), 97);
    assert_int(pyx_subscript(value, int(1)), 98);
    assert_int(pyx_subscript(value, int(-1)), 98);
    assert_int(pyx_subscript(value, int(-2)), 97);

    pyx_subscript(value, int(2));
    expect_error(INDEX_ERROR);
    pyx_subscript(value, int(-3));
    expect_error(INDEX_ERROR);

    pyx_subscript(value, text("x"));
    expect_error(TYPE_ERROR);
}

#[test]
fn slicing_returns_bytes() {
    pyx_runtime_init();
    let value = bytes(b"abcdef");
    let none = pyx_none();

    assert_bytes(pyx_slice(value, int(1), int(5), none), b"bcde");
    assert_bytes(pyx_slice(value, int(0), none, int(2)), b"ace");
    assert_bytes(pyx_slice(value, none, none, int(-1)), b"fedcba");
    assert_bytes(pyx_slice(value, int(-2), none, none), b"ef");
    assert_bytes(pyx_slice(value, int(4), int(1), int(-1)), b"edc");
    assert_bytes(pyx_slice(value, int(100), none, none), b"");

    pyx_slice(value, int(0), none, int(0));
    expect_error(VALUE_ERROR);
}

#[test]
fn iteration_and_list_conversion() {
    pyx_runtime_init();
    let value = bytes(b"AB");
    let iterator = pyx_iter_new(value);
    assert_no_error();
    let mut out = 0i64;
    assert_eq!(pyx_iter_next(iterator, &mut out), 1);
    assert_eq!(out, int(65));
    assert_eq!(pyx_iter_next(iterator, &mut out), 1);
    assert_eq!(out, int(66));
    assert_eq!(pyx_iter_next(iterator, &mut out), 0);
    assert_no_error();

    let listed = pyx_list_from(value);
    assert_int(pyx_len(listed), 2);
    assert_int(pyx_subscript(listed, int(0)), 65);
    assert_int(pyx_subscript(listed, int(1)), 66);
    assert_no_error();
}

// -------------------------------------------------------------------------
// Operators
// -------------------------------------------------------------------------

#[test]
fn concatenation_and_repetition() {
    pyx_runtime_init();
    assert_bytes(pyx_binop(OP_ADD, bytes(b"a"), bytes(b"b")), b"ab");
    assert_bytes(pyx_binop(OP_ADD, bytes(b""), bytes(b"x")), b"x");
    assert_bytes(pyx_binop(OP_MUL, bytes(b"ab"), int(3)), b"ababab");
    assert_bytes(pyx_binop(OP_MUL, int(2), bytes(b"xy")), b"xyxy");
    assert_bytes(pyx_binop(OP_MUL, bytes(b"ab"), int(0)), b"");
    assert_bytes(pyx_binop(OP_MUL, bytes(b"ab"), int(-5)), b"");
    assert_bytes(pyx_binop(OP_MUL, bytes(b""), int(1000)), b"");

    // `bytes + str` is a type error.
    pyx_binop(OP_ADD, bytes(b"a"), text("b"));
    expect_error(TYPE_ERROR);
}

#[test]
fn comparisons_are_lexicographic() {
    pyx_runtime_init();
    assert_eq!(pyx_binop(OP_EQ, bytes(b"abc"), bytes(b"abc")), pyx_true());
    assert_eq!(pyx_binop(OP_NE, bytes(b"abc"), bytes(b"abd")), pyx_true());
    assert_eq!(pyx_binop(OP_LT, bytes(b"abc"), bytes(b"abd")), pyx_true());
    assert_eq!(pyx_binop(OP_LE, bytes(b"abc"), bytes(b"abc")), pyx_true());
    assert_eq!(pyx_binop(OP_GT, bytes(b"b"), bytes(b"abc")), pyx_true());
    assert_eq!(pyx_binop(OP_GE, bytes(b"a"), bytes(b"a")), pyx_true());
    assert_eq!(pyx_binop(OP_NE, bytes(b"a"), bytes(b"a")), pyx_false());
    // Comparisons against other types: equality is defined, ordering is not.
    assert_eq!(pyx_binop(OP_EQ, bytes(b"a"), text("a")), pyx_false());
    assert_eq!(pyx_binop(OP_NE, bytes(b"a"), text("a")), pyx_true());
    pyx_binop(OP_LT, bytes(b"a"), text("a"));
    expect_error(TYPE_ERROR);
}

#[test]
fn membership_checks() {
    pyx_runtime_init();
    let haystack = bytes(b"hello");
    assert_eq!(pyx_binop(OP_IN, bytes(b"ell"), haystack), pyx_true());
    assert_eq!(pyx_binop(OP_IN, bytes(b"xyz"), haystack), pyx_false());
    assert_eq!(pyx_binop(OP_IN, bytes(b""), haystack), pyx_true());
    assert_eq!(pyx_binop(OP_NOT_IN, bytes(b"xyz"), haystack), pyx_true());
    assert_eq!(pyx_binop(OP_IN, int(108), haystack), pyx_true());
    assert_eq!(pyx_binop(OP_IN, int(256), haystack), pyx_false());
    assert_eq!(pyx_binop(OP_IN, int(-1), haystack), pyx_false());

    pyx_binop(OP_IN, text("ell"), haystack);
    expect_error(TYPE_ERROR);
}

// -------------------------------------------------------------------------
// Methods
// -------------------------------------------------------------------------

#[test]
fn decode_and_encode() {
    pyx_runtime_init();
    assert_str(method(bytes(b"hi"), "decode", &[]), "hi");
    assert_str(
        method(bytes("hé".as_bytes()), "decode", &[text("utf-8")]),
        "hé",
    );

    let invalid = [0x66, 0xff, 0x6f];
    pyx_method(bytes(&invalid), "decode".as_ptr(), 6, 0, std::ptr::null());
    expect_error(VALUE_ERROR);
    assert_str(
        method(bytes(&invalid), "decode", &[text("utf-8"), text("ignore")]),
        "fo",
    );
    assert_str(
        method(bytes(&invalid), "decode", &[text("utf-8"), text("replace")]),
        "f\u{fffd}o",
    );
    // Unknown encoding / error handler.
    pyx_method(
        bytes(b"x"),
        "decode".as_ptr(),
        6,
        1,
        [text("latin-1")].as_ptr(),
    );
    expect_error(VALUE_ERROR);
    // Unknown error handler.
    pyx_method(
        bytes(b"\xff"),
        "decode".as_ptr(),
        6,
        2,
        [text("utf-8"), text("bogus")].as_ptr(),
    );
    expect_error(VALUE_ERROR);
    assert_str(
        method(bytes(b"x"), "decode", &[text("utf-8"), text("strict")]),
        "x",
    );
    pyx_method(bytes(b"x"), "decode".as_ptr(), 6, 3, std::ptr::null());
    expect_error(TYPE_ERROR);

    // str.encode is the inverse.
    let encoded = method(text("hé"), "encode", &[]);
    assert_bytes(encoded, "hé".as_bytes());
    assert_bytes(method(text("hi"), "encode", &[text("utf-8")]), b"hi");
    pyx_method(
        text("hi"),
        "encode".as_ptr(),
        6,
        1,
        [text("latin-1")].as_ptr(),
    );
    expect_error(VALUE_ERROR);
}

#[test]
fn hex_and_search_methods() {
    pyx_runtime_init();
    let value = bytes(b"ab\x00\xff");
    assert_str(method(value, "hex", &[]), "616200ff");
    assert_str(method(bytes(b""), "hex", &[]), "");

    let haystack = bytes(b"banana");
    assert_int(method(haystack, "count", &[bytes(b"an")]), 2);
    assert_int(method(haystack, "count", &[bytes(b"")]), 7);
    assert_int(method(haystack, "count", &[bytes(b"z")]), 0);
    assert_int(method(haystack, "find", &[bytes(b"an")]), 1);
    assert_int(method(haystack, "find", &[bytes(b"")]), 0);
    assert_int(method(haystack, "find", &[bytes(b"z")]), -1);

    assert_eq!(method(haystack, "startswith", &[bytes(b"ban")]), pyx_true());
    assert_eq!(
        method(haystack, "startswith", &[bytes(b"nan")]),
        pyx_false()
    );
    assert_eq!(method(haystack, "endswith", &[bytes(b"ana")]), pyx_true());
    assert_eq!(method(haystack, "endswith", &[bytes(b"ba")]), pyx_false());

    // Wrong argument types are type errors.
    pyx_method(haystack, "count".as_ptr(), 5, 1, [text("a")].as_ptr());
    expect_error(TYPE_ERROR);
    pyx_method(haystack, "find".as_ptr(), 4, 0, std::ptr::null());
    expect_error(TYPE_ERROR);
    pyx_method(haystack, "startswith".as_ptr(), 10, 1, [text("b")].as_ptr());
    expect_error(TYPE_ERROR);
    pyx_method(haystack, "upper".as_ptr(), 5, 1, [int(1)].as_ptr());
    expect_error(TYPE_ERROR);
}

#[test]
fn replace_split_and_join() {
    pyx_runtime_init();
    assert_bytes(
        method(bytes(b"a,b,c"), "replace", &[bytes(b","), bytes(b"-")]),
        b"a-b-c",
    );
    assert_bytes(
        method(
            bytes(b"aaaa"),
            "replace",
            &[bytes(b"a"), bytes(b"b"), int(2)],
        ),
        b"bbaa",
    );
    assert_bytes(
        method(bytes(b"abc"), "replace", &[bytes(b""), bytes(b"-")]),
        b"-a-b-c-",
    );
    assert_bytes(
        method(bytes(b"abc"), "replace", &[bytes(b""), bytes(b"-"), int(2)]),
        b"-a-bc",
    );

    let parts = method(bytes(b"a,b,,c"), "split", &[bytes(b",")]);
    assert_int(pyx_len(parts), 4);
    assert_bytes(pyx_subscript(parts, int(0)), b"a");
    assert_bytes(pyx_subscript(parts, int(2)), b"");
    assert_bytes(pyx_subscript(parts, int(3)), b"c");

    let words = method(bytes(b"  a\tb\n c "), "split", &[]);
    assert_int(pyx_len(words), 3);
    assert_bytes(pyx_subscript(words, int(0)), b"a");
    assert_bytes(pyx_subscript(words, int(1)), b"b");
    assert_bytes(pyx_subscript(words, int(2)), b"c");

    pyx_method(bytes(b"a"), "split".as_ptr(), 5, 1, [bytes(b"")].as_ptr());
    expect_error(VALUE_ERROR);

    let items = [bytes(b"a"), bytes(b"b"), bytes(b"c")];
    let list = pyx_list_new(items.len(), items.as_ptr());
    assert_bytes(method(bytes(b","), "join", &[list]), b"a,b,c");
    let tuple = pyx_tuple_new(items.len(), items.as_ptr());
    assert_bytes(method(bytes(b"-"), "join", &[tuple]), b"a-b-c");
    assert_bytes(
        method(bytes(b","), "join", &[pyx_list_new(0, std::ptr::null())]),
        b"",
    );

    let mixed = [bytes(b"a"), text("b")];
    let list = pyx_list_new(mixed.len(), mixed.as_ptr());
    pyx_method(bytes(b","), "join".as_ptr(), 4, 1, [list].as_ptr());
    expect_error(TYPE_ERROR);
}

#[test]
fn strip_and_case_methods() {
    pyx_runtime_init();
    assert_bytes(method(bytes(b"  hi \n"), "strip", &[]), b"hi");
    assert_bytes(method(bytes(b"  hi \n"), "lstrip", &[]), b"hi \n");
    assert_bytes(method(bytes(b"  hi \n"), "rstrip", &[]), b"  hi");
    assert_bytes(method(bytes(b"xxhixx"), "strip", &[bytes(b"x")]), b"hi");
    assert_bytes(method(bytes(b"xyhixy"), "lstrip", &[bytes(b"xy")]), b"hixy");
    assert_bytes(method(bytes(b"xyhixy"), "rstrip", &[bytes(b"xy")]), b"xyhi");
    assert_bytes(method(bytes(b"abc"), "strip", &[bytes(b"")]), b"abc");

    assert_bytes(method(bytes(b"aBc"), "upper", &[]), b"ABC");
    assert_bytes(method(bytes(b"aBc"), "lower", &[]), b"abc");
    // ASCII-only casing leaves non-ASCII bytes untouched.
    assert_bytes(method(bytes(b"\xe9a"), "upper", &[]), b"\xe9A");
    assert_no_error();
}

#[test]
fn unknown_attribute_is_reported() {
    pyx_runtime_init();
    pyx_method(bytes(b"a"), "nope".as_ptr(), 4, 0, std::ptr::null());
    expect_error(9); // ATTRIBUTE_ERROR
}

// -------------------------------------------------------------------------
// Hashing: dict and set keys
// -------------------------------------------------------------------------

#[test]
fn bytes_are_hashable() {
    pyx_runtime_init();
    let keys = [bytes(b"key")];
    let values = [int(42)];
    let dict = pyx_dict_new(keys.len(), keys.as_ptr(), values.as_ptr());
    assert_int(pyx_subscript(dict, bytes(b"key")), 42);
    assert_eq!(pyx_binop(OP_IN, bytes(b"key"), dict), pyx_true());
    // A missing key raises KeyError (tag 7).
    pyx_subscript(dict, bytes(b"missing"));
    expect_error(7);

    // Equal bytes inserted twice collapse to one set element.
    let items = [bytes(b"dup"), bytes(b"dup")];
    let set = pyx_set_new(items.len(), items.as_ptr());
    assert_int(pyx_len(set), 1);
    assert_eq!(pyx_binop(OP_IN, bytes(b"dup"), set), pyx_true());
    assert_eq!(pyx_binop(OP_IN, bytes(b"other"), set), pyx_false());

    // Dict keys work nested inside lists too.
    let nested_keys = [bytes(b"k")];
    let nested_values = [pyx_list_new(keys.len(), keys.as_ptr())];
    let dict = pyx_dict_new(
        nested_keys.len(),
        nested_keys.as_ptr(),
        nested_values.as_ptr(),
    );
    assert_int(pyx_len(pyx_subscript(dict, bytes(b"k"))), 1);
    assert_no_error();
}

// -------------------------------------------------------------------------
// Serialization
// -------------------------------------------------------------------------

#[test]
fn pickle_roundtrip() {
    pyx_runtime_init();
    let value = bytes(b"binary\x00\xff");
    let dumped = module("pickle", "dumps", &[value]);
    assert_no_error();
    let loaded = module("pickle", "loads", &[dumped]);
    assert_bytes(loaded, b"binary\x00\xff");

    // Nested in a list and as a dict key.
    let items = [bytes(b"a"), bytes(b"")];
    let list = pyx_list_new(items.len(), items.as_ptr());
    let loaded_list = module("pickle", "loads", &[module("pickle", "dumps", &[list])]);
    assert_no_error();
    assert_int(pyx_len(loaded_list), 2);
    assert_bytes(pyx_subscript(loaded_list, int(0)), b"a");
    assert_bytes(pyx_subscript(loaded_list, int(1)), b"");

    let keys = [bytes(b"k")];
    let values = [bytes(b"v")];
    let dict = pyx_dict_new(keys.len(), keys.as_ptr(), values.as_ptr());
    let loaded_dict = module("pickle", "loads", &[module("pickle", "dumps", &[dict])]);
    assert_no_error();
    assert_bytes(pyx_subscript(loaded_dict, bytes(b"k")), b"v");
}

// -------------------------------------------------------------------------
// GC safety
// -------------------------------------------------------------------------

#[test]
fn bytes_survive_collection() {
    pyx_runtime_init();
    let reachable = bytes(b"survivor");
    let payload = [reachable];
    let list = pyx_list_new(payload.len(), payload.as_ptr());

    make_garbage(300);
    scrub_stack();
    let _freed = pyx_gc_collect();

    assert_bytes(reachable, b"survivor");
    assert_bytes(pyx_subscript(list, int(0)), b"survivor");
    assert_int(pyx_len(reachable), 8);
    assert_no_error();
}

#[test]
fn bytes_methods_are_gc_safe_under_pressure() {
    pyx_runtime_init();
    let value = bytes(b"a,b,c");
    let separator = bytes(b",");
    for _ in 0..50 {
        let parts = method(value, "split", &[separator]);
        assert_int(pyx_len(parts), 3);
        let joined = method(separator, "join", &[parts]);
        assert_bytes(joined, b"a,b,c");
        let _ = method(joined, "replace", &[separator, bytes(b"-")]);
        let _ = method(joined, "hex", &[]);
        let _ = method(joined, "upper", &[]);
        let _ = pyx_gc_collect();
    }
    assert_bytes(value, b"a,b,c");
    assert_no_error();
}
