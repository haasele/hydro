//! Integration tests for the class/instance runtime: construction through
//! `hydro_class_new`, instantiation and `__init__` binding, instance attributes
//! through `hydro_getattr`/`hydro_setattr`, method dispatch through `hydro_method`,
//! inheritance/overrides, deterministic reprs and collector survival.
//!
//! Every test drives the runtime through the public ABI.

use hydro_runtime::abi::*;

// Exception tags, fixed by ARCHITECTURE §3.2.
const TYPE_ERROR: i64 = 1;
const ATTRIBUTE_ERROR: i64 = 9;
// `operator.eq` uses the comparison ABI's `OP_EQ`.
const OP_EQ: u32 = 7;
const OP_ADD: u32 = 0;
const OP_IS: u32 = 15;

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn int(value: i64) -> i64 {
    // `hydro_int` tags a raw literal (these values are already tagged by the
    // ABI; the helper stays explicit about that).
    hydro_int(value)
}

fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    hydro_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len(), 0)
}

fn class(name: &str, base: i64, methods: &[(&str, i64)]) -> i64 {
    let names: Vec<i64> = methods.iter().map(|(name, _)| text(name)).collect();
    let funcs: Vec<i64> = methods.iter().map(|(_, func)| *func).collect();
    let kinds: Vec<i64> = vec![0; methods.len()];
    let (names_ptr, funcs_ptr, kinds_ptr) = if methods.is_empty() {
        (std::ptr::null(), std::ptr::null(), std::ptr::null())
    } else {
        (names.as_ptr(), funcs.as_ptr(), kinds.as_ptr())
    };
    hydro_class_new(
        name.as_ptr(),
        name.len(),
        base,
        methods.len(),
        names_ptr,
        funcs_ptr,
        kinds_ptr,
        0,
        std::ptr::null(),
        std::ptr::null(),
    )
}

fn class_multi(name: &str, bases: &[i64], methods: &[(&str, i64)]) -> i64 {
    let names: Vec<i64> = methods.iter().map(|(name, _)| text(name)).collect();
    let funcs: Vec<i64> = methods.iter().map(|(_, func)| *func).collect();
    let kinds: Vec<i64> = vec![0; methods.len()];
    let (names_ptr, funcs_ptr, kinds_ptr) = if methods.is_empty() {
        (std::ptr::null(), std::ptr::null(), std::ptr::null())
    } else {
        (names.as_ptr(), funcs.as_ptr(), kinds.as_ptr())
    };
    hydro_class_new_multi(
        name.as_ptr(),
        name.len(),
        bases.as_ptr(),
        bases.len(),
        hydro_none(),
        names_ptr,
        funcs_ptr,
        kinds_ptr,
        methods.len(),
        std::ptr::null(),
        std::ptr::null(),
        0,
    )
}

fn call_class(class: i64, args: &[i64]) -> i64 {
    hydro_call(
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
    hydro_getattr(obj, name.as_ptr(), name.len())
}

fn setattr(obj: i64, name: &str, value: i64) -> i64 {
    hydro_setattr(obj, name.as_ptr(), name.len(), value)
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

fn same(a: i64, b: i64) -> bool {
    hydro_binop(OP_EQ, a, b) == hydro_true()
}

fn same_object(a: i64, b: i64) -> bool {
    hydro_binop(OP_IS, a, b) == hydro_true()
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(actual, text(expected)),
        "value mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert!(
        same(actual, int(expected)),
        "value mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn assert_error_tag(expected: i64) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), expected);
    hydro_exc_clear();
}

fn assert_error_message(expected: &str) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    let message = hydro_to_str(hydro_exc_value());
    assert!(
        same(message, text(expected)),
        "message mismatch, got {:?}",
        message
    );
    hydro_exc_clear();
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
    hydro_binop(OP_ADD, x, args[1])
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
    setattr(args[0], "exited", hydro_true());
    hydro_true()
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
    hydro_runtime_init();
    let point = class(
        "Point",
        hydro_none(),
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
    assert_eq!(method(instance, "set", &[int(20)]), hydro_none());
    assert_no_error();
    assert_int(method(instance, "get", &[]), 20);
}

#[test]
fn class_repr_is_deterministic() {
    hydro_runtime_init();
    let empty = class("Empty", hydro_none(), &[]);
    assert_str(hydro_to_str(empty), "<class 'Empty'>");

    let instance = call_class(empty, &[]);
    assert_no_error();
    // No heap address: the rendering is stable across runs.
    assert_str(hydro_to_str(instance), "<Empty object>");
}

#[test]
fn setattr_replaces_and_appends() {
    hydro_runtime_init();
    let empty = class("Bag", hydro_none(), &[]);
    let instance = call_class(empty, &[]);

    assert_eq!(setattr(instance, "a", int(1)), hydro_none());
    assert_eq!(setattr(instance, "b", int(2)), hydro_none());
    assert_int(getattr(instance, "a"), 1);
    assert_int(getattr(instance, "b"), 2);

    // Overwriting keeps the attribute readable with the new value.
    assert_eq!(setattr(instance, "a", int(3)), hydro_none());
    assert_int(getattr(instance, "a"), 3);
    assert_int(getattr(instance, "b"), 2);

    // A second instance has its own attribute table.
    let other = call_class(empty, &[]);
    assert_eq!(hydro_exc_pending(), 0);
    getattr(other, "a");
    assert_error_tag(ATTRIBUTE_ERROR);
}

#[test]
fn unknown_attribute_and_method_raise_attribute_error() {
    hydro_runtime_init();
    let empty = class("Empty", hydro_none(), &[]);
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
    hydro_runtime_init();
    let base = class(
        "Base",
        hydro_none(),
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
    hydro_runtime_init();
    let cls = class(
        "Binder",
        hydro_none(),
        &[("me", function(identity, 1, "me"))],
    );
    let instance = call_class(cls, &[]);
    assert_eq!(method(instance, "me", &[]), instance);
    assert_no_error();
}

#[test]
fn user_class_context_manager_dispatch() {
    hydro_runtime_init();
    let ctx = class(
        "Ctx",
        hydro_none(),
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
    let exited = method(
        instance,
        "__exit__",
        &[hydro_none(), hydro_none(), hydro_none()],
    );
    assert_no_error();
    assert_eq!(exited, hydro_true());
    assert!(same(getattr(instance, "exited"), hydro_true()));

    // The bound arity is enforced: the three explicit arguments are required.
    method(instance, "__exit__", &[]);
    assert_error_tag(TYPE_ERROR);
}

#[test]
fn class_without_init_rejects_arguments() {
    hydro_runtime_init();
    let empty = class("Empty", hydro_none(), &[]);

    let instance = call_class(empty, &[]);
    assert_no_error();
    assert_str(hydro_to_str(instance), "<Empty object>");

    call_class(empty, &[int(1)]);
    assert_error_message("Empty() takes no arguments");
}

#[test]
fn class_definition_validation() {
    hydro_runtime_init();
    let kinds = [0i64];

    // Base must be a class or None.
    let names = [text("who")];
    let funcs = [function(who_a, 1, "who")];
    hydro_class_new(
        b"Bad".as_ptr(),
        3,
        int(1),
        names.len(),
        names.as_ptr(),
        funcs.as_ptr(),
        kinds.as_ptr(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_error_tag(TYPE_ERROR);

    // Method names must be strings.
    let bad_names = [int(1)];
    let funcs = [function(who_a, 1, "who")];
    hydro_class_new(
        b"Bad".as_ptr(),
        3,
        hydro_none(),
        bad_names.len(),
        bad_names.as_ptr(),
        funcs.as_ptr(),
        kinds.as_ptr(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_error_tag(TYPE_ERROR);

    // Methods must be functions.
    let names = [text("who")];
    let bad_funcs = [int(1)];
    hydro_class_new(
        b"Bad".as_ptr(),
        3,
        hydro_none(),
        names.len(),
        names.as_ptr(),
        bad_funcs.as_ptr(),
        kinds.as_ptr(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_error_tag(TYPE_ERROR);
}

#[test]
fn setattr_rejects_non_instances() {
    hydro_runtime_init();
    let cls = class("C", hydro_none(), &[]);

    // Classes have a writable namespace; the attribute is visible.
    setattr(cls, "x", int(1));
    assert_no_error();
    assert_int(getattr(cls, "x"), 1);

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
    let base = class("Base", hydro_none(), &[("who", function(who_a, 1, "who"))]);
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
    hydro_runtime_init();
    let instance = make_nested_instance();
    assert_no_error();

    scrub_stack();
    let _ = hydro_gc_collect();

    // The instance is a root; its class and base class (and their methods)
    // are only reachable through it, so this proves the class edges are
    // traced. Attributes are traced through the instance table.
    assert_int(getattr(instance, "x"), 41);
    assert_str(getattr(instance, "label"), "kept");
    assert_str(method(instance, "who", &[]), "A");
    assert_eq!(hydro_exc_pending(), 0);
}

/// Creates an unreachable instance that references itself through a data
/// attribute, exercising a class-instance cycle.
#[inline(never)]
fn make_instance_cycle() {
    let cls = class("Cycle", hydro_none(), &[]);
    let instance = call_class(cls, &[]);
    assert_eq!(hydro_exc_pending(), 0);
    setattr(instance, "self", instance);
}

#[test]
fn unreachable_instance_cycle_is_collected() {
    hydro_runtime_init();
    make_instance_cycle();
    let before = hydro_gc_objects();
    assert!(before >= 2, "class and instance were not allocated");

    scrub_stack();
    let freed = hydro_gc_collect();

    assert!(freed >= 1, "self-referential instance was not collected");
    assert!(
        hydro_gc_objects() < before,
        "registry still contains the garbage instance"
    );
    assert_eq!(hydro_exc_pending(), 0);
}

#[test]
fn generic_class_subscript_builds_a_typing_alias() {
    hydro_runtime_init();
    // A PEP 695 class publishes its runtime parameter tuple as the class
    // attribute `__type_params__` (the frontend stores it through this ABI).
    let parameter = text("T");
    let params = hydro_tuple_new(1, [parameter].as_ptr());
    let attr_names = [text("__type_params__")];
    let attr_values = [params];
    let generic = hydro_class_new(
        "Box".as_ptr(),
        3,
        hydro_none(),
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        1,
        attr_names.as_ptr(),
        attr_values.as_ptr(),
    );
    assert_no_error();
    let int_class = hydro_class_new(
        "int".as_ptr(),
        3,
        hydro_none(),
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_no_error();

    // `Box[int]` is a `_GenericAlias` over the class, not the class itself.
    let alias = hydro_subscript(generic, int_class);
    assert_no_error();
    assert_ne!(alias, generic, "a generic class specializes into an alias");
    assert_eq!(getattr(alias, "__origin__"), generic);
    let args = getattr(alias, "__args__");
    // Exactly one argument, and it is the operand.
    assert_eq!(hydro_unpack(args, 1), hydro_none());
    assert_no_error();
    assert_eq!(hydro_subscript(args, int(0)), int_class);
    assert_no_error();
    // The parameters of a specialized alias are the ones the arguments bind.
    let parameters = getattr(alias, "__parameters__");
    assert_eq!(hydro_unpack(parameters, 0), hydro_none());
    assert_no_error();
    assert_str(hydro_to_str(alias), "Box[int]");

    // A class without `__type_params__` keeps the previous behaviour: the
    // class value itself is the alias.
    let plain = class("Plain", hydro_none(), &[]);
    assert_eq!(hydro_subscript(plain, int_class), plain);
    assert_no_error();

    // `class Child(Box[int])`: the base is specialized to its origin, so the
    // alias never reaches the MRO, and the parameters are inherited.
    let child = hydro_class_new(
        "Child".as_ptr(),
        5,
        alias,
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_no_error();
    assert_eq!(getattr(child, "__type_params__"), params);
    let inherited = hydro_subscript(child, int_class);
    assert_no_error();
    assert_eq!(getattr(inherited, "__origin__"), child);
}

// -------------------------------------------------------------------------
// PEP 560 `__mro_entries__` base resolution
// -------------------------------------------------------------------------

/// `TypedDict`-style special form: `__mro_entries__(self, bases)` verifies it
/// received the whole original bases tuple and expands to two classes.
///
/// The closure layout is `[tuple_base, dict_base, marker, self, bases]`.
extern "C" fn typed_dict_mro_entries(argv: *const i64) -> i64 {
    // SAFETY: the runtime supplies captures followed by arity-2 arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 5) };
    let bases = args[4];
    let first = hydro_subscript(bases, int(0));
    let second = hydro_subscript(bases, int(1));
    let full_bases = same_object(first, args[2]) && same_object(second, args[3]);
    if !full_bases {
        // A non-iterable result makes the failure obvious in the test.
        return hydro_none();
    }
    let items = [args[0], args[1]];
    hydro_tuple_new(2, items.as_ptr())
}

/// `NamedTuple`-style hook stored as an instance attribute: expands the base
/// to the single captured class. The result is a list, not a tuple, so the
/// resolution path is exercising generic iterable flattening.
///
/// The closure layout is `[namedtuple_base, bases]`.
extern "C" fn named_tuple_mro_entries(argv: *const i64) -> i64 {
    // SAFETY: the runtime supplies one capture followed by the `bases` arg.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let items = [args[0]];
    hydro_list_new(1, items.as_ptr())
}

/// A hook that violates the contract by returning a non-class element.
extern "C" fn non_class_mro_entries(_argv: *const i64) -> i64 {
    let items = [int(7)];
    hydro_tuple_new(1, items.as_ptr())
}

#[test]
fn mro_entries_special_form_expands_to_multiple_bases() {
    hydro_runtime_init();
    let tuple_base = class("MroTupleBase", hydro_none(), &[]);
    let dict_base = class("MroDictBase", hydro_none(), &[]);
    let marker = class("MroMarker", hydro_none(), &[]);
    let captures = [tuple_base, dict_base, marker];
    let hook = hydro_closure_new(
        typed_dict_mro_entries as *const u8,
        2,
        "__mro_entries__".as_ptr(),
        "__mro_entries__".len(),
        captures.len(),
        captures.as_ptr(),
        0,
    );
    assert_no_error();
    let form_class = class("TypedDictForm", hydro_none(), &[("__mro_entries__", hook)]);
    let form = call_class(form_class, &[]);
    assert_no_error();

    // `class Config(marker, form)`: the hook is called with (marker, form) and
    // its two classes are spliced into the base list in place.
    let config = class_multi("Config", &[marker, form], &[]);
    assert_no_error();
    let bases = getattr(config, "__bases__");
    assert_eq!(hydro_len(bases) >> 3, 3);
    assert!(same_object(hydro_subscript(bases, int(0)), marker));
    assert!(same_object(hydro_subscript(bases, int(1)), tuple_base));
    assert!(same_object(hydro_subscript(bases, int(2)), dict_base));

    // `__orig_bases__` reports the declared bases, not the expansion.
    let original = getattr(config, "__orig_bases__");
    assert_eq!(hydro_len(original) >> 3, 2);
    assert!(same_object(hydro_subscript(original, int(0)), marker));
    assert!(same_object(hydro_subscript(original, int(1)), form));
    assert_no_error();
}

#[test]
fn mro_entries_single_class_instance_attribute() {
    hydro_runtime_init();
    let namedtuple_base = class("NamedTupleStandIn", hydro_none(), &[]);
    let captures = [namedtuple_base];
    let hook = hydro_closure_new(
        named_tuple_mro_entries as *const u8,
        1,
        "__mro_entries__".as_ptr(),
        "__mro_entries__".len(),
        captures.len(),
        captures.as_ptr(),
        0,
    );
    assert_no_error();
    let form_class = class("NamedTupleForm", hydro_none(), &[]);
    let form = call_class(form_class, &[]);
    setattr(form, "__mro_entries__", hook);
    assert_no_error();

    // The single-base ABI path must hand the instance to the runtime instead
    // of rejecting it statically.
    let point = class("Point", form, &[]);
    assert_no_error();
    let bases = getattr(point, "__bases__");
    assert_eq!(hydro_len(bases) >> 3, 1);
    assert!(same_object(hydro_subscript(bases, int(0)), namedtuple_base));
    let original = getattr(point, "__orig_bases__");
    assert_eq!(hydro_len(original) >> 3, 1);
    assert!(same_object(hydro_subscript(original, int(0)), form));
    assert_no_error();
}

#[test]
fn instance_base_without_mro_entries_reports_type_error() {
    hydro_runtime_init();
    let plain = class("PlainBase", hydro_none(), &[]);
    let instance = call_class(plain, &[]);
    assert_no_error();

    let derived = class("Derived", instance, &[]);
    assert_eq!(derived, hydro_none());
    assert_eq!(hydro_exc_pending(), 1);
    assert_eq!(hydro_exc_tag(), TYPE_ERROR);
    let message = hydro_to_str(hydro_exc_value());
    assert!(
        same(
            message,
            text("class base of 'Derived' must be a class object or None, got 'instance'")
        ),
        "message mismatch, got {:?}",
        hydro_to_str(message)
    );
    hydro_exc_clear();
}

#[test]
fn mro_entries_non_class_result_reports_type_error() {
    hydro_runtime_init();
    let hook = function(non_class_mro_entries, 2, "__mro_entries__");
    let form_class = class("BadForm", hydro_none(), &[("__mro_entries__", hook)]);
    let form = call_class(form_class, &[]);
    assert_no_error();

    let derived = class("BadDerived", form, &[]);
    assert_eq!(derived, hydro_none());
    assert_eq!(hydro_exc_pending(), 1);
    assert_eq!(hydro_exc_tag(), TYPE_ERROR);
    let message = hydro_to_str(hydro_exc_value());
    assert!(
        same(
            message,
            text("__mro_entries__ on a base of 'BadDerived' returned a non-class object 'int'")
        ),
        "message mismatch, got {:?}",
        hydro_to_str(message)
    );
    hydro_exc_clear();
}
