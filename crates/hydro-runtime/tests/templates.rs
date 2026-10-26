//! Runtime ABI tests for PEP 750 template strings.
//!
//! Generated code builds each interpolation through `hydro_interpolation_new`
//! and the template through `hydro_template_new`. These tests pin the value
//! surface (`str`, `repr`, `strings`, `interpolations`, `values`,
//! `Interpolation.convert`), conversion/format-spec behaviour, the
//! self-documenting flag, GC tracing of template children and the no-panic
//! contract for malformed buffers.

use hydro_runtime::abi::{
    hydro_binop, hydro_exc_clear, hydro_exc_pending, hydro_false, hydro_gc_collect, hydro_getattr,
    hydro_int, hydro_interpolation_new, hydro_len, hydro_method, hydro_none, hydro_runtime_init,
    hydro_str_new, hydro_subscript, hydro_template_new, hydro_to_repr, hydro_to_str, hydro_true,
};

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn same(a: i64, b: i64) -> bool {
    hydro_binop(hydro_ir::BinOp::Eq.code(), a, b) == hydro_true()
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    let rendered = hydro_to_str(actual);
    assert_no_error();
    assert!(
        same(rendered, text(expected)),
        "string mismatch, got {:?}",
        hydro_to_str(hydro_to_repr(actual))
    );
}

fn attr(value: i64, name: &str) -> i64 {
    let attribute = hydro_getattr(value, name.as_ptr(), name.len());
    assert_no_error();
    attribute
}

fn tuple_len(value: i64) -> usize {
    let len = hydro_len(value);
    assert_no_error();
    assert!(len >= 0, "negative tuple length");
    (len >> 3) as usize
}

/// Builds an interpolation with an already-evaluated spec (or `None`).
fn interpolation(value: i64, expression: &str, conversion: u8, spec: i64) -> i64 {
    let result = hydro_interpolation_new(
        value,
        expression.as_ptr(),
        expression.len(),
        conversion,
        spec,
        0,
    );
    assert_no_error();
    result
}

/// Builds a template from literal parts and interpolation values.
fn template(parts: &[&str], interpolations: &[i64]) -> i64 {
    let strings: Vec<i64> = parts.iter().map(|part| text(part)).collect();
    let result = hydro_template_new(
        strings.len(),
        strings.as_ptr(),
        interpolations.len(),
        interpolations.as_ptr(),
    );
    assert_no_error();
    result
}

#[test]
fn simple_template_str_and_repr() {
    let _guard = test_guard();
    hydro_runtime_init();
    let value = hydro_int(42);
    let interp = interpolation(value, "x", 0, hydro_none());
    let template = template(&["answer: ", ""], &[interp]);
    assert_str(template, "answer: 42");
    let repr = hydro_to_repr(template);
    assert_str(
        repr,
        "Template(strings=('answer: ', ''), interpolations=(Interpolation('x', 42, None, None),))",
    );
    let repr = hydro_to_repr(interp);
    assert_str(repr, "Interpolation('x', 42, None, None)");
    assert_str(interp, "42");
}

#[test]
fn conversions_match_cpython() {
    let _guard = test_guard();
    hydro_runtime_init();
    // !r quotes a str, !s is plain text, !a escapes non-ASCII.
    let repr_interp = interpolation(text("hi"), "x", 2, hydro_none());
    assert_str(repr_interp, "'hi'");
    let str_interp = interpolation(text("hi"), "x", 1, hydro_none());
    assert_str(str_interp, "hi");
    let ascii_interp = interpolation(text("héllo"), "x", 3, hydro_none());
    assert_str(ascii_interp, "'h\\xe9llo'");

    let template = template(&["", "", "", ""], &[repr_interp, str_interp, ascii_interp]);
    assert_str(template, "'hi'hi'h\\xe9llo'");
}

#[test]
fn format_spec_is_applied() {
    let _guard = test_guard();
    hydro_runtime_init();
    let spec = text("04d");
    let interp = interpolation(hydro_int(42), "x", 0, spec);
    assert_str(interp, "0042");
    let right = interpolation(text("ab"), "x", 0, text(">6"));
    assert_str(right, "    ab");
    let template = template(&["<", ">"], &[interp]);
    assert_str(template, "<0042>");
}

#[test]
fn self_documenting_flag_is_exposed() {
    let _guard = test_guard();
    hydro_runtime_init();
    // The parser appends `x=` to the literal part and defaults the conversion
    // to `!r`; the runtime only has to keep the flag and the conversion.
    let interp = hydro_interpolation_new(hydro_int(5), b"x".as_ptr(), 1, 2, hydro_none(), 1);
    assert_no_error();
    assert_eq!(attr(interp, "self_documenting"), hydro_true());
    let template = template(&["x=", ""], &[interp]);
    assert_str(template, "x=5");
}

#[test]
fn attributes_expose_structure() {
    let _guard = test_guard();
    hydro_runtime_init();
    let first = interpolation(hydro_int(1), "a", 0, hydro_none());
    let second = interpolation(text("b"), "b", 2, hydro_none());
    let template = template(&["s", "m", "e"], &[first, second]);

    let strings = attr(template, "strings");
    assert_eq!(tuple_len(strings), 3);
    assert_str(hydro_subscript(strings, hydro_int(0)), "s");
    assert_str(hydro_subscript(strings, hydro_int(1)), "m");
    assert_str(hydro_subscript(strings, hydro_int(2)), "e");

    let interpolations = attr(template, "interpolations");
    assert_eq!(tuple_len(interpolations), 2);
    let values = attr(template, "values");
    assert_eq!(tuple_len(values), 2);
    assert_eq!(hydro_subscript(values, hydro_int(0)), hydro_int(1));
    assert_str(hydro_subscript(values, hydro_int(1)), "b");

    assert_str(attr(first, "expression"), "a");
    assert_str(attr(second, "conversion"), "r");
    assert_eq!(attr(first, "conversion"), hydro_none());
    assert_eq!(attr(first, "format_spec"), hydro_none());
    assert_eq!(attr(first, "value"), hydro_int(1));
    assert_eq!(attr(first, "self_documenting"), hydro_false());
}

#[test]
fn convert_applies_conversion_and_spec() {
    let _guard = test_guard();
    hydro_runtime_init();
    let interp = interpolation(hydro_int(7), "x", 0, text("03d"));
    let converted = hydro_method(interp, b"convert".as_ptr(), 7, 0, std::ptr::null());
    assert_no_error();
    assert_str(converted, "007");
}

#[test]
fn template_children_survive_collection() {
    let _guard = test_guard();
    hydro_runtime_init();
    let interp = interpolation(text("world"), "name", 0, hydro_none());
    let template = template(&["hello ", ""], &[interp]);
    hydro_gc_collect();
    // Both the literal string and the interpolation value are traced through
    // the template, so they survive even though their temporaries are gone.
    assert_str(template, "hello world");
    let strings = attr(template, "strings");
    assert_str(hydro_subscript(strings, hydro_int(0)), "hello ");
    let interpolations = attr(template, "interpolations");
    assert_eq!(tuple_len(interpolations), 1);
    assert_str(attr(interp, "value"), "world");
}

#[test]
fn malformed_templates_raise_without_panicking() {
    let _guard = test_guard();
    hydro_runtime_init();
    // No strings for zero interpolations violates the invariant.
    let result = hydro_template_new(0, std::ptr::null(), 0, std::ptr::null());
    assert_eq!(result, hydro_none());
    assert_ne!(hydro_exc_pending(), 0);
    hydro_exc_clear();

    // Null expression buffer with a nonzero length.
    let result = hydro_interpolation_new(hydro_none(), std::ptr::null(), 4, 0, hydro_none(), 0);
    assert_eq!(result, hydro_none());
    assert_ne!(hydro_exc_pending(), 0);
    hydro_exc_clear();

    // A template literal part must be a str.
    let bad = [hydro_int(1)];
    let result = hydro_template_new(1, bad.as_ptr(), 0, std::ptr::null());
    assert_eq!(result, hydro_none());
    assert_ne!(hydro_exc_pending(), 0);
    hydro_exc_clear();

    // Invalid conversion tag.
    let result = hydro_interpolation_new(hydro_none(), b"x".as_ptr(), 1, 9, hydro_none(), 0);
    assert_eq!(result, hydro_none());
    assert_ne!(hydro_exc_pending(), 0);
    hydro_exc_clear();
}
