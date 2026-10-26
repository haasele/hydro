//! Runtime object-model integration tests: `bytearray`, `memoryview`, live
//! dict views, the descriptor protocol, class namespace mutation, `__slots__`,
//! C3 multiple inheritance, `super`, functional `type()` and user-defined
//! exception identity (including re-raising inside an active handler).
//!
//! Every test drives the runtime through the public ABI.

use hydro_runtime::abi::*;

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

const NONE: i64 = 1;

// Exception tags (ARCHITECTURE §3.2).
const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const ATTRIBUTE_ERROR: i64 = 9;

// `hydro_binop` op codes.
const OP_ADD: u32 = 0;
const OP_EQ: u32 = 7;
const OP_IN: u32 = 13;
const OP_BIT_AND: u32 = 17;

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

/// Tagged integer for a raw compile-time literal. `hydro_int` treats values
/// whose low tag bits already look valid as tagged, so the tests tag directly.
fn int(value: i64) -> i64 {
    value << 3
}

fn bytes_lit(value: &[u8]) -> i64 {
    hydro_bytes_new(value.as_ptr(), value.len())
}

fn bytearray(value: &[u8]) -> i64 {
    hydro_bytearray_new(value.as_ptr(), value.len())
}

fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    hydro_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len(), 0)
}

fn call(function: i64, args: &[i64]) -> i64 {
    hydro_call(
        function,
        args.len(),
        if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        },
    )
}

fn call_builtin(name: &str, args: &[i64]) -> i64 {
    let builtin = hydro_builtin_function(name.as_ptr(), name.len());
    call(builtin, args)
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

fn setattr(obj: i64, name: &str, value: i64) -> i64 {
    hydro_setattr(obj, name.as_ptr(), name.len(), value)
}

fn subscript(obj: i64, index: i64) -> i64 {
    hydro_subscript(obj, index)
}

fn slice(obj: i64, start: i64, stop: i64, step: i64) -> i64 {
    hydro_slice(obj, start, stop, step)
}

fn binop(op: u32, a: i64, b: i64) -> i64 {
    hydro_binop(op, a, b)
}

fn same(a: i64, b: i64) -> bool {
    binop(OP_EQ, a, b) == hydro_true()
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert!(
        same(actual, int(expected)),
        "expected {expected}, rendering is {:?}",
        hydro_to_str(actual)
    );
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(actual, text(expected)),
        "expected {expected:?}, rendering is {:?}",
        hydro_to_str(actual)
    );
}

fn assert_repr(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(hydro_to_repr(actual), text(expected)),
        "expected repr {expected:?}, got {:?}",
        hydro_to_str(hydro_to_repr(actual))
    );
}

fn assert_pending_tag(expected: i64) {
    assert_eq!(
        hydro_exc_pending(),
        1,
        "expected a pending exception with tag {expected}"
    );
    assert_eq!(hydro_exc_tag(), expected);
    hydro_exc_clear();
}

// Wrapper bodies used as method implementations.

extern "C" fn returns_one(_args: *const i64) -> i64 {
    int(1)
}

extern "C" fn returns_two(_args: *const i64) -> i64 {
    int(2)
}

extern "C" fn returns_seven(_args: *const i64) -> i64 {
    int(7)
}

/// `__get__(self, obj, owner)`: returns the receiver (`obj`).
extern "C" fn descriptor_get(args: *const i64) -> i64 {
    if args.is_null() {
        return NONE;
    }
    // SAFETY: the binder passes three arguments for `__get__`.
    let values = unsafe { std::slice::from_raw_parts(args, 3) };
    values[1]
}

/// `__set__(self, obj, value)`: records the assigned value on the instance.
extern "C" fn descriptor_set(args: *const i64) -> i64 {
    if args.is_null() {
        return NONE;
    }
    // SAFETY: the binder passes three arguments for `__set__`.
    let values = unsafe { std::slice::from_raw_parts(args, 3) };
    hydro_setattr(values[1], b"stored".as_ptr(), 6, values[2]);
    NONE
}

/// `describe2(self, value)`: returns the receiver with one extra positional
/// argument (used for starred super calls).
extern "C" fn describe_two(args: *const i64) -> i64 {
    if args.is_null() {
        return NONE;
    }
    // SAFETY: two receiver arguments.
    let values = unsafe { std::slice::from_raw_parts(args, 2) };
    values[0]
}

/// `second(self, value)`: returns the second full-slot argument (observes
/// keyword binding through a generated-style signature).
extern "C" fn second_argument(args: *const i64) -> i64 {
    if args.is_null() {
        return NONE;
    }
    // SAFETY: two full-argv slots.
    let values = unsafe { std::slice::from_raw_parts(args, 2) };
    values[1]
}

/// `describe(self)`: returns the receiver, proving `self` binding.
extern "C" fn describe(args: *const i64) -> i64 {
    if args.is_null() {
        return NONE;
    }
    // SAFETY: one receiver argument.
    let values = unsafe { std::slice::from_raw_parts(args, 1) };
    values[0]
}

/// `__init__(self)`: records that the initializer ran.
extern "C" fn init_marker(args: *const i64) -> i64 {
    if args.is_null() {
        return NONE;
    }
    // SAFETY: one receiver argument.
    let values = unsafe { std::slice::from_raw_parts(args, 1) };
    hydro_setattr(values[0], b"made".as_ptr(), 4, int(1));
    NONE
}

fn class(name: &str, base: i64, methods: &[(&str, i64)]) -> i64 {
    let tagged: Vec<(&str, i64, i64)> = methods.iter().map(|(n, f)| (*n, *f, 0)).collect();
    class_with_kinds(name, base, &tagged)
}

fn class_with_kinds(name: &str, base: i64, methods: &[(&str, i64, i64)]) -> i64 {
    let names: Vec<i64> = methods.iter().map(|(name, _, _)| text(name)).collect();
    let funcs: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let kinds: Vec<i64> = methods.iter().map(|(_, _, kind)| *kind).collect();
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

fn class_multi(
    name: &str,
    bases: &[i64],
    methods: &[(&str, i64, i64)],
    attrs: &[(&str, i64)],
) -> i64 {
    class_multi_with_meta(name, bases, NONE, methods, attrs)
}

fn class_multi_with_meta(
    name: &str,
    bases: &[i64],
    metaclass: i64,
    methods: &[(&str, i64, i64)],
    attrs: &[(&str, i64)],
) -> i64 {
    let names: Vec<i64> = methods.iter().map(|(name, _, _)| text(name)).collect();
    let funcs: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let kinds: Vec<i64> = methods.iter().map(|(_, _, kind)| *kind).collect();
    let attr_names: Vec<i64> = attrs.iter().map(|(name, _)| text(name)).collect();
    let attr_values: Vec<i64> = attrs.iter().map(|(_, value)| *value).collect();
    hydro_class_new_multi(
        name.as_ptr(),
        name.len(),
        bases.as_ptr(),
        bases.len(),
        metaclass,
        if methods.is_empty() {
            std::ptr::null()
        } else {
            names.as_ptr()
        },
        if methods.is_empty() {
            std::ptr::null()
        } else {
            funcs.as_ptr()
        },
        if methods.is_empty() {
            std::ptr::null()
        } else {
            kinds.as_ptr()
        },
        methods.len(),
        if attrs.is_empty() {
            std::ptr::null()
        } else {
            attr_names.as_ptr()
        },
        if attrs.is_empty() {
            std::ptr::null()
        } else {
            attr_values.as_ptr()
        },
        attrs.len(),
    )
}

fn dict(pairs: &[(&str, i64)]) -> i64 {
    let keys: Vec<i64> = pairs.iter().map(|(key, _)| text(key)).collect();
    let values: Vec<i64> = pairs.iter().map(|(_, value)| *value).collect();
    hydro_dict_new(pairs.len(), keys.as_ptr(), values.as_ptr())
}

fn list_of(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
}

fn set_of(items: &[i64]) -> i64 {
    hydro_set_from(list_of(items))
}

// bytearray

#[test]
fn bytearray_is_mutable_and_bytes_like() {
    let _guard = test_guard();
    hydro_runtime_init();
    let buffer = bytearray(b"abc");
    assert_int(hydro_len(buffer), 3);
    assert_int(subscript(buffer, int(1)), b'b' as i64);
    assert_repr(buffer, "bytearray(b'abc')");
    assert_eq!(hydro_truthy(bytearray(b"")), 0);

    // Item assignment mutates in place.
    hydro_subscript_set(buffer, int(0), int(b'A' as i64));
    assert_int(subscript(buffer, int(0)), b'A' as i64);

    // Mutation methods.
    method(buffer, "append", &[int(b'!' as i64)]);
    assert_int(hydro_len(buffer), 4);
    method(buffer, "extend", &[bytes_lit(b"de")]);
    assert_int(hydro_len(buffer), 6);
    assert_int(method(buffer, "pop", &[]), b'e' as i64);
    method(buffer, "reverse", &[]);
    assert_int(subscript(buffer, int(0)), b'd' as i64);
    method(buffer, "clear", &[]);
    assert_int(hydro_len(buffer), 0);
    assert_no_error();

    // Slicing returns a bytearray and concatenation keeps the mutable type.
    let source = bytearray(b"abcdef");
    let middle = slice(source, int(1), int(4), NONE);
    assert_repr(middle, "bytearray(b'bcd')");
    let joined = binop(OP_ADD, source, bytes_lit(b"!"));
    assert_repr(joined, "bytearray(b'abcdef!')");

    // Iteration yields integers; equality compares content with bytes.
    let iterator = hydro_iter_new(bytearray(b"hi"));
    let mut item = NONE;
    assert_eq!(hydro_iter_next(iterator, &mut item), 1);
    assert_int(item, b'h' as i64);
    assert_eq!(
        binop(OP_EQ, bytearray(b"hi"), bytes_lit(b"hi")),
        hydro_true()
    );
    assert_eq!(
        binop(OP_EQ, bytearray(b"hi"), bytes_lit(b"no")),
        hydro_false()
    );
    assert_no_error();
}

// memoryview

#[test]
fn memoryview_is_live_read_write_with_tagged_errors() {
    let _guard = test_guard();
    hydro_runtime_init();
    let buffer = bytearray(b"abc");
    let view = hydro_memoryview_new(buffer);
    assert_str(getattr(view, "format"), "B");
    assert_int(getattr(view, "itemsize"), 1);
    assert_int(getattr(view, "ndim"), 1);
    assert_int(getattr(view, "nbytes"), 3);
    assert_eq!(getattr(view, "readonly"), hydro_false());
    assert_int(hydro_len(view), 3);

    // Writes through the view alias the exporter, and exporter growth is seen.
    hydro_subscript_set(view, int(0), int(b'Z' as i64));
    assert_int(subscript(buffer, int(0)), b'Z' as i64);
    method(buffer, "append", &[int(b'd' as i64)]);
    assert_int(hydro_len(view), 4);

    // Slicing keeps a live window.
    let tail = slice(view, int(1), NONE, NONE);
    assert!(same(method(tail, "tobytes", &[]), bytes_lit(b"bcd")));
    assert_repr(method(view, "tolist", &[]), "[90, 98, 99, 100]");
    assert_repr(method(view, "cast", &[text("c")]), "<memory at 0x0>");

    // Unsupported formats fail with a tagged error.
    method(view, "cast", &[text("I")]);
    assert_eq!(hydro_exc_pending(), 1);
    assert_ne!(hydro_exc_tag(), 0); // NotImplementedError, not ValueError
    hydro_exc_clear();

    // A view over immutable bytes is read-only.
    let readonly = hydro_memoryview_new(bytes_lit(b"xy"));
    hydro_subscript_set(readonly, int(0), int(65));
    assert_pending_tag(TYPE_ERROR);
    // Non-bytes-like exporters are rejected.
    hydro_memoryview_new(int(1));
    assert_pending_tag(TYPE_ERROR);

    // `release()` invalidates the view.
    method(view, "release", &[]);
    assert_no_error();
    subscript(view, int(0));
    assert_pending_tag(VALUE_ERROR);
}

// dict views

#[test]
fn dict_views_are_live_and_support_set_operations() {
    let _guard = test_guard();
    hydro_runtime_init();
    let mapping = dict(&[("a", int(1)), ("b", int(2))]);
    let keys = method(mapping, "keys", &[]);
    let values = method(mapping, "values", &[]);
    let items = method(mapping, "items", &[]);

    assert_repr(keys, "dict_keys(['a', 'b'])");
    assert_repr(values, "dict_values([1, 2])");
    assert_repr(items, "dict_items([('a', 1), ('b', 2)])");
    assert_int(hydro_len(keys), 2);
    assert_str(ty_name(keys), "dict_keys");

    // Live: dict mutations are observed, and views are not snapshots.
    method(mapping, "__setitem__", &[text("c"), int(3)]);
    assert_int(hydro_len(keys), 3);
    assert_repr(keys, "dict_keys(['a', 'b', 'c'])");
    method(mapping, "__delitem__", &[text("a")]);
    assert_repr(values, "dict_values([2, 3])");

    // Membership and set operators.
    assert_eq!(binop(OP_IN, text("b"), keys), hydro_true());
    assert_eq!(binop(OP_IN, text("a"), keys), hydro_false());
    let pair_items = [text("b"), int(2)];
    let pair = hydro_tuple_new(2, pair_items.as_ptr());
    assert_eq!(binop(OP_IN, pair, items), hydro_true());
    let wanted = set_of(&[text("b")]);
    let intersection = binop(OP_BIT_AND, keys, wanted);
    assert_repr(intersection, "{'b'}");
    assert_no_error();

    // Iteration follows insertion order.
    let iterator = hydro_iter_new(keys);
    let mut item = NONE;
    assert_eq!(hydro_iter_next(iterator, &mut item), 1);
    assert_str(item, "b");
    assert_eq!(hydro_iter_next(iterator, &mut item), 1);
    assert_str(item, "c");
    assert_eq!(hydro_iter_next(iterator, &mut item), 0);
    assert_no_error();
}

fn ty_name(value: i64) -> i64 {
    let class = call_builtin("type", &[value]);
    getattr(class, "__name__")
}

// descriptors, class namespace mutation and __slots__

#[test]
fn descriptors_bind_and_data_descriptors_win() {
    let _guard = test_guard();
    hydro_runtime_init();
    let descriptor_class = class(
        "Descriptor",
        NONE,
        &[
            ("__get__", function(descriptor_get, 3, "__get__")),
            ("__set__", function(descriptor_set, 3, "__set__")),
        ],
    );
    let descriptor = hydro_instance_new(descriptor_class);
    let holder = class_multi("Holder", &[], &[], &[("x", descriptor)]);
    let instance = hydro_instance_new(holder);

    // Reading through the instance invokes `__get__(self, obj, owner)`.
    assert_eq!(getattr(instance, "x"), instance);
    // A data descriptor intercepts assignment.
    assert_eq!(setattr(instance, "x", int(5)), NONE);
    assert_int(getattr(instance, "stored"), 5);
    // Reading from the class passes `None` as the instance.
    assert_eq!(getattr(holder, "x"), NONE);

    // A plain class attribute is shadowed by the instance dictionary.
    let plain = class_multi("Plain", &[], &[], &[("n", int(10))]);
    let plain_instance = hydro_instance_new(plain);
    assert_int(getattr(plain_instance, "n"), 10);
    assert_eq!(setattr(plain_instance, "n", int(20)), NONE);
    assert_int(getattr(plain_instance, "n"), 20);

    // A property is a data descriptor and wins over the instance dictionary.
    let with_property = class_with_kinds(
        "WithProperty",
        NONE,
        &[("p", function(returns_seven, 1, "p"), 1)],
    );
    let property_instance = hydro_instance_new(with_property);
    assert_int(getattr(property_instance, "p"), 7);
    setattr(property_instance, "p", int(9));
    assert_pending_tag(ATTRIBUTE_ERROR);

    // Runtime class assignment is visible to existing instances.
    assert_eq!(setattr(holder, "extra", int(42)), NONE);
    assert_int(getattr(instance, "extra"), 42);
    assert_no_error();
}

#[test]
fn slots_reject_undeclared_attributes() {
    let _guard = test_guard();
    hydro_runtime_init();
    let slots = list_of(&[text("a")]);
    let slotted = class_multi("Slotted", &[], &[], &[("__slots__", slots)]);
    let instance = hydro_instance_new(slotted);
    assert_eq!(setattr(instance, "a", int(1)), NONE);
    assert_int(getattr(instance, "a"), 1);
    setattr(instance, "b", int(2));
    assert_pending_tag(ATTRIBUTE_ERROR);
    // A slots-only class has no instance `__dict__`.
    getattr(instance, "__dict__");
    assert_pending_tag(ATTRIBUTE_ERROR);
}

// multiple inheritance, MRO and super

#[test]
fn c3_multiple_inheritance_and_super_follow_the_mro() {
    let _guard = test_guard();
    hydro_runtime_init();
    let a = class("A", NONE, &[("who", function(returns_one, 1, "who"))]);
    let b = class("B", NONE, &[("who", function(returns_two, 1, "who"))]);
    let c = class_multi("C", &[b, a], &[], &[]);
    let d = class_multi("D", &[a, b], &[], &[]);

    // Neither order collapses to the first base.
    assert_repr(
        getattr(c, "__mro__"),
        "(<class 'C'>, <class 'B'>, <class 'A'>)",
    );
    assert_repr(
        getattr(d, "__mro__"),
        "(<class 'D'>, <class 'A'>, <class 'B'>)",
    );
    let c_instance = hydro_instance_new(c);
    let d_instance = hydro_instance_new(d);
    assert_int(method(c_instance, "who", &[]), 2);
    assert_int(method(d_instance, "who", &[]), 1);

    // `super()` searches the remaining MRO entries.
    let proxy = method(c_instance, "$super_new", &[]);
    assert_int(method(proxy, "who", &[]), 2);
    let derived_proxy = method(d_instance, "$super_new", &[]);
    assert_int(method(derived_proxy, "who", &[]), 1);
    assert_repr(proxy, "<super: <class 'C'>, <C object>>");

    // Diamond: the shared base appears once in the linearization.
    let left = class_multi("Left", &[a], &[], &[]);
    let right = class_multi("Right", &[a], &[], &[]);
    let diamond = class_multi("Diamond", &[left, right], &[], &[]);
    assert_repr(
        getattr(diamond, "__mro__"),
        "(<class 'Diamond'>, <class 'Left'>, <class 'Right'>, <class 'A'>)",
    );

    // `super().__init__()` walks the MRO and runs the base initializer.
    let base = class(
        "Base",
        NONE,
        &[("__init__", function(init_marker, 1, "__init__"))],
    );
    let derived = class_multi("Derived", &[base], &[], &[]);
    let derived_instance = hydro_instance_new(derived);
    method(derived_instance, "$super_init", &[]);
    assert_int(getattr(derived_instance, "made"), 1);
    assert_no_error();
}

// functional type() and user-defined exception identity

#[test]
fn type_three_arg_and_user_exceptions_keep_identity() {
    let _guard = test_guard();
    hydro_runtime_init();
    // `type(name, bases, namespace)`.
    let namespace = dict(&[
        ("twice", function(returns_two, 1, "twice")),
        ("tag", int(9)),
    ]);
    let created = call_builtin(
        "type",
        &[
            text("Created"),
            hydro_tuple_new(0, std::ptr::null()),
            namespace,
        ],
    );
    assert_no_error();
    assert_str(getattr(created, "__name__"), "Created");
    let instance = hydro_instance_new(created);
    assert_int(method(instance, "twice", &[]), 2);
    assert_int(getattr(instance, "tag"), 9);

    // A user exception class derived from ValueError keeps its identity.
    let value_error = hydro_exception_class(VALUE_ERROR as u32);
    let my_error = class("MyError", value_error, &[]);
    let raised = call(my_error, &[text("boom")]);
    assert_no_error();
    assert_eq!(call_builtin("type", &[raised]), my_error);
    assert_eq!(
        call_builtin("isinstance", &[raised, my_error]),
        hydro_true()
    );
    assert_eq!(
        call_builtin("isinstance", &[raised, value_error]),
        hydro_true()
    );
    assert_repr(raised, "MyError('boom')");
    assert_str(hydro_to_str(raised), "boom");

    // Raising keeps the class while the native ancestry decides the tag.
    hydro_raise_value(raised);
    assert_eq!(hydro_exc_tag(), VALUE_ERROR);
    assert_eq!(call_builtin("type", &[hydro_exc_value()]), my_error);
    hydro_exc_clear();

    // Re-raising inside an active handler (the context-clone path) preserves
    // the class identity too.
    let again = call(my_error, &[text("again")]);
    let depth = hydro_exc_handler_push(raised);
    hydro_raise_value(again);
    assert_eq!(hydro_exc_tag(), VALUE_ERROR);
    assert_eq!(call_builtin("type", &[hydro_exc_value()]), my_error);
    hydro_exc_clear();
    hydro_exc_handler_truncate(depth);
    assert_no_error();
}

// collector stress

#[test]
fn gc_stress_keeps_live_object_model_values() {
    let _guard = test_guard();
    hydro_runtime_init();
    let kept_buffer = bytearray(b"keep");
    let kept_view = hydro_memoryview_new(kept_buffer);
    let kept_mapping = dict(&[("k", kept_view)]);
    let kept_keys = method(kept_mapping, "keys", &[]);

    for index in 0..2000 {
        let temporary = bytearray(&[index as u8, 1, 2, 3]);
        let temporary_view = hydro_memoryview_new(temporary);
        let temporary_class = class_multi("Temp", &[], &[], &[("v", temporary_view)]);
        let temporary_instance = hydro_instance_new(temporary_class);
        assert_no_error();
        // Keep the temporary graph observable so it stays reachable during
        // the iteration.
        assert_repr(getattr(temporary_instance, "v"), "<memory at 0x0>");
    }

    let _ = hydro_gc_collect();
    assert_int(hydro_len(kept_view), 4);
    assert_int(hydro_len(kept_keys), 1);
    assert_eq!(binop(OP_IN, text("k"), kept_keys), hydro_true());
    method(kept_buffer, "append", &[int(b'!' as i64)]);
    assert_int(hydro_len(kept_view), 5);
    assert_no_error();
}

// user-defined exception instances keep the user class surface

#[test]
fn user_exception_instances_expose_class_methods_and_attributes() {
    hydro_runtime_init();
    let value_error = hydro_exception_class(VALUE_ERROR as u32);
    let error_class = class(
        "E",
        value_error,
        &[("describe", function(describe, 1, "describe"))],
    );
    let instance = call(error_class, &[text("boom")]);
    assert_no_error();

    // Class methods bind the exception instance as `self`.
    assert_eq!(method(instance, "describe", &[]), instance);
    assert_no_error();

    // Instance attributes live on the exception instance.
    assert_eq!(setattr(instance, "extra", int(7)), NONE);
    assert_int(getattr(instance, "extra"), 7);
    assert_repr(getattr(instance, "__dict__"), "{'extra': 7}");
    assert_repr(getattr(instance, "args"), "('boom',)");

    // Exception identity and behaviour are unchanged.
    assert_repr(instance, "E('boom')");
    assert_str(hydro_to_str(instance), "boom");
    assert_eq!(call_builtin("type", &[instance]), error_class);
    assert_eq!(
        call_builtin("isinstance", &[instance, error_class]),
        hydro_true()
    );
    assert_eq!(
        call_builtin("isinstance", &[instance, value_error]),
        hydro_true()
    );
    hydro_raise_value(instance);
    assert_eq!(hydro_exc_tag(), VALUE_ERROR);
    assert_eq!(call_builtin("type", &[hydro_exc_value()]), error_class);
    hydro_exc_clear();
    assert_no_error();
}

// super keywords/classmethod receivers and function metadata

#[test]
fn super_proxy_keywords_classmethods_and_function_metadata() {
    hydro_runtime_init();

    // Function values expose their definition name; `__doc__` is None.
    let add = function(describe, 1, "add");
    assert_str(getattr(add, "__name__"), "add");
    assert_str(getattr(add, "__qualname__"), "add");
    assert_eq!(getattr(add, "__doc__"), NONE);
    assert_eq!(
        call_builtin("hasattr", &[add, text("__name__")]),
        hydro_true()
    );
    assert_no_error();

    // Keyword calls reach a super method through the MRO and bind through
    // the generated full-argv signature (slot `k` is the second parameter).
    let keyword_defaults = [NONE, NONE];
    let keyword_flags = [0i64, 0i64];
    let keyword_kinds = [0i64, 0i64];
    let keyword_names = [b"self".as_ptr() as i64, 4, b"k".as_ptr() as i64, 1];
    let keyword_function = function(second_argument, 2, "kw");
    hydro_function_meta(
        keyword_function,
        second_argument as *const () as usize as i64,
        2,
        keyword_names.as_ptr(),
        keyword_kinds.as_ptr(),
        keyword_defaults.as_ptr(),
        keyword_flags.as_ptr(),
    );
    let base = class("SA", NONE, &[("kw", keyword_function)]);
    let sub = class_multi("SB", &[base], &[], &[]);
    let instance = hydro_instance_new(sub);
    let proxy = method(instance, "$super_new", &[]);
    let keywords = [text("k"), int(5)];
    let via_keyword = hydro_method_kw(
        proxy,
        b"kw".as_ptr(),
        2,
        0,
        std::ptr::null(),
        1,
        keywords.as_ptr(),
    );
    assert_int(via_keyword, 5);
    assert_no_error();
    // A starred positional super call with one extra argument reaches the
    // two-argument base implementation.
    let star_base = class("SX", NONE, &[("two", function(describe_two, 2, "two"))]);
    let star_sub = class_multi("SY", &[star_base], &[], &[]);
    let star_instance = hydro_instance_new(star_sub);
    let star_proxy = method(star_instance, "$super_new", &[]);
    let star = list_of(&[int(1)]);
    let via_star = hydro_method_star_kw(
        star_proxy,
        b"two".as_ptr(),
        3,
        0,
        std::ptr::null(),
        star,
        0,
        std::ptr::null(),
        NONE,
    );
    assert_eq!(via_star, star_instance);
    assert_no_error();

    // `super()` in a classmethod: the class object is an accepted `__self__`
    // and the lookup continues after the defining class in the MRO.
    let class_base = class("CB", NONE, &[("who", function(describe, 1, "who"))]);
    let class_mid = class_multi("CM", &[class_base], &[], &[]);
    let class_leaf = class_multi("CL", &[class_mid], &[], &[]);
    let class_proxy = method(class_leaf, "$super_new", &[class_mid]);
    assert_eq!(method(class_proxy, "who", &[]), class_leaf);
    assert_no_error();
}

// metaclass from the bare `type` builtin and inheritance

#[test]
fn metaclass_from_bare_type_builtin_and_inheritance() {
    hydro_runtime_init();
    let type_builtin = hydro_builtin_function(b"type".as_ptr(), 4);

    // `class Meta(type): pass` accepts the builtin value as a base and maps it
    // to the native type class.
    let meta = class("Meta", type_builtin, &[]);
    assert_no_error();
    assert_eq!(
        call_builtin("isinstance", &[meta, type_builtin]),
        hydro_true()
    );
    // `type(Meta)` is the native type class (the bare `type` value is the
    // builtin function, so compare with the class `type(Plain)` reports).
    let plain = class("Plain", NONE, &[]);
    let native_type = call_builtin("type", &[plain]);
    assert_eq!(call_builtin("type", &[meta]), native_type);

    // A class whose metaclass is Meta (a metaclass instance).
    let c = class_multi_with_meta("C", &[], meta, &[], &[]);
    assert_no_error();
    assert_eq!(call_builtin("type", &[c]), meta);
    assert_eq!(call_builtin("isinstance", &[c, meta]), hydro_true());
    assert_eq!(call_builtin("isinstance", &[c, type_builtin]), hydro_true());

    // `class D(C)` inherits the metaclass without repeating it.
    let d = class("D", c, &[]);
    assert_no_error();
    assert_eq!(call_builtin("type", &[d]), meta);
    assert_eq!(call_builtin("isinstance", &[d, meta]), hydro_true());
    assert_repr(getattr(d, "__mro__"), "(<class 'D'>, <class 'C'>)");
    assert_no_error();
}
