//! Integration tests for the `dis` tail blockers: `ascii()`, `max`/`min`
//! `default=`, `argparse.ArgumentParser(color=...)` and
//! `collections.namedtuple`.
//!
//! The tests drive the public ABI exactly like generated code: direct module
//! calls use the positional slots the frontend fills, and keyword calls go
//! through `hydro_call_kw`/`hydro_method_kw`.

use hydro_runtime::abi::*;

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

const NONE: i64 = 1;
const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;

fn int(value: i64) -> i64 {
    hydro_int(value)
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn list(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
}

fn tuple(items: &[i64]) -> i64 {
    hydro_tuple_new(items.len(), items.as_ptr())
}

fn call(module: &str, function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn builtin(name: &str) -> i64 {
    hydro_builtin_function(name.as_ptr(), name.len())
}

fn call_kw(function: i64, args: &[i64], keywords: &[(&str, i64)]) -> i64 {
    let mut words: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
    for (name, value) in keywords {
        words.push(text(name));
        words.push(*value);
    }
    hydro_call_kw(
        function,
        args.len(),
        args.as_ptr(),
        keywords.len(),
        words.as_ptr(),
    )
}

fn method_kw(obj: i64, name: &str, keywords: &[(&str, i64)]) -> i64 {
    let mut words: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
    for (keyword, value) in keywords {
        words.push(text(keyword));
        words.push(*value);
    }
    hydro_method_kw(
        obj,
        name.as_ptr(),
        name.len(),
        0,
        std::ptr::null(),
        keywords.len(),
        words.as_ptr(),
    )
}

fn getattr(obj: i64, name: &str) -> i64 {
    hydro_getattr(obj, name.as_ptr(), name.len())
}

fn same(left: i64, right: i64) -> bool {
    call("operator", "eq", &[left, right]) == hydro_true()
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(actual, text(expected)),
        "string mismatch: expected {expected:?}, got {:?}",
        hydro_to_str(actual)
    );
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_to_int(actual), expected << 3);
}

fn assert_error(actual: i64, tag: i64, message: &str) {
    assert_eq!(actual, NONE, "a failing call must return None");
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), tag, "wrong exception tag");
    assert!(
        same(hydro_to_str(hydro_exc_value()), text(message)),
        "message mismatch: expected {message:?}, got {:?}",
        hydro_to_str(hydro_exc_value())
    );
    hydro_exc_clear();
}

// -------------------------------------------------------------------------
// ascii()
// -------------------------------------------------------------------------

#[test]
fn ascii_escapes_non_ascii_characters() {
    let _guard = test_guard();
    hydro_runtime_init();

    // Latin-1 range becomes `\xNN`.
    assert_str(call("builtins", "ascii", &[text("héllo")]), "'h\\xe9llo'");
    // Latin-1-exceeding BMP characters become `\uNNNN`.
    assert_str(call("builtins", "ascii", &[text("€")]), "'\\u20ac'");
    // Astral characters become `\UNNNNNNNN`.
    assert_str(call("builtins", "ascii", &[text("😀")]), "'\\U0001f600'");
    // ASCII is untouched and non-strings render like `repr`.
    assert_str(call("builtins", "ascii", &[text("plain")]), "'plain'");
    assert_str(call("builtins", "ascii", &[int(42)]), "42");
}

#[test]
fn ascii_is_available_as_a_first_class_builtin() {
    let _guard = test_guard();
    hydro_runtime_init();

    let ascii = builtin("ascii");
    assert_no_error();
    assert_str(hydro_call(ascii, 1, [text("é")].as_ptr()), "'\\xe9'");
    assert_error(
        hydro_call(ascii, 0, std::ptr::null()),
        TYPE_ERROR,
        "ascii() takes exactly 1 argument(s) (0 given)",
    );
}

// -------------------------------------------------------------------------
// max()/min() `default=`
// -------------------------------------------------------------------------

#[test]
fn max_and_min_default_applies_to_an_empty_iterable() {
    let _guard = test_guard();
    hydro_runtime_init();

    let empty = list(&[]);
    let max = builtin("max");
    let min = builtin("min");

    assert_int(call_kw(max, &[empty], &[("default", int(-1))]), -1);
    assert_int(call_kw(min, &[empty], &[("default", int(4))]), 4);

    // A non-empty iterable still wins over the default.
    let values = list(&[int(3), int(7), int(2)]);
    assert_int(call_kw(max, &[values], &[("default", int(0))]), 7);
}

#[test]
fn max_default_composes_with_key() {
    let _guard = test_guard();
    hydro_runtime_init();

    let values = list(&[int(-5), int(2)]);
    let key = builtin("abs");
    assert_int(
        call_kw(
            builtin("max"),
            &[values],
            &[("key", key), ("default", int(0))],
        ),
        -5,
    );
}

#[test]
fn max_default_requires_the_single_iterable_form() {
    let _guard = test_guard();
    hydro_runtime_init();

    let max = builtin("max");
    let result = call_kw(max, &[int(1), int(2)], &[("default", int(3))]);
    assert_error(
        result,
        TYPE_ERROR,
        "Cannot specify a default for max() with multiple positional arguments",
    );

    let min = builtin("min");
    let result = call_kw(min, &[int(1), int(2)], &[("default", int(3))]);
    assert_error(
        result,
        TYPE_ERROR,
        "Cannot specify a default for min() with multiple positional arguments",
    );
}

#[test]
fn empty_extremum_without_default_still_raises() {
    let _guard = test_guard();
    hydro_runtime_init();

    let empty = list(&[]);
    assert_error(
        hydro_call(builtin("max"), 1, [empty].as_ptr()),
        VALUE_ERROR,
        "max() arg is an empty sequence",
    );
    assert_error(
        hydro_call(builtin("min"), 1, [empty].as_ptr()),
        VALUE_ERROR,
        "min() arg is an empty sequence",
    );
}

// -------------------------------------------------------------------------
// argparse.ArgumentParser(color=...)
// -------------------------------------------------------------------------

#[test]
fn argument_parser_accepts_color_false() {
    let _guard = test_guard();
    hydro_runtime_init();

    // Generated code folds `color=False` into the fourteenth positional slot
    // (prog, usage, description, epilog, parents, formatter_class,
    // prefix_chars, fromfile_prefix_chars, argument_default,
    // conflict_handler, add_help, allow_abbrev, exit_on_error, color).
    let mut slots = [NONE; 14];
    slots[13] = hydro_false();
    let parser = call("argparse", "ArgumentParser", &slots);
    assert_no_error();
    assert_eq!(getattr(parser, "color"), hydro_false());
    assert_eq!(getattr(parser, "add_help"), hydro_true());

    let mut slots = [NONE; 14];
    slots[13] = hydro_true();
    let parser = call("argparse", "ArgumentParser", &slots);
    assert_no_error();
    assert_eq!(getattr(parser, "color"), hydro_true());
}

#[test]
fn argument_parser_color_defaults_to_true() {
    let _guard = test_guard();
    hydro_runtime_init();

    let parser = call("argparse", "ArgumentParser", &[]);
    assert_no_error();
    assert_eq!(getattr(parser, "color"), hydro_true());
}

// -------------------------------------------------------------------------
// collections.namedtuple
// -------------------------------------------------------------------------

fn point_class() -> i64 {
    let names = [text("x"), text("y")];
    let class = call("collections", "namedtuple", &[text("Point"), list(&names)]);
    assert_no_error();
    class
}

#[test]
fn namedtuple_instances_are_attributed_tuples() {
    let _guard = test_guard();
    hydro_runtime_init();

    let class = point_class();
    assert_str(getattr(class, "__name__"), "Point");
    assert_str(hydro_to_repr(getattr(class, "_fields")), "('x', 'y')");
    assert_str(hydro_to_repr(getattr(class, "_field_defaults")), "{}");

    let instance = hydro_call(class, 2, [int(1), int(2)].as_ptr());
    assert_no_error();
    assert_int(getattr(instance, "x"), 1);
    assert_int(getattr(instance, "y"), 2);
    assert_int(hydro_len(instance), 2);
    assert_int(hydro_subscript(instance, int(1)), 2);
    assert_int(hydro_subscript(instance, int(-1)), 2);
    assert_str(hydro_to_repr(instance), "Point(x=1, y=2)");

    // Tuple subclass semantics: equality with a plain tuple and iteration.
    assert_eq!(
        call("operator", "eq", &[instance, tuple(&[int(1), int(2)])]),
        hydro_true()
    );
    let iterator = hydro_iter_new(instance);
    let mut item = NONE;
    assert_eq!(hydro_iter_next(iterator, &mut item), 1);
    assert_int(item, 1);
    assert_eq!(hydro_iter_next(iterator, &mut item), 1);
    assert_int(item, 2);
    assert_eq!(hydro_iter_next(iterator, &mut item), 0);

    let tuple_class = hydro_call(builtin("type"), 1, [tuple(&[])].as_ptr());
    let is_tuple = hydro_call(builtin("isinstance"), 2, [instance, tuple_class].as_ptr());
    assert_eq!(is_tuple, hydro_true());
}

#[test]
fn namedtuple_keywords_and_defaults_are_stored() {
    let _guard = test_guard();
    hydro_runtime_init();

    let names = [text("a"), text("b")];
    let defaults = list(&[int(7)]);
    let class = call(
        "collections",
        "namedtuple",
        &[text("P"), list(&names), NONE, defaults],
    );
    assert_no_error();
    assert_str(hydro_to_repr(getattr(class, "_field_defaults")), "{'b': 7}");

    let instance = hydro_call(class, 1, [int(1)].as_ptr());
    assert_no_error();
    assert_int(getattr(instance, "a"), 1);
    assert_int(getattr(instance, "b"), 7);
    assert_str(hydro_to_repr(instance), "P(a=1, b=7)");

    // Keyword construction binds by field name.
    let mut words = [text("b"), int(5), text("a"), int(2)];
    let instance = hydro_call_kw(class, 0, std::ptr::null(), 2, words.as_mut_ptr());
    assert_no_error();
    assert_str(hydro_to_repr(instance), "P(a=2, b=5)");
}

#[test]
fn namedtuple_make_replace_and_asdict() {
    let _guard = test_guard();
    hydro_runtime_init();

    let class = point_class();

    let made = hydro_method(
        class,
        "_make".as_ptr(),
        5,
        1,
        [list(&[int(5), int(6)])].as_ptr(),
    );
    assert_no_error();
    assert_str(hydro_to_repr(made), "Point(x=5, y=6)");

    let made = hydro_method(class, "_make".as_ptr(), 5, 1, [list(&[int(5)])].as_ptr());
    assert_error(made, TYPE_ERROR, "Expected 2 arguments, got 1");

    let instance = hydro_call(class, 2, [int(1), int(2)].as_ptr());
    assert_no_error();
    assert_str(
        hydro_to_repr(hydro_method(
            instance,
            "_asdict".as_ptr(),
            7,
            0,
            std::ptr::null(),
        )),
        "{'x': 1, 'y': 2}",
    );

    let replaced = method_kw(instance, "_replace", &[("y", int(9))]);
    assert_no_error();
    assert_str(hydro_to_repr(replaced), "Point(x=1, y=9)");

    let replaced = method_kw(instance, "_replace", &[("z", int(9))]);
    assert_error(replaced, TYPE_ERROR, "Got unexpected field names: ['z']");
}

#[test]
fn namedtuple_subclasses_inherit_field_binding() {
    let _guard = test_guard();
    hydro_runtime_init();

    // `class _ExceptionTableEntry(_ExceptionTableEntryBase): pass` in dis.
    let class = point_class();
    let subclass = hydro_class_new(
        b"Sub".as_ptr(),
        3,
        class,
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_no_error();
    let instance = hydro_call(subclass, 2, [int(3), int(4)].as_ptr());
    assert_no_error();
    assert_int(getattr(instance, "x"), 3);
    assert_int(getattr(instance, "y"), 4);
    assert_str(hydro_to_repr(instance), "Sub(x=3, y=4)");
    assert_eq!(
        call("builtins", "isinstance", &[instance, class]),
        hydro_true()
    );
}

#[test]
fn namedtuple_field_descriptors_accept_doc_assignments() {
    let _guard = test_guard();
    hydro_runtime_init();

    // `_Instruction.opname.__doc__ = "..."` at dis import time.
    let class = point_class();
    let descriptor = getattr(class, "x");
    assert_no_error();
    assert_eq!(
        hydro_setattr(descriptor, "__doc__".as_ptr(), 7, text("the x field")),
        NONE
    );
    assert_no_error();
    assert_str(getattr(descriptor, "__doc__"), "the x field");

    // Instance attribute access still reads the stored field.
    let instance = hydro_call(class, 2, [int(11), int(12)].as_ptr());
    assert_no_error();
    assert_int(getattr(instance, "x"), 11);
}

#[test]
fn namedtuple_rejects_duplicate_field_names() {
    let _guard = test_guard();
    hydro_runtime_init();

    let names = [text("a"), text("a")];
    let class = call("collections", "namedtuple", &[text("X"), list(&names)]);
    assert_error(class, VALUE_ERROR, "Encountered duplicate field name: 'a'");
}
