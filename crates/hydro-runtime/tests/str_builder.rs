//! Runtime ABI tests for the string accumulator (`hydro_str_builder_*`).
//!
//! The frontend lowers a proved `s += ...` loop to these entry points, so they
//! must append in place, keep `str + value` semantics exactly, finish as a
//! normal `str` and keep the accumulator and its result alive across
//! collections.

use hydro_runtime::abi::{
    hydro_binop, hydro_exc_clear, hydro_exc_pending, hydro_exc_tag, hydro_exc_value,
    hydro_gc_collect, hydro_int, hydro_runtime_init, hydro_str_builder_append,
    hydro_str_builder_finish, hydro_str_builder_new, hydro_str_new, hydro_to_str, hydro_true,
};

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
    assert!(
        same(actual, text(expected)),
        "string mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

#[test]
fn builder_appends_and_finishes_as_a_string() {
    hydro_runtime_init();
    let builder = hydro_str_builder_new(text("a"));
    for _ in 0..2000 {
        let appended = hydro_str_builder_append(builder, text("xy"));
        assert_eq!(appended, builder);
        assert_no_error();
    }
    let result = hydro_str_builder_finish(builder);
    assert_str(result, &format!("a{}", "xy".repeat(2000)));
    // The finished value is a normal `str`: `len` sees every character.
    assert_str(
        hydro_binop(hydro_ir::BinOp::Add.code(), text(""), result),
        &format!("a{}", "xy".repeat(2000)),
    );
}

#[test]
fn empty_builder_finishes_as_the_empty_string() {
    hydro_runtime_init();
    let builder = hydro_str_builder_new(text(""));
    let result = hydro_str_builder_finish(builder);
    assert_str(result, "");
    // A second finish is idempotent (the generated code finishes once).
    assert_str(hydro_str_builder_finish(builder), "");
}

#[test]
fn finish_returns_the_seed_when_the_loop_never_runs() {
    hydro_runtime_init();
    let builder = hydro_str_builder_new(text("seed"));
    assert_str(hydro_str_builder_finish(builder), "seed");
}

#[test]
fn append_reports_the_same_type_error_as_string_addition() {
    hydro_runtime_init();
    // Reference: what `s + 1` raises today.
    let _ = hydro_binop(hydro_ir::BinOp::Add.code(), text("a"), hydro_int(1));
    assert_eq!(hydro_exc_pending(), 1, "str + int must raise");
    let generic_tag = hydro_exc_tag();
    let generic_message = hydro_to_str(hydro_exc_value());
    hydro_exc_clear();

    let builder = hydro_str_builder_new(text("a"));
    let appended = hydro_str_builder_append(builder, hydro_int(1));
    assert_eq!(appended, builder);
    assert_eq!(hydro_exc_pending(), 1, "appending an int must raise");
    assert_eq!(hydro_exc_tag(), generic_tag, "exception tag must match `+`");
    let appended_message = hydro_to_str(hydro_exc_value());
    hydro_exc_clear();
    // Compare the rendered messages with no pending exception in flight.
    assert!(
        same(appended_message, generic_message),
        "exception message must match `+`"
    );
}

#[test]
fn collections_keep_the_builder_and_its_result_alive() {
    hydro_runtime_init();
    let builder = hydro_str_builder_new(text("seed"));
    let mut expected = String::from("seed");
    // Interleave allocations and full collections with the appends so the
    // collector must keep the builder reachable between them.
    for index in 0..500 {
        let digits = index.to_string();
        expected.push_str(&digits);
        hydro_str_builder_append(builder, text(&digits));
        if index % 7 == 0 {
            hydro_gc_collect();
        }
    }
    hydro_gc_collect();
    let result = hydro_str_builder_finish(builder);
    hydro_gc_collect();
    assert_str(result, &expected);
}

#[test]
fn discarded_builders_are_reclaimed() {
    hydro_runtime_init();
    for index in 0..50 {
        let builder = hydro_str_builder_new(text("temporary"));
        hydro_str_builder_append(builder, text(&index.to_string()));
        let _ = hydro_str_builder_finish(builder);
    }
    hydro_gc_collect();
    // A fresh builder still works after the collector reclaimed the garbage.
    let builder = hydro_str_builder_new(text("ok"));
    hydro_str_builder_append(builder, text("!"));
    assert_str(hydro_str_builder_finish(builder), "ok!");
}
