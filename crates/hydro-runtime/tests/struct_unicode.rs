//! Integration tests for the `_struct` and `unicodedata` native modules.
//!
//! The tests drive the public runtime ABI exactly like generated code. They
//! assume the registration entries described in the module's report:
//! `_struct`/`unicodedata` are added to `stdlib::MODULES`, `_struct` exposes
//! `Struct` and `error` as class values, and `struct_mod::class_value` runs
//! before the function table in `module_get_named`.

use hydro_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const KEY_ERROR: i64 = 7;
const EXCEPTION: i64 = 12;

fn int(n: i64) -> i64 {
    // The ABI takes tagged immediates; encode explicitly.
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

fn call(function: i64, args: &[i64]) -> i64 {
    hydro_call(function, args.len(), args.as_ptr())
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn getattr(obj: i64, name: &str) -> i64 {
    hydro_getattr(obj, name.as_ptr(), name.len())
}

fn item(value: i64, index: i64) -> i64 {
    hydro_subscript(value, int(index))
}

fn eq(a: i64, b: i64) -> bool {
    hydro_binop(7, a, b) == hydro_true()
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_to_int(actual), hydro_to_int(int(expected)));
}

fn assert_float(actual: i64, expected: f64) {
    assert_no_error();
    assert!(
        eq(actual, hydro_float_new(expected)),
        "float mismatch, expected {expected}"
    );
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        eq(actual, text(expected)),
        "string mismatch, expected {expected:?}"
    );
}

fn assert_bytes(actual: i64, expected: &[u8]) {
    assert_no_error();
    assert!(
        eq(actual, bytes(expected)),
        "bytes mismatch, expected {expected:?}"
    );
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_true(), "expected True");
}

fn assert_false(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_false(), "expected False");
}

fn expect_error(tag: i64) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), tag, "wrong exception tag");
    hydro_exc_clear();
}

// -------------------------------------------------------------------------
// _struct
// -------------------------------------------------------------------------

#[test]
fn struct_calcsize_uses_standard_and_native_sizes() {
    hydro_runtime_init();
    assert_int(module("_struct", "calcsize", &[text("@bi")]), 8);
    assert_int(module("_struct", "calcsize", &[text("=bi")]), 5);
    assert_int(module("_struct", "calcsize", &[text("<bi")]), 5);
    assert_int(module("_struct", "calcsize", &[text(">bh")]), 3);
    assert_int(module("_struct", "calcsize", &[text("il")]), 16);
    assert_int(module("_struct", "calcsize", &[text("!2I")]), 8);
    assert_int(module("_struct", "calcsize", &[text("2sx3?")]), 6);
    assert_int(module("_struct", "calcsize", &[text("0s")]), 0);
}

#[test]
fn struct_pack_and_unpack_round_trip() {
    hydro_runtime_init();

    assert_bytes(
        module("_struct", "pack", &[text("<i"), int(258)]),
        &[0x02, 0x01, 0x00, 0x00],
    );
    assert_bytes(
        module("_struct", "pack", &[text(">H"), int(258)]),
        &[0x01, 0x02],
    );
    assert_bytes(
        module("_struct", "pack", &[text("<?B"), hydro_true(), int(255)]),
        &[0x01, 0xFF],
    );
    assert_bytes(
        module("_struct", "pack", &[text("<4s"), bytes(b"ab")]),
        b"ab\0\0",
    );

    let packed = module(
        "_struct",
        "pack",
        &[
            text("<bBhHiqfd?"),
            int(-5),
            int(250),
            int(-2),
            int(65535),
            int(-2),
            int(1),
            hydro_float_new(1.5),
            hydro_float_new(2.25),
            hydro_true(),
        ],
    );
    assert_no_error();
    let unpacked = module("_struct", "unpack", &[text("<bBhHiqfd?"), packed]);
    assert_no_error();
    assert_int(item(unpacked, 0), -5);
    assert_int(item(unpacked, 1), 250);
    assert_int(item(unpacked, 2), -2);
    assert_int(item(unpacked, 3), 65535);
    assert_int(item(unpacked, 4), -2);
    assert_int(item(unpacked, 5), 1);
    assert_float(item(unpacked, 6), 1.5);
    assert_float(item(unpacked, 7), 2.25);
    assert_true(item(unpacked, 8));

    let strings = module("_struct", "unpack", &[text("<4s"), bytes(b"ab\0\0")]);
    assert_no_error();
    assert_bytes(item(strings, 0), b"ab\0\0");
}

#[test]
fn struct_pack_into_and_unpack_from_offsets() {
    hydro_runtime_init();
    let zeros = [0u8; 4];
    let buffer = hydro_bytearray_new(zeros.as_ptr(), zeros.len());
    let offset_buffer = hydro_bytearray_new(zeros.as_ptr(), zeros.len());
    assert_no_error();

    module(
        "_struct",
        "pack_into",
        &[text("=H"), buffer, int(1), int(258)],
    );
    assert_no_error();
    assert_bytes(buffer, b"\0\x02\x01\0");
    assert_int(
        item(
            module("_struct", "unpack_from", &[text("=H"), buffer, int(1)]),
            0,
        ),
        258,
    );

    module(
        "_struct",
        "pack_into",
        &[text("<H"), offset_buffer, int(-2), int(258)],
    );
    assert_no_error();
    assert_bytes(offset_buffer, b"\0\0\x02\x01");
}

#[test]
fn struct_iter_unpack_returns_tuples() {
    hydro_runtime_init();
    let iterator = module(
        "_struct",
        "iter_unpack",
        &[text("<H"), bytes(b"\x01\0\x02\0")],
    );
    assert_no_error();
    let list_function = hydro_builtin_function("list".as_ptr(), 4);
    let values = call(list_function, &[iterator]);
    assert_no_error();
    assert_int(hydro_len(values), 2);
    assert_int(item(item(values, 0), 0), 1);
    assert_int(item(item(values, 1), 0), 2);
}

#[test]
fn struct_class_exposes_format_size_and_methods() {
    hydro_runtime_init();
    let class = module_get("_struct", "Struct");
    assert_no_error();
    let instance = call(class, &[text(">H")]);
    assert_no_error();
    assert_str(getattr(instance, "format"), ">H");
    assert_int(getattr(instance, "size"), 2);
    assert_bytes(method(instance, "pack", &[int(258)]), b"\x01\x02");
    assert_int(
        item(method(instance, "unpack", &[bytes(b"\x01\x02")]), 0),
        258,
    );
    assert_int(
        item(
            method(instance, "unpack_from", &[bytes(b"x\x01\x02"), int(1)]),
            0,
        ),
        258,
    );
}

#[test]
fn struct_reports_errors_as_struct_error() {
    hydro_runtime_init();

    // Out-of-range integer.
    module("_struct", "pack", &[text("<B"), int(256)]);
    expect_error(EXCEPTION);

    // Non-integer where the format requires one.
    module("_struct", "pack", &[text("<i"), hydro_float_new(1.0)]);
    expect_error(EXCEPTION);

    // Wrong number of values.
    module("_struct", "pack", &[text("<i")]);
    expect_error(EXCEPTION);

    // Truncated unpack buffer.
    module("_struct", "unpack", &[text("<i"), bytes(b"\x01")]);
    expect_error(EXCEPTION);

    // Bad format character.
    module("_struct", "calcsize", &[text("<z")]);
    expect_error(EXCEPTION);

    // Unknown attribute.
    module("_struct", "no_such_function", &[]);
    expect_error(9);
}

// -------------------------------------------------------------------------
// unicodedata
// -------------------------------------------------------------------------

#[test]
fn unicodedata_categories_and_combining_classes() {
    hydro_runtime_init();
    assert_str(module("unicodedata", "category", &[text("A")]), "Lu");
    assert_str(module("unicodedata", "category", &[text("a")]), "Ll");
    assert_str(module("unicodedata", "category", &[text("中")]), "Lo");
    assert_str(module("unicodedata", "category", &[text("1")]), "Nd");
    assert_str(module("unicodedata", "category", &[text(" ")]), "Zs");
    assert_str(module("unicodedata", "category", &[text("\u{301}")]), "Mn");
    assert_str(module("unicodedata", "category", &[text("😀")]), "So");
    assert_str(module("unicodedata", "category", &[text("\u{378}")]), "Cn");

    assert_int(module("unicodedata", "combining", &[text("A")]), 0);
    assert_int(module("unicodedata", "combining", &[text("\u{301}")]), 230);
    assert_int(module("unicodedata", "combining", &[text("\u{20D0}")]), 230);
}

#[test]
fn unicodedata_east_asian_width_and_bidirectional() {
    hydro_runtime_init();
    assert_str(
        module("unicodedata", "east_asian_width", &[text("A")]),
        "Na",
    );
    assert_str(module("unicodedata", "east_asian_width", &[text("é")]), "A");
    assert_str(
        module("unicodedata", "east_asian_width", &[text("中")]),
        "W",
    );
    assert_str(
        module("unicodedata", "east_asian_width", &[text("😀")]),
        "W",
    );
    assert_str(
        module("unicodedata", "east_asian_width", &[text("\u{301}")]),
        "A",
    );

    assert_str(module("unicodedata", "bidirectional", &[text("A")]), "L");
    assert_str(module("unicodedata", "bidirectional", &[text("א")]), "R");
    assert_str(
        module("unicodedata", "bidirectional", &[text("\u{301}")]),
        "NSM",
    );
    assert_str(module("unicodedata", "bidirectional", &[text("1")]), "EN");
}

#[test]
fn unicodedata_decomposition_and_normalization() {
    hydro_runtime_init();
    assert_str(
        module("unicodedata", "decomposition", &[text("é")]),
        "0065 0301",
    );
    assert_str(
        module("unicodedata", "decomposition", &[text("①")]),
        "<circle> 0031",
    );
    assert_str(
        module("unicodedata", "decomposition", &[text("가")]),
        "1100 1161",
    );
    assert_str(module("unicodedata", "decomposition", &[text("A")]), "");

    let nfd = module("unicodedata", "normalize", &[text("NFD"), text("é")]);
    assert_str(nfd, "e\u{301}");
    assert_str(module("unicodedata", "normalize", &[text("NFC"), nfd]), "é");
    assert_str(
        module("unicodedata", "normalize", &[text("NFKC"), text("①")]),
        "1",
    );
    assert_str(
        module("unicodedata", "normalize", &[text("NFKC"), text("ﬁ")]),
        "fi",
    );
    assert_str(
        module("unicodedata", "normalize", &[text("NFD"), text("가")]),
        "\u{1100}\u{1161}",
    );
    assert_str(
        module(
            "unicodedata",
            "normalize",
            &[text("NFC"), text("\u{1100}\u{1161}\u{11A8}")],
        ),
        "각",
    );

    assert_true(module(
        "unicodedata",
        "is_normalized",
        &[text("NFC"), text("é")],
    ));
    assert_false(module(
        "unicodedata",
        "is_normalized",
        &[text("NFD"), text("é")],
    ));

    module("unicodedata", "normalize", &[text("BAD"), text("x")]);
    expect_error(VALUE_ERROR);
}

#[test]
fn unicodedata_names_lookup_and_numeric_values() {
    hydro_runtime_init();
    assert_str(
        module("unicodedata", "name", &[text("é")]),
        "LATIN SMALL LETTER E WITH ACUTE",
    );
    assert_str(
        module(
            "unicodedata",
            "lookup",
            &[text("LATIN SMALL LETTER E WITH ACUTE")],
        ),
        "é",
    );
    assert_str(
        module(
            "unicodedata",
            "lookup",
            &[text("latin small letter e with acute")],
        ),
        "é",
    );
    assert_str(
        module("unicodedata", "name", &[text("\u{4E00}")]),
        "CJK UNIFIED IDEOGRAPH-4E00",
    );
    assert_str(
        module("unicodedata", "name", &[text("中")]),
        "CJK UNIFIED IDEOGRAPH-4E2D",
    );
    assert_str(
        module(
            "unicodedata",
            "lookup",
            &[text("cjk unified ideograph-4e00")],
        ),
        "\u{4E00}",
    );
    assert_str(
        module("unicodedata", "name", &[text("가")]),
        "HANGUL SYLLABLE GA",
    );
    assert_str(
        module("unicodedata", "lookup", &[text("Hangul Syllable GA")]),
        "가",
    );
    assert_str(
        module("unicodedata", "name", &[text("\u{378}"), text("fallback")]),
        "fallback",
    );
    module("unicodedata", "lookup", &[text("NO SUCH NAME")]);
    expect_error(KEY_ERROR);

    assert_float(module("unicodedata", "numeric", &[text("½")]), 0.5);
    assert_float(module("unicodedata", "numeric", &[text("一")]), 1.0);
    assert_float(module("unicodedata", "numeric", &[text("⅐")]), 1.0 / 7.0);
    assert_int(module("unicodedata", "digit", &[text("7")]), 7);
    assert_int(module("unicodedata", "decimal", &[text("7")]), 7);
    assert_str(
        module("unicodedata", "digit", &[text("½"), text("none")]),
        "none",
    );
    module("unicodedata", "decimal", &[text("½")]);
    expect_error(VALUE_ERROR);
}

#[test]
fn unicodedata_rejects_bad_arguments() {
    hydro_runtime_init();
    module("unicodedata", "category", &[text("ab")]);
    expect_error(TYPE_ERROR);
    module("unicodedata", "combining", &[int(1)]);
    expect_error(TYPE_ERROR);
    module("unicodedata", "normalize", &[text("NFC"), int(1)]);
    expect_error(TYPE_ERROR);
}
