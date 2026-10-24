//! Integration tests for the class/instance runtime: construction through
//! `pyx_class_new`, instantiation and `__init__` binding, instance attributes
//! through `pyx_getattr`/`pyx_setattr`, method dispatch through `pyx_method`,
//! inheritance/overrides, deterministic reprs and collector survival.
//!
//! Every test drives the runtime through the public ABI.

use pyx_runtime::abi::*;

// Exception tags, fixed by ARCHITECTURE §3.2.
const TYPE_ERROR: i64 = 1;
const ATTRIBUTE_ERROR: i64 = 9;
// `operator.eq` uses the comparison ABI's `OP_EQ`.
const OP_EQ: u32 = 7;
const OP_ADD: u32 = 0;

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
}

fn int(value: i64) -> i64 {
    // `pyx_int` tags a raw literal (these values are already tagged by the
    // ABI; the helper stays explicit about that).
    pyx_int(value)
}

fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    pyx_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len())
}

fn class(name: &str, base: i64, methods: &[(&str, i64)]) -> i64 {
    let names: Vec<i64> = methods.iter().map(|(name, _)| text(name)).collect();
    let funcs: Vec<i64> = methods.iter().map(|(_, func)| *func).collect();
    let (names_ptr, funcs_ptr) = if methods.is_empty() {
        (std::ptr::null(), std::ptr::null())
    } else {
        (names.as_ptr(), funcs.as_ptr())
    };
    pyx_class_new(
        name.as_ptr(),
        name.len(),
        base,
        methods.len(),
        names_ptr,
        funcs_ptr,
    )
}

fn call_class(class: i64, args: &[i64]) -> i64 {
    pyx_call(
        class,
        args.len(),
        if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        },
    )
}

fn getattr(obj: i64, name: &str) -> i64 {
    pyx_getattr(obj, name.as_ptr(), name.len())
}

fn setattr(obj: i64, name: &str, value: i64) -> i64 {
    pyx_setattr(obj, name.as_ptr(), name.len(), value)
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    pyx_method(
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

fn same(a: i64, b: i64) -> bool {
    pyx_binop(OP_EQ, a, b) == pyx_true()
}

fn assert_no_error() {
    assert_eq!(pyx_exc_pending(), 0, "unexpected pending exception");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(actual, text(expected)),
        "value mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert!(
        same(actual, int(expected)),
        "value mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn assert_error_tag(expected: i64) {
    assert_eq!(pyx_exc_pending(), 1, "expected a pending exception");
    assert_eq!(pyx_exc_tag(), expected);
    pyx_exc_clear();
}

fn assert_error_message(expected: &str) {
    assert_eq!(pyx_exc_pending(), 1, "expected a pending exception");
    let message = pyx_to_str(pyx_exc_value());
    assert!(
        same(message, text(expected)),
        "message mismatch, got {:?}",
        message
    );
    pyx_exc_clear();
}

// -------------------------------------------------------------------------
// Method bodies used by the tests
// -------------------------------------------------------------------------

/// `__init__(self, value)`: stores `value` in `self.x`.
extern "C" fn init_x(argv: *const i64) -> i64 {
    // SAFETY: the runtime checks the function arity before calling.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    setattr(args[0], "x", args[1])
}

/// `get(self)`: returns `self.x`.
extern "C" fn get_x(argv: *const i64) -> i64 {
    // SAFETY: arity 1 (`self`).
    let args = unsafe { std::slice::from_raw_parts(argv, 1) };
    getattr(args[0], "x")
}

/// `offset(self, n)`: returns `self.x + n`.
extern "C" fn offset_x(argv: *const i64) -> i64 {
    // SAFETY: arity 2 (`self`, `n`).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let x = getattr(args[0], "x");
    pyx_binop(OP_ADD, x, args[1])
}

/// `set_x(self, value)`: overwrites `self.x`.
extern "C" fn set_x(argv: *const i64) -> i64 {
    // SAFETY: arity 2.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    setattr(args[0], "x", args[1])
}

/// `identity(self)`: returns the bound instance.
extern "C" fn identity(argv: *const i64) -> i64 {
    // SAFETY: arity 1 (`self`).
    unsafe { *argv }
}

/// `__exit__(self, exc_type, exc_value, traceback)`: records the call and
/// returns `True`. The `with` lowering ignores the return value, so returning
/// `True` must not suppress an exception.
extern "C" fn exit_marks(argv: *const i64) -> i64 {
    // SAFETY: arity 4 (`self` plus three exception arguments).
    let args = unsafe { std::slice::from_raw_parts(argv, 4) };
    setattr(args[0], "exited", pyx_true());
    pyx_true()
}

/// `who(self)`: returns `"A"`.
extern "C" fn who_a(argv: *const i64) -> i64 {
    let _ = argv;
    text("A")
}

/// `who(self)`: returns `"B"`.
extern "C" fn who_b(argv: *const i64) -> i64 {
    let _ = argv;
    text("B")
}

// -------------------------------------------------------------------------
// Construction, attributes and method binding
// -------------------------------------------------------------------------

#[test]
fn init_attributes_and_methods() {
    pyx_runtime_init();
    let point = class(
        "Point",
        pyx_none(),
        &[
            ("__init__", function(init_x, 2, "__init__")),
            ("get", function(get_x, 1, "get")),
            ("offset", function(offset_x, 2, "offset")),
            ("set", function(set_x, 2, "set")),
        ],
    );
    assert_no_error();

    let instance = call_class(point, &[int(7)]);
    assert_no_error();
    assert_int(getattr(instance, "x"), 7);

    // `self` is bound: `get()` takes no explicit arguments.
    assert_int(method(instance, "get", &[]), 7);
    // Positional arguments follow the bound `self`.
    assert_int(method(instance, "offset", &[int(5)]), 12);
    assert_eq!(method(instance, "set", &[int(20)]), pyx_none());
    assert_no_error();
    assert_int(method(instance, "get", &[]), 20);
}

#[test]
fn class_repr_is_deterministic() {
    pyx_runtime_init();
    let empty = class("Empty", pyx_none(), &[]);
    assert_str(pyx_to_str(empty), "<class 'Empty'>");

    let instance = call_class(empty, &[]);
    assert_no_error();
    // No heap address: the rendering is stable across runs.
    assert_str(pyx_to_str(instance), "<Empty object>");
}

#[test]
fn setattr_replaces_and_appends() {
    pyx_runtime_init();
    let empty = class("Bag", pyx_none(), &[]);
    let instance = call_class(empty, &[]);

    assert_eq!(setattr(instance, "a", int(1)), pyx_none());
    assert_eq!(setattr(instance, "b", int(2)), pyx_none());
    assert_int(getattr(instance, "a"), 1);
    assert_int(getattr(instance, "b"), 2);

    // Overwriting keeps the attribute readable with the new value.
    assert_eq!(setattr(instance, "a", int(3)), pyx_none());
    assert_int(getattr(instance, "a"), 3);
    assert_int(getattr(instance, "b"), 2);

    // A second instance has its own attribute table.
    let other = call_class(empty, &[]);
    assert_eq!(pyx_exc_pending(), 0);
    getattr(other, "a");
    assert_error_tag(ATTRIBUTE_ERROR);
}

#[test]
fn unknown_attribute_and_method_raise_attribute_error() {
    pyx_runtime_init();
    let empty = class("Empty", pyx_none(), &[]);
    let instance = call_class(empty, &[]);

    getattr(instance, "missing");
    assert_error_message("'Empty' object has no attribute 'missing'");

    method(instance, "missing", &[]);
    assert_error_message("'Empty' object has no attribute 'missing'");

    // A data attribute is not a method: reading it works, calling it does not
    // resolve a class method of that name.
    setattr(instance, "value", int(3));
    assert_int(getattr(instance, "value"), 3);
}

// -------------------------------------------------------------------------
// Inheritance and overrides
// -------------------------------------------------------------------------

#[test]
fn inheritance_inherits_init_and_methods_and_overrides() {
    pyx_runtime_init();
    let base = class(
        "Base",
        pyx_none(),
        &[
            ("__init__", function(init_x, 2, "__init__")),
            ("get", function(get_x, 1, "get")),
            ("who", function(who_a, 1, "who")),
        ],
    );
    let sub = class("Sub", base, &[("who", function(who_b, 1, "who"))]);
    assert_no_error();

    let instance = call_class(sub, &[int(31)]);
    assert_no_error();

    // `__init__` and `get` are inherited.
    assert_int(getattr(instance, "x"), 31);
    assert_int(method(instance, "get", &[]), 31);
    // `who` is overridden by the subclass.
    assert_str(method(instance, "who", &[]), "B");

    // The base class still dispatches to its own method.
    let base_instance = call_class(base, &[int(5)]);
    assert_str(method(base_instance, "who", &[]), "A");
}

#[test]
fn method_receives_the_bound_instance_as_self() {
    pyx_runtime_init();
    let cls = class("Binder", pyx_none(), &[("me", function(identity, 1, "me"))]);
    let instance = call_class(cls, &[]);
    assert_eq!(method(instance, "me", &[]), instance);
    assert_no_error();
}

#[test]
fn user_class_context_manager_dispatch() {
    pyx_runtime_init();
    let ctx = class(
        "Ctx",
        pyx_none(),
        &[
            ("__enter__", function(identity, 1, "__enter__")),
            ("__exit__", function(exit_marks, 4, "__exit__")),
        ],
    );
    assert_no_error();
    let instance = call_class(ctx, &[]);
    assert_no_error();

    // `with Ctx() as c` binds `c` to the `__enter__` result.
    let entered = method(instance, "__enter__", &[]);
    assert_no_error();
    assert!(
        same(entered, instance),
        "__enter__ must return the instance"
    );

    // `__exit__` receives the three exception arguments after `self`; its
    // return value is observable but the lowering ignores it.
    let exited = method(instance, "__exit__", &[pyx_none(), pyx_none(), pyx_none()]);
    assert_no_error();
    assert_eq!(exited, pyx_true());
    assert!(same(getattr(instance, "exited"), pyx_true()));

    // The bound arity is enforced: the three explicit arguments are required.
    method(instance, "__exit__", &[]);
    assert_error_tag(TYPE_ERROR);
}

#[test]
fn class_without_init_rejects_arguments() {
    pyx_runtime_init();
    let empty = class("Empty", pyx_none(), &[]);

    let instance = call_class(empty, &[]);
    assert_no_error();
    assert_str(pyx_to_str(instance), "<Empty object>");

    call_class(empty, &[int(1)]);
    assert_error_message("Empty() takes no arguments");
}

#[test]
fn class_definition_validation() {
    pyx_runtime_init();

    // Base must be a class or None.
    let names = [text("who")];
    let funcs = [function(who_a, 1, "who")];
    pyx_class_new(
        b"Bad".as_ptr(),
        3,
        int(1),
        names.len(),
        names.as_ptr(),
        funcs.as_ptr(),
    );
    assert_error_tag(TYPE_ERROR);

    // Method names must be strings.
    let bad_names = [int(1)];
    let funcs = [function(who_a, 1, "who")];
    pyx_class_new(
        b"Bad".as_ptr(),
        3,
        pyx_none(),
        bad_names.len(),
        bad_names.as_ptr(),
        funcs.as_ptr(),
    );
    assert_error_tag(TYPE_ERROR);

    // Methods must be functions.
    let names = [text("who")];
    let bad_funcs = [int(1)];
    pyx_class_new(
        b"Bad".as_ptr(),
        3,
        pyx_none(),
        names.len(),
        names.as_ptr(),
        bad_funcs.as_ptr(),
    );
    assert_error_tag(TYPE_ERROR);
}

#[test]
fn setattr_rejects_non_instances() {
    pyx_runtime_init();
    let cls = class("C", pyx_none(), &[]);

    // Classes have no writable namespace in this runtime.
    setattr(cls, "x", int(1));
    assert_error_tag(TYPE_ERROR);

    // Native values are read-only.
    setattr(int(1), "x", int(1));
    assert_error_tag(ATTRIBUTE_ERROR);
}

// -------------------------------------------------------------------------
// Garbage collection
// -------------------------------------------------------------------------

/// Zeroes a large stack region so stale tagged values from nested frames are
/// unlikely to be found by the conservative scan.
#[inline(never)]
fn scrub_stack() {
    let mut buffer = [0u8; 64 * 1024];
    std::hint::black_box(&mut buffer);
    buffer[0] = 0;
    std::hint::black_box(&buffer);
}

/// Builds an instance whose class, base class and methods are reachable only
/// through the instance.
#[inline(never)]
fn make_nested_instance() -> i64 {
    let base = class("Base", pyx_none(), &[("who", function(who_a, 1, "who"))]);
    let sub = class(
        "Sub",
        base,
        &[("__init__", function(init_x, 2, "__init__"))],
    );
    let instance = call_class(sub, &[int(41)]);
    // This string is reachable only through the instance attribute table.
    setattr(instance, "label", text("kept"));
    instance
}

#[test]
fn class_chain_and_attributes_survive_collection() {
    pyx_runtime_init();
    let instance = make_nested_instance();
    assert_no_error();

    scrub_stack();
    let _ = pyx_gc_collect();

    // The instance is a root; its class and base class (and their methods)
    // are only reachable through it, so this proves the class edges are
    // traced. Attributes are traced through the instance table.
    assert_int(getattr(instance, "x"), 41);
    assert_str(getattr(instance, "label"), "kept");
    assert_str(method(instance, "who", &[]), "A");
    assert_eq!(pyx_exc_pending(), 0);
}

/// Creates an unreachable instance that references itself through a data
/// attribute, exercising a class-instance cycle.
#[inline(never)]
fn make_instance_cycle() {
    let cls = class("Cycle", pyx_none(), &[]);
    let instance = call_class(cls, &[]);
    assert_eq!(pyx_exc_pending(), 0);
    setattr(instance, "self", instance);
}

#[test]
fn unreachable_instance_cycle_is_collected() {
    pyx_runtime_init();
    make_instance_cycle();
    let before = pyx_gc_objects();
    assert!(before >= 2, "class and instance were not allocated");

    scrub_stack();
    let freed = pyx_gc_collect();

    assert!(freed >= 1, "self-referential instance was not collected");
    assert!(
        pyx_gc_objects() < before,
        "registry still contains the garbage instance"
    );
    assert_eq!(pyx_exc_pending(), 0);
}
