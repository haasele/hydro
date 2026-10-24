#![allow(clippy::approx_constant)]
//! Integration tests for Python-compatible formatting through the public ABI:
//! `pyx_format_value` (f-string fields), `str.format` and `%` formatting.
//!
//! Expected strings were taken from CPython 3.12.

use pyx_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const INDEX_ERROR: i64 = 6;
const OP_MOD: u32 = 5;
const OP_EQ: u32 = 7;

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
}

fn int(number: i64) -> i64 {
    number << 3
}

fn tuple(items: &[i64]) -> i64 {
    pyx_tuple_new(items.len(), items.as_ptr())
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    pyx_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn format_field(value: i64, spec: &str, conversion: u8) -> i64 {
    pyx_format_value(value, spec.as_ptr(), spec.len(), conversion)
}

fn assert_no_error() {
    assert_eq!(pyx_exc_pending(), 0, "unexpected pending exception");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    let equal = pyx_binop(OP_EQ, pyx_to_str(actual), text(expected));
    assert_eq!(equal, pyx_true(), "value mismatch, got {actual}");
}

fn assert_error(tag: i64) {
    assert_ne!(pyx_exc_pending(), 0, "expected a pending exception");
    assert_eq!(pyx_exc_tag(), tag, "unexpected exception tag");
    pyx_exc_clear();
}

#[test]
fn format_value_matches_cpython() {
    pyx_exc_clear();
    assert_str(format_field(int(3), "04d", 0), "0003");
    assert_str(format_field(int(-42), "08d", 0), "-0000042");
    assert_str(format_field(int(255), "#x", 0), "0xff");
    assert_str(format_field(int(1234567), ",d", 0), "1,234,567");
    assert_str(format_field(pyx_float_new(3.14159), ".2f", 0), "3.14");
    assert_str(format_field(pyx_float_new(3.5), ".1%", 0), "350.0%");
    assert_str(format_field(text("abc"), ">5", 0), "  abc");
    assert_str(format_field(text("abc"), ".2", 0), "ab");
    assert_str(format_field(pyx_true(), "", 0), "True");
    assert_str(format_field(pyx_none(), "", 0), "None");
    assert_str(format_field(pyx_true(), ">5", 0), "    1");
}

#[test]
fn format_value_conversions() {
    pyx_exc_clear();
    assert_str(format_field(text("x"), "", 2), "'x'");
    assert_str(format_field(text("x"), ">5", 2), "  'x'");
    assert_str(format_field(text("é"), "", 3), "'\\xe9'");
    assert_str(format_field(pyx_float_new(2.5), "", 1), "2.5");
}

#[test]
fn format_value_rejects_invalid_combinations() {
    pyx_exc_clear();
    assert_str(format_field(text("abc"), "", 0), "abc");
    let _ = format_field(text("abc"), "d", 0);
    assert_error(VALUE_ERROR);
    let items = [int(1)];
    let _ = format_field(pyx_list_new(items.len(), items.as_ptr()), ">5", 0);
    assert_error(TYPE_ERROR);
    let _ = format_field(pyx_float_new(1.5), "d", 0);
    assert_error(VALUE_ERROR);
}

#[test]
fn str_format_method_through_the_abi() {
    pyx_exc_clear();
    let template = text("{0} {1} {1:.1f}");
    let result = method(template, "format", &[text("a"), pyx_float_new(2.25)]);
    assert_str(result, "a 2.25 2.2");

    let result = method(text("{{x}} {}"), "format", &[int(7)]);
    assert_str(result, "{x} 7");

    let result = method(text("{!r}"), "format", &[text("x")]);
    assert_str(result, "'x'");

    let _ = method(text("{name}"), "format", &[int(1)]);
    assert_error(VALUE_ERROR);

    let _ = method(text("{}"), "format", &[]);
    assert_error(INDEX_ERROR);
}

#[test]
fn percent_formatting_through_the_abi() {
    pyx_exc_clear();
    let result = pyx_binop(OP_MOD, text("%s:%d"), tuple(&[text("x"), int(3)]));
    assert_str(result, "x:3");

    let result = pyx_binop(OP_MOD, text("%05.2f"), pyx_float_new(3.14159));
    assert_str(result, "03.14");

    let result = pyx_binop(OP_MOD, text("%%"), tuple(&[]));
    assert_str(result, "%");

    // A non-string left operand keeps numeric modulo semantics.
    assert_eq!(pyx_binop(OP_MOD, int(7), int(3)), int(1));
    assert_no_error();

    let _ = pyx_binop(OP_MOD, text("%s %s"), tuple(&[text("a")]));
    assert_error(TYPE_ERROR);

    let _ = pyx_binop(OP_MOD, text("%s"), tuple(&[int(1), int(2)]));
    assert_error(TYPE_ERROR);

    let _ = pyx_binop(OP_MOD, text("%d"), text("x"));
    assert_error(TYPE_ERROR);
}
