//! Integration tests for the `ctypes`, `locale`, `sysconfig`, `zoneinfo` and
//! `decimal` native modules.
//!
//! The tests drive the public runtime ABI exactly like generated code: module
//! functions through `hydro_module_call`, constants through `hydro_module_get`,
//! classes through `hydro_call`, and attributes through `hydro_getattr`/
//! `hydro_setattr`. Every negative case pins the pending exception tag.

use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const ZERO_DIVISION_ERROR: i64 = 2;
const KEY_ERROR: i64 = 7;
const ATTRIBUTE_ERROR: i64 = 9;
const OS_ERROR: i64 = 10;
const NOT_IMPLEMENTED_ERROR: i64 = 32;

const OP_ADD: u32 = 0;
const OP_EQ: u32 = 7;
const OP_LT: u32 = 9;
const OP_GE: u32 = 12;
const OP_IN: u32 = 13;
const OP_IS: u32 = 15;

static SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn setup() -> MutexGuard<'static, ()> {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    hydro_runtime_init();
    guard
}

fn num(n: i64) -> i64 {
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn bytes(value: &[u8]) -> i64 {
    hydro_bytes_new(value.as_ptr(), value.len())
}

fn tuple(items: &[i64]) -> i64 {
    hydro_tuple_new(items.len(), items.as_ptr())
}

fn list(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
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

fn call(value: i64, args: &[i64]) -> i64 {
    hydro_call(
        value,
        args.len(),
        if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        },
    )
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(
        obj,
        name.as_ptr(),
        name.len(),
        args.len(),
        if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        },
    )
}

fn getattr(obj: i64, name: &str) -> i64 {
    hydro_getattr(obj, name.as_ptr(), name.len())
}

fn setattr(obj: i64, name: &str, value: i64) {
    hydro_setattr(obj, name.as_ptr(), name.len(), value);
}

fn index(container: i64, position: i64) -> i64 {
    hydro_subscript(container, num(position))
}

/// Subscript with an arbitrary key (strings, tuples); unlike [`index`] the key
/// is already a tagged value and must not be shifted.
fn item(container: i64, key: i64) -> i64 {
    hydro_subscript(container, key)
}

fn class_with_attrs(name: &str, base: i64, attrs: &[(&str, i64)]) -> i64 {
    let names: Vec<i64> = attrs.iter().map(|(name, _)| text(name)).collect();
    let values: Vec<i64> = attrs.iter().map(|(_, value)| *value).collect();
    hydro_class_new(
        name.as_ptr(),
        name.len(),
        base,
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        attrs.len(),
        if attrs.is_empty() {
            std::ptr::null()
        } else {
            names.as_ptr()
        },
        if attrs.is_empty() {
            std::ptr::null()
        } else {
            values.as_ptr()
        },
    )
}

/// `_fields_` value: a list of `(name, type)` (or `(name, type, bits)`) tuples.
fn field_list(entries: &[(&str, i64, Option<i64>)]) -> i64 {
    let tuples: Vec<i64> = entries
        .iter()
        .map(|(name, ty, bits)| match bits {
            Some(width) => tuple(&[text(name), *ty, num(*width)]),
            None => tuple(&[text(name), *ty]),
        })
        .collect();
    list(&tuples)
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
    assert_eq!(
        hydro_binop(OP_EQ, actual, num(expected)),
        hydro_true(),
        "expected {expected}, got {}",
        hydro_to_int(actual) >> 3
    );
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, actual, text(expected)),
        hydro_true(),
        "wrong string value"
    );
}

fn assert_bytes(actual: i64, expected: &[u8]) {
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, actual, bytes(expected)),
        hydro_true(),
        "wrong bytes value"
    );
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_true());
}

// -------------------------------------------------------------------------
// ctypes
// -------------------------------------------------------------------------

#[test]
fn ctypes_scalar_sizes_and_values() {
    let _guard = setup();
    for (name, size) in [
        ("c_bool", 1),
        ("c_byte", 1),
        ("c_char", 1),
        ("c_short", 2),
        ("c_int", 4),
        ("c_uint", 4),
        ("c_long", 8),
        ("c_longlong", 8),
        ("c_float", 4),
        ("c_double", 8),
        ("c_void_p", 8),
        ("c_size_t", 8),
    ] {
        let ty = module_get("ctypes", name);
        assert_no_error();
        assert_int(module("ctypes", "sizeof", &[ty]), size);
        assert_int(module("ctypes", "alignment", &[ty]), size);
    }
    assert_int(
        module("ctypes", "sizeof", &[module_get("ctypes", "c_longdouble")]),
        16,
    );

    let c_int = module_get("ctypes", "c_int");
    let value = call(c_int, &[num(41)]);
    assert_int(getattr(value, "value"), 41);
    setattr(value, "value", num(-7));
    assert_int(getattr(value, "value"), -7);
    assert_int(module("ctypes", "sizeof", &[value]), 4);

    // `c_uint` truncates negatives like CPython.
    let c_uint = module_get("ctypes", "c_uint");
    let wrapped = call(c_uint, &[num(-1)]);
    assert_int(getattr(wrapped, "value"), 4_294_967_295);

    let c_void_p = module_get("ctypes", "c_void_p");
    let null = call(c_void_p, &[hydro_none()]);
    assert_eq!(
        hydro_binop(OP_IS, getattr(null, "value"), hydro_none()),
        hydro_true()
    );

    // Aliases share the class object.
    assert_eq!(
        hydro_binop(
            OP_IS,
            module_get("ctypes", "c_int8"),
            module_get("ctypes", "c_byte")
        ),
        hydro_true()
    );
}

#[test]
fn ctypes_structures_unions_and_arrays() {
    let _guard = setup();
    let c_int = module_get("ctypes", "c_int");
    let c_long = module_get("ctypes", "c_long");

    let structure = module_get("ctypes", "Structure");
    let fields = field_list(&[("x", c_int, None), ("y", c_int, None)]);
    let point = class_with_attrs("Point", structure, &[("_fields_", fields)]);
    assert_int(module("ctypes", "sizeof", &[point]), 8);
    assert_int(module("ctypes", "alignment", &[point]), 4);
    assert_int(module("ctypes", "offsetof", &[point, text("y")]), 4);

    let instance = call(point, &[num(1), num(2)]);
    assert_int(getattr(getattr(instance, "x"), "value"), 1);
    assert_int(getattr(getattr(instance, "y"), "value"), 2);
    assert_int(module("ctypes", "sizeof", &[instance]), 8);

    // Field views alias the structure storage.
    setattr(getattr(instance, "x"), "value", num(9));
    assert_int(getattr(getattr(instance, "x"), "value"), 9);
    assert_int(getattr(getattr(instance, "y"), "value"), 2);

    let union = module_get("ctypes", "Union");
    let union_fields = field_list(&[("a", c_int, None), ("b", c_long, None)]);
    let union_type = class_with_attrs("Value", union, &[("_fields_", union_fields)]);
    assert_int(module("ctypes", "sizeof", &[union_type]), 8);
    assert_int(module("ctypes", "offsetof", &[union_type, text("a")]), 0);
    assert_int(module("ctypes", "offsetof", &[union_type, text("b")]), 0);

    let array = module_get("ctypes", "Array");
    let array_type = class_with_attrs(
        "IntArray",
        array,
        &[("_type_", c_int), ("_length_", num(4))],
    );
    assert_int(module("ctypes", "sizeof", &[array_type]), 16);
    let values = list(&[num(10), num(20), num(30), num(40)]);
    let buffer = call(array_type, &[values]);
    assert_int(hydro_len(buffer), 4);
    assert_int(getattr(index(buffer, 2), "value"), 30);
    setattr(index(buffer, 2), "value", num(31));
    assert_int(getattr(index(buffer, 2), "value"), 31);

    // Bitfield layout is supported; bitfield values are explicitly not.
    let c_ubyte = module_get("ctypes", "c_ubyte");
    let bitfields = field_list(&[("low", c_ubyte, Some(3)), ("high", c_ubyte, Some(5))]);
    let bits = class_with_attrs("Bits", structure, &[("_fields_", bitfields)]);
    assert_int(module("ctypes", "sizeof", &[bits]), 1);
    let bit_instance = call(bits, &[]);
    let _ = getattr(getattr(bit_instance, "low"), "value");
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
}

#[test]
fn ctypes_pointers_buffers_and_memory() {
    let _guard = setup();
    let c_int = module_get("ctypes", "c_int");
    let value = call(c_int, &[num(123)]);
    let address = module("ctypes", "addressof", &[value]);
    assert_int(module("ctypes", "sizeof", &[value]), 4);

    let pointer_type = module("ctypes", "POINTER", &[c_int]);
    assert_no_error();
    let pointer = call(pointer_type, &[value]);
    let contents = getattr(pointer, "contents");
    assert_int(getattr(contents, "value"), 123);
    setattr(contents, "value", num(77));
    assert_int(getattr(value, "value"), 77);

    // cast() from the raw address and byref() address the same storage.
    let recast = module("ctypes", "cast", &[address, pointer_type]);
    assert_int(getattr(getattr(recast, "contents"), "value"), 77);
    let reference = module("ctypes", "byref", &[value]);
    assert_int(getattr(getattr(reference, "contents"), "value"), 77);

    let buffer = module("ctypes", "create_string_buffer", &[bytes(b"abc")]);
    assert_bytes(getattr(buffer, "value"), b"abc");
    assert_bytes(getattr(buffer, "raw"), b"abc\0");
    assert_bytes(index(buffer, 0), b"a");
    assert_int(hydro_len(buffer), 4);

    let buffer_address = module("ctypes", "addressof", &[buffer]);
    assert_bytes(
        module("ctypes", "string_at", &[buffer_address, num(3)]),
        b"abc",
    );

    let destination = module("ctypes", "create_string_buffer", &[bytes(b"xxxx")]);
    let source = module("ctypes", "create_string_buffer", &[bytes(b"yy")]);
    module("ctypes", "memmove", &[destination, source, num(2)]);
    assert_bytes(getattr(destination, "value"), b"yyxx");
    module(
        "ctypes",
        "memset",
        &[destination, num(i64::from(b'z')), num(4)],
    );
    assert_bytes(getattr(destination, "value"), b"zzzz");

    // Out-of-bounds requests are rejected instead of touching memory.
    let _ = module("ctypes", "string_at", &[buffer_address, num(1_000_000)]);
    assert_error_tag(VALUE_ERROR);

    // Wrong value types and unsupported targets report tagged errors.
    let _ = module("ctypes", "sizeof", &[num(1)]);
    assert_error_tag(TYPE_ERROR);
    let _ = module("ctypes", "cast", &[address, num(1)]);
    assert_error_tag(TYPE_ERROR);
    let _ = module_get("ctypes", "no_such_ctypes_member");
    assert_error_tag(ATTRIBUTE_ERROR);
}

#[test]
fn ctypes_cdll_and_callbacks() {
    let _guard = setup();
    let cdll = module_get("ctypes", "CDLL");
    assert_no_error();

    let _ = call(cdll, &[text("libhydro-no-such-library.so")]);
    assert_error_tag(OS_ERROR);

    // Loading libc is environment dependent; when the library and symbol are
    // available, call `strlen` through the real C ABI.
    let library = call(cdll, &[text("libc.so.6")]);
    if hydro_exc_pending() != 0 {
        hydro_exc_clear();
    } else {
        let strlen = getattr(library, "strlen");
        if hydro_exc_pending() == 0 {
            assert_int(call(strlen, &[bytes(b"hello")]), 5);
        } else {
            hydro_exc_clear();
        }
    }

    // CFUNCTYPE installs a real trampoline and can be called from Python.
    let c_int = module_get("ctypes", "c_int");
    let prototype = call(module_get("ctypes", "CFUNCTYPE"), &[hydro_none(), c_int]);
    assert_no_error();
    let absolute = hydro_builtin_function("abs".as_ptr(), 3);
    let callback = call(prototype, &[absolute]);
    assert_no_error();
    let _ = call(callback, &[num(-5)]);
    assert_no_error();
}

// -------------------------------------------------------------------------
// locale
// -------------------------------------------------------------------------

#[test]
fn locale_settings_and_helpers() {
    let _guard = setup();
    let all = module_get("locale", "LC_ALL");
    let ctype = module_get("locale", "LC_CTYPE");
    assert_no_error();

    let current = module("locale", "setlocale", &[all, hydro_none()]);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, current, hydro_none()),
        hydro_false(),
        "setlocale(category) must return the active locale string"
    );

    let locale = module("locale", "getlocale", &[ctype]);
    assert_int(hydro_len(locale), 2);

    let encoding = module("locale", "getpreferredencoding", &[]);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, encoding, hydro_none()),
        hydro_false(),
        "getpreferredencoding() must return a string"
    );

    assert_str(
        module("locale", "normalize", &[text("en_us.utf8")]),
        "en_US.UTF-8",
    );
    let domain = module("locale", "textdomain", &[]);
    assert_eq!(hydro_binop(OP_IS, domain, hydro_none()), hydro_true());
    assert_str(
        module("locale", "gettext", &[text("unchanged")]),
        "unchanged",
    );

    let _ = module(
        "locale",
        "setlocale",
        &[all, text("hydro_definitely_not_a_locale")],
    );
    assert_error_tag(VALUE_ERROR);

    let _ = module("locale", "getlocale", &[num(999)]);
    assert_error_tag(VALUE_ERROR);

    // `Error` is exposed as an exception class.
    let error_class = module_get("locale", "Error");
    assert_no_error();
    assert_eq!(hydro_binop(OP_IS, error_class, hydro_none()), hydro_false());
}

// -------------------------------------------------------------------------
// sysconfig
// -------------------------------------------------------------------------

#[test]
fn sysconfig_reports_the_emulated_installation() {
    let _guard = setup();
    assert_str(module("sysconfig", "get_python_version", &[]), "3.14");
    let platform = module("sysconfig", "get_platform", &[]);
    assert_no_error();
    assert_true(hydro_binop(OP_IN, text("linux"), platform));

    assert_str(
        module("sysconfig", "get_config_var", &[text("py_version_short")]),
        "3.14",
    );
    assert_str(
        module("sysconfig", "get_config_var", &[text("py_version_nodot")]),
        "314",
    );
    let unknown = module(
        "sysconfig",
        "get_config_var",
        &[text("hydro_unknown_probe")],
    );
    assert_eq!(hydro_binop(OP_IS, unknown, hydro_none()), hydro_true());

    let paths = module("sysconfig", "get_paths", &[]);
    assert_no_error();
    let stdlib = item(paths, text("stdlib"));
    assert_no_error();
    assert_true(hydro_binop(OP_IN, text("python3.14"), stdlib));
    let scripts = module("sysconfig", "get_path", &[text("scripts")]);
    assert_no_error();
    assert_true(hydro_binop(OP_IN, text("/bin"), scripts));
    let purelib = item(paths, text("purelib"));
    assert_true(hydro_binop(OP_IN, text("site-packages"), purelib));

    let variables = module("sysconfig", "get_config_vars", &[]);
    assert_no_error();
    let soabi = item(variables, text("SOABI"));
    assert_eq!(hydro_binop(OP_IS, soabi, hydro_none()), hydro_false());
    let scheme_names = module("sysconfig", "get_scheme_names", &[]);
    assert_int(hydro_len(scheme_names), 2);
}

// -------------------------------------------------------------------------
// zoneinfo
// -------------------------------------------------------------------------

#[test]
fn zoneinfo_reads_the_system_database() {
    let _guard = setup();
    let zone_info = module_get("zoneinfo", "ZoneInfo");
    let datetime = module_get("datetime", "datetime");
    let now = method(datetime, "now", &[]);
    assert_no_error();

    let utc = call(zone_info, &[text("UTC")]);
    assert_str(getattr(utc, "key"), "UTC");
    assert_int(method(utc, "utcoffset", &[now]), 0);
    assert_str(method(utc, "tzname", &[now]), "UTC");

    let new_york = call(zone_info, &[text("America/New_York")]);
    assert_str(getattr(new_york, "key"), "America/New_York");
    let offset = method(new_york, "utcoffset", &[now]);
    assert_no_error();
    // EST (-5h) or EDT (-4h).
    assert_true(hydro_binop(OP_GE, offset, num(-18_000)));
    assert_true(hydro_binop(OP_LT, offset, num(-14_000)));

    let zones = module("zoneinfo", "available_timezones", &[]);
    assert_no_error();
    assert_true(hydro_binop(OP_IN, text("UTC"), zones));
    assert_true(hydro_binop(OP_IN, text("America/New_York"), zones));

    let path = module_get("zoneinfo", "TZPATH");
    assert_no_error();
    assert_true(hydro_binop(OP_GE, hydro_len(path), num(1)));

    let _ = call(zone_info, &[text("Hydro/Nowhere")]);
    assert_error_tag(KEY_ERROR);

    assert_eq!(
        hydro_binop(
            OP_IS,
            module_get("zoneinfo", "ZoneInfoNotFoundError"),
            hydro_none(),
        ),
        hydro_false()
    );
}

// -------------------------------------------------------------------------
// decimal
// -------------------------------------------------------------------------

#[test]
fn decimal_exact_arithmetic_and_formatting() {
    let _guard = setup();
    let decimal = module_get("decimal", "Decimal");

    let tenth = call(decimal, &[text("0.1")]);
    let fifth = call(decimal, &[text("0.2")]);
    let third = call(decimal, &[text("0.3")]);
    let sum = hydro_binop(OP_ADD, tenth, fifth);
    assert_no_error();
    assert_true(hydro_binop(OP_EQ, sum, third));
    assert_str(hydro_to_str(sum), "0.3");

    assert_true(hydro_binop(OP_LT, tenth, fifth));
    assert_str(hydro_to_str(call(decimal, &[num(42)])), "42");
    assert_str(hydro_to_str(call(decimal, &[text("1E+2")])), "1E+2");
    assert_str(hydro_to_str(call(decimal, &[text("1.50")])), "1.50");

    // as_tuple: (sign, digits, exponent)
    let value = method(tenth, "as_tuple", &[]);
    assert_no_error();
    assert_int(index(value, 0), 0);
    let digits = index(value, 1);
    assert_int(hydro_len(digits), 1);
    assert_int(index(digits, 0), 1);
    assert_int(index(value, 2), -1);

    // Method-form arithmetic (the runtime dispatches only `+` for classes).
    let product = method(third, "multiply", &[call(decimal, &[num(3)])]);
    assert_str(hydro_to_str(product), "0.9");
    let quotient = method(
        call(decimal, &[num(1)]),
        "divide",
        &[call(decimal, &[num(4)])],
    );
    assert_str(hydro_to_str(quotient), "0.25");
    let difference = method(
        call(decimal, &[num(5)]),
        "subtract",
        &[call(decimal, &[num(2)])],
    );
    assert_str(hydro_to_str(difference), "3");

    let rounded = method(
        call(decimal, &[text("1.2345")]),
        "quantize",
        &[call(decimal, &[text("0.01")])],
    );
    assert_str(hydro_to_str(rounded), "1.23");

    assert_true(method(call(decimal, &[text("NaN")]), "is_nan", &[]));
    assert_true(method(
        call(decimal, &[text("Infinity")]),
        "is_infinite",
        &[],
    ));
    assert_true(method(call(decimal, &[num(0)]), "is_zero", &[]));

    // Context precision drives division rounding.
    let context = module("decimal", "getcontext", &[]);
    assert_no_error();
    assert_int(getattr(context, "prec"), 28);
    setattr(context, "prec", num(4));
    let repeating = method(
        call(decimal, &[num(1)]),
        "divide",
        &[call(decimal, &[num(3)])],
    );
    assert_str(hydro_to_str(repeating), "0.3333");
    setattr(context, "prec", num(28));

    let _ = call(decimal, &[text("not-a-number")]);
    assert_error_tag(VALUE_ERROR);

    let _ = method(
        call(decimal, &[num(1)]),
        "divide",
        &[call(decimal, &[num(0)])],
    );
    assert_error_tag(ZERO_DIVISION_ERROR);
}

#[test]
fn decimal_and_ctypes_are_shared_with_their_accelerator_names() {
    let _guard = setup();
    assert_eq!(
        hydro_binop(
            OP_IS,
            module_get("decimal", "Decimal"),
            module_get("_decimal", "Decimal"),
        ),
        hydro_true()
    );
    assert_eq!(
        hydro_binop(
            OP_IS,
            module_get("ctypes", "c_int"),
            module_get("_ctypes", "c_int"),
        ),
        hydro_true()
    );
    assert_no_error();

    // Error classes are exposed for `except` compatibility.
    assert_eq!(
        hydro_binop(
            OP_IS,
            module_get("_decimal", "InvalidOperation"),
            hydro_none(),
        ),
        hydro_false()
    );
    assert_no_error();
}
