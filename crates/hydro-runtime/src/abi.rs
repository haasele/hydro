use std::os::raw::c_uchar;

use crate::gc::roots;
use crate::{builtins, container, exc, fiber, format, heap, iter, methods, modules, ops, value};

// Constants (tagged representation, see ARCHITECTURE §3.1)

#[no_mangle]
pub extern "C" fn hydro_none() -> i64 {
    value::NONE
}

#[no_mangle]
pub extern "C" fn hydro_true() -> i64 {
    value::TRUE
}

#[no_mangle]
pub extern "C" fn hydro_false() -> i64 {
    value::FALSE
}

// Constructors

#[no_mangle]
pub extern "C" fn hydro_bool(b: i64) -> i64 {
    value::bool_value(value::truthy(b))
}

#[no_mangle]
pub extern "C" fn hydro_int(v: i64) -> i64 {
    // Identity for tagged immediates; tolerates raw literals from codegen.
    value::normalize_int(v)
}

/// Arbitrary-precision `int` literal: parses a decimal string (optional
/// leading `-`, no underscores or whitespace in IR buffers) and returns a
/// tagged immediate when the value fits, otherwise a heap `BigInt`.
///
/// Invalid text raises `ValueError` (the caller polls `hydro_exc_pending`).
#[no_mangle]
pub extern "C" fn hydro_bigint_from_str(ptr: *const c_uchar, len: usize) -> i64 {
    value::bigint_from_ptr(ptr, len)
}

#[no_mangle]
pub extern "C" fn hydro_str_new(ptr: *const c_uchar, len: usize) -> i64 {
    value::str_from_ptr(ptr, len)
}

/// Starts a mutable string accumulator for a proved `s = ""` / `s += ...`
/// loop and returns it. `init` is a `str`; the value never reaches user code,
/// [`hydro_str_builder_finish`] turns it back into a normal `str`.
#[no_mangle]
pub extern "C" fn hydro_str_builder_new(init: i64) -> i64 {
    value::str_builder_new(init)
}

/// `accumulator = accumulator + value` for a builder: appends strings without
/// copying the accumulated prefix and keeps the exact `+` semantics (and the
/// raised `TypeError`) for every other value. Returns the builder.
#[no_mangle]
pub extern "C" fn hydro_str_builder_append(builder: i64, value: i64) -> i64 {
    value::str_builder_append(builder, value)
}

/// Ends a builder and returns the accumulated text as a normal `str`.
#[no_mangle]
pub extern "C" fn hydro_str_builder_finish(builder: i64) -> i64 {
    value::str_builder_finish(builder)
}

/// `bytes` literal: copies `len` bytes from `ptr` into a bytes object.
#[no_mangle]
pub extern "C" fn hydro_bytes_new(ptr: *const c_uchar, len: usize) -> i64 {
    value::bytes_from_ptr(ptr, len)
}

/// `bytearray` literal: copies `len` bytes from `ptr` into a mutable
/// bytearray. A NULL buffer or zero length yields an empty bytearray.
#[no_mangle]
pub extern "C" fn hydro_bytearray_new(ptr: *const c_uchar, len: usize) -> i64 {
    value::bytearray_from_bytes(ptr, len)
}

/// `memoryview(value)`: creates a memoryview over a bytes or bytearray value.
/// Any other operand raises `TypeError` and returns `None`.
#[no_mangle]
pub extern "C" fn hydro_memoryview_new(value: i64) -> i64 {
    crate::value::memoryview_abi(value)
}

/// `bytes(...)`: the constructor (0 to 3 arguments).
#[no_mangle]
pub extern "C" fn hydro_bytes_build(argc: usize, argv: *const i64) -> i64 {
    builtins::bytes_build(argc, argv)
}

#[no_mangle]
pub extern "C" fn hydro_float_new(f: f64) -> i64 {
    value::float_new(f)
}

/// Complex literal constructor: one native `complex` instance carrying `real`
/// and `imag` (both binary64, signs preserved).
#[no_mangle]
pub extern "C" fn hydro_complex_new(real: f64, imag: f64) -> i64 {
    builtins::build_complex(real, imag)
}

/// One PEP 750 template interpolation (`string.templatelib.Interpolation`).
///
/// `expression` is the source text of the field's expression (a pointer/length
/// pair), `conversion` is `0` none / `1` `!s` / `2` `!r` / `3` `!a`,
/// `format_spec` is a tagged `str` or `None`, and `self_documenting` marks the
/// `=` debug specifier. Invalid input raises and returns `None`; this function
/// never panics.
#[no_mangle]
pub extern "C" fn hydro_interpolation_new(
    value: i64,
    expression: *const c_uchar,
    expression_len: usize,
    conversion: u8,
    format_spec: i64,
    self_documenting: u8,
) -> i64 {
    crate::core::template::interpolation_new(
        value,
        expression,
        expression_len,
        conversion,
        format_spec,
        self_documenting,
    )
}

/// A PEP 750 template string (`string.templatelib.Template`).
///
/// `strings` points to `strings_len` tagged `str` values and `interpolations`
/// to `interpolations_len` tagged `Interpolation` values. The
/// `strings_len == interpolations_len + 1` invariant is enforced; invalid
/// input raises and returns `None`; this function never panics.
#[no_mangle]
pub extern "C" fn hydro_template_new(
    strings_len: usize,
    strings: *const i64,
    interpolations_len: usize,
    interpolations: *const i64,
) -> i64 {
    crate::core::template::template_new(strings_len, strings, interpolations_len, interpolations)
}

/// `list` literal: copies `argc` tagged items from `argv`.
#[no_mangle]
pub extern "C" fn hydro_list_new(argc: usize, argv: *const i64) -> i64 {
    container::list_new(argc, argv)
}

/// `tuple` literal: copies `argc` tagged items from `argv`.
#[no_mangle]
pub extern "C" fn hydro_tuple_new(argc: usize, argv: *const i64) -> i64 {
    container::tuple_new(argc, argv)
}

/// Validates `a, b, ... = value` for lists, tuples, strings and bytes of
/// exactly `expected` elements. Returns `None` on success; raises `ValueError`
/// (length mismatch) or `TypeError` (non-sequence) otherwise.
#[no_mangle]
pub extern "C" fn hydro_unpack(value: i64, expected: usize) -> i64 {
    container::unpack(value, expected)
}

/// Unpacking-assignment source check: validates any iterable against
/// `expected` and returns the sequence to read elements from.
///
/// Lists, tuples, strings and bytes are returned unchanged; every other
/// iterable (including an existing iterator or generator) is materialised into
/// a fresh list. Errors match [`hydro_unpack`]: `ValueError` for a length
/// mismatch, `TypeError` for a non-iterable, with `None` returned on error.
#[no_mangle]
pub extern "C" fn hydro_unpack_seq(value: i64, expected: usize) -> i64 {
    container::unpack_seq(value, expected)
}

/// Starred unpacking assignment (`a, *rest, b = value`): `before` and `after`
/// count the plain targets around the starred one. Returns a fresh list whose
/// slot `before` is the middle list, or `None` with a pending exception.
#[no_mangle]
pub extern "C" fn hydro_unpack_star(value: i64, before: usize, after: usize) -> i64 {
    container::unpack_star(value, before, after)
}

/// `dict` literal from parallel key/value buffers. Unhashable keys raise
/// `TypeError` and return `None`.
#[no_mangle]
pub extern "C" fn hydro_dict_new(argc: usize, keys: *const i64, values: *const i64) -> i64 {
    container::dict_new(argc, keys, values)
}

/// `set` literal: dedupes `argc` tagged items while preserving first
/// occurrence. Unhashable items raise `TypeError` and return `None`.
#[no_mangle]
pub extern "C" fn hydro_set_new(argc: usize, argv: *const i64) -> i64 {
    container::set_new(argc, argv)
}

/// `set(iterable)` for any iterable (lists, tuples, sets, dicts, strings,
/// ranges, iterators, generators); a non-iterable raises `TypeError` and
/// returns `None`.
#[no_mangle]
pub extern "C" fn hydro_set_from(iterable: i64) -> i64 {
    container::set_from(iterable)
}

/// `range(start, stop, step)`; tagged integer arguments, `step != 0`.
#[no_mangle]
pub extern "C" fn hydro_range_new(start: i64, stop: i64, step: i64) -> i64 {
    builtins::range_new(start, stop, step)
}

/// `list(iterable)`.
#[no_mangle]
pub extern "C" fn hydro_list_from(iterable: i64) -> i64 {
    builtins::list_from(iterable)
}

/// `tuple(iterable)`.
#[no_mangle]
pub extern "C" fn hydro_tuple_from(iterable: i64) -> i64 {
    builtins::tuple_from(iterable)
}

/// `dict(iterable)`.
#[no_mangle]
pub extern "C" fn hydro_dict_from(iterable: i64) -> i64 {
    builtins::dict_from(iterable)
}

/// `sum(iterable, start)`.
#[no_mangle]
pub extern "C" fn hydro_sum(iterable: i64, start: i64) -> i64 {
    builtins::sum(iterable, start)
}

/// `enumerate(iterable, start)`: list of `(index, value)` tuples.
#[no_mangle]
pub extern "C" fn hydro_enumerate(iterable: i64, start: i64) -> i64 {
    builtins::enumerate(iterable, start)
}

/// `zip(argc, argv)`: list of tuples up to the shortest source.
#[no_mangle]
pub extern "C" fn hydro_zip(argc: usize, argv: *const i64) -> i64 {
    builtins::zip(argc, argv)
}

// First-class functions

/// Copies the function name passed by generated code. A missing or empty
/// buffer yields the fallback `"<function>"`.
fn function_name(ptr: *const c_uchar, len: usize) -> String {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return "<function>".to_string();
    }
    // SAFETY: ABI contract: `ptr..ptr+len` is readable; length checked above.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let text = String::from_utf8_lossy(bytes).into_owned();
    if text.is_empty() {
        "<function>".to_string()
    } else {
        text
    }
}

/// Decodes a required UTF-8 name, returning `None` for a NULL/empty buffer or
/// an invalid length. Empty names are not valid class or attribute names.
fn required_name(ptr: *const c_uchar, len: usize) -> Option<String> {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return None;
    }
    // SAFETY: ABI contract: `ptr..ptr+len` is readable; length checked above.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let text = String::from_utf8_lossy(bytes).into_owned();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Creates a class object: `class Name(Base):` with already-lowered methods
/// and frozen class attributes.
///
/// `names_ptr`/`funcs_ptr`/`kinds_ptr` point at `method_count` entries: method
/// names (tagged strings), function values and raw [`hydro_ir::MethodKind`] tags
/// (`0` normal, `1` property, `2` classmethod, `3` staticmethod).
/// `attr_names_ptr`/`attr_values_ptr` point at `attr_count` class attribute
/// name/value pairs; names are tagged strings and values any tagged value.
/// `base` is `NONE`, a class object, or a PEP 560 special form whose
/// `__mro_entries__` expands it into classes.
///
/// Duplicate method or attribute names keep their first position and the last
/// value, mirroring successive assignments in a class body. Malformed buffers,
/// names, method kinds and non-function methods raise `TypeError` and return
/// `None`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref, clippy::too_many_arguments)]
pub extern "C" fn hydro_class_new(
    name_ptr: *const c_uchar,
    name_len: usize,
    base: i64,
    method_count: usize,
    names_ptr: *const i64,
    funcs_ptr: *const i64,
    kinds_ptr: *const i64,
    attr_count: usize,
    attr_names_ptr: *const i64,
    attr_values_ptr: *const i64,
) -> i64 {
    let Some(name) = required_name(name_ptr, name_len) else {
        return exc::raise_with_message(exc::TYPE_ERROR, "class name must be a non-empty string");
    };
    let base = if let Some(function) = value::as_function(base) {
        match &function.body {
            value::FunctionBody::Native {
                module: "threading",
                name: "Thread",
            } => crate::modules::threading_thread_base(),
            _ => base,
        }
    } else {
        base
    };
    let base = value::normalize_class_base(base);
    // Non-class bases are not rejected here: PEP 560 special forms
    // (`TypedDict`, `Protocol`, `typing_extensions` fixtures) reach the
    // runtime as ordinary values whose `__mro_entries__` expands them into
    // real classes. `class_new_full` forwards the value and the runtime
    // reports the precise `TypeError` when the hook is absent.
    let mut methods: Vec<(String, i64, hydro_ir::MethodKind)> = Vec::new();
    if method_count > 0 {
        if names_ptr.is_null()
            || funcs_ptr.is_null()
            || kinds_ptr.is_null()
            || method_count
                > isize::MAX as usize / std::mem::size_of::<(String, i64, hydro_ir::MethodKind)>()
        {
            return exc::raise_with_message(
                exc::TYPE_ERROR,
                &format!(
                    "class definition received {method_count} method(s) without a method buffer"
                ),
            );
        }
        methods.reserve(method_count);
        // SAFETY: ABI contract: three buffers hold `method_count` readable entries; checked above.
        let names = unsafe { std::slice::from_raw_parts(names_ptr, method_count) };
        let funcs = unsafe { std::slice::from_raw_parts(funcs_ptr, method_count) };
        let kinds = unsafe { std::slice::from_raw_parts(kinds_ptr, method_count) };
        for ((name_value, function), kind_tag) in names.iter().zip(funcs).zip(kinds) {
            let Some(method_name) = value::as_str(*name_value) else {
                return exc::raise_with_message(
                    exc::TYPE_ERROR,
                    "class method name must be a string",
                );
            };
            if method_name.is_empty() {
                return exc::raise_with_message(
                    exc::TYPE_ERROR,
                    "class method name must not be empty",
                );
            }
            if value::as_function(*function).is_none() {
                return exc::raise_with_message(
                    exc::TYPE_ERROR,
                    &format!("class method '{method_name}' is not a function"),
                );
            }
            let Some(kind) = hydro_ir::MethodKind::from_abi_tag(*kind_tag) else {
                return exc::raise_with_message(
                    exc::TYPE_ERROR,
                    &format!("class method '{method_name}' has an unknown method kind"),
                );
            };
            let position = methods
                .iter()
                .position(|(existing, _, _)| existing.as_str() == method_name);
            match position {
                Some(index) => {
                    methods[index].1 = *function;
                    methods[index].2 = kind;
                }
                None => methods.push((method_name.to_string(), *function, kind)),
            }
        }
    }
    let mut attrs: Vec<(String, i64)> = Vec::new();
    if attr_count > 0 {
        if attr_names_ptr.is_null()
            || attr_values_ptr.is_null()
            || attr_count > isize::MAX as usize / std::mem::size_of::<(String, i64)>()
        {
            return exc::raise_with_message(
                exc::TYPE_ERROR,
                &format!(
                    "class definition received {attr_count} attribute(s) without an attribute buffer"
                ),
            );
        }
        attrs.reserve(attr_count);
        // SAFETY: ABI contract: both buffers hold `attr_count` readable values; checked above.
        let attr_names = unsafe { std::slice::from_raw_parts(attr_names_ptr, attr_count) };
        let attr_values = unsafe { std::slice::from_raw_parts(attr_values_ptr, attr_count) };
        for (name_value, attr_value) in attr_names.iter().zip(attr_values) {
            let Some(attr_name) = value::as_str(*name_value) else {
                return exc::raise_with_message(
                    exc::TYPE_ERROR,
                    "class attribute name must be a string",
                );
            };
            if attr_name.is_empty() {
                return exc::raise_with_message(
                    exc::TYPE_ERROR,
                    "class attribute name must not be empty",
                );
            }
            let position = attrs
                .iter()
                .position(|(existing, _)| existing.as_str() == attr_name);
            match position {
                Some(index) => attrs[index].1 = *attr_value,
                None => attrs.push((attr_name.to_string(), *attr_value)),
            }
        }
    }
    value::class_new_full(name, base, methods, attrs)
}

/// `class Name(Base1, Base2, metaclass=Meta): ...` with multiple bases.
///
/// `bases_ptr` holds `base_count` tagged bases: class objects, `None`, or
/// PEP 560 special forms whose `__mro_entries__` expands them,
/// `metaclass` is the tagged metaclass or `None`, and the method/attribute
/// buffers follow [`hydro_class_new`]'s layout. Forwards to the runtime's
/// multi-base constructor, which validates every operand, resolves the MRO and
/// raises `TypeError` on malformed input; it never panics.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref, clippy::too_many_arguments)]
pub extern "C" fn hydro_class_new_multi(
    name_ptr: *const c_uchar,
    name_len: usize,
    bases_ptr: *const i64,
    base_count: usize,
    metaclass: i64,
    names_ptr: *const i64,
    funcs_ptr: *const i64,
    kinds_ptr: *const i64,
    method_count: usize,
    attr_names_ptr: *const i64,
    attr_values_ptr: *const i64,
    attr_count: usize,
) -> i64 {
    crate::value::class_new_multi_abi(
        name_ptr,
        name_len,
        bases_ptr,
        base_count,
        metaclass,
        names_ptr,
        funcs_ptr,
        kinds_ptr,
        method_count,
        attr_names_ptr,
        attr_values_ptr,
        attr_count,
    )
}

/// Routes a dynamic `**mapping` operand into a class-constructor slot array.
///
/// `slots` holds one tagged value per `__init__` parameter after `self`, in
/// parameter order. `names` is an array of `name_count` `[ptr, len]` word
/// pairs (variadic slots use an empty name). A key that matches a bindable
/// name overwrites that slot and is removed from the `**kwargs` dict at
/// `kwarg_index`; an unmatched key is inserted into that dict, or raises
/// `TypeError` when the signature has no `**kwargs`. Returns `None`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn hydro_bind_class_kwargs(
    mapping: i64,
    names_ptr: *const i64,
    name_count: usize,
    slots: *mut i64,
    kwarg_index: i64,
    class_ptr: *const c_uchar,
    class_len: usize,
) -> i64 {
    let Some(class) = required_name(class_ptr, class_len) else {
        return exc::raise_with_message(exc::TYPE_ERROR, "class name must be a non-empty string");
    };
    if name_count > 0 && (names_ptr.is_null() || slots.is_null()) {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            "class constructor received parameters without a binding buffer",
        );
    }
    let Some(dict) = value::as_dict_data(mapping) else {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            &format!(
                "argument after ** must be a mapping, not '{}'",
                value::type_name(mapping)
            ),
        );
    };
    // Snapshot before mutating: `slots`/the kwargs dict may be updated while
    // iterating, and the mapping itself is merely a live argument.
    let entries = dict.entries().to_vec();
    let kwargs = if kwarg_index >= 0 {
        Some(slots.wrapping_add(kwarg_index as usize))
    } else {
        None
    };
    for (key, value) in entries {
        let Some(name) = value::as_str(key) else {
            return exc::raise_with_message(
                exc::TYPE_ERROR,
                &format!(
                    "class constructor keywords must be strings, not '{}'",
                    value::type_name(key)
                ),
            );
        };
        let mut matched = None;
        for index in 0..name_count {
            // SAFETY: ABI contract: `2 * name_count` readable words; NULL and empty names skipped.
            let ptr = unsafe { *names_ptr.add(index * 2) } as *const c_uchar;
            let len = unsafe { *names_ptr.add(index * 2 + 1) } as usize;
            if len == 0 {
                continue;
            }
            let candidate = unsafe { std::slice::from_raw_parts(ptr, len) };
            if candidate == name.as_bytes() {
                matched = Some(index);
                break;
            }
        }
        if let Some(index) = matched {
            // SAFETY: `name_count` slots were checked above.
            unsafe { *slots.add(index) = value };
            if let Some(kwargs_ptr) = kwargs {
                let kwargs_dict = unsafe { *kwargs_ptr };
                if value::as_dict_data(kwargs_dict)
                    .and_then(|data| data.find(key))
                    .is_some()
                {
                    let _ = crate::core::container::delete_subscript(kwargs_dict, key);
                }
            }
        } else if let Some(kwargs_ptr) = kwargs {
            let kwargs_dict = unsafe { *kwargs_ptr };
            crate::core::container::dict_set(kwargs_dict, key, value);
        } else {
            return exc::raise_with_message(
                exc::TYPE_ERROR,
                &format!("{class}() got an unexpected keyword argument '{name}'"),
            );
        }
    }
    value::NONE
}

/// Allocates an instance of `class` without running `__init__`.
///
/// Used by generated class-constructor calls, which bind the initializer
/// arguments themselves and then invoke the concrete `__init__` entry. A
/// non-class value raises `TypeError` and returns `None`.
#[no_mangle]
pub extern "C" fn hydro_instance_new(class: i64) -> i64 {
    if value::as_class(class).is_none() {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            &format!("class expected, got '{}'", value::type_name(class)),
        );
    }
    value::instance_new(class)
}

/// Returns the built-in exception class object for `tag` as a first-class
/// value (`x = ValueError`, `class E(Exception):`).
///
/// The class is an empty native class named after the tag (`Class.__name__`
/// and `repr` report it), cached per thread so repeated expressions share one
/// object. The cache is cleared by [`hydro_runtime_init`] (through `exc::init`)
/// before the heap is freed, so a later run never observes a dangling class
/// pointer. An unknown tag raises `RuntimeError` and returns `None`.
#[no_mangle]
pub extern "C" fn hydro_exception_class(tag: u32) -> i64 {
    exc::exception_class(tag)
}

/// Creates a function value from the generated wrapper entry point.
/// A `NULL` wrapper is an internal error (`RuntimeError`). `flags` bit 0
/// marks an `async def` wrapper (calls create a coroutine) and bit 1 a
/// generator wrapper (calls create a generator).
#[no_mangle]
pub extern "C" fn hydro_function_new(
    wrapper: *const c_uchar,
    arity: usize,
    name_ptr: *const c_uchar,
    name_len: usize,
    flags: u32,
) -> i64 {
    if wrapper.is_null() {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "function wrapper is NULL");
    }
    let name = function_name(name_ptr, name_len);
    value::function_new_with_flags(wrapper as usize, arity, name, flags)
}

/// Creates a closure value from a generated wrapper entry point, the
/// user-visible arity and `capture_count` captured tagged values. A `NULL`
/// wrapper or a missing/oversized capture buffer is an internal error
/// (`RuntimeError`). `flags` has the same meaning as in [`hydro_function_new`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn hydro_closure_new(
    wrapper: *const c_uchar,
    arity: usize,
    name_ptr: *const c_uchar,
    name_len: usize,
    capture_count: usize,
    captures_ptr: *const i64,
    flags: u32,
) -> i64 {
    if wrapper.is_null() {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "function wrapper is NULL");
    }
    if capture_count > 0 && (captures_ptr.is_null() || capture_count > isize::MAX as usize) {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "closure capture buffer is invalid");
    }
    let captures = if capture_count == 0 {
        Vec::new()
    } else {
        // SAFETY: ABI contract: `captures_ptr..+capture_count` holds readable values; checked.
        unsafe { std::slice::from_raw_parts(captures_ptr, capture_count) }.to_vec()
    };
    let name = function_name(name_ptr, name_len);
    value::closure_new_with_flags(wrapper as usize, arity, name, captures, flags)
}

/// Allocates a closure cell holding `value`.
#[no_mangle]
pub extern "C" fn hydro_cell_new(value: i64) -> i64 {
    value::cell_new(value)
}

/// Reads the current value of a closure cell.
#[no_mangle]
pub extern "C" fn hydro_cell_get(cell: i64) -> i64 {
    value::cell_get(cell)
}

/// Stores `value` into a closure cell (shared by `nonlocal` writers).
#[no_mangle]
pub extern "C" fn hydro_cell_set(cell: i64, value: i64) {
    value::cell_set(cell, value)
}

/// Stores full parameter metadata and the uniform full-argv trampoline on a
/// generated function value.
///
/// `names` is `[ptr, len]` word pairs, `kinds` uses 0=positional, 1=*args,
/// 2=keyword-only, 3=**kwargs, and `defaults`/`default_flags` describe the
/// literal default per slot. The values become GC edges of the function object.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn hydro_function_meta(
    function: i64,
    full_entry: i64,
    count: usize,
    names: *const i64,
    kinds: *const i64,
    defaults: *const i64,
    default_flags: *const i64,
) -> i64 {
    if count > 0
        && (names.is_null() || kinds.is_null() || defaults.is_null() || default_flags.is_null())
    {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            "function metadata received parameters without a metadata buffer",
        );
    }
    if count > isize::MAX as usize / std::mem::size_of::<i64>() {
        return exc::raise_with_message(exc::TYPE_ERROR, "function metadata is too large");
    }
    let mut signature = Vec::with_capacity(count);
    for index in 0..count {
        let ptr = unsafe { *names.add(index * 2) } as *const c_uchar;
        let len = unsafe { *names.add(index * 2 + 1) } as usize;
        let name = if len == 0 {
            String::new()
        } else {
            match required_name(ptr, len) {
                Some(name) => name,
                None => {
                    return exc::raise_with_message(
                        exc::TYPE_ERROR,
                        "function parameter name must be a string",
                    )
                }
            }
        };
        let kind = match unsafe { *kinds.add(index) } {
            0 => hydro_ir::ParamKind::Positional,
            1 => hydro_ir::ParamKind::VarArg,
            2 => hydro_ir::ParamKind::KwOnly,
            3 => hydro_ir::ParamKind::KwArg,
            4 => hydro_ir::ParamKind::PosOnly,
            _ => {
                return exc::raise_with_message(
                    exc::TYPE_ERROR,
                    "function parameter kind is invalid",
                )
            }
        };
        let default = if unsafe { *default_flags.add(index) } != 0 {
            Some(unsafe { *defaults.add(index) })
        } else {
            None
        };
        signature.push(value::ParamMeta {
            name,
            kind,
            default,
        });
    }
    value::function_set_full_entry(function, full_entry as usize);
    value::function_set_signature(function, signature);
    value::NONE
}

/// Attaches the user-visible positional parameter names of a generated wrapper
/// to its function value. `names` points at `2 * count` words in
/// `[name_ptr, name_len, ...]` order, mirroring the keyword buffers of
/// `hydro_method_kw`; the names are only used to bind `functools.partial`
/// keywords. Returns the function value; a malformed buffer is ignored.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn hydro_function_params(function: i64, count: usize, names: *const i64) -> i64 {
    if count == 0 || names.is_null() || count > isize::MAX as usize / 16 {
        return function;
    }
    // SAFETY: caller passes `2 * count` readable words (codegen stack array); checked.
    let words = unsafe { std::slice::from_raw_parts(names, count * 2) };
    let params = words
        .chunks(2)
        .map(|pair| {
            let ptr = pair[0] as usize as *const c_uchar;
            let len = pair[1] as usize;
            if ptr.is_null() || len == 0 || len > isize::MAX as usize {
                return String::new();
            }
            // SAFETY: caller passes a readable `name_ptr..name_ptr+len` range per pair; checked.
            String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) }).into_owned()
        })
        .collect();
    value::function_set_params(function, params);
    function
}

/// Copies `argc` tagged arguments for a new fiber. `NULL` with `argc > 0` and
/// over-long counts are reported as an internal error.
fn copy_fiber_args(argc: usize, argv: *const i64) -> Option<Vec<i64>> {
    if argc == 0 {
        return Some(Vec::new());
    }
    if argv.is_null() || argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return None;
    }
    // SAFETY: ABI contract: `argv..argv+argc` is readable; length checked above.
    Some(unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec())
}

/// Creates a lazy generator object from a generated wrapper entry point. The
/// body does not run until the first `hydro_iter_next`.
#[no_mangle]
pub extern "C" fn hydro_generator_new(entry: *const c_uchar, argc: usize, argv: *const i64) -> i64 {
    fiber_new(entry, argc, argv, fiber::FiberKind::Generator)
}

/// Creates a coroutine object from a generated wrapper entry point. The body
/// does not run until the first `hydro_await`.
#[no_mangle]
pub extern "C" fn hydro_coroutine_new(entry: *const c_uchar, argc: usize, argv: *const i64) -> i64 {
    fiber_new(entry, argc, argv, fiber::FiberKind::Coroutine)
}

/// Creates a lazy async generator object from a generated wrapper entry point.
/// The body does not run until the first `__anext__`/`asend`.
#[no_mangle]
pub extern "C" fn hydro_async_generator_new(
    entry: *const c_uchar,
    argc: usize,
    argv: *const i64,
) -> i64 {
    fiber_new(entry, argc, argv, fiber::FiberKind::AsyncGenerator)
}

fn fiber_new(entry: *const c_uchar, argc: usize, argv: *const i64, kind: fiber::FiberKind) -> i64 {
    if entry.is_null() {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "fiber entry point is NULL");
    }
    let Some(args) = copy_fiber_args(argc, argv) else {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "fiber argument buffer is invalid");
    };
    let created = match kind {
        fiber::FiberKind::Generator => {
            value::fiber_generator_new(entry as usize, String::new(), args)
        }
        fiber::FiberKind::Coroutine => {
            value::fiber_coroutine_new(entry as usize, String::new(), args)
        }
        fiber::FiberKind::AsyncGenerator => {
            value::fiber_async_generator_new(entry as usize, String::new(), args)
        }
    };
    match created {
        Some(fiber) => fiber,
        None => exc::raise_with_message(
            exc::RUNTIME_ERROR,
            "cannot allocate a generator/coroutine stack",
        ),
    }
}

/// `await value`: drives a coroutine object to completion. Values that are
/// not fibers are returned unchanged.
#[no_mangle]
pub extern "C" fn hydro_await(value: i64) -> i64 {
    fiber::await_value(value)
}

/// `yield value`: suspends the running generator and returns the value sent
/// on resume (`None` for plain iteration). Outside a running generator this
/// raises `RuntimeError`.
#[no_mangle]
pub extern "C" fn hydro_yield(value: i64) -> i64 {
    fiber::yield_value(value)
}

/// `hydro_yield` for generated code.
///
/// Returns [`fiber::YIELD_ERROR`] (not a valid tagged value) when the
/// suspension point raised. Generated code branches on it instead of making
/// a separate `hydro_exc_pending` call at every yield; the pending exception
/// remains the authoritative state.
#[no_mangle]
pub extern "C" fn hydro_yield_checked(value: i64) -> i64 {
    fiber::yield_value_checked(value)
}

/// `yield from value`: delegates iteration to `value`. Each delegated item is
/// yielded through the running generator; `send` values, `throw` exceptions
/// and `close` are forwarded to a delegated generator. Returns the delegate's
/// return value (`None` for a plain iterator).
#[no_mangle]
pub extern "C" fn hydro_yield_from(value: i64) -> i64 {
    fiber::yield_from_value(value)
}

/// Creates a function value for a builtin referenced as a first-class value.
/// The name is the Python-visible spelling (`int`, `map`, ...); unknown names
/// raise `AttributeError` and return `None`.
#[no_mangle]
pub extern "C" fn hydro_builtin_function(name_ptr: *const c_uchar, name_len: usize) -> i64 {
    let name = function_name(name_ptr, name_len);
    match hydro_ir::Builtin::from_name(&name) {
        Some(builtin) => value::builtin_function_new(builtin),
        None => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("unknown builtin function '{name}'"),
        ),
    }
}

/// `fn(argc, argv)`: indirect call of a function value.
#[no_mangle]
pub extern "C" fn hydro_call(function: i64, argc: usize, argv: *const i64) -> i64 {
    builtins::call(function, argc, argv)
}

/// `fn(args..., *star)`: indirect call with one starred positional operand.
///
/// `argc`/`argv` are the positional operands that preceded the star; `star`
/// must be a list or tuple whose elements are appended to them. Any other
/// value raises `TypeError`. Argument evaluation order is the caller's
/// responsibility (callee, then `args`, then `star`).
#[no_mangle]
pub extern "C" fn hydro_call_star(function: i64, argc: usize, argv: *const i64, star: i64) -> i64 {
    builtins::call_star(function, argc, argv, star)
}

/// `f(args..., name=value...)` for a function value.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn hydro_call_kw(
    function: i64,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    crate::core::builtins::call_kw(function, argc, argv, kwargc, kw_argv)
}

/// `f(prefix_args..., *star, named_kw..., **star_map)`.
///
/// `star` and `star_map` are the tagged `None` value when the operand is
/// absent. `kw_argv` holds `2 * kwargc` tagged values in `[name, value, ...]`
/// order. Forwarded to the runtime binder; invalid operands raise and return
/// `None`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn hydro_call_star_kw(
    function: i64,
    argc: usize,
    argv: *const i64,
    star: i64,
    kwargc: usize,
    kw_argv: *const i64,
    star_map: i64,
) -> i64 {
    crate::core::builtins::call_star_kw(function, argc, argv, star, kwargc, kw_argv, star_map)
}

/// A call whose operands the frontend could not bind: the runtime callable has
/// no keyword slot (a compiled extension member, or a coroutine/generator with
/// a dynamic signature).
///
/// The generated code evaluates the callee and every operand (in source
/// order) before calling this helper, so side effects still happen; the helper
/// itself always raises `RuntimeError`. It returns `None` with a pending
/// exception, which the codegen propagates like any other failing runtime
/// call.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn hydro_deferred_call(argc: usize, argv: *const i64) -> i64 {
    // The evaluated operands are intentionally unused: their evaluation
    // already happened, and the call cannot be represented by the static ABI.
    let _ = (argc, argv);
    exc::raise_with_message(
        exc::RUNTIME_ERROR,
        "this callable does not accept keyword arguments at runtime",
    )
}

/// Arity failure raised by a generated wrapper when a dynamic signature
/// receives more positional arguments than it can bind (no `*args` slot).
#[no_mangle]
pub extern "C" fn hydro_arity_error(expected: i64, actual: i64) -> i64 {
    exc::raise_with_message(
        exc::TYPE_ERROR,
        &format!("function takes at most {expected} positional arguments ({actual} given)"),
    )
}

/// `map(function, iterable)`: materialised list of results.
#[no_mangle]
pub extern "C" fn hydro_map(function: i64, iterable: i64) -> i64 {
    builtins::map(function, iterable)
}

/// `filter(function_or_none, iterable)`: materialised filtered list.
#[no_mangle]
pub extern "C" fn hydro_filter(function: i64, iterable: i64) -> i64 {
    builtins::filter(function, iterable)
}

// Builtin modules

/// `module.function(args...)` for a name from `hydro_ir::stdlib`.
///
/// Names are decoded as UTF-8, arity is validated against the central
/// spec, and unknown modules/members raise `AttributeError`. Returns the
/// result value or `None` with a pending exception.
#[no_mangle]
pub extern "C" fn hydro_module_call(
    module: *const c_uchar,
    module_len: usize,
    function: *const c_uchar,
    function_len: usize,
    argc: usize,
    argv: *const i64,
) -> i64 {
    modules::module_call(module, module_len, function, function_len, argc, argv)
}

/// Fast-path fallback for `math.floor(math.sqrt(x))` with an `int` `x`.
///
/// Codegen inlines the non-negative tagged-int case as unboxed `f64`
/// operations; this helper handles the remaining values with the generic
/// semantics (negative inputs raise the `math.sqrt` domain `ValueError`).
/// Returns the tagged `int` result or `None` with a pending exception.
#[no_mangle]
pub extern "C" fn hydro_math_floor_sqrt_int(value: i64) -> i64 {
    modules::floor_sqrt_int(value)
}

/// Keyword variant of [`hydro_module_call`]: `kw_argv` holds `2 * kwargc` tagged
/// values in `[name, value, ...]` order. Only specs that declare dynamic
/// keywords (`functools.partial`) accept them; every other spec raises
/// `TypeError` instead of dropping the names.
#[no_mangle]
pub extern "C" fn hydro_module_call_kw(
    module: *const c_uchar,
    module_len: usize,
    function: *const c_uchar,
    function_len: usize,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    modules::module_call_kw(
        module,
        module_len,
        function,
        function_len,
        argc,
        argv,
        kwargc,
        kw_argv,
    )
}

/// `module.name`: a constant value or a native function object.
///
/// Native functions carry their arity from the central spec and are
/// callable through `hydro_call`/`hydro_map`/`hydro_filter`. Unknown names raise
/// `AttributeError` and return `None`.
#[no_mangle]
pub extern "C" fn hydro_module_get(
    module: *const c_uchar,
    module_len: usize,
    name: *const c_uchar,
    name_len: usize,
) -> i64 {
    modules::module_get(module, module_len, name, name_len)
}

/// `module.name = value` for a module-level global.
///
/// Module globals live in a process-wide side store keyed by module and name,
/// so every thread observes the binding; the stored value is also installed as
/// a process-wide GC root. Invalid ABI buffers raise `TypeError`. Returns
/// `None`.
#[no_mangle]
pub extern "C" fn hydro_global_set(
    module_ptr: *const c_uchar,
    module_len: usize,
    name_ptr: *const c_uchar,
    name_len: usize,
    value: i64,
) -> i64 {
    let (Some(module), Some(name)) = (
        required_name(module_ptr, module_len),
        required_name(name_ptr, name_len),
    ) else {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            "module global name must be a non-empty string",
        );
    };
    crate::core::globals::set_value(&module, &name, value)
}

/// `del module.name`: removes a module-level global. A missing binding raises
/// `NameError` and returns `None`; malformed names raise `TypeError`.
#[no_mangle]
pub extern "C" fn hydro_global_delete(
    module_ptr: *const c_uchar,
    module_len: usize,
    name_ptr: *const c_uchar,
    name_len: usize,
) -> i64 {
    let (Some(module), Some(name)) = (
        required_name(module_ptr, module_len),
        required_name(name_ptr, name_len),
    ) else {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            "module global name must be a non-empty string",
        );
    };
    crate::core::globals::delete_value(&module, &name)
}

/// `module.name` for a module-level global.
///
/// Reads the runtime global store and then the module's cached `globals()`
/// view, so names bound with `globals()[name] = value` resolve without an
/// intervening `globals()` call. A missing entry raises CPython's `NameError`
/// (`name '<name>' is not defined`) with a pending exception and returns
/// `None`. Invalid ABI buffers raise `TypeError` instead.
#[no_mangle]
pub extern "C" fn hydro_global_get(
    module_ptr: *const c_uchar,
    module_len: usize,
    name_ptr: *const c_uchar,
    name_len: usize,
) -> i64 {
    let (Some(module), Some(name)) = (
        required_name(module_ptr, module_len),
        required_name(name_ptr, name_len),
    ) else {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            "module global name must be a non-empty string",
        );
    };
    crate::core::globals::get(&module, &name)
}

/// `module_name` used as a value (`getattr(module_name, attribute)`).
///
/// Materialises the runtime module object for an import name, reusing the
/// `sys.modules` entry so every read shares one identity. Its attributes are
/// resolved on demand by `hydro_getattr`/`getattr`. An empty or malformed name
/// raises `TypeError` and returns `None`.
#[no_mangle]
pub extern "C" fn hydro_module_value(name_ptr: *const c_uchar, name_len: usize) -> i64 {
    let Some(name) = required_name(name_ptr, name_len) else {
        return exc::raise_with_message(exc::TYPE_ERROR, "module name must be a non-empty string");
    };
    modules::module_value_by_name(&name)
}

/// `extension_module_name` used as a value (`import ext as e; getattr(e, n)`).
///
/// Materialises a module object that keeps the compiled shared-object `path`;
/// attribute reads dispatch through the extension ABI. The value is cached
/// under its import name, so repeated reads and `sys.modules` share identity.
/// Empty or malformed buffers raise `TypeError`.
#[no_mangle]
pub extern "C" fn hydro_extension_module_value(
    name_ptr: *const c_uchar,
    name_len: usize,
    path_ptr: *const c_uchar,
    path_len: usize,
) -> i64 {
    let (Some(name), Some(path)) = (
        required_name(name_ptr, name_len),
        required_name(path_ptr, path_len),
    ) else {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            "extension module name and path must be non-empty strings",
        );
    };
    modules::extension_module_value(&name, &path)
}

/// Name bound by an import statement that was not linked at compile time.
///
/// Python defers imports until the statement runs; Hydro resolves imports while
/// compiling, so evaluating the name resolves it from the runtime module
/// registry instead of importing anything. A module alias returns the module
/// value; `from <module> import <member>` returns the attribute. A module that
/// is not registered raises `ModuleNotFoundError` and a member the module does
/// not provide raises `ImportError` (`cannot import name '<member>' from
/// '<module>'`), so `try: ... except ImportError:` handlers run with CPython's
/// error taxonomy. An empty/missing member means the name is a module alias
/// (`import x as y`); a non-empty `path` names the shared object of a compiled
/// extension module, which the runtime materialises lazily.
#[no_mangle]
pub extern "C" fn hydro_deferred_import(
    module_ptr: *const c_uchar,
    module_len: usize,
    member_ptr: *const c_uchar,
    member_len: usize,
    path_ptr: *const c_uchar,
    path_len: usize,
) -> i64 {
    let Some(module) = required_name(module_ptr, module_len) else {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            "deferred import name must be a non-empty string",
        );
    };
    let member = required_name(member_ptr, member_len);
    let path = required_name(path_ptr, path_len);
    builtins::deferred_import(&module, member.as_deref(), path.as_deref())
}

// CPython limited-API extensions

/// `extension.function(args...)` for a loaded `abi3` extension module.
///
/// `path` is the shared-object path resolved by the frontend and `member` the
/// Python attribute name. Unknown modules/attributes raise `AttributeError`,
/// call failures propagate as pending exceptions. Returns the result value or
/// `None` with a pending exception.
#[no_mangle]
pub extern "C" fn hydro_extension_call(
    path: *const c_uchar,
    path_len: usize,
    member: *const c_uchar,
    member_len: usize,
    argc: usize,
    argv: *const i64,
) -> i64 {
    modules::extension::call(path, path_len, member, member_len, argc, argv)
}

/// `extension.member`: a scalar value or a callable function object.
///
/// Callables become `KIND_EXT_FUNCTION` values and remain callable through
/// `hydro_call`. Unknown names raise `AttributeError`; returns `None` with a
/// pending exception.
#[no_mangle]
pub extern "C" fn hydro_extension_get(
    path: *const c_uchar,
    path_len: usize,
    member: *const c_uchar,
    member_len: usize,
) -> i64 {
    modules::extension::get(path, path_len, member, member_len)
}

// Comprehension builders

/// Appends a list-comprehension element. Non-lists raise `TypeError`;
/// always returns `None` otherwise.
#[no_mangle]
pub extern "C" fn hydro_list_append(list: i64, value: i64) -> i64 {
    container::list_append(list, value)
}

/// Adds a set-comprehension element unless already present. Unhashable
/// values and non-sets raise `TypeError`; returns `None`.
#[no_mangle]
pub extern "C" fn hydro_set_add(set: i64, value: i64) -> i64 {
    container::set_add(set, value)
}

/// `dict[key] = value` for a dict-comprehension result: an existing key
/// keeps its first position and takes the last value. Unhashable keys and
/// non-dicts raise `TypeError`; returns `None`.
#[no_mangle]
pub extern "C" fn hydro_dict_set(dict: i64, key: i64, value: i64) -> i64 {
    container::dict_set(dict, key, value)
}

/// `{**left, **right}`: a fresh dict with `left`'s entries followed by
/// `right`'s. Duplicate keys keep their first position and take the right
/// value. Non-dict operands raise `TypeError`; returns `None` then.
#[no_mangle]
pub extern "C" fn hydro_dict_merge(left: i64, right: i64) -> i64 {
    builtins::dict_merge(left, right)
}

// Indexing and iteration

/// `object[index]` for lists, strings, dicts and tuples.
#[no_mangle]
pub extern "C" fn hydro_subscript(obj: i64, index: i64) -> i64 {
    container::subscript(obj, index)
}

/// `object[index] = value` for lists and dicts. Returns `None`; errors are
/// reported through the pending exception.
#[no_mangle]
pub extern "C" fn hydro_subscript_set(obj: i64, index: i64, new_value: i64) -> i64 {
    container::subscript_set(obj, index, new_value)
}

/// `del object[index]` for lists and dicts. A list index out of range raises
/// `IndexError`, a missing dict key `KeyError` and any other receiver
/// `TypeError`. Returns `None`; errors are reported through the pending
/// exception.
#[no_mangle]
pub extern "C" fn hydro_delete_subscript(obj: i64, index: i64) -> i64 {
    container::delete_subscript(obj, index)
}

/// `object[start:stop:step]` for lists, strings, tuples, bytes-likes and user
/// instances (through `__getitem__` with a real `slice` object). A tagged
/// `None` in any bound position means the argument was omitted. Errors are
/// reported through the pending exception and yield `None`.
#[no_mangle]
pub extern "C" fn hydro_slice(obj: i64, start: i64, stop: i64, step: i64) -> i64 {
    container::slice(obj, start, stop, step)
}

/// `object[start:stop:step] = value` for lists, `bytearray`, `memoryview` and
/// user instances (through `__setitem__` with a real `slice` object). A tagged
/// `None` in any bound position means the argument was omitted. The right-hand
/// side may be any iterable; a step of 1 splices it in, an extended slice
/// requires a matching length (`ValueError`). Strings, bytes and tuples are
/// immutable (`TypeError`).
/// Returns `None`; errors are reported through the pending exception.
#[no_mangle]
pub extern "C" fn hydro_slice_set(
    obj: i64,
    start: i64,
    stop: i64,
    step: i64,
    new_value: i64,
) -> i64 {
    container::slice_set(obj, start, stop, step, new_value)
}

/// `del object[start:stop:step]` for lists. A tagged `None` in any bound
/// position means the argument was omitted; `del xs[:]` clears the list.
/// Strings, bytes and tuples are immutable (`TypeError`), and a dict reports
/// the unhashable `slice` key. Returns `None`; errors are reported through the
/// pending exception.
#[no_mangle]
pub extern "C" fn hydro_delete_slice(obj: i64, start: i64, stop: i64, step: i64) -> i64 {
    container::delete_slice(obj, start, stop, step)
}

/// `obj.name` for a data attribute of a native value or class instance.
/// Unknown receivers and names raise `AttributeError`; the runtime never
/// aborts.
#[no_mangle]
pub extern "C" fn hydro_getattr(obj: i64, name: *const c_uchar, name_len: usize) -> i64 {
    methods::getattr(obj, name, name_len)
}

/// `hasattr(obj, name)`: a bool. Only `AttributeError` is swallowed.
#[no_mangle]
pub extern "C" fn hydro_hasattr(obj: i64, name: *const c_uchar, name_len: usize) -> i64 {
    methods::hasattr(obj, name, name_len)
}

/// `obj.name = value` for modules, class instances and classes.
///
/// Module assignment stores an override keyed by module name, so reads, calls
/// and constants resolve it before the native spec default; user modules also
/// write the value into their globals store. Class namespaces are immutable;
/// assigning to a class raises `TypeError`, other receivers `AttributeError`.
/// The stored value is published with a GC insertion barrier; returns `None`
/// on success.
#[no_mangle]
pub extern "C" fn hydro_setattr(
    obj: i64,
    name: *const c_uchar,
    name_len: usize,
    new_value: i64,
) -> i64 {
    methods::setattr(obj, name, name_len, new_value)
}

/// `del obj.name` for modules, class instances and classes. Deleting a module
/// attribute drops the override (restoring the native spec default) or the user
/// module's global binding. A missing attribute raises `AttributeError`,
/// deleting from a class raises `TypeError` (the namespace is frozen) and every
/// other receiver raises `AttributeError`. Returns `None` on success.
#[no_mangle]
pub extern "C" fn hydro_delete_attr(obj: i64, name: *const c_uchar, name_len: usize) -> i64 {
    methods::delete_attr(obj, name, name_len)
}

/// Dynamic method call: `obj.<name>(args...)`. Dispatch is by receiver
/// kind; unknown methods raise `AttributeError`, never abort the process.
#[no_mangle]
pub extern "C" fn hydro_method(
    obj: i64,
    name: *const c_uchar,
    name_len: usize,
    argc: usize,
    argv: *const i64,
) -> i64 {
    methods::call(obj, name, name_len, argc, argv)
}

/// Dynamic method call with keyword arguments:
/// `obj.<name>(args..., name=value...)`. `kw_argv` holds `2 * kwargc` tagged
/// values in `[name, value, ...]` order. Without keywords this behaves like
/// [`hydro_method`].
#[no_mangle]
pub extern "C" fn hydro_method_kw(
    obj: i64,
    name: *const c_uchar,
    name_len: usize,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    methods::call_kw(obj, name, name_len, argc, argv, kwargc, kw_argv)
}

/// Starred dynamic method call: `obj.<name>(*star, name=value...)`.
///
/// `star` must be a list or tuple; its elements become the positional
/// arguments (in order) of the method, followed by the keyword pairs in
/// `kw_argv` (`2 * kwargc` tagged values, `[name, value, ...]`). A non-list/
/// tuple operand raises `TypeError` and returns `None`.
#[no_mangle]
pub extern "C" fn hydro_method_star(
    obj: i64,
    name: *const c_uchar,
    name_len: usize,
    star: i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    let items = match crate::core::builtins::star_sequence(star) {
        Ok(items) => items,
        Err(err) => return err,
    };
    // The element copies stay reachable through `star`, which is a live
    // argument for the whole call, so a collection during dispatch cannot
    // lose them.
    methods::call_kw(
        obj,
        name,
        name_len,
        items.len(),
        items.as_ptr(),
        kwargc,
        kw_argv,
    )
}

/// `receiver.name(prefix_args..., *star, named_kw..., **star_map)`.
///
/// `star` and `star_map` are the tagged `None` value when the operand is
/// absent; `kw_argv` holds `2 * kwargc` tagged values in `[name, value, ...]`
/// order. Forwarded to the runtime binder; invalid operands raise and return
/// `None`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn hydro_method_star_kw(
    receiver: i64,
    name_ptr: *const c_uchar,
    name_len: usize,
    argc: usize,
    argv: *const i64,
    star: i64,
    kwargc: usize,
    kw_argv: *const i64,
    star_map: i64,
) -> i64 {
    crate::core::methods::method_star_kw(
        receiver, name_ptr, name_len, argc, argv, star, kwargc, kw_argv, star_map,
    )
}

/// Creates an iterator over a list, string, dict, tuple or generator object.
#[no_mangle]
pub extern "C" fn hydro_iter_new(obj: i64) -> i64 {
    iter::iter_new(obj)
}

/// Writes the next element to `out` and returns `1`, or returns `0` at the
/// end. `NULL` out-pointers are rejected without writing.
#[no_mangle]
pub extern "C" fn hydro_iter_next(iterator: i64, out: *mut i64) -> i64 {
    iter::iter_next(iterator, out)
}

/// `iter(obj)`: an existing iterator/generator is returned unchanged,
/// otherwise a new iterator is created. Non-iterables raise `TypeError`.
#[no_mangle]
pub extern "C" fn hydro_iter(obj: i64) -> i64 {
    iter::iter_of(obj)
}

/// `next(obj[, default])`: the next element of an iterator/generator.
/// `has_default` is `1` when the tagged `default` argument was supplied.
/// Exhaustion raises `StopIteration` without a default.
#[no_mangle]
pub extern "C" fn hydro_next(obj: i64, default: i64, has_default: i64) -> i64 {
    iter::next_value(obj, default, has_default != 0)
}

// Operators

#[no_mangle]
pub extern "C" fn hydro_binop(op: u32, a: i64, b: i64) -> i64 {
    ops::binop(op, a, b)
}

#[no_mangle]
pub extern "C" fn hydro_unop(op: u32, a: i64) -> i64 {
    ops::unop(op, a)
}

/// Branch helper: returns `1` for truthy and `0` for falsy (C-style),
/// unlike `hydro_bool`, which returns the tagged `True`/`False` values.
#[no_mangle]
pub extern "C" fn hydro_truthy(v: i64) -> i64 {
    if value::truthy(v) {
        1
    } else {
        0
    }
}

// Builtins

#[no_mangle]
pub extern "C" fn hydro_print(argc: usize, argv: *const i64) -> i64 {
    builtins::print(argc, argv)
}

#[no_mangle]
pub extern "C" fn hydro_len(v: i64) -> i64 {
    builtins::len(v)
}

/// `ord(character)`: the Unicode scalar value of a one-character string.
/// Any other string length or a non-string argument raises `TypeError` and
/// returns `None`.
#[no_mangle]
pub extern "C" fn hydro_ord(v: i64) -> i64 {
    builtins::ord_value(v)
}

/// `chr(codepoint)`: the one-character string for an integer in
/// `0..=0x10FFFF`. Surrogate code points raise `ValueError` (Hydro strings are
/// UTF-8), as does an out-of-range integer; a non-integer raises `TypeError`.
#[no_mangle]
pub extern "C" fn hydro_chr(v: i64) -> i64 {
    builtins::chr_value(v)
}

#[no_mangle]
pub extern "C" fn hydro_to_str(v: i64) -> i64 {
    builtins::to_str(v)
}

/// `repr(value)`: Python's representation string.
#[no_mangle]
pub extern "C" fn hydro_to_repr(v: i64) -> i64 {
    builtins::to_repr(v)
}

/// `hash(v)`: dict-key hash mapped into a tagged integer; unhashable values
/// raise `TypeError`.
#[no_mangle]
pub extern "C" fn hydro_hash(v: i64) -> i64 {
    builtins::hash(v)
}

#[no_mangle]
pub extern "C" fn hydro_to_int(v: i64) -> i64 {
    builtins::to_int(v)
}

#[no_mangle]
pub extern "C" fn hydro_to_float(v: i64) -> i64 {
    builtins::to_float(v)
}

#[no_mangle]
pub extern "C" fn hydro_abs(v: i64) -> i64 {
    builtins::abs(v)
}

/// `all(iterable)`: tagged `True` when every element is truthy, `False` for a
/// falsy element. Non-iterables raise `TypeError` and return `None`.
#[no_mangle]
pub extern "C" fn hydro_all(iterable: i64) -> i64 {
    builtins::all(iterable)
}

/// Copies the static format spec passed by generated code. A missing buffer
/// is treated as the empty spec.
fn format_spec(ptr: *const c_uchar, len: usize) -> String {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return String::new();
    }
    // SAFETY: ABI contract: `ptr..ptr+len` is readable; length checked above.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Formats one f-string field: `value` with the static `spec` and a
/// conversion flag (`0` none, `1` `!s`, `2` `!r`, `3` `!a`).
///
/// Invalid specs raise `ValueError`; wrong operand types raise `TypeError`.
#[no_mangle]
pub extern "C" fn hydro_format_value(
    value: i64,
    spec: *const c_uchar,
    spec_len: usize,
    conversion: u8,
) -> i64 {
    let spec = format_spec(spec, spec_len);
    match format::format_value(value, &spec, conversion) {
        Ok(text) => value::str_alloc(text),
        Err(error) => exc::raise_with_message(error.tag, &error.message),
    }
}

// Source location and frames

#[no_mangle]
pub extern "C" fn hydro_set_file(ptr: *const c_uchar, len: usize) {
    exc::set_file_ptr(ptr, len);
}

#[no_mangle]
pub extern "C" fn hydro_set_line(line: u32) {
    exc::set_line(line);
}

#[no_mangle]
pub extern "C" fn hydro_frame_enter(name: *const c_uchar, len: usize) {
    exc::frame_enter_ptr(name, len);
}

#[no_mangle]
pub extern "C" fn hydro_frame_leave() {
    exc::frame_leave();
}

/// Binds the innermost activation to its live locals and code metadata.
///
/// `base` is the same precise slot array passed to [`hydro_gc_frame_push`]
/// (`locals` entries of it are real locals, in slot order); `names` is a
/// NUL-separated UTF-8 list of those local names. `module` is the key of the
/// module globals the frame belongs to. The pointer pair is only stored for
/// the lifetime of the activation, and the runtime never dereferences an
/// invalid one: an unusable argument list leaves the frame with metadata but
/// no live locals, and never panics.
///
/// # Safety
///
/// When `base` is non-NULL it must point at `slots` initialised tagged values
/// (the registered precise frame) for as long as the activation runs; the
/// pointer must be cleared by `hydro_frame_leave` before the storage dies.
#[no_mangle]
pub extern "C" fn hydro_frame_bind(
    base: *mut i64,
    slots: usize,
    names: *const c_uchar,
    names_len: usize,
    params: usize,
    flags: u32,
    module: *const c_uchar,
    module_len: usize,
    first_line: u32,
) {
    exc::frame_bind(
        base, slots, names, names_len, params, flags, module, module_len, first_line,
    );
}

// Exceptions

#[no_mangle]
pub extern "C" fn hydro_exc_pending() -> i64 {
    if exc::pending() {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn hydro_exc_tag() -> i64 {
    exc::pending_tag()
}

#[no_mangle]
pub extern "C" fn hydro_exc_value() -> i64 {
    exc::pending_value()
}

#[no_mangle]
pub extern "C" fn hydro_exc_clear() -> i64 {
    exc::clear()
}

/// Pushes an exception (or `None` for a `finally` entered without one) as the
/// active handler context used for implicit chaining.
#[no_mangle]
pub extern "C" fn hydro_exc_handler_push(value: i64) -> i64 {
    exc::handler_push(value)
}

/// Pops the innermost handler context.
#[no_mangle]
pub extern "C" fn hydro_exc_handler_pop() -> i64 {
    exc::handler_pop()
}

/// Number of active handler contexts, captured at function entry.
#[no_mangle]
pub extern "C" fn hydro_exc_handler_depth() -> i64 {
    exc::handler_depth()
}

/// Pops handler contexts down to an absolute depth.
#[no_mangle]
pub extern "C" fn hydro_exc_handler_truncate(depth: i64) -> i64 {
    exc::handler_truncate(depth)
}

#[no_mangle]
pub extern "C" fn hydro_raise(tag: u32, msg: i64) -> i64 {
    exc::raise(tag, msg)
}

#[no_mangle]
pub extern "C" fn hydro_raise_msg(tag: u32, ptr: *const c_uchar, len: usize) -> i64 {
    exc::raise_msg_ptr(tag, ptr, len)
}

#[no_mangle]
pub extern "C" fn hydro_raise_value(v: i64) -> i64 {
    exc::raise_value(v)
}

/// `raise X(msg) from cause`: `cause` is `None` for `from None` (no chained
/// traceback), otherwise a tagged exception value stored as the explicit
/// cause.
#[no_mangle]
pub extern "C" fn hydro_raise_from(tag: u32, msg: i64, cause: i64) -> i64 {
    exc::raise_from(tag, msg, cause)
}

/// `raise value from cause` for an existing exception object; `cause` is
/// `None` for `from None`.
#[no_mangle]
pub extern "C" fn hydro_raise_value_from(value: i64, cause: i64) -> i64 {
    exc::raise_value_from(value, cause)
}

/// Handler dispatch: `1` when the pending exception is caught by a handler
/// for `handler_tag`, `0` otherwise (and when nothing is pending).
///
/// Matching walks CPython's exception hierarchy instead of comparing tags for
/// equality: every exception is a `BaseException`, an ordinary exception is
/// also an `Exception`, and the modelled branches are honoured —
/// `ArithmeticError`, `LookupError`, `ImportError`/`ModuleNotFoundError`,
/// `OSError` and its subclasses, `SyntaxError`/`IndentationError`/`TabError`,
/// the `UnicodeError` family and the `Warning` categories. That includes the
/// diamond around `ExceptionGroup`, which is both an `Exception` and a
/// `BaseExceptionGroup`; the runtime-internal tags keep their documented
/// exclusions. An unknown pending tag matches only `BaseException` and
/// `Exception`, and an unknown handler tag matches nothing.
#[no_mangle]
pub extern "C" fn hydro_exc_matches(handler_tag: u32) -> i32 {
    exc::matches_handler(handler_tag)
}

#[no_mangle]
pub extern "C" fn hydro_uncaught() -> i64 {
    exc::uncaught()
}

// Garbage collector (ARCHITECTURE §7)

/// Forces a conservative collection and returns the number of objects
/// freed by this call. In shared-heap mode this stops the other threads
/// at their safepoints; it returns `0` when another collector is already
/// running.
#[no_mangle]
pub extern "C" fn hydro_gc_collect() -> i64 {
    heap::collect()
}

/// Cooperative GC safepoint.
///
/// Generated code calls this at every function entry and loop backedge.
/// The fast path is a single atomic load; if a collection is requested,
/// the calling thread saves its context, parks and waits for the sweep to
/// finish. The runtime also calls it before every allocation and every
/// blocking operation, so native wrappers need not emit it themselves.
#[no_mangle]
pub extern "C" fn hydro_safepoint() {
    heap::safepoint()
}

/// Number of live heap objects registered for this thread, or for the
/// process once `_thread` has switched to the shared heap.
#[no_mangle]
pub extern "C" fn hydro_gc_objects() -> i64 {
    heap::objects() as i64
}

// Precise shadow-stack roots (P2-06/P2-07)

/// Registers a precise shadow frame and returns its token (never `0`).
///
/// `base` points at `capacity` writable `i64` slots owned by the generated
/// frame; all of them are initialised to tagged `None` before the frame
/// becomes visible to the collector, so the capacity must cover every local,
/// temporary and return-value slot that can hold a tagged value. The frame
/// is popped with the returned token. The call allocates no Hydro objects and
/// takes no heap lock.
#[no_mangle]
pub extern "C" fn hydro_gc_frame_push(base: *mut i64, capacity: usize) -> usize {
    roots::frame_push(base, capacity)
}

/// Unregisters the innermost shadow frame pushed by
/// [`hydro_gc_frame_push`]. Tokens must be popped in LIFO order.
#[no_mangle]
pub extern "C" fn hydro_gc_frame_pop(token: usize) {
    roots::frame_pop(token);
}

/// Narrows the scanned prefix of the frame identified by `token`.
///
/// The frame must still be the innermost one and `used` is clamped to its
/// capacity.
#[no_mangle]
pub extern "C" fn hydro_gc_frame_used(token: usize, used: usize) {
    roots::frame_used(token, used);
}

/// Registers a precise root range covering `count` tagged values owned by
/// the caller (`argv`, a packed argument buffer, a stack array) and returns
/// its token (never `0`). The values are already initialised; the range is
/// popped with [`hydro_gc_scope_pop`].
#[no_mangle]
pub extern "C" fn hydro_gc_scope_push(base: *const i64, count: usize) -> usize {
    roots::scope_push(base, count)
}

/// Unregisters the innermost root range pushed by [`hydro_gc_scope_push`].
/// Tokens must be popped in LIFO order.
#[no_mangle]
pub extern "C" fn hydro_gc_scope_pop(token: usize) {
    roots::scope_pop(token);
}

#[no_mangle]
pub extern "C" fn hydro_runtime_init() {
    // Unit tests run concurrently in one process, and this reset frees the
    // whole heap; hold the same guard as the reset tests so a test that owns
    // the heap for its whole body cannot see its objects freed mid-run.
    #[cfg(test)]
    let _reset_guard = crate::heap::heap_reset_test_guard();
    // Order matters: resetting the exception state drops the pending
    // tagged value before the collector frees the objects it may point
    // at, so no dangling root survives into the next program run.
    //
    // Called once at program entry. Spawned `_thread` threads must never
    // call it: it frees the whole heap (including objects other threads
    // may still reach) and resets the shared-heap switch.
    crate::process::reset_start_method();
    exc::init();
    heap::reset();
    // AOT binaries have no CLI front end; fill `sys.argv` from the process
    // arguments. An explicit `set_argv` call (the JIT front end) wins.
    crate::init_argv();
    // A fresh program run starts with no module globals (the unittest class
    // registry is cleared by `heap::reset`).
    crate::core::globals::reset();
    // Anchor the CPython shim so its symbols are linked into AOT binaries and
    // never dropped as unreachable (JIT links the same runtime object).
    crate::capi::anchor();
}

// Spawn start method

/// Registers a generated wrapper for the `spawn` start method.
///
/// Called by `hydro_main` after `hydro_runtime_init` and before the module
/// body, once per program function except `<module>`.
#[no_mangle]
pub extern "C" fn hydro_register_function(
    name_ptr: *const c_uchar,
    name_len: usize,
    wrapper: *const u8,
    arity: usize,
) {
    crate::spawn::register_from_abi(name_ptr, name_len, wrapper as usize, arity);
}

/// Runs a spawned child's target without executing the module body.
///
/// Returns `-1` when `HYDRO_SPAWN_PAYLOAD` is absent (normal program start),
/// otherwise the child exit code (`0` success, `1` error).
#[no_mangle]
pub extern "C" fn hydro_spawn_child() -> i32 {
    crate::spawn::spawn_child()
}

// Profiling

/// Reconstructs a borrowed byte slice from an ABI `(ptr, len)` pair.
///
/// A null pointer with a non-zero length is a caller bug; report it as absent
/// instead of invoking undefined behaviour. A zero length is accepted with a
/// null pointer (the returned slice is empty and never dereferenced).
///
/// # Safety
///
/// When `ptr` is non-null it must point to `len` readable bytes.
unsafe fn abi_bytes<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if ptr.is_null() {
        return None;
    }
    Some(std::slice::from_raw_parts(ptr, len))
}

/// Clears every recorded function-entry count.
#[no_mangle]
pub extern "C" fn hydro_profile_reset() {
    crate::profile::reset();
}

/// Records one entry into the function named by `name_ptr[..name_len]`.
///
/// Generated code calls this at function entry when profiling is enabled. A
/// null or non-UTF-8 name is ignored; the call never panics.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)] // ABI contract: generated code passes a readable byte slot.
pub extern "C" fn hydro_profile_hit(name_ptr: *const u8, name_len: usize) {
    // SAFETY: generated code passes a stack slot holding `name_len` bytes.
    let Some(bytes) = (unsafe { abi_bytes(name_ptr, name_len) }) else {
        return;
    };
    if let Ok(name) = std::str::from_utf8(bytes) {
        crate::profile::hit(name);
    }
}

/// Writes the collected profile to the filesystem path in
/// `path_ptr[..path_len]`.
///
/// Returns 0 on success and -1 when the path is not valid UTF-8 or the file
/// cannot be created or written. Never panics.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)] // ABI contract: caller passes a readable byte slot.
pub extern "C" fn hydro_profile_dump(path_ptr: *const u8, path_len: usize) -> i32 {
    // SAFETY: the caller passes a stack slot holding `path_len` bytes.
    let Some(bytes) = (unsafe { abi_bytes(path_ptr, path_len) }) else {
        return -1;
    };
    let Ok(path) = std::str::from_utf8(bytes) else {
        return -1;
    };
    crate::profile::dump(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests in this binary: the module-global store and the
    /// module caches derived from it are process-wide, so one test's
    /// `hydro_runtime_init` must not clear them while another test compares
    /// cached values.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Initialises the runtime for a test, holding the process-wide test lock
    /// for the rest of the test thread's life.
    ///
    /// A test that resets the runtime mid-body (`reset` tests call
    /// `hydro_runtime_init` again) reuses the already-held lock instead of
    /// deadlocking on it.
    fn test_init() {
        thread_local! {
            static SESSION: std::cell::RefCell<Option<std::sync::MutexGuard<'static, ()>>> =
                const { std::cell::RefCell::new(None) };
        }
        SESSION.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(
                    TEST_LOCK
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()),
                );
            }
        });
        super::hydro_runtime_init();
    }

    #[test]
    fn constants_and_constructors() {
        test_init();
        assert_eq!(hydro_none(), 1);
        assert_eq!(hydro_false(), 9);
        assert_eq!(hydro_true(), 17);
        assert_eq!(hydro_bool(hydro_none()), hydro_false());
        assert_eq!(hydro_bool(hydro_int(0)), hydro_false());
        assert_eq!(hydro_bool(hydro_int(3)), hydro_true());
        assert_eq!(hydro_int(hydro_int(-4)), hydro_int(-4));

        let s = hydro_str_new(b"hi".as_ptr(), 2);
        assert_eq!(value::as_str(s), Some("hi"));
        assert_eq!(value::as_float(hydro_float_new(1.5)), Some(1.5));
    }

    #[test]
    fn bigint_literals_parse_through_the_abi() {
        test_init();
        let text = b"987654321098765432109876543210";
        let huge = hydro_bigint_from_str(text.as_ptr(), text.len());
        assert!(value::is_bigint(huge));
        assert_eq!(value::to_string(huge), "987654321098765432109876543210");
        assert_eq!(value::type_name(huge), "int");

        // Values inside the tagged range become immediates.
        assert_eq!(hydro_bigint_from_str(b"-42".as_ptr(), 3), hydro_int(-42));
        let min = b"-1152921504606846976";
        let tagged_min = hydro_bigint_from_str(min.as_ptr(), min.len());
        assert_eq!(value::as_integer(tagged_min), Some(value::MIN_INT));
        assert!(!value::is_bigint(tagged_min));

        // Malformed text raises ValueError through the pending exception.
        exc::init();
        assert_eq!(hydro_bigint_from_str(b"oops".as_ptr(), 4), value::NONE);
        assert_eq!(exc::pending_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "invalid literal for int() with base 10: 'oops'"
        );
    }

    #[test]
    fn argv_is_preserved_across_runtime_init() {
        crate::set_argv(
            "bench_hydro.py".to_string(),
            vec!["--help".to_string(), "--n=1".to_string()],
        );
        test_init();
        assert_eq!(
            crate::argv(),
            vec![
                "bench_hydro.py".to_string(),
                "--help".to_string(),
                "--n=1".to_string()
            ]
        );
        let argv = crate::modules::module_get_by_name("sys", "argv");
        let items = value::as_list(argv).expect("argv list");
        assert_eq!(items.len(), 3);
        assert_eq!(value::as_str(items[0]), Some("bench_hydro.py"));
        assert_eq!(value::as_str(items[1]), Some("--help"));
        assert_eq!(value::as_str(items[2]), Some("--n=1"));
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn operator_entry_points() {
        test_init();
        let two = hydro_int(2);
        let ten = hydro_int(10);
        let result = hydro_binop(ops::OP_POW, two, ten);
        assert_eq!(value::as_integer(result), Some(1024));

        let half = hydro_binop(ops::OP_DIV, hydro_int(1), two);
        assert_eq!(value::as_float(half), Some(0.5));

        assert_eq!(hydro_truthy(hydro_none()), 0);
        assert_eq!(hydro_truthy(hydro_str_new(b"x".as_ptr(), 1)), 1);
    }

    #[test]
    fn container_entry_points() {
        test_init();
        let items = [hydro_int(1), hydro_int(2)];
        let list = hydro_list_new(items.len(), items.as_ptr());
        assert_eq!(hydro_len(list), hydro_int(2));
        assert_eq!(hydro_subscript(list, hydro_int(-1)), hydro_int(2));
        assert_eq!(
            hydro_subscript_set(list, hydro_int(0), hydro_int(7)),
            hydro_none()
        );
        assert_eq!(hydro_subscript(list, hydro_int(0)), hydro_int(7));
        assert_eq!(hydro_exc_pending(), 0);

        let keys = [hydro_str_new(b"k".as_ptr(), 1)];
        let values = [hydro_int(9)];
        let dict = hydro_dict_new(1, keys.as_ptr(), values.as_ptr());
        assert_eq!(hydro_subscript(dict, keys[0]), hydro_int(9));
        assert_eq!(hydro_len(dict), hydro_int(1));
        assert_eq!(hydro_exc_pending(), 0);

        let iter = hydro_iter_new(list);
        let mut out = 0i64;
        assert_eq!(hydro_iter_next(iter, &mut out), 1);
        assert_eq!(out, hydro_int(7));
        assert_eq!(hydro_iter_next(iter, &mut out), 1);
        assert_eq!(out, hydro_int(2));
        assert_eq!(hydro_iter_next(iter, &mut out), 0);
        assert_eq!(hydro_exc_pending(), 0);

        assert_eq!(hydro_iter_next(hydro_int(1), &mut out), 0);
        assert_eq!(hydro_exc_pending(), 1);
        hydro_exc_clear();
    }

    #[test]
    fn tuple_entry_points() {
        test_init();
        let items = [hydro_int(1), hydro_int(2)];
        let t = hydro_tuple_new(items.len(), items.as_ptr());
        assert_eq!(value::type_name(t), "tuple");
        assert_eq!(hydro_len(t), hydro_int(2));
        assert_eq!(hydro_subscript(t, hydro_int(-1)), hydro_int(2));
        assert_eq!(
            value::to_string(hydro_slice(t, hydro_int(1), hydro_none(), hydro_none())),
            "(2,)"
        );

        let mut out = 0i64;
        let iter = hydro_iter_new(t);
        assert_eq!(hydro_iter_next(iter, &mut out), 1);
        assert_eq!(out, hydro_int(1));
        assert_eq!(hydro_iter_next(iter, &mut out), 1);
        assert_eq!(out, hydro_int(2));
        assert_eq!(hydro_iter_next(iter, &mut out), 0);

        assert_eq!(hydro_unpack(t, 2), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);

        assert_eq!(hydro_unpack(t, 1), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "too many values to unpack (expected 1)"
        );
        hydro_exc_clear();

        // A `NULL` buffer never panics and yields the empty tuple.
        let empty = hydro_tuple_new(1, std::ptr::null());
        assert_eq!(value::to_string(empty), "()");
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn unpack_seq_entry_point() {
        test_init();

        // Lists, tuples, strings and bytes are returned unchanged.
        let list = make_list(&[1, 2, 3]);
        assert_eq!(hydro_unpack_seq(list, 3), list);
        assert_eq!(hydro_exc_pending(), 0);
        let tuple = hydro_tuple_new(2, [hydro_int(4), hydro_int(5)].as_ptr());
        assert_eq!(hydro_unpack_seq(tuple, 2), tuple);
        let text = hydro_str_new("ab".as_ptr(), 2);
        assert_eq!(hydro_unpack_seq(text, 2), text);
        assert_eq!(hydro_exc_pending(), 0);

        // An existing iterator is consumed into a fresh list.
        let iter = hydro_iter_new(list);
        let sequence = hydro_unpack_seq(iter, 3);
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(list_ints(sequence), vec![1, 2, 3]);
        assert_eq!(hydro_subscript(sequence, hydro_int(1)), hydro_int(2));

        // Length mismatches use the unpacking ValueError wording.
        let iter = hydro_iter_new(make_list(&[1, 2]));
        assert_eq!(hydro_unpack_seq(iter, 3), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "not enough values to unpack (expected 3, got 2)"
        );
        hydro_exc_clear();

        let iter = hydro_iter_new(make_list(&[1, 2, 3]));
        assert_eq!(hydro_unpack_seq(iter, 1), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "too many values to unpack (expected 1)"
        );
        hydro_exc_clear();

        // Non-iterables keep the unpack-specific TypeError.
        assert_eq!(hydro_unpack_seq(hydro_int(1), 1), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "cannot unpack non-iterable int object"
        );
        hydro_exc_clear();
    }

    #[test]
    fn unpack_star_entry_point() {
        test_init();

        // `a, *rest, b = [1, 2, 3, 4]`: slot 1 is the fresh middle list.
        let list = make_list(&[1, 2, 3, 4]);
        let sequence = hydro_unpack_star(list, 1, 1);
        assert_eq!(hydro_exc_pending(), 0);
        let items = value::as_list(sequence).expect("fresh list");
        assert_eq!(items.len(), 3);
        assert_eq!(value::as_integer(items[0]), Some(1));
        assert_eq!(value::as_integer(items[2]), Some(4));
        let middle = value::as_list(items[1]).expect("middle list");
        assert_eq!(
            middle
                .iter()
                .map(|item| value::as_integer(*item).expect("integer item"))
                .collect::<Vec<i64>>(),
            vec![2, 3]
        );

        // A tuple source is copied, and the source is not mutated.
        let tuple = hydro_tuple_new(3, [hydro_int(1), hydro_int(2), hydro_int(3)].as_ptr());
        let starred = hydro_unpack_star(tuple, 0, 0);
        let items = value::as_list(starred).expect("fresh list");
        assert_eq!(items.len(), 1);
        assert_eq!(
            value::as_integer(value::as_list(items[0]).unwrap()[0]),
            Some(1)
        );
        assert_eq!(hydro_len(tuple), hydro_int(3));

        // Too few elements uses CPython's "at least" wording.
        let short = make_list(&[1]);
        assert_eq!(hydro_unpack_star(short, 1, 1), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "not enough values to unpack (expected at least 2, got 1)"
        );
        hydro_exc_clear();

        // Non-iterables keep the unpack-specific TypeError.
        assert_eq!(hydro_unpack_star(hydro_int(1), 0, 0), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "cannot unpack non-iterable int object"
        );
        hydro_exc_clear();

        // `tuple(iterable)` and `hasattr` entry points.
        assert_eq!(
            value::as_tuple(hydro_tuple_from(list)).map(|items| items.len()),
            Some(4)
        );
        let math = hydro_module_value(b"math".as_ptr(), 4);
        assert_eq!(hydro_hasattr(math, b"pi".as_ptr(), 2), hydro_true());
        assert_eq!(hydro_hasattr(math, b"tau".as_ptr(), 3), hydro_true());
        assert_eq!(hydro_hasattr(math, b"nope".as_ptr(), 4), hydro_false());
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn module_values_share_identity_and_extensions_keep_their_name() {
        test_init();

        // Every materialisation of a module value returns the same
        // `sys.modules` object, including a read through `sys.modules`.
        let first = hydro_module_value(b"math".as_ptr(), 4);
        let second = hydro_module_value(b"math".as_ptr(), 4);
        assert_eq!(first, second);
        assert_eq!(
            value::as_module(first).map(|module| module.name()),
            Some("math")
        );
        let modules = hydro_module_get(b"sys".as_ptr(), 3, b"modules".as_ptr(), 7);
        let key = hydro_str_new(b"math".as_ptr(), 4);
        assert_eq!(hydro_subscript(modules, key), first);

        // Extension module values are cached under their import name and keep
        // the shared-object path for attribute reads.
        let name = b"pkg._ext";
        let path = b"/tmp/_ext.abi3.so";
        let ext =
            hydro_extension_module_value(name.as_ptr(), name.len(), path.as_ptr(), path.len());
        let again =
            hydro_extension_module_value(name.as_ptr(), name.len(), path.as_ptr(), path.len());
        assert_eq!(ext, again);
        assert_eq!(hydro_exc_pending(), 0);
        let reported = hydro_getattr(ext, b"__name__".as_ptr(), 8);
        assert_eq!(value::as_str(reported), Some("pkg._ext"));
        assert_eq!(
            value::as_module(ext).and_then(|module| module.path()),
            Some("/tmp/_ext.abi3.so")
        );
    }

    fn make_list(items: &[i64]) -> i64 {
        let tagged: Vec<i64> = items.iter().map(|n| n << 3).collect();
        hydro_list_new(tagged.len(), tagged.as_ptr())
    }

    fn list_ints(v: i64) -> Vec<i64> {
        value::as_list(v)
            .expect("list value")
            .iter()
            .map(|item| value::as_integer(*item).expect("integer item"))
            .collect()
    }

    #[test]
    fn slice_entry_points() {
        test_init();

        let list = make_list(&[0, 1, 2, 3, 4]);
        assert_eq!(
            list_ints(hydro_slice(list, hydro_int(1), hydro_int(4), hydro_none())),
            vec![1, 2, 3]
        );
        assert_eq!(
            list_ints(hydro_slice(list, hydro_none(), hydro_none(), hydro_int(-1))),
            vec![4, 3, 2, 1, 0]
        );
        assert_eq!(
            list_ints(hydro_slice(list, hydro_none(), hydro_none(), hydro_int(2))),
            vec![0, 2, 4]
        );
        // Negative bounds.
        assert_eq!(
            list_ints(hydro_slice(
                list,
                hydro_int(-3),
                hydro_int(-1),
                hydro_none()
            )),
            vec![2, 3]
        );
        // Clamping on both sides.
        assert_eq!(
            list_ints(hydro_slice(
                list,
                hydro_int(-100),
                hydro_int(100),
                hydro_int(2)
            )),
            vec![0, 2, 4]
        );
        assert_eq!(
            list_ints(hydro_slice(
                list,
                hydro_int(100),
                hydro_none(),
                hydro_int(-2)
            )),
            vec![4, 2, 0]
        );
        // Empty results.
        assert!(list_ints(hydro_slice(list, hydro_int(10), hydro_none(), hydro_none())).is_empty());
        assert!(list_ints(hydro_slice(
            list,
            hydro_none(),
            hydro_int(-100),
            hydro_none()
        ))
        .is_empty());
        assert!(list_ints(hydro_slice(list, hydro_int(2), hydro_int(2), hydro_none())).is_empty());
        assert!(list_ints(hydro_slice(list, hydro_int(0), hydro_int(0), hydro_int(-1))).is_empty());
        // Booleans are integers.
        assert_eq!(
            list_ints(hydro_slice(list, hydro_true(), hydro_int(3), hydro_none())),
            vec![1, 2]
        );
        assert_eq!(hydro_exc_pending(), 0);

        // String slices use Unicode scalar indices.
        let text = "aé😀z";
        let s = hydro_str_new(text.as_ptr(), text.len());
        assert_eq!(
            value::as_str(hydro_slice(s, hydro_int(1), hydro_int(3), hydro_none())),
            Some("é😀")
        );
        assert_eq!(
            value::as_str(hydro_slice(s, hydro_none(), hydro_none(), hydro_int(-1))),
            Some("z😀éa")
        );
        assert_eq!(
            value::as_str(hydro_slice(s, hydro_none(), hydro_none(), hydro_int(2))),
            Some("a😀")
        );
        assert_eq!(
            value::as_str(hydro_slice(s, hydro_int(3), hydro_none(), hydro_none())),
            Some("z")
        );
        assert_eq!(
            value::as_str(hydro_slice(s, hydro_int(9), hydro_none(), hydro_none())),
            Some("")
        );
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn slice_results_are_independent() {
        test_init();

        let source = make_list(&[1, 2, 3]);
        let sliced = hydro_slice(source, hydro_none(), hydro_none(), hydro_none());
        assert_ne!(source, sliced);

        // Mutating the source must not affect the slice, and vice versa.
        hydro_subscript_set(source, hydro_int(0), hydro_int(9));
        assert_eq!(list_ints(sliced), vec![1, 2, 3]);
        hydro_subscript_set(sliced, hydro_int(1), hydro_int(8));
        assert_eq!(list_ints(source), vec![9, 2, 3]);
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn slice_errors() {
        test_init();
        let list = make_list(&[1, 2, 3]);

        assert_eq!(
            hydro_slice(list, hydro_none(), hydro_none(), hydro_int(0)),
            hydro_none()
        );
        assert_eq!(hydro_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "slice step cannot be zero"
        );
        hydro_exc_clear();

        assert_eq!(
            hydro_slice(list, hydro_float_new(1.0), hydro_none(), hydro_none()),
            hydro_none()
        );
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "slice indices must be integers or None or have an __index__ method"
        );
        hydro_exc_clear();

        let keys = [hydro_str_new(b"k".as_ptr(), 1)];
        let values = [hydro_int(1)];
        let dict = hydro_dict_new(1, keys.as_ptr(), values.as_ptr());
        assert_eq!(
            hydro_slice(dict, hydro_none(), hydro_none(), hydro_none()),
            hydro_none()
        );
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "unhashable type: 'slice'"
        );
        hydro_exc_clear();

        assert_eq!(
            hydro_slice(hydro_int(5), hydro_none(), hydro_none(), hydro_none()),
            hydro_none()
        );
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "'int' object is not subscriptable"
        );
        hydro_exc_clear();
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn slice_assignment_entry_point() {
        test_init();

        let list = make_list(&[0, 1, 2, 3, 4]);
        // Same-length middle replacement. The ABI returns `None`.
        assert_eq!(
            hydro_slice_set(
                list,
                hydro_int(1),
                hydro_int(3),
                hydro_none(),
                make_list(&[9, 8])
            ),
            hydro_none()
        );
        assert_eq!(list_ints(list), vec![0, 9, 8, 3, 4]);
        // A longer right-hand side grows the list, a shorter one shrinks it.
        hydro_slice_set(
            list,
            hydro_int(1),
            hydro_int(2),
            hydro_none(),
            make_list(&[7, 6, 5]),
        );
        assert_eq!(list_ints(list), vec![0, 7, 6, 5, 8, 3, 4]);
        hydro_slice_set(
            list,
            hydro_int(1),
            hydro_int(4),
            hydro_none(),
            make_list(&[]),
        );
        assert_eq!(list_ints(list), vec![0, 8, 3, 4]);

        // Head, tail, empty (pure insertion) and full slices.
        hydro_slice_set(
            list,
            hydro_int(0),
            hydro_int(1),
            hydro_none(),
            make_list(&[1, 2]),
        );
        assert_eq!(list_ints(list), vec![1, 2, 8, 3, 4]);
        hydro_slice_set(
            list,
            hydro_int(3),
            hydro_none(),
            hydro_none(),
            make_list(&[9]),
        );
        assert_eq!(list_ints(list), vec![1, 2, 8, 9]);
        hydro_slice_set(
            list,
            hydro_int(2),
            hydro_int(2),
            hydro_none(),
            make_list(&[4, 5]),
        );
        assert_eq!(list_ints(list), vec![1, 2, 4, 5, 8, 9]);
        hydro_slice_set(
            list,
            hydro_none(),
            hydro_none(),
            hydro_none(),
            make_list(&[6]),
        );
        assert_eq!(list_ints(list), vec![6]);
        // A start past the end appends, and a backwards range inserts at
        // `start` (Python raises `stop` to `start` for a step of 1).
        hydro_slice_set(
            list,
            hydro_int(10),
            hydro_none(),
            hydro_none(),
            make_list(&[7, 8]),
        );
        assert_eq!(list_ints(list), vec![6, 7, 8]);
        hydro_slice_set(
            list,
            hydro_int(3),
            hydro_int(1),
            hydro_none(),
            make_list(&[9]),
        );
        assert_eq!(list_ints(list), vec![6, 7, 8, 9]);
        assert_eq!(hydro_exc_pending(), 0);

        // Negative bounds follow slice.indices.
        let negative = make_list(&[0, 1, 2, 3, 4]);
        hydro_slice_set(
            negative,
            hydro_int(-2),
            hydro_none(),
            hydro_none(),
            make_list(&[8, 9]),
        );
        assert_eq!(list_ints(negative), vec![0, 1, 2, 8, 9]);
        hydro_slice_set(
            negative,
            hydro_int(-100),
            hydro_int(2),
            hydro_none(),
            make_list(&[5]),
        );
        assert_eq!(list_ints(negative), vec![5, 2, 8, 9]);
        assert_eq!(hydro_exc_pending(), 0);

        // Extended slices assign position by position, in both directions.
        let extended = make_list(&[0, 1, 2, 3, 4]);
        hydro_slice_set(
            extended,
            hydro_none(),
            hydro_none(),
            hydro_int(2),
            make_list(&[7, 8, 9]),
        );
        assert_eq!(list_ints(extended), vec![7, 1, 8, 3, 9]);
        hydro_slice_set(
            extended,
            hydro_none(),
            hydro_none(),
            hydro_int(-1),
            make_list(&[10, 11, 12, 13, 14]),
        );
        assert_eq!(list_ints(extended), vec![14, 13, 12, 11, 10]);
        hydro_slice_set(
            extended,
            hydro_int(1),
            hydro_int(4),
            hydro_int(2),
            make_list(&[0, 1]),
        );
        assert_eq!(list_ints(extended), vec![14, 0, 12, 1, 10]);
        assert_eq!(hydro_exc_pending(), 0);

        // Any iterable is materialised: a string contributes characters and
        // an iterator is consumed.
        let mixed = make_list(&[0, 1, 2, 3, 4]);
        hydro_slice_set(
            mixed,
            hydro_int(1),
            hydro_int(2),
            hydro_none(),
            hydro_str_new(b"ab".as_ptr(), 2),
        );
        assert_eq!(value::repr(mixed), "[0, 'a', 'b', 2, 3, 4]");
        let iterator = hydro_iter_new(make_list(&[7, 8]));
        hydro_slice_set(mixed, hydro_int(0), hydro_int(1), hydro_none(), iterator);
        assert_eq!(value::repr(mixed), "[7, 8, 'a', 'b', 2, 3, 4]");
        assert_eq!(hydro_exc_pending(), 0);

        // A list assigned to one of its own slices is snapshotted first.
        let source = make_list(&[1, 2, 3]);
        hydro_slice_set(source, hydro_none(), hydro_none(), hydro_none(), source);
        assert_eq!(list_ints(source), vec![1, 2, 3]);
        hydro_slice_set(source, hydro_int(1), hydro_int(1), hydro_none(), source);
        assert_eq!(list_ints(source), vec![1, 1, 2, 3, 2, 3]);
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn slice_assignment_errors() {
        test_init();
        let list = make_list(&[0, 1, 2, 3]);

        // An extended slice needs exactly as many values as positions, and a
        // mismatch leaves the list unchanged.
        assert_eq!(
            hydro_slice_set(
                list,
                hydro_none(),
                hydro_none(),
                hydro_int(2),
                make_list(&[1])
            ),
            hydro_none()
        );
        assert_eq!(hydro_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "attempt to assign sequence of size 1 to extended slice of size 2"
        );
        hydro_exc_clear();
        assert_eq!(list_ints(list), vec![0, 1, 2, 3]);

        // Bound errors match slicing.
        hydro_slice_set(
            list,
            hydro_none(),
            hydro_none(),
            hydro_int(0),
            make_list(&[1]),
        );
        assert_eq!(hydro_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "slice step cannot be zero"
        );
        hydro_exc_clear();
        hydro_slice_set(
            list,
            hydro_float_new(1.0),
            hydro_none(),
            hydro_none(),
            make_list(&[1]),
        );
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "slice indices must be integers or None or have an __index__ method"
        );
        hydro_exc_clear();

        // Strings, bytes and tuples are immutable.
        let empty_tuple = hydro_tuple_new(0, std::ptr::null());
        for immutable in [
            hydro_str_new(b"abc".as_ptr(), 3),
            hydro_bytes_new(b"abc".as_ptr(), 3),
            empty_tuple,
        ] {
            hydro_slice_set(
                immutable,
                hydro_none(),
                hydro_none(),
                hydro_none(),
                make_list(&[1]),
            );
            assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
            assert!(
                value::to_string(hydro_exc_value())
                    .ends_with("object does not support item assignment"),
                "{}",
                value::to_string(hydro_exc_value())
            );
            hydro_exc_clear();
        }

        // A dict slice assignment reports the unhashable slice key.
        let dict = hydro_dict_new(0, std::ptr::null(), std::ptr::null());
        hydro_slice_set(
            dict,
            hydro_none(),
            hydro_none(),
            hydro_none(),
            make_list(&[1]),
        );
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "unhashable type: 'slice'"
        );
        hydro_exc_clear();

        // The right-hand side must be iterable; CPython uses the same wording
        // for simple and extended slices.
        hydro_slice_set(list, hydro_none(), hydro_none(), hydro_none(), hydro_int(5));
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "must assign iterable to extended slice"
        );
        hydro_exc_clear();
        hydro_slice_set(list, hydro_none(), hydro_none(), hydro_int(2), hydro_int(5));
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "must assign iterable to extended slice"
        );
        hydro_exc_clear();
        assert_eq!(list_ints(list), vec![0, 1, 2, 3]);
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn gc_entry_points() {
        test_init();
        assert_eq!(hydro_gc_objects(), 0);

        let s = hydro_str_new(b"gc".as_ptr(), 2);
        let f = hydro_float_new(1.0);
        // At least the most recent allocation is live. Conservative
        // scanning may keep older garbage, so no exact count.
        assert!(hydro_gc_objects() >= 1);

        let before = hydro_gc_objects();
        let freed = hydro_gc_collect();
        assert!(freed >= 0 && freed <= before);
        // `s` and `f` are used after the forced collection, so they are
        // roots and must have survived.
        assert_eq!(value::as_str(s), Some("gc"));
        assert_eq!(value::as_float(f), Some(1.0));
        assert!(hydro_gc_objects() >= 2);

        test_init();
        assert_eq!(hydro_gc_objects(), 0);
    }

    #[test]
    fn set_entry_points() {
        test_init();
        let items = [hydro_int(1), hydro_int(2), hydro_int(2), hydro_int(3)];
        let s = hydro_set_new(items.len(), items.as_ptr());
        assert_eq!(value::type_name(s), "set");
        assert_eq!(hydro_len(s), hydro_int(3));
        assert_eq!(hydro_binop(ops::OP_IN, hydro_int(2), s), hydro_true());
        assert_eq!(hydro_binop(ops::OP_NOT_IN, hydro_int(9), s), hydro_true());

        // set(iterable) from list, string and dict.
        let list = make_list(&[3, 1, 3, 2]);
        let from_list = hydro_set_from(list);
        assert_eq!(value::as_set(from_list).map(|i| i.len()), Some(3));

        let text = "aba";
        let from_str = hydro_set_from(hydro_str_new(text.as_ptr(), text.len()));
        assert_eq!(value::as_set(from_str).map(|i| i.len()), Some(2));

        let keys = [
            hydro_str_new(b"k".as_ptr(), 1),
            hydro_str_new(b"j".as_ptr(), 1),
        ];
        let values = [hydro_int(1), hydro_int(2)];
        let dict = hydro_dict_new(2, keys.as_ptr(), values.as_ptr());
        let from_dict = hydro_set_from(dict);
        let from_dict = value::as_set(from_dict).expect("set");
        assert_eq!(from_dict.len(), 2);

        // Any iterable is consumed through the iterator protocol.
        let from_range = hydro_set_from(hydro_range_new(hydro_int(0), hydro_int(5), hydro_int(2)));
        assert_eq!(value::as_set(from_range).map(|items| items.len()), Some(3));

        assert_eq!(hydro_set_from(hydro_int(3)), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "'int' object is not iterable"
        );
        hydro_exc_clear();
    }

    #[test]
    fn gc_traces_set_elements() {
        test_init();
        let alpha = hydro_str_new(b"alpha".as_ptr(), 5);
        let beta = hydro_str_new(b"beta".as_ptr(), 4);
        let items = [alpha, beta];
        let s = hydro_set_new(items.len(), items.as_ptr());
        assert_eq!(value::as_set(s).map(|i| i.len()), Some(2));

        // Allocate unrelated garbage so the collector has sweeping work
        // while the set (and its string elements) are live.
        for _ in 0..64 {
            let _ = hydro_str_new(b"garbage".as_ptr(), 7);
        }
        hydro_gc_collect();

        let elements = value::as_set(s).expect("set survived collection");
        assert_eq!(elements.len(), 2);
        assert_eq!(value::as_str(elements[0]), Some("alpha"));
        assert_eq!(value::as_str(elements[1]), Some("beta"));
        assert_eq!(value::as_str(alpha), Some("alpha"));
        assert_eq!(value::as_str(beta), Some("beta"));
    }

    #[test]
    fn builtins_entry_points() {
        test_init();
        let r = hydro_range_new(hydro_int(0), hydro_int(5), hydro_int(1));
        assert_eq!(value::type_name(r), "range");
        assert_eq!(value::to_string(r), "range(0, 5)");
        assert_eq!(hydro_len(r), hydro_int(5));

        let mut out = 0i64;
        let iter = hydro_iter_new(r);
        for expected in [0, 1, 2, 3, 4] {
            assert_eq!(hydro_iter_next(iter, &mut out), 1);
            assert_eq!(out, hydro_int(expected));
        }
        assert_eq!(hydro_iter_next(iter, &mut out), 0);
        assert_eq!(hydro_exc_pending(), 0);

        assert_eq!(value::to_string(hydro_list_from(r)), "[0, 1, 2, 3, 4]");
        assert_eq!(hydro_sum(r, hydro_int(0)), hydro_int(10));
        assert_eq!(hydro_sum(r, hydro_int(100)), hydro_int(110));

        // Range indexing and slicing follow CPython.
        assert_eq!(hydro_subscript(r, hydro_int(-1)), hydro_int(4));
        assert_eq!(hydro_subscript(r, hydro_int(5)), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::INDEX_ERROR as i64);
        hydro_exc_clear();
        assert_eq!(
            value::to_string(hydro_slice(r, hydro_none(), hydro_none(), hydro_int(-1))),
            "range(4, -1, -1)"
        );
        assert_eq!(hydro_exc_pending(), 0);

        let source = make_list(&[1, 2]);
        let enumerated = hydro_enumerate(source, hydro_int(10));
        assert_eq!(value::to_string(enumerated), "[(10, 1), (11, 2)]");

        let right = make_list(&[3, 4]);
        let sources = [source, right];
        let zipped = hydro_zip(sources.len(), sources.as_ptr());
        assert_eq!(value::to_string(zipped), "[(1, 3), (2, 4)]");
        assert_eq!(hydro_exc_pending(), 0);

        let key = hydro_str_new(b"a".as_ptr(), 1);
        let pair1 = hydro_tuple_new(2, [key, hydro_int(1)].as_ptr());
        let pair2 = hydro_tuple_new(2, [hydro_int(2), value::str_from_bytes(b"b")].as_ptr());
        let pair_items = [pair1, pair2];
        let pair_list = hydro_list_new(pair_items.len(), pair_items.as_ptr());
        assert_eq!(
            value::to_string(hydro_dict_from(pair_list)),
            "{'a': 1, 2: 'b'}"
        );
        assert_eq!(hydro_exc_pending(), 0);

        assert_eq!(
            hydro_range_new(hydro_int(0), hydro_int(5), hydro_int(0)),
            hydro_none()
        );
        assert_eq!(hydro_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "range() arg 3 must not be zero"
        );
        hydro_exc_clear();

        assert_eq!(hydro_list_from(hydro_int(1)), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        hydro_exc_clear();
        assert_eq!(hydro_dict_from(hydro_int(1)), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        hydro_exc_clear();
    }

    #[test]
    fn all_and_dict_merge_entry_points() {
        test_init();
        assert_eq!(hydro_all(make_list(&[1, 2])), hydro_true());
        assert_eq!(hydro_all(make_list(&[1, 0])), hydro_false());
        assert_eq!(hydro_all(make_list(&[])), hydro_true());
        assert_eq!(hydro_exc_pending(), 0);

        assert_eq!(hydro_all(hydro_int(1)), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        hydro_exc_clear();

        let left_keys = [hydro_str_new(b"a".as_ptr(), 1)];
        let left_values = [hydro_int(1)];
        let left = hydro_dict_new(1, left_keys.as_ptr(), left_values.as_ptr());
        let right_keys = [
            hydro_str_new(b"a".as_ptr(), 1),
            hydro_str_new(b"b".as_ptr(), 1),
        ];
        let right_values = [hydro_int(2), hydro_int(3)];
        let right = hydro_dict_new(2, right_keys.as_ptr(), right_values.as_ptr());
        let merged = hydro_dict_merge(left, right);
        assert_eq!(value::to_string(merged), "{'a': 2, 'b': 3}");
        assert_eq!(value::to_string(left), "{'a': 1}");
        assert_eq!(hydro_exc_pending(), 0);

        assert_eq!(hydro_dict_merge(hydro_int(1), right), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        hydro_exc_clear();
    }

    #[test]
    fn starred_method_entry_point() {
        test_init();
        let receiver = value::str_from_bytes(b"a-a");
        let args = [value::str_from_bytes(b"a"), value::str_from_bytes(b"b")];
        let star = hydro_tuple_new(args.len(), args.as_ptr());
        let replaced =
            hydro_method_star(receiver, b"replace".as_ptr(), 7, star, 0, std::ptr::null());
        assert_eq!(value::to_string(replaced), "b-b");
        assert_eq!(hydro_exc_pending(), 0);

        // Keyword pairs are bound after the flattened positional arguments.
        let star_args = [value::str_from_bytes(b"a"), value::str_from_bytes(b"b")];
        let star = hydro_list_new(star_args.len(), star_args.as_ptr());
        let keyword_name = value::str_from_bytes(b"count");
        let kw = [keyword_name, hydro_int(1)];
        let replaced = hydro_method_star(receiver, b"replace".as_ptr(), 7, star, 1, kw.as_ptr());
        assert_eq!(value::to_string(replaced), "b-a");
        assert_eq!(hydro_exc_pending(), 0);

        assert_eq!(
            hydro_method_star(
                receiver,
                b"replace".as_ptr(),
                7,
                hydro_int(1),
                0,
                std::ptr::null(),
            ),
            hydro_none()
        );
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        hydro_exc_clear();
    }

    #[test]
    fn builtins_survive_gc() {
        test_init();
        let r = hydro_range_new(hydro_int(0), hydro_int(4), hydro_int(1));
        let listed = hydro_list_from(r);
        let summed = hydro_sum(r, hydro_int(0));
        let source_items = [hydro_str_new(b"a".as_ptr(), 1)];
        let source = hydro_list_new(source_items.len(), source_items.as_ptr());
        let enumerated = hydro_enumerate(source, hydro_int(0));
        let sources = [source];
        let zipped = hydro_zip(1, sources.as_ptr());

        for _ in 0..64 {
            let _ = hydro_str_new(b"garbage".as_ptr(), 7);
        }
        hydro_gc_collect();

        assert_eq!(value::to_string(listed), "[0, 1, 2, 3]");
        assert_eq!(summed, hydro_int(6));
        assert_eq!(value::to_string(enumerated), "[(0, 'a')]");
        assert_eq!(value::to_string(zipped), "[('a',)]");
        assert_eq!(hydro_exc_pending(), 0);
    }

    // First-class functions

    extern "C" fn add_wrapper(argv: *const i64) -> i64 {
        // SAFETY: the caller guarantees two readable arguments.
        let args = unsafe { std::slice::from_raw_parts(argv, 2) };
        hydro_binop(ops::OP_ADD, args[0], args[1])
    }

    extern "C" fn double_wrapper(argv: *const i64) -> i64 {
        // SAFETY: `map` always passes exactly one argument.
        let arg = unsafe { *argv };
        hydro_binop(ops::OP_MUL, arg, hydro_int(2))
    }

    extern "C" fn is_even_wrapper(argv: *const i64) -> i64 {
        // SAFETY: `filter` always passes exactly one argument.
        let arg = unsafe { *argv };
        let remainder = hydro_binop(ops::OP_MOD, arg, hydro_int(2));
        hydro_binop(ops::OP_EQ, remainder, hydro_int(0))
    }

    extern "C" fn zero_wrapper(_argv: *const i64) -> i64 {
        hydro_int(42)
    }

    extern "C" fn fail_wrapper(_argv: *const i64) -> i64 {
        hydro_raise_msg(exc::TYPE_ERROR, b"boom".as_ptr(), 4)
    }

    fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
        hydro_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len(), 0)
    }

    #[test]
    fn function_values_and_indirect_calls() {
        test_init();
        let add = function(add_wrapper, 2, "add");
        assert_eq!(value::type_name(add), "function");
        assert!(value::truthy(add));
        assert_eq!(value::to_string(add), "<function add>");
        assert_eq!(value::repr(add), "<function add>");

        let args = [hydro_int(2), hydro_int(3)];
        assert_eq!(hydro_call(add, 2, args.as_ptr()), hydro_int(5));
        assert_eq!(hydro_exc_pending(), 0);

        // A zero-argument call passes `NULL` to the wrapper.
        let zero = hydro_function_new(zero_wrapper as *const u8, 0, b"zero".as_ptr(), 4, 0);
        assert_eq!(hydro_call(zero, 0, std::ptr::null()), hydro_int(42));
        assert_eq!(hydro_exc_pending(), 0);

        // Missing name buffers fall back to `<function>`.
        let unnamed = hydro_function_new(add_wrapper as *const u8, 2, std::ptr::null(), 0, 0);
        assert_eq!(value::to_string(unnamed), "<function <function>>");

        // Functions are ordinary heap values in containers.
        let suite = hydro_list_new(2, [add, zero].as_ptr());
        assert_eq!(
            value::type_name(hydro_subscript(suite, hydro_int(1))),
            "function"
        );
        assert_eq!(
            hydro_call(hydro_subscript(suite, hydro_int(0)), 2, args.as_ptr()),
            hydro_int(5)
        );
        assert_eq!(hydro_len(suite), hydro_int(2));
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn function_call_errors() {
        test_init();
        let add = function(add_wrapper, 2, "add");
        let args = [hydro_int(1)];

        assert_eq!(hydro_call(add, 1, args.as_ptr()), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "function takes 2 arguments (1 given)"
        );
        hydro_exc_clear();

        assert_eq!(hydro_call(add, 2, std::ptr::null()), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        hydro_exc_clear();

        assert_eq!(hydro_call(hydro_int(7), 0, std::ptr::null()), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "'int' object is not callable"
        );
        hydro_exc_clear();

        assert_eq!(hydro_call(hydro_none(), 0, std::ptr::null()), hydro_none());
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "'NoneType' object is not callable"
        );
        hydro_exc_clear();

        // A null wrapper is rejected at construction.
        assert_eq!(
            hydro_function_new(std::ptr::null(), 0, b"bad".as_ptr(), 3, 0),
            hydro_none()
        );
        assert_eq!(hydro_exc_tag(), exc::RUNTIME_ERROR as i64);
        hydro_exc_clear();

        // Function values reject the existing container operations.
        assert_eq!(hydro_len(add), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "object of type 'function' has no len()"
        );
        hydro_exc_clear();

        assert_eq!(hydro_subscript(add, hydro_int(0)), hydro_none());
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "'function' object is not subscriptable"
        );
        hydro_exc_clear();

        assert_eq!(
            hydro_slice(add, hydro_none(), hydro_none(), hydro_none()),
            hydro_none()
        );
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "'function' object is not subscriptable"
        );
        hydro_exc_clear();
    }

    #[test]
    fn map_and_filter_values() {
        test_init();
        let double = function(double_wrapper, 1, "<lambda>");
        let even = function(is_even_wrapper, 1, "<lambda>");
        let r5 = hydro_range_new(hydro_int(0), hydro_int(5), hydro_int(1));
        let r8 = hydro_range_new(
            hydro_int(0),
            value::encode_int(8).expect("tagged 8"),
            hydro_int(1),
        );

        assert_eq!(value::to_string(hydro_map(double, r5)), "[0, 2, 4, 6, 8]");
        assert_eq!(value::to_string(hydro_filter(even, r8)), "[0, 2, 4, 6]");
        assert_eq!(hydro_exc_pending(), 0);

        // filter(None, ...) uses item truthiness.
        let zero = hydro_int(0);
        let one = hydro_int(1);
        let empty_string = hydro_str_new(b"".as_ptr(), 0);
        let x = hydro_str_new(b"x".as_ptr(), 1);
        let empty_list = hydro_list_new(0, std::ptr::null());
        let one_list = hydro_list_new(1, [one].as_ptr());
        let source = hydro_list_new(
            6,
            [zero, one, empty_string, x, empty_list, one_list].as_ptr(),
        );
        let filtered = hydro_filter(hydro_none(), source);
        assert_eq!(value::to_string(filtered), "[1, 'x', [1]]");
        assert_eq!(hydro_exc_pending(), 0);

        // map accepts any iterable, e.g. a list.
        let listed = hydro_map(
            double,
            hydro_list_new(3, [hydro_int(1), hydro_int(2), hydro_int(3)].as_ptr()),
        );
        assert_eq!(value::to_string(listed), "[2, 4, 6]");
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn map_and_filter_errors() {
        test_init();
        let source = make_list(&[1, 2, 3]);
        let double = function(double_wrapper, 1, "<lambda>");

        assert_eq!(hydro_map(hydro_int(1), source), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "map() argument 1 must be callable"
        );
        hydro_exc_clear();

        assert_eq!(hydro_filter(hydro_int(1), source), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "filter() argument 1 must be callable or None"
        );
        hydro_exc_clear();

        assert_eq!(hydro_map(double, hydro_int(3)), hydro_none());
        assert_eq!(hydro_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "'int' object is not iterable"
        );
        hydro_exc_clear();

        assert_eq!(hydro_filter(double, hydro_int(3)), hydro_none());
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "'int' object is not iterable"
        );
        hydro_exc_clear();

        // Arity mismatches surface through the callback path.
        let binary = function(add_wrapper, 2, "add");
        assert_eq!(hydro_map(binary, source), hydro_none());
        assert_eq!(
            value::to_string(hydro_exc_value()),
            "function takes 2 arguments (1 given)"
        );
        hydro_exc_clear();
    }

    #[test]
    fn function_exceptions_propagate() {
        test_init();
        let fail = function(fail_wrapper, 1, "fail");
        let source = make_list(&[1, 2]);

        assert_eq!(hydro_call(fail, 1, [hydro_int(1)].as_ptr()), hydro_none());
        assert_eq!(hydro_exc_pending(), 1);
        assert_eq!(value::to_string(hydro_exc_value()), "boom");
        hydro_exc_clear();

        assert_eq!(hydro_map(fail, source), hydro_none());
        assert_eq!(hydro_exc_pending(), 1);
        assert_eq!(value::to_string(hydro_exc_value()), "boom");
        hydro_exc_clear();

        assert_eq!(hydro_filter(fail, source), hydro_none());
        assert_eq!(hydro_exc_pending(), 1);
        assert_eq!(value::to_string(hydro_exc_value()), "boom");
        hydro_exc_clear();
    }

    #[test]
    fn functions_survive_gc() {
        test_init();
        let add = function(add_wrapper, 2, "add");
        let double = function(double_wrapper, 1, "<lambda>");

        // Function values inside a list and a dict, plus the wrappers
        // themselves allocated as garbage, exercise the collector.
        let items = [add, double];
        let funcs = hydro_list_new(items.len(), items.as_ptr());
        let keys = [hydro_str_new(b"add".as_ptr(), 3)];
        let values = [add];
        let table = hydro_dict_new(1, keys.as_ptr(), values.as_ptr());

        for _ in 0..256 {
            let _ = function(double_wrapper, 1, "garbage");
        }
        hydro_gc_collect();

        let first = hydro_subscript(funcs, hydro_int(0));
        assert_eq!(value::type_name(first), "function");
        assert_eq!(
            hydro_call(first, 2, [hydro_int(20), hydro_int(30)].as_ptr()),
            hydro_int(50)
        );
        let second = hydro_subscript(funcs, hydro_int(1));
        assert_eq!(
            value::to_string(hydro_map(
                second,
                hydro_range_new(hydro_int(0), hydro_int(3), hydro_int(1))
            )),
            "[0, 2, 4]"
        );
        assert_eq!(
            value::type_name(hydro_subscript(table, keys[0])),
            "function"
        );

        // Iteration yields the function values unchanged.
        let mut out = 0i64;
        let iter = hydro_iter_new(funcs);
        assert_eq!(hydro_iter_next(iter, &mut out), 1);
        assert_eq!(value::type_name(out), "function");
        assert_eq!(hydro_iter_next(iter, &mut out), 1);
        assert_eq!(value::type_name(out), "function");
        assert_eq!(hydro_iter_next(iter, &mut out), 0);
        assert_eq!(hydro_exc_pending(), 0);
    }

    #[test]
    fn exception_entry_points() {
        test_init();
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(hydro_exc_tag(), -1);

        let msg = hydro_str_new(b"boom".as_ptr(), 4);
        assert_eq!(hydro_raise(exc::VALUE_ERROR, msg), hydro_none());
        assert_eq!(hydro_exc_pending(), 1);
        assert_eq!(hydro_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(value::to_string(hydro_exc_value()), "boom");
        assert_eq!(hydro_exc_clear(), 0);
        assert_eq!(hydro_exc_pending(), 0);

        hydro_raise_msg(exc::ZERO_DIVISION_ERROR, b"custom".as_ptr(), 6);
        assert_eq!(value::to_string(hydro_exc_value()), "custom");

        hydro_set_file(b"test.py".as_ptr(), 7);
        hydro_set_line(7);
        hydro_frame_enter(b"main".as_ptr(), 4);
        hydro_raise(exc::ZERO_DIVISION_ERROR, hydro_none());
        assert_eq!(value::to_string(hydro_exc_value()), "division by zero");
        let data = value::exception_data(hydro_exc_value()).expect("exception");
        assert_eq!(data.file, "test.py");
        assert_eq!(data.line, 7);
        assert_eq!(data.frame, "main");

        // Hierarchy dispatch: Exception catches normal exceptions but not
        // KeyboardInterrupt; BaseException catches everything.
        assert_eq!(hydro_exc_matches(exc::EXCEPTION), 1);
        assert_eq!(hydro_exc_matches(exc::BASE_EXCEPTION), 1);
        assert_eq!(hydro_exc_matches(exc::ZERO_DIVISION_ERROR), 1);
        assert_eq!(hydro_exc_matches(exc::VALUE_ERROR), 0);
        hydro_raise(exc::KEYBOARD_INTERRUPT, hydro_none());
        assert_eq!(hydro_exc_matches(exc::EXCEPTION), 0);
        assert_eq!(hydro_exc_matches(exc::BASE_EXCEPTION), 1);

        // `raise RuntimeError(custom=?) from cause`: the new pending
        // exception keeps the previous one as its explicit cause.
        hydro_raise_msg(exc::ZERO_DIVISION_ERROR, b"custom".as_ptr(), 6);
        let cause = hydro_exc_value();
        assert_eq!(
            hydro_raise_from(exc::RUNTIME_ERROR, hydro_none(), cause),
            hydro_none()
        );
        assert_eq!(hydro_exc_tag(), exc::RUNTIME_ERROR as i64);
        let chained = value::exception_data(hydro_exc_value()).expect("exception");
        assert_eq!(chained.cause, Some(cause));

        // `from None` stores no cause.
        assert_eq!(
            hydro_raise_value_from(hydro_exc_value(), hydro_none()),
            hydro_none()
        );
        let suppressed = value::exception_data(hydro_exc_value()).expect("exception");
        assert_eq!(suppressed.cause, None);

        assert_eq!(hydro_exc_clear(), 0);
        assert_eq!(hydro_exc_matches(exc::BASE_EXCEPTION), 0);

        hydro_raise(exc::ZERO_DIVISION_ERROR, hydro_none());
        assert_eq!(hydro_uncaught(), 1);
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(hydro_uncaught(), 0);
    }
}
