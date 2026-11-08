//! `_testbuffer`: the PEP-3118 test objects built on real Hydro buffers.
//!
//! The CPython `_testbuffer` extension exposes a full ndarray implementation
//! together with helpers that take raw `Py_buffer` pointers. Hydro values are
//! tagged `i64`s and buffers are Hydro `bytearray`/`bytes` payloads, so this
//! module implements the parts that have real state:
//!
//! * `ndarray` stores its bytes in a real `bytearray` and its `shape`,
//!   `strides`, `suboffsets`, `offset`, `format`, `itemsize`, `readonly` and
//!   `obj` as instance attributes. `tolist`, `tobytes`, integer/tuple indexing,
//!   `push`/`pop`, `add_suboffsets`, `memoryview_from_buffer` and the buffer
//!   hooks operate on that state.
//! * `getbuffer`/`releasebuffer` and the `Py_buffer` class expose the real
//!   metadata of `bytes`, `bytearray` and `ndarray` exporters. `len`,
//!   `itemsize`, `readonly`, `ndim`, `format`, `shape`, `strides`,
//!   `suboffsets`, `nbytes` and `obj` are real; `buf`/`ptr` raise
//!   `NotImplementedError` because a garbage-collected `Vec<u8>` has no
//!   stable address that could be handed to C.
//! * `get_pointer` returns the *value* addressed by the index tuple (the
//!   pointer dereference itself stays inside this module); address arithmetic
//!   that would need suboffset pointer dereferencing raises
//!   `NotImplementedError`.
//! * `slice_indices`, `is_contiguous`, `cmp_contig`, `get_contiguous`,
//!   `py_buffer_to_contiguous`, `get_sizeof_void_p` and the `ND_*`/`PyBUF_*`
//!   constants are implemented from the stored structure.
//!
//! `BUFFER_MAX_SIZE` is the largest byte length this implementation will
//! materialise (1 GiB), matching the allocation guard used by the struct
//! helpers.

use hydro_ir::MethodKind;

use crate::container;
use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, BUFFER_ERROR, INDEX_ERROR, NOT_IMPLEMENTED_ERROR, OVERFLOW_ERROR,
    TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

// -------------------------------------------------------------------------
// Flags
// -------------------------------------------------------------------------

pub(crate) const ND_MAX_NDIM: i64 = 128;
pub(crate) const ND_VAREXPORT: i64 = 0x001;
pub(crate) const ND_WRITABLE: i64 = 0x002;
pub(crate) const ND_FORTRAN: i64 = 0x004;
pub(crate) const ND_SCALAR: i64 = 0x008;
pub(crate) const ND_PIL: i64 = 0x010;
pub(crate) const ND_REDIRECT: i64 = 0x020;
pub(crate) const ND_GETBUF_FAIL: i64 = 0x040;
pub(crate) const ND_GETBUF_UNDEFINED: i64 = 0x080;

pub(crate) const PYBUF_SIMPLE: i64 = 0;
pub(crate) const PYBUF_WRITABLE: i64 = 0x0001;
pub(crate) const PYBUF_FORMAT: i64 = 0x0004;
pub(crate) const PYBUF_ND: i64 = 0x0008;
pub(crate) const PYBUF_STRIDES: i64 = 0x0010 | PYBUF_ND;
pub(crate) const PYBUF_C_CONTIGUOUS: i64 = 0x0020 | PYBUF_STRIDES;
pub(crate) const PYBUF_F_CONTIGUOUS: i64 = 0x0040 | PYBUF_STRIDES;
pub(crate) const PYBUF_ANY_CONTIGUOUS: i64 = 0x0080 | PYBUF_STRIDES;
pub(crate) const PYBUF_INDIRECT: i64 = 0x0100 | PYBUF_STRIDES;
pub(crate) const PYBUF_FULL_RO: i64 = PYBUF_INDIRECT | PYBUF_FORMAT;
pub(crate) const PYBUF_FULL: i64 = PYBUF_FULL_RO | PYBUF_WRITABLE;
pub(crate) const PYBUF_RECORDS_RO: i64 = PYBUF_STRIDES | PYBUF_FORMAT;
pub(crate) const PYBUF_RECORDS: i64 = PYBUF_RECORDS_RO | PYBUF_WRITABLE;
pub(crate) const PYBUF_STRIDED_RO: i64 = PYBUF_STRIDES;
pub(crate) const PYBUF_STRIDED: i64 = PYBUF_STRIDED_RO | PYBUF_WRITABLE;
pub(crate) const PYBUF_CONTIG_RO: i64 = PYBUF_ND;
pub(crate) const PYBUF_CONTIG: i64 = PYBUF_CONTIG_RO | PYBUF_WRITABLE;
pub(crate) const PYBUF_READ: i64 = 0x100;
pub(crate) const PYBUF_WRITE: i64 = 0x200;

pub(crate) const BUFFER_MAX_SIZE: i64 = 1 << 30;

const GETBUF_UNSET: i64 = i64::MIN;

const NDARRAY_CLASS_KEY: &str = "_testbuffer.ndarray.class";
const PYBUFFER_CLASS_KEY: &str = "_testbuffer.Py_buffer.class";

const ATTR_DATA: &str = "_data";
const ATTR_SHAPE: &str = "_shape";
const ATTR_STRIDES: &str = "_strides";
const ATTR_SUBOFFSETS: &str = "_suboffsets";
const ATTR_FORMAT: &str = "_format";
const ATTR_ITEMSIZE: &str = "_itemsize";
const ATTR_OFFSET: &str = "_offset";
const ATTR_WRITABLE: &str = "_writable";
const ATTR_FLAGS: &str = "_flags";
const ATTR_OBJ: &str = "_obj";
const ATTR_EXPORTS: &str = "_exports";
const ATTR_VAREXPORT: &str = "_varexport";
const ATTR_CONSUMER: &str = "_consumer";
const ATTR_PREV: &str = "_prev";

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn buffer_error(message: &str) -> i64 {
    exc::raise_with_message(BUFFER_ERROR, message)
}

fn overflow_error(message: &str) -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, message)
}

fn not_implemented(message: &str) -> i64 {
    exc::raise_with_message(NOT_IMPLEMENTED_ERROR, message)
}

// -------------------------------------------------------------------------
// Attribute helpers
// -------------------------------------------------------------------------

fn set_attr(obj: i64, name: &str, stored: i64) {
    value::instance_setattr(obj, name.to_string(), stored);
}

fn attr(obj: i64, name: &str) -> Option<i64> {
    value::instance_attr(obj, name)
}

fn int_attr(obj: i64, name: &str) -> i64 {
    attr(obj, name)
        .and_then(value::as_integer)
        .unwrap_or(0)
}

fn bool_attr(obj: i64, name: &str) -> bool {
    attr(obj, name) == Some(value::TRUE)
}

fn ints_attr(obj: i64, name: &str) -> Vec<i64> {
    attr(obj, name)
        .and_then(value::as_tuple)
        .map(|items| items.iter().filter_map(|&item| value::as_integer(item)).collect())
        .unwrap_or_default()
}

fn format_attr(obj: i64) -> Option<String> {
    match attr(obj, ATTR_FORMAT) {
        Some(format) if format != NONE => value::as_str(format).map(|text| text.to_string()),
        _ => None,
    }
}

fn set_int_attr(obj: i64, name: &str, number: i64) {
    set_attr(obj, name, ops::int_result(number));
}

fn set_bool_attr(obj: i64, name: &str, flag: bool) {
    set_attr(obj, name, value::bool_value(flag));
}

fn ints_value(items: &[i64]) -> i64 {
    let values: Vec<i64> = items.iter().map(|&item| ops::int_result(item)).collect();
    value::tuple_new(values)
}

fn set_ints_attr(obj: i64, name: &str, items: &[i64]) {
    set_attr(obj, name, ints_value(items));
}

fn list_of(items: Vec<i64>) -> i64 {
    let roots = heap::root_values(&items);
    let list = value::list_new(items);
    heap::release_roots(roots);
    list
}

fn raw_snapshot(obj: i64) -> Vec<u8> {
    attr(obj, ATTR_DATA)
        .and_then(value::as_bytes)
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default()
}

fn shape_of(obj: i64) -> Vec<i64> {
    ints_attr(obj, ATTR_SHAPE)
}

fn strides_of(obj: i64) -> Vec<i64> {
    ints_attr(obj, ATTR_STRIDES)
}

fn suboffsets_of(obj: i64) -> Vec<i64> {
    ints_attr(obj, ATTR_SUBOFFSETS)
}

fn itemsize_of(obj: i64) -> i64 {
    int_attr(obj, ATTR_ITEMSIZE).max(1)
}

fn offset_of(obj: i64) -> i64 {
    int_attr(obj, ATTR_OFFSET)
}

fn product(items: &[i64]) -> i64 {
    let mut total: i128 = 1;
    for &item in items {
        total = total.saturating_mul(item.max(0) as i128);
        if total > i64::MAX as i128 {
            return i64::MAX;
        }
    }
    total as i64
}

fn nbytes_of(obj: i64) -> i64 {
    product(&shape_of(obj)).saturating_mul(itemsize_of(obj))
}

fn is_ndarray(v: i64) -> bool {
    value::instance_class_value(v) == Some(ndarray_class())
}

fn is_pybuffer(v: i64) -> bool {
    value::instance_class_value(v) == Some(pybuffer_class())
}

fn is_exporter(v: i64) -> bool {
    is_ndarray(v) || value::as_bytes(v).is_some()
}

fn is_slice(v: i64) -> bool {
    value::instance_class_value(v) == Some(crate::builtins::slice_class())
}

fn is_ellipsis(v: i64) -> bool {
    value::type_name(v) == "ellipsis"
}

// -------------------------------------------------------------------------
// Structure helpers
// -------------------------------------------------------------------------

fn c_strides(shape: &[i64], itemsize: i64) -> Vec<i64> {
    let mut strides = vec![0i64; shape.len()];
    let mut stride = itemsize;
    for index in (0..shape.len()).rev() {
        strides[index] = stride;
        stride = stride.saturating_mul(shape[index]);
    }
    strides
}

fn f_strides(shape: &[i64], itemsize: i64) -> Vec<i64> {
    let mut strides = vec![0i64; shape.len()];
    let mut stride = itemsize;
    for index in 0..shape.len() {
        strides[index] = stride;
        stride = stride.saturating_mul(shape[index]);
    }
    strides
}

fn strides_match(shape: &[i64], strides: &[i64], expected: &[i64]) -> bool {
    if strides.len() != expected.len() {
        return false;
    }
    for index in 0..shape.len() {
        if shape[index] > 1 && strides[index] != expected[index] {
            return false;
        }
    }
    true
}

fn has_suboffsets(suboffsets: &[i64]) -> bool {
    suboffsets.iter().any(|&offset| offset >= 0)
}

fn c_contiguous(shape: &[i64], strides: &[i64], suboffsets: &[i64], itemsize: i64) -> bool {
    if has_suboffsets(suboffsets) {
        return false;
    }
    if strides.is_empty() {
        return true;
    }
    strides_match(shape, strides, &c_strides(shape, itemsize))
}

fn f_contiguous(shape: &[i64], strides: &[i64], suboffsets: &[i64], itemsize: i64) -> bool {
    if has_suboffsets(suboffsets) {
        return false;
    }
    if strides.is_empty() {
        return true;
    }
    strides_match(shape, strides, &f_strides(shape, itemsize))
}

fn set_structure(
    obj: i64,
    data: i64,
    shape: &[i64],
    strides: &[i64],
    suboffsets: &[i64],
    format: Option<&str>,
    itemsize: i64,
    offset: i64,
    writable: bool,
    flags: i64,
    base: i64,
) {
    set_attr(obj, ATTR_DATA, data);
    set_ints_attr(obj, ATTR_SHAPE, shape);
    set_ints_attr(obj, ATTR_STRIDES, strides);
    set_ints_attr(obj, ATTR_SUBOFFSETS, suboffsets);
    set_attr(
        obj,
        ATTR_FORMAT,
        match format {
            Some(format) => value::str_alloc(format.to_string()),
            None => NONE,
        },
    );
    set_int_attr(obj, ATTR_ITEMSIZE, itemsize);
    set_int_attr(obj, ATTR_OFFSET, offset);
    set_bool_attr(obj, ATTR_WRITABLE, writable);
    set_int_attr(obj, ATTR_FLAGS, flags);
    set_attr(obj, ATTR_OBJ, base);
    publish(obj);
}

/// Publish the derived read-only properties of an ndarray/Py_buffer.
fn publish(obj: i64) {
    let shape = shape_of(obj);
    let strides = strides_of(obj);
    let suboffsets = suboffsets_of(obj);
    let itemsize = itemsize_of(obj);
    let offset = offset_of(obj);
    let writable = bool_attr(obj, ATTR_WRITABLE);
    let format = format_attr(obj);
    let data_len = raw_snapshot(obj).len();
    let nbytes = product(&shape).saturating_mul(itemsize);
    set_int_attr(obj, "flags", int_attr(obj, ATTR_FLAGS));
    set_int_attr(obj, "offset", offset);
    set_attr(obj, "obj", attr(obj, ATTR_OBJ).unwrap_or(NONE));
    set_int_attr(obj, "nbytes", nbytes);
    set_bool_attr(obj, "readonly", !writable);
    set_int_attr(obj, "itemsize", itemsize);
    set_attr(
        obj,
        "format",
        match &format {
            Some(format) => value::str_alloc(format.clone()),
            None => NONE,
        },
    );
    set_int_attr(obj, "ndim", shape.len() as i64);
    set_attr(obj, "shape", ints_value(&shape));
    set_attr(obj, "strides", ints_value(&strides));
    set_attr(obj, "suboffsets", ints_value(&suboffsets));
    let c = c_contiguous(&shape, &strides, &suboffsets, itemsize);
    let f = f_contiguous(&shape, &strides, &suboffsets, itemsize);
    set_bool_attr(obj, "c_contiguous", c);
    set_bool_attr(obj, "f_contiguous", f);
    set_bool_attr(obj, "contiguous", c || f);
    let _ = (data_len, offset);
}

// -------------------------------------------------------------------------
// Formats
// -------------------------------------------------------------------------

fn calcsize(format: &str) -> Result<i64, i64> {
    if format.is_empty() {
        return Err(value_error("invalid format: empty string"));
    }
    let format_value = value::str_alloc(format.to_string());
    let size = super::struct_mod::call("calcsize", &[format_value]);
    if exc::pending() {
        return Err(NONE);
    }
    match value::as_integer(size) {
        Some(size) if size > 0 => Ok(size),
        _ => Err(value_error("invalid format")),
    }
}

fn format_value_count(format: &str, itemsize: i64) -> Result<usize, i64> {
    let format_value = value::str_alloc(format.to_string());
    let zeros = value::bytes_new(vec![0u8; itemsize.max(0) as usize]);
    let unpacked = super::struct_mod::call("unpack_from", &[format_value, zeros]);
    if exc::pending() {
        return Err(NONE);
    }
    Ok(value::as_tuple(unpacked).map_or(1, |values| values.len()))
}

fn pack_item(format: &str, count: usize, item: i64) -> Result<Vec<u8>, i64> {
    let format_value = value::str_alloc(format.to_string());
    let mut arguments = Vec::with_capacity(count + 1);
    arguments.push(format_value);
    if count == 1 {
        if value::as_list(item).or_else(|| value::as_tuple(item)).is_some() {
            return Err(value_error(
                "invalid items: a single-field format takes a scalar item",
            ));
        }
        arguments.push(item);
    } else {
        let Some(values) = value::as_list(item).or_else(|| value::as_tuple(item)) else {
            return Err(value_error("invalid items: expected a tuple for a multi-field format"));
        };
        if values.len() != count {
            return Err(value_error("invalid items: item count does not match the format"));
        }
        arguments.extend_from_slice(values);
    }
    let packed = super::struct_mod::call("pack", &arguments);
    if exc::pending() {
        return Err(NONE);
    }
    match value::as_bytes(packed) {
        Some(bytes) => Ok(bytes.to_vec()),
        None => Err(value_error("invalid items")),
    }
}

fn read_item(obj: i64, position: i64) -> i64 {
    let Some(format) = format_attr(obj) else {
        return value_error("ndarray: tolist() does not support format=NULL, use tobytes()");
    };
    let itemsize = itemsize_of(obj);
    let data = raw_snapshot(obj);
    let start = position.max(0) as usize;
    let end = match start.checked_add(itemsize.max(0) as usize) {
        Some(end) => end,
        None => return value_error("item is outside the buffer"),
    };
    if end > data.len() {
        return buffer_error("item is outside the buffer");
    }
    let chunk = value::bytes_new(data[start..end].to_vec());
    let format_value = value::str_alloc(format);
    let unpacked = super::struct_mod::call("unpack_from", &[format_value, chunk]);
    if exc::pending() {
        return NONE;
    }
    match value::as_tuple(unpacked) {
        Some(values) if values.len() == 1 => values[0],
        _ => unpacked,
    }
}

fn pointer_position(obj: i64, indices: &[i64]) -> Result<i64, i64> {
    let shape = shape_of(obj);
    let strides = strides_of(obj);
    let suboffsets = suboffsets_of(obj);
    if indices.len() != shape.len() {
        return Err(value_error("invalid number of indices"));
    }
    if has_suboffsets(&suboffsets) {
        return Err(not_implemented(
            "suboffset dereferencing needs real pointer storage, which this runtime does not expose",
        ));
    }
    let mut position = offset_of(obj);
    for (dimension, &index) in indices.iter().enumerate() {
        let mut index = index;
        if index < 0 {
            index += shape[dimension];
        }
        if index < 0 || index >= shape[dimension] {
            return Err(value_error("index out of bounds"));
        }
        let stride = strides.get(dimension).copied().unwrap_or(0);
        position = position.saturating_add(index.saturating_mul(stride));
    }
    let data_len = raw_snapshot(obj).len() as i64;
    if position < 0 || position + itemsize_of(obj) > data_len {
        return Err(value_error("striding out of the memory bounds"));
    }
    Ok(position)
}

fn for_each_index(shape: &[i64], mut visit: impl FnMut(&[i64])) {
    if shape.is_empty() {
        visit(&[]);
        return;
    }
    if shape.iter().any(|&dimension| dimension <= 0) {
        return;
    }
    let mut indices = vec![0i64; shape.len()];
    loop {
        visit(&indices);
        let mut dimension = shape.len();
        loop {
            if dimension == 0 {
                return;
            }
            dimension -= 1;
            indices[dimension] += 1;
            if indices[dimension] < shape[dimension] {
                break;
            }
            indices[dimension] = 0;
        }
    }
}

fn tobytes(obj: i64) -> i64 {
    let shape = shape_of(obj);
    let strides = strides_of(obj);
    let suboffsets = suboffsets_of(obj);
    let itemsize = itemsize_of(obj);
    let data = raw_snapshot(obj);
    let nbytes = product(&shape).saturating_mul(itemsize);
    if nbytes == 0 {
        return value::bytes_new(Vec::new());
    }
    if c_contiguous(&shape, &strides, &suboffsets, itemsize) {
        let start = offset_of(obj).max(0) as usize;
        let end = start.saturating_add(nbytes.max(0) as usize);
        if end > data.len() {
            return buffer_error("buffer is outside its storage");
        }
        return value::bytes_new(data[start..end].to_vec());
    }
    let mut output = Vec::with_capacity(nbytes.max(0) as usize);
    let base = offset_of(obj);
    for_each_index(&shape, |indices| {
        let mut position = base;
        for (dimension, &index) in indices.iter().enumerate() {
            position += index * strides.get(dimension).copied().unwrap_or(0);
        }
        let start = position.max(0) as usize;
        let end = start.saturating_add(itemsize.max(0) as usize);
        if end <= data.len() {
            output.extend_from_slice(&data[start..end]);
        }
    });
    value::bytes_new(output)
}

fn tolist_at(obj: i64, shape: &[i64], strides: &[i64], dimension: usize, base: i64) -> i64 {
    if dimension == shape.len() {
        return read_item(obj, base);
    }
    let mut items = Vec::with_capacity(shape[dimension].max(0) as usize);
    for index in 0..shape[dimension] {
        let position = base + index * strides.get(dimension).copied().unwrap_or(0);
        items.push(tolist_at(obj, shape, strides, dimension + 1, position));
        if exc::pending() {
            return NONE;
        }
    }
    list_of(items)
}

fn tolist(obj: i64) -> i64 {
    if format_attr(obj).is_none() {
        return value_error("ndarray: tolist() does not support format=NULL, use tobytes()");
    }
    let shape = shape_of(obj);
    let strides = strides_of(obj);
    tolist_at(obj, &shape, &strides, 0, offset_of(obj))
}

// -------------------------------------------------------------------------
// Classes
// -------------------------------------------------------------------------

fn method(name: &'static str, native: &'static str, arity: usize) -> (String, i64, MethodKind) {
    (
        name.to_string(),
        value::native_function_new("_testbuffer", native, arity),
        MethodKind::Normal,
    )
}

pub(crate) fn ndarray_class() -> i64 {
    if let Some(class) = globals::registry_get(NDARRAY_CLASS_KEY) {
        return class;
    }
    let methods = vec![
        method("__init__", "ndarray.__init__", 8),
        method("tolist", "ndarray.tolist", 1),
        method("tobytes", "ndarray.tobytes", 1),
        method("push", "ndarray.push", 6),
        method("pop", "ndarray.pop", 1),
        method("add_suboffsets", "ndarray.add_suboffsets", 1),
        method("memoryview_from_buffer", "ndarray.memoryview_from_buffer", 1),
        method("getbuffer", "ndarray.getbuffer", 2),
        method("releasebuffer", "ndarray.releasebuffer", 2),
        method("__buffer__", "ndarray.__buffer__", 2),
        method("__release_buffer__", "ndarray.__release_buffer__", 2),
        method("__getitem__", "ndarray.__getitem__", 2),
        method("__setitem__", "ndarray.__setitem__", 3),
        method("__len__", "ndarray.__len__", 1),
        method("__repr__", "ndarray.__repr__", 1),
        method("__eq__", "ndarray.__eq__", 2),
        method("__hash__", "ndarray.__hash__", 1),
    ];
    let class = value::class_new_full("ndarray".to_string(), NONE, methods, Vec::new());
    globals::registry_set(NDARRAY_CLASS_KEY, class);
    class
}

pub(crate) fn pybuffer_class() -> i64 {
    if let Some(class) = globals::registry_get(PYBUFFER_CLASS_KEY) {
        return class;
    }
    let methods = vec![
        method("release", "Py_buffer.release", 1),
        method("buf", "Py_buffer.buf", 1),
        method("ptr", "Py_buffer.ptr", 1),
        method("__repr__", "Py_buffer.__repr__", 1),
    ];
    let class = value::class_new_full("Py_buffer".to_string(), NONE, methods, Vec::new());
    globals::registry_set(PYBUFFER_CLASS_KEY, class);
    class
}

fn new_ndarray() -> i64 {
    let class = ndarray_class();
    let class_root = heap::root_values(std::slice::from_ref(&class));
    let instance = value::instance_new(class);
    let instance_root = heap::root_values(std::slice::from_ref(&instance));
    let empty = value::tuple_new(Vec::new());
    set_attr(instance, ATTR_DATA, value::bytearray_new(Vec::new()));
    set_attr(instance, ATTR_SHAPE, empty);
    set_attr(instance, ATTR_STRIDES, empty);
    set_attr(instance, ATTR_SUBOFFSETS, empty);
    set_attr(instance, ATTR_FORMAT, NONE);
    set_int_attr(instance, ATTR_ITEMSIZE, 1);
    set_int_attr(instance, ATTR_OFFSET, 0);
    set_bool_attr(instance, ATTR_WRITABLE, false);
    set_int_attr(instance, ATTR_FLAGS, 0);
    set_attr(instance, ATTR_OBJ, NONE);
    set_int_attr(instance, ATTR_EXPORTS, 0);
    set_bool_attr(instance, ATTR_VAREXPORT, false);
    set_bool_attr(instance, ATTR_CONSUMER, false);
    set_attr(instance, ATTR_PREV, value::list_new(Vec::new()));
    publish(instance);
    heap::release_roots(instance_root);
    heap::release_roots(class_root);
    instance
}

fn new_pybuffer() -> i64 {
    let class = pybuffer_class();
    let class_root = heap::root_values(std::slice::from_ref(&class));
    let instance = value::instance_new(class);
    let instance_root = heap::root_values(std::slice::from_ref(&instance));
    set_attr(instance, "_obj", NONE);
    set_attr(instance, "_source", NONE);
    set_bool_attr(instance, "_released", false);
    set_int_attr(instance, "len", 0);
    set_int_attr(instance, "itemsize", 1);
    set_bool_attr(instance, "readonly", true);
    set_int_attr(instance, "ndim", 1);
    set_attr(instance, "format", NONE);
    set_attr(instance, "shape", NONE);
    set_attr(instance, "strides", NONE);
    set_attr(instance, "suboffsets", NONE);
    set_int_attr(instance, "nbytes", 0);
    set_attr(instance, "_obj", NONE);
    heap::release_roots(instance_root);
    heap::release_roots(class_root);
    instance
}

// -------------------------------------------------------------------------
// getbuffer
// -------------------------------------------------------------------------

fn request_writable(flags: i64) -> bool {
    flags & PYBUF_WRITABLE != 0
}

fn request_format(flags: i64) -> bool {
    flags & PYBUF_FORMAT != 0
}

fn request_shape(flags: i64) -> bool {
    flags & PYBUF_ND != 0
}

fn request_strides(flags: i64) -> bool {
    flags & PYBUF_STRIDES == PYBUF_STRIDES
}

fn request_indirect(flags: i64) -> bool {
    flags & PYBUF_INDIRECT == PYBUF_INDIRECT
}

fn validate_getbuf(source: i64, flags: i64) -> Result<(), i64> {
    if int_attr(source, ATTR_FLAGS) & ND_GETBUF_FAIL != 0 {
        return Err(buffer_error("ND_GETBUF_FAIL: forced test exception"));
    }
    if request_writable(flags) && !bool_attr(source, ATTR_WRITABLE) {
        return Err(buffer_error("ndarray is not writable"));
    }
    let shape = shape_of(source);
    let strides = strides_of(source);
    let suboffsets = suboffsets_of(source);
    let itemsize = itemsize_of(source);
    if flags & PYBUF_C_CONTIGUOUS == PYBUF_C_CONTIGUOUS
        && !c_contiguous(&shape, &strides, &suboffsets, itemsize)
    {
        return Err(buffer_error("ndarray is not C-contiguous"));
    }
    if flags & PYBUF_F_CONTIGUOUS == PYBUF_F_CONTIGUOUS
        && !f_contiguous(&shape, &strides, &suboffsets, itemsize)
    {
        return Err(buffer_error("ndarray is not Fortran contiguous"));
    }
    if flags & PYBUF_ANY_CONTIGUOUS == PYBUF_ANY_CONTIGUOUS
        && !c_contiguous(&shape, &strides, &suboffsets, itemsize)
        && !f_contiguous(&shape, &strides, &suboffsets, itemsize)
    {
        return Err(buffer_error("ndarray is not contiguous"));
    }
    if !request_indirect(flags) && has_suboffsets(&suboffsets) {
        return Err(buffer_error("ndarray cannot be represented without suboffsets"));
    }
    if !request_strides(flags) && !c_contiguous(&shape, &strides, &suboffsets, itemsize) {
        return Err(buffer_error("ndarray is not C-contiguous"));
    }
    Ok(())
}

fn buffer_from(source: i64, flags: i64) -> i64 {
    let buffer = new_pybuffer();
    let buffer_root = heap::root_values(std::slice::from_ref(&buffer));
    let mut released = false;
    if is_ndarray(source) || is_pybuffer(source) {
        if is_pybuffer(source) {
            let source_root = heap::root_values(std::slice::from_ref(&source));
            set_attr(buffer, "_obj", attr(source, "_obj").unwrap_or(NONE));
            set_int_attr(buffer, "len", int_attr(source, "len"));
            set_int_attr(buffer, "itemsize", int_attr(source, "itemsize"));
            set_bool_attr(buffer, "readonly", bool_attr(source, "readonly"));
            set_int_attr(buffer, "ndim", int_attr(source, "ndim"));
            set_attr(buffer, "format", attr(source, "format").unwrap_or(NONE));
            set_attr(buffer, "shape", attr(source, "shape").unwrap_or(NONE));
            set_attr(buffer, "strides", attr(source, "strides").unwrap_or(NONE));
            set_attr(buffer, "suboffsets", attr(source, "suboffsets").unwrap_or(NONE));
            set_int_attr(buffer, "nbytes", int_attr(source, "nbytes"));
            set_attr(buffer, "_source", source);
            heap::release_roots(source_root);
            heap::release_roots(buffer_root);
            return buffer;
        }
        if let Err(error) = validate_getbuf(source, flags) {
            heap::release_roots(buffer_root);
            return error;
        }
        let shape = shape_of(source);
        let strides = strides_of(source);
        let suboffsets = suboffsets_of(source);
        let nbytes = nbytes_of(source);
        set_attr(buffer, "_obj", source);
        set_attr(buffer, "_source", source);
        set_int_attr(buffer, "len", nbytes);
        set_int_attr(buffer, "itemsize", itemsize_of(source));
        set_bool_attr(buffer, "readonly", !bool_attr(source, ATTR_WRITABLE));
        set_int_attr(buffer, "ndim", shape.len() as i64);
        set_attr(
            buffer,
            "format",
            if request_format(flags) {
                match format_attr(source) {
                    Some(format) => value::str_alloc(format),
                    None => value::str_alloc("B".to_string()),
                }
            } else {
                NONE
            },
        );
        set_attr(
            buffer,
            "shape",
            if request_shape(flags) {
                ints_value(&shape)
            } else {
                NONE
            },
        );
        set_attr(
            buffer,
            "strides",
            if request_strides(flags) {
                ints_value(&strides)
            } else {
                NONE
            },
        );
        set_attr(
            buffer,
            "suboffsets",
            if request_indirect(flags) {
                ints_value(&suboffsets)
            } else {
                NONE
            },
        );
        set_int_attr(buffer, "nbytes", nbytes);
        let exports = int_attr(source, ATTR_EXPORTS);
        set_int_attr(source, ATTR_EXPORTS, exports.saturating_add(1));
        released = false;
    } else if let Some(bytes) = value::as_bytes(source) {
        let length = bytes.len() as i64;
        let writable = value::as_bytearray(source).is_some();
        if request_writable(flags) && !writable {
            heap::release_roots(buffer_root);
            return buffer_error("object is not writable");
        }
        set_attr(buffer, "_obj", source);
        set_attr(buffer, "_source", source);
        set_int_attr(buffer, "len", length);
        set_int_attr(buffer, "itemsize", 1);
        set_bool_attr(buffer, "readonly", !writable);
        set_int_attr(buffer, "ndim", 1);
        set_attr(
            buffer,
            "format",
            if request_format(flags) {
                value::str_alloc("B".to_string())
            } else {
                NONE
            },
        );
        set_attr(
            buffer,
            "shape",
            if request_shape(flags) {
                ints_value(&[length])
            } else {
                NONE
            },
        );
        set_attr(
            buffer,
            "strides",
            if request_strides(flags) {
                ints_value(&[1])
            } else {
                NONE
            },
        );
        set_attr(buffer, "suboffsets", NONE);
        set_int_attr(buffer, "nbytes", length);
    } else {
        heap::release_roots(buffer_root);
        return type_error("object does not implement the buffer protocol");
    }
    set_bool_attr(buffer, "_released", released);
    heap::release_roots(buffer_root);
    buffer
}

fn release_buffer(view: i64) -> i64 {
    if !is_pybuffer(view) {
        return type_error("releasebuffer: argument must be a Py_buffer");
    }
    if bool_attr(view, "_released") {
        return value_error("operation forbidden on released buffer");
    }
    let source = attr(view, "_source").unwrap_or(NONE);
    if is_ndarray(source) {
        let exports = int_attr(source, ATTR_EXPORTS);
        set_int_attr(source, ATTR_EXPORTS, exports.saturating_sub(1).max(0));
    }
    set_bool_attr(view, "_released", true);
    NONE
}

// -------------------------------------------------------------------------
// Indexing
// -------------------------------------------------------------------------

fn normalize_index(index: i64, length: i64) -> Result<i64, i64> {
    let mut index = index;
    if index < 0 {
        index += length;
    }
    if index < 0 || index >= length {
        return Err(exc::raise_with_message(INDEX_ERROR, "index out of bounds"));
    }
    Ok(index)
}

fn make_subview(
    source: i64,
    base: i64,
    shape: Vec<i64>,
    strides: Vec<i64>,
    suboffsets: Vec<i64>,
) -> i64 {
    let view = new_ndarray();
    let view_root = heap::root_values(std::slice::from_ref(&view));
    let data = attr(source, ATTR_DATA).unwrap_or(NONE);
    set_structure(
        view,
        data,
        &shape,
        &strides,
        &suboffsets,
        format_attr(source).as_deref(),
        itemsize_of(source),
        base,
        bool_attr(source, ATTR_WRITABLE),
        int_attr(source, ATTR_FLAGS),
        source,
    );
    set_bool_attr(view, ATTR_CONSUMER, true);
    let exports = int_attr(source, ATTR_EXPORTS);
    set_int_attr(source, ATTR_EXPORTS, exports.saturating_add(1));
    heap::release_roots(view_root);
    view
}

fn slice_bounds(slice: i64, length: i64) -> Result<(i64, i64, i64), i64> {
    let bound = |field: &str| -> Result<Option<i64>, i64> {
        let value_arg = attr(slice, field).unwrap_or(NONE);
        if value_arg == NONE {
            return Ok(None);
        }
        match value::as_integer(value_arg) {
            Some(number) => Ok(Some(number)),
            None => Err(type_error(
                "slice indices must be integers or None or have an __index__ method",
            )),
        }
    };
    let start = bound("start")?;
    let stop = bound("stop")?;
    let step = bound("step")?;
    let step = step.unwrap_or(1) as i128;
    if step == 0 {
        return Err(value_error("slice step cannot be zero"));
    }
    let length = length as i128;
    let start = match start {
        None => {
            if step < 0 {
                length - 1
            } else {
                0
            }
        }
        Some(start) => {
            let start = start as i128;
            if start < 0 {
                (start + length).max(if step < 0 { -1 } else { 0 })
            } else {
                start.min(if step < 0 { length - 1 } else { length })
            }
        }
    };
    let stop = match stop {
        None => {
            if step < 0 {
                -1
            } else {
                length
            }
        }
        Some(stop) => {
            let stop = stop as i128;
            if stop < 0 {
                (stop + length).max(-1)
            } else {
                stop.min(length)
            }
        }
    };
    let slicelen = if step > 0 {
        if stop > start {
            (stop - start + step - 1) / step
        } else {
            0
        }
    } else if start > stop {
        (start - stop + (-step) - 1) / (-step)
    } else {
        0
    };
    let clamp = |number: i128| i64::try_from(number).unwrap_or(if number < 0 {
        i64::MIN
    } else {
        i64::MAX
    });
    Ok((clamp(start), clamp(stop), clamp(step)))
}

fn indexed_position(obj: i64, indices: &[i64]) -> Result<i64, i64> {
    pointer_position(obj, indices)
}

fn getitem(obj: i64, key: i64) -> i64 {
    let shape = shape_of(obj);
    if shape.is_empty() {
        if is_ellipsis(key)
            || (value::as_tuple(key).is_some_and(|items| items.is_empty()))
        {
            return read_item(obj, offset_of(obj));
        }
        return type_error("invalid indexing of scalar");
    }
    if let Some(index) = value::as_integer(key) {
        return getitem_index(obj, index);
    }
    if let Some(big) = value::to_bigint(key) {
        return overflow_error(&format!("index {big} does not fit in a C ssize_t"));
    }
    if is_slice(key) {
        if shape.len() != 1 {
            return type_error("cannot slice a multi-dimensional array with a single slice");
        }
        let Ok((start, _stop, step)) = slice_bounds(key, shape[0]) else {
            return NONE;
        };
        let slicelen = slice_length(key, shape[0]);
        let strides = strides_of(obj);
        let base = offset_of(obj) + start * strides.first().copied().unwrap_or(0);
        let stride = strides.first().copied().unwrap_or(itemsize_of(obj)) * step;
        return make_subview(obj, base, vec![slicelen], vec![stride], Vec::new());
    }
    if let Some(items) = value::as_tuple(key).or_else(|| value::as_list(key)) {
        if items.len() != shape.len() {
            return type_error("invalid number of indices");
        }
        let strides = strides_of(obj);
        let mut base = offset_of(obj);
        let mut sliced = false;
        for (dimension, &item) in items.iter().enumerate() {
            if is_slice(item) {
                sliced = true;
                let Ok((start, _stop, step)) = slice_bounds(item, shape[dimension]) else {
                    return NONE;
                };
                let stride = strides.get(dimension).copied().unwrap_or(0);
                base += start * stride;
                // The stride is rewritten below through a rebuilt vector.
                let _ = step;
            } else if let Some(index) = value::as_integer(item) {
                let Ok(index) = normalize_index(index, shape[dimension]) else {
                    return NONE;
                };
                base += index * strides.get(dimension).copied().unwrap_or(0);
            } else {
                return type_error("cannot index memory using this key type");
            }
        }
        if !sliced {
            return read_item(obj, base);
        }
        let mut new_shape = Vec::with_capacity(shape.len());
        let mut new_strides = Vec::with_capacity(shape.len());
        for (dimension, &item) in items.iter().enumerate() {
            if is_slice(item) {
                let Ok((_start, _stop, step)) = slice_bounds(item, shape[dimension]) else {
                    return NONE;
                };
                new_shape.push(slice_length(item, shape[dimension]));
                new_strides.push(strides.get(dimension).copied().unwrap_or(0) * step);
            } else {
                new_shape.push(1);
                new_strides.push(0);
            }
        }
        return make_subview(obj, base, new_shape, new_strides, Vec::new());
    }
    type_error("cannot index memory using this key type")
}

fn slice_length(slice: i64, length: i64) -> i64 {
    let Ok((start, stop, step)) = slice_bounds(slice, length) else {
        return 0;
    };
    if step > 0 {
        if stop > start {
            (stop - start + step - 1) / step
        } else {
            0
        }
    } else if start > stop {
        (start - stop + (-step) - 1) / (-step)
    } else {
        0
    }
}

fn getitem_index(obj: i64, index: i64) -> i64 {
    let shape = shape_of(obj);
    let Ok(index) = normalize_index(index, shape[0]) else {
        return NONE;
    };
    let strides = strides_of(obj);
    let stride = strides.first().copied().unwrap_or(itemsize_of(obj));
    let base = offset_of(obj) + index * stride;
    if shape.len() == 1 {
        return read_item(obj, base);
    }
    let suboffsets = suboffsets_of(obj);
    make_subview(
        obj,
        base,
        shape[1..].to_vec(),
        strides[1..].to_vec(),
        if suboffsets.is_empty() {
            Vec::new()
        } else {
            suboffsets[1..].to_vec()
        },
    )
}

fn setitem(obj: i64, key: i64, new_value: i64) -> i64 {
    if !bool_attr(obj, ATTR_WRITABLE) {
        return type_error("cannot modify read-only memory");
    }
    let shape = shape_of(obj);
    if shape.is_empty() {
        if !(is_ellipsis(key) || value::as_tuple(key).is_some_and(|items| items.is_empty())) {
            return type_error("invalid indexing of scalar");
        }
        return write_item(obj, offset_of(obj), new_value);
    }
    let indices: Vec<i64> = if let Some(index) = value::as_integer(key) {
        vec![index]
    } else if let Some(items) = value::as_tuple(key).or_else(|| value::as_list(key)) {
        let mut indices = Vec::with_capacity(items.len());
        for &item in items {
            if is_slice(item) {
                return not_implemented(
                    "slice assignment is not supported by this buffer implementation",
                );
            }
            match value::as_integer(item) {
                Some(index) => indices.push(index),
                None => return type_error("invalid index"),
            }
        }
        indices
    } else {
        return type_error("invalid index");
    };
    if indices.len() != shape.len() {
        return type_error("invalid number of indices");
    }
    let Ok(position) = indexed_position(obj, &indices) else {
        return NONE;
    };
    write_item(obj, position, new_value)
}

fn write_item(obj: i64, position: i64, new_value: i64) -> i64 {
    let Some(format) = format_attr(obj) else {
        return value_error("cannot write items to a buffer without a format");
    };
    let itemsize = itemsize_of(obj);
    let count = match format_value_count(&format, itemsize) {
        Ok(count) => count,
        Err(error) => return error,
    };
    let bytes = match pack_item(&format, count, new_value) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    if bytes.len() != itemsize.max(0) as usize {
        return value_error("packed item size does not match itemsize");
    }
    let data = attr(obj, ATTR_DATA).unwrap_or(NONE);
    let Some(target) = value::bytearray_bytes_mut(data) else {
        return type_error("cannot modify read-only memory");
    };
    let start = position.max(0) as usize;
    let end = start.saturating_add(bytes.len());
    if end > target.len() {
        return buffer_error("item is outside the buffer");
    }
    target[start..end].copy_from_slice(&bytes);
    NONE
}

// -------------------------------------------------------------------------
// Construction
// -------------------------------------------------------------------------

struct InitSpec {
    items: Option<i64>,
    shape: Option<i64>,
    strides: Option<i64>,
    offset: i64,
    format: i64,
    flags: i64,
    getbuf: i64,
}

fn init_spec_from_positional(args: &[i64]) -> InitSpec {
    let mut spec = InitSpec {
        items: args.get(1).copied(),
        shape: args.get(2).copied().filter(|&v| v != NONE),
        strides: args.get(3).copied().filter(|&v| v != NONE),
        offset: 0,
        format: value::str_alloc("B".to_string()),
        flags: 0,
        getbuf: GETBUF_UNSET,
    };
    if let Some(offset) = args.get(4).copied() {
        if offset != NONE {
            spec.offset = value::as_integer(offset).unwrap_or(0);
        }
    }
    if let Some(format) = args.get(5).copied() {
        if format != NONE {
            spec.format = format;
        }
    }
    if let Some(flags) = args.get(6).copied() {
        spec.flags = value::as_integer(flags).unwrap_or(0);
    }
    if let Some(getbuf) = args.get(7).copied() {
        if getbuf != NONE {
            spec.getbuf = getbuf;
        }
    }
    spec
}

fn init_spec_with_keywords(args: &[i64], keywords: &[(String, i64)]) -> Result<InitSpec, i64> {
    let mut spec = init_spec_from_positional(args);
    for (name, value_arg) in keywords {
        match name.as_str() {
            "obj" => spec.items = Some(*value_arg),
            "shape" => spec.shape = Some(*value_arg),
            "strides" => spec.strides = Some(*value_arg),
            "offset" => match value::as_integer(*value_arg) {
                Some(offset) => spec.offset = offset,
                None => return Err(type_error("offset must be an integer")),
            },
            "format" => spec.format = *value_arg,
            "flags" => match value::as_integer(*value_arg) {
                Some(flags) => spec.flags = flags,
                None => return Err(type_error("flags must be an integer")),
            },
            "getbuf" => spec.getbuf = *value_arg,
            other => {
                return Err(type_error(&format!(
                    "ndarray() got an unexpected keyword argument '{other}'"
                )))
            }
        }
    }
    Ok(spec)
}

fn parse_int_sequence(value_arg: i64, what: &str) -> Result<Vec<i64>, i64> {
    let Some(items) = value::as_list(value_arg).or_else(|| value::as_tuple(value_arg)) else {
        return Err(type_error(&format!("{what} must be a list or a tuple")));
    };
    let mut result = Vec::with_capacity(items.len());
    for &item in items {
        match value::as_integer(item) {
            Some(number) => result.push(number),
            None if value::to_bigint(item).is_some() => {
                return Err(overflow_error(&format!(
                    "{what} item does not fit in a C ssize_t"
                )))
            }
            None => {
                return Err(type_error(&format!(
                    "{what} items must be integers, not {}",
                    value::type_name(item)
                )))
            }
        }
    }
    Ok(result)
}

fn validate_strides(
    shape: &[i64],
    strides: &[i64],
    itemsize: i64,
    offset: i64,
    data_len: i64,
) -> Result<(), i64> {
    if itemsize > 0 && data_len > 0 && (offset < 0 || offset + itemsize > data_len) {
        return Err(value_error("offset is outside the buffer"));
    }
    let total = product(shape);
    if total == 0 || total > 100_000 {
        return Ok(());
    }
    let mut indices = vec![0i64; shape.len()];
    loop {
        let mut position = offset;
        for (dimension, &index) in indices.iter().enumerate() {
            position += index * strides.get(dimension).copied().unwrap_or(0);
        }
        if position < 0 || position + itemsize > data_len {
            return Err(value_error("striding out of the memory bounds"));
        }
        if shape.is_empty() {
            break;
        }
        let mut dimension = shape.len();
        let mut done = false;
        loop {
            if dimension == 0 {
                done = true;
                break;
            }
            dimension -= 1;
            indices[dimension] += 1;
            if indices[dimension] < shape[dimension] {
                break;
            }
            indices[dimension] = 0;
        }
        if done || total == 0 {
            break;
        }
    }
    Ok(())
}

fn init_impl(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(self_object) = args.first().copied() else {
        return type_error("ndarray.__init__() missing self");
    };
    let spec = match init_spec_with_keywords(args, keywords) {
        Ok(spec) => spec,
        Err(error) => return error,
    };
    let Some(items) = spec.items else {
        return type_error("ndarray() missing the item argument");
    };

    let exporter = spec.shape.is_none() && is_exporter(items);
    if exporter {
        if spec.strides.is_some() || spec.offset != 0 {
            return type_error(
                "construction from exporter object only takes 'obj', 'getbuf' and 'flags' arguments",
            );
        }
        if !(spec.flags == 0 || spec.flags == ND_REDIRECT) {
            return type_error(
                "construction from exporter object only takes 'obj', 'getbuf' and 'flags' arguments",
            );
        }
        let getbuf = if spec.getbuf == GETBUF_UNSET {
            PYBUF_FULL_RO
        } else {
            spec.getbuf
        };
        let consumer = new_ndarray();
        let consumer_root = heap::root_values(std::slice::from_ref(&consumer));
        let result = if is_ndarray(items) {
            match validate_getbuf(items, getbuf) {
                Ok(()) => {
                    let data = attr(items, ATTR_DATA).unwrap_or(NONE);
                    let shape = shape_of(items);
                    let strides = if request_strides(getbuf) {
                        strides_of(items)
                    } else {
                        Vec::new()
                    };
                    let suboffsets = if request_indirect(getbuf) {
                        suboffsets_of(items)
                    } else {
                        Vec::new()
                    };
                    let format = if request_format(getbuf) {
                        Some(format_attr(items).unwrap_or_else(|| "B".to_string()))
                    } else {
                        None
                    };
                    set_structure(
                        consumer,
                        data,
                        &shape,
                        &strides,
                        &suboffsets,
                        format.as_deref(),
                        itemsize_of(items),
                        offset_of(items),
                        bool_attr(items, ATTR_WRITABLE),
                        int_attr(items, ATTR_FLAGS),
                        items,
                    );
                    let exports = int_attr(items, ATTR_EXPORTS);
                    set_int_attr(items, ATTR_EXPORTS, exports.saturating_add(1));
                    NONE
                }
                Err(error) => error,
            }
        } else {
            let bytes = value::as_bytes(items).unwrap_or_default().to_vec();
            let writable = value::as_bytearray(items).is_some();
            if request_writable(getbuf) && !writable {
                buffer_error("object is not writable")
            } else {
                let length = bytes.len() as i64;
                let data = if writable {
                    items
                } else {
                    value::bytes_new(bytes)
                };
                let simple_shape = [length];
                let simple_strides = [1i64];
                let no_strides: [i64; 0] = [];
                set_structure(
                    consumer,
                    data,
                    &simple_shape,
                    if request_strides(getbuf) {
                        &simple_strides
                    } else {
                        &no_strides
                    },
                    &[],
                    if request_format(getbuf) {
                        Some("B")
                    } else {
                        None
                    },
                    1,
                    0,
                    writable,
                    0,
                    items,
                );
                NONE
            }
        };
        set_bool_attr(consumer, ATTR_CONSUMER, true);
        if exc::pending() {
            heap::release_roots(consumer_root);
            return result;
        }
        heap::release_roots(consumer_root);
        return copy_into(self_object, consumer);
    }

    if spec.getbuf != GETBUF_UNSET {
        return type_error("getbuf argument only valid for construction from exporter object");
    }
    let Some(shape_value) = spec.shape else {
        return type_error("shape is a required argument when constructing from list, tuple or scalar");
    };
    let flags = spec.flags;
    let varyexport = flags & ND_VAREXPORT != 0;
    let flags = flags & !ND_VAREXPORT;

    let shape = match parse_int_sequence(shape_value, "shape") {
        Ok(shape) => shape,
        Err(error) => return error,
    };
    if shape.len() > ND_MAX_NDIM as usize {
        return value_error("shape has too many dimensions");
    }
    if shape.iter().any(|&dimension| dimension < 0) {
        return value_error("shape items must be non-negative");
    }
    if flags & ND_PIL != 0 && shape.is_empty() {
        return type_error("ND_PIL is only valid for arrays with ndim > 0");
    }
    if flags & ND_FORTRAN != 0 && spec.strides.is_some() {
        return type_error("ND_FORTRAN is only valid without strides");
    }

    let Some(format_text) = value::as_str(spec.format).map(|text| text.to_string()) else {
        return type_error("format must be a str");
    };
    let itemsize = match calcsize(&format_text) {
        Ok(itemsize) => itemsize,
        Err(error) => return error,
    };
    let count = match format_value_count(&format_text, itemsize) {
        Ok(count) => count,
        Err(error) => return error,
    };
    let nitems = product(&shape);
    if nitems as usize > BUFFER_MAX_SIZE as usize {
        return overflow_error("array is too large");
    }

    let values: Vec<i64> = if shape.is_empty() {
        vec![items]
    } else {
        match value::as_list(items).or_else(|| value::as_tuple(items)) {
            Some(values) if values.len() as i64 == nitems => values.to_vec(),
            Some(_) => return value_error("number of items does not match the shape"),
            None if nitems == 1 => vec![items],
            None => return type_error("items must be a list or a tuple"),
        }
    };

    let mut data = Vec::with_capacity(nitems.max(0) as usize * itemsize.max(0) as usize);
    for item in &values {
        match pack_item(&format_text, count, *item) {
            Ok(bytes) => data.extend_from_slice(&bytes),
            Err(error) => return error,
        }
    }
    if data.len() as i64 > BUFFER_MAX_SIZE {
        return overflow_error("array is too large");
    }

    let strides = match spec.strides {
        Some(strides_value) => match parse_int_sequence(strides_value, "strides") {
            Ok(strides) => {
                if strides.len() != shape.len() {
                    return value_error("strides must have one entry per dimension");
                }
                strides
            }
            Err(error) => return error,
        },
        None => {
            if flags & ND_FORTRAN != 0 {
                f_strides(&shape, itemsize)
            } else {
                c_strides(&shape, itemsize)
            }
        }
    };
    if itemsize > 0 && strides.iter().any(|stride| stride % itemsize != 0) {
        return value_error("strides must be a multiple of itemsize");
    }
    let suboffsets = if flags & ND_PIL != 0 {
        let mut suboffsets = vec![-1i64; shape.len()];
        if let Some(first) = suboffsets.first_mut() {
            *first = 0;
        }
        suboffsets
    } else {
        Vec::new()
    };
    let offset = spec.offset;
    if itemsize > 0 && offset % itemsize != 0 {
        return value_error("offset must be a multiple of itemsize");
    }
    let data_len = data.len() as i64;
    if let Err(error) = validate_strides(&shape, &strides, itemsize, offset, data_len) {
        return error;
    }

    let array = new_ndarray();
    let array_root = heap::root_values(std::slice::from_ref(&array));
    set_structure(
        array,
        value::bytearray_new(data),
        &shape,
        &strides,
        &suboffsets,
        Some(format_text.as_str()),
        itemsize,
        offset,
        flags & ND_WRITABLE != 0,
        flags,
        NONE,
    );
    set_bool_attr(array, ATTR_VAREXPORT, varyexport);
    heap::release_roots(array_root);
    copy_into(self_object, array)
}

/// Copies the freshly built array state into the `__init__` receiver.
fn copy_into(target: i64, source: i64) -> i64 {
    for name in [
        ATTR_DATA,
        ATTR_SHAPE,
        ATTR_STRIDES,
        ATTR_SUBOFFSETS,
        ATTR_FORMAT,
        ATTR_ITEMSIZE,
        ATTR_OFFSET,
        ATTR_WRITABLE,
        ATTR_FLAGS,
        ATTR_OBJ,
        ATTR_EXPORTS,
        ATTR_VAREXPORT,
        ATTR_CONSUMER,
        ATTR_PREV,
    ] {
        let value_arg = attr(source, name).unwrap_or(NONE);
        set_attr(target, name, value_arg);
    }
    publish(target);
    NONE
}

// -------------------------------------------------------------------------
// ndarray methods
// -------------------------------------------------------------------------

fn method_self(args: &[i64]) -> Option<i64> {
    args.first().copied()
}

fn ndarray_push(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(obj) = method_self(args) else {
        return type_error("push() missing self");
    };
    if bool_attr(obj, ATTR_CONSUMER) {
        return buffer_error("structure of re-exporting object is immutable");
    }
    let mut spec = InitSpec {
        items: args.get(1).copied(),
        shape: args.get(2).copied().filter(|&v| v != NONE),
        strides: args.get(3).copied().filter(|&v| v != NONE),
        offset: args.get(4).copied().and_then(value::as_integer).unwrap_or(0),
        format: args
            .get(5)
            .copied()
            .filter(|&v| v != NONE)
            .unwrap_or_else(|| value::str_alloc("B".to_string())),
        flags: args.get(6).copied().and_then(value::as_integer).unwrap_or(0),
        getbuf: GETBUF_UNSET,
    };
    for (name, value_arg) in keywords {
        match name.as_str() {
            "items" => spec.items = Some(*value_arg),
            "shape" => spec.shape = Some(*value_arg),
            "strides" => spec.strides = Some(*value_arg),
            "offset" => match value::as_integer(*value_arg) {
                Some(offset) => spec.offset = offset,
                None => return type_error("offset must be an integer"),
            },
            "format" => spec.format = *value_arg,
            "flags" => match value::as_integer(*value_arg) {
                Some(flags) => spec.flags = flags,
                None => return type_error("flags must be an integer"),
            },
            other => {
                return type_error(&format!(
                    "push() got an unexpected keyword argument '{other}'"
                ))
            }
        }
    }
    if spec.flags & ND_VAREXPORT != 0 {
        return value_error("ND_VAREXPORT flag can only be used during object creation");
    }
    if !bool_attr(obj, ATTR_VAREXPORT) && int_attr(obj, ATTR_EXPORTS) > 0 {
        return buffer_error("cannot change structure: exported buffer");
    }
    // Save the current structure on the stack.
    let state = value::tuple_new(vec![
        attr(obj, ATTR_DATA).unwrap_or(NONE),
        ints_value(&shape_of(obj)),
        ints_value(&strides_of(obj)),
        ints_value(&suboffsets_of(obj)),
        attr(obj, ATTR_FORMAT).unwrap_or(NONE),
        ops::int_result(itemsize_of(obj)),
        ops::int_result(offset_of(obj)),
        value::bool_value(bool_attr(obj, ATTR_WRITABLE)),
        ops::int_result(int_attr(obj, ATTR_FLAGS)),
        attr(obj, ATTR_OBJ).unwrap_or(NONE),
    ]);
    let stack = attr(obj, ATTR_PREV).unwrap_or(NONE);
    if value::as_list(stack).is_some() {
        container::list_append(stack, state);
    } else {
        set_attr(obj, ATTR_PREV, list_of(vec![state]));
    }
    // Rebuild the structure from the new base description.
    let Some(items) = spec.items else {
        return type_error("push() missing the item argument");
    };
    let Some(shape_value) = spec.shape else {
        return type_error("push() missing the shape argument");
    };
    let shape = match parse_int_sequence(shape_value, "shape") {
        Ok(shape) => shape,
        Err(error) => return error,
    };
    let Some(format_text) = value::as_str(spec.format).map(|text| text.to_string()) else {
        return type_error("format must be a str");
    };
    let itemsize = match calcsize(&format_text) {
        Ok(itemsize) => itemsize,
        Err(error) => return error,
    };
    let count = match format_value_count(&format_text, itemsize) {
        Ok(count) => count,
        Err(error) => return error,
    };
    let nitems = product(&shape);
    let values: Vec<i64> = if shape.is_empty() {
        vec![items]
    } else {
        match value::as_list(items).or_else(|| value::as_tuple(items)) {
            Some(values) if values.len() as i64 == nitems => values.to_vec(),
            Some(_) => return value_error("number of items does not match the shape"),
            None if nitems == 1 => vec![items],
            None => return type_error("items must be a list or a tuple"),
        }
    };
    let mut data = Vec::new();
    for item in &values {
        match pack_item(&format_text, count, *item) {
            Ok(bytes) => data.extend_from_slice(&bytes),
            Err(error) => return error,
        }
    }
    let strides = match spec.strides {
        Some(strides_value) => match parse_int_sequence(strides_value, "strides") {
            Ok(strides) => strides,
            Err(error) => return error,
        },
        None => c_strides(&shape, itemsize),
    };
    if itemsize > 0 && strides.iter().any(|stride| stride % itemsize != 0) {
        return value_error("strides must be a multiple of itemsize");
    }
    if let Err(error) = validate_strides(&shape, &strides, itemsize, spec.offset, data.len() as i64)
    {
        return error;
    }
    set_structure(
        obj,
        value::bytearray_new(data),
        &shape,
        &strides,
        &[],
        Some(format_text.as_str()),
        itemsize,
        spec.offset,
        spec.flags & ND_WRITABLE != 0,
        spec.flags,
        NONE,
    );
    NONE
}

fn ndarray_pop(args: &[i64]) -> i64 {
    let Some(obj) = method_self(args) else {
        return type_error("pop() missing self");
    };
    if bool_attr(obj, ATTR_CONSUMER) {
        return buffer_error("structure of re-exporting object is immutable");
    }
    if int_attr(obj, ATTR_EXPORTS) > 0 {
        return buffer_error("cannot change structure: exported buffer");
    }
    let stack = attr(obj, ATTR_PREV).unwrap_or(NONE);
    let Some(items) = value::as_list(stack) else {
        return buffer_error("list only has a single base");
    };
    if items.is_empty() {
        return buffer_error("list only has a single base");
    }
    let mut previous = items.to_vec();
    let state = previous.pop().unwrap_or(NONE);
    let remaining = list_of(previous);
    set_attr(obj, ATTR_PREV, remaining);
    let Some(fields) = value::as_tuple(state) else {
        return value_error("corrupt ndarray base stack");
    };
    if fields.len() != 10 {
        return value_error("corrupt ndarray base stack");
    }
    set_attr(obj, ATTR_DATA, fields[0]);
    set_attr(obj, ATTR_SHAPE, fields[1]);
    set_attr(obj, ATTR_STRIDES, fields[2]);
    set_attr(obj, ATTR_SUBOFFSETS, fields[3]);
    set_attr(obj, ATTR_FORMAT, fields[4]);
    set_attr(obj, ATTR_ITEMSIZE, fields[5]);
    set_attr(obj, ATTR_OFFSET, fields[6]);
    set_attr(obj, ATTR_WRITABLE, fields[7]);
    set_attr(obj, ATTR_FLAGS, fields[8]);
    set_attr(obj, ATTR_OBJ, fields[9]);
    publish(obj);
    NONE
}

fn ndarray_add_suboffsets(args: &[i64]) -> i64 {
    let Some(obj) = method_self(args) else {
        return type_error("add_suboffsets() missing self");
    };
    if !suboffsets_of(obj).is_empty() {
        return type_error("cannot add suboffsets to PIL-style array");
    }
    if strides_of(obj).is_empty() {
        return type_error("cannot add suboffsets to array without strides");
    }
    let suboffsets = vec![-1i64; shape_of(obj).len()];
    set_ints_attr(obj, ATTR_SUBOFFSETS, &suboffsets);
    publish(obj);
    NONE
}

fn ndarray_memoryview_from_buffer(args: &[i64]) -> i64 {
    let Some(obj) = method_self(args) else {
        return type_error("memoryview_from_buffer() missing self");
    };
    if format_attr(obj).is_none() {
        return type_error("ndarray: memoryview_from_buffer() needs a format");
    }
    if shape_of(obj).len() >= ND_MAX_NDIM as usize {
        return value_error("ndarray: ndim is too large for a memoryview");
    }
    let data = attr(obj, ATTR_DATA).unwrap_or(NONE);
    if itemsize_of(obj) == 1 && c_contiguous(
        &shape_of(obj),
        &strides_of(obj),
        &suboffsets_of(obj),
        itemsize_of(obj),
    ) {
        return value::memoryview_abi(data);
    }
    let snapshot = tobytes(obj);
    if exc::pending() {
        return NONE;
    }
    value::memoryview_abi(snapshot)
}

// -------------------------------------------------------------------------
// Module functions
// -------------------------------------------------------------------------

fn get_sizeof_void_p() -> i64 {
    ops::int_result(std::mem::size_of::<*const libc::c_void>() as i64)
}

fn slice_indices(args: &[i64]) -> i64 {
    let slice = args.first().copied().unwrap_or(NONE);
    if !is_slice(slice) {
        return type_error("slice_indices: first argument must be a slice");
    }
    let Some(length) = args.get(1).copied().and_then(value::as_integer) else {
        return type_error("slice_indices: length must be an integer");
    };
    let Ok((start, stop, step)) = slice_bounds(slice, length) else {
        return NONE;
    };
    let slicelen = slice_length(slice, length);
    value::tuple_new(vec![
        ops::int_result(start),
        ops::int_result(stop),
        ops::int_result(step),
        ops::int_result(slicelen),
    ])
}

fn order_argument(value_arg: i64) -> Result<char, i64> {
    let Some(text) = value::as_str(value_arg) else {
        return Err(type_error("order must be a str"));
    };
    let mut chars = text.chars();
    let Some(order) = chars.next() else {
        return Err(value_error("order must be 'C', 'F' or 'A'"));
    };
    if chars.next().is_some() {
        return Err(value_error("order must be 'C', 'F' or 'A'"));
    }
    match order {
        'C' | 'F' | 'A' => Ok(order),
        _ => Err(value_error("order must be 'C', 'F' or 'A'")),
    }
}

fn order_contiguous(obj: i64, order: char) -> i64 {
    if !is_ndarray(obj) {
        if value::as_bytes(obj).is_some() {
            return value::TRUE;
        }
        return type_error("object does not implement the buffer protocol");
    }
    let shape = shape_of(obj);
    let strides = strides_of(obj);
    let suboffsets = suboffsets_of(obj);
    let itemsize = itemsize_of(obj);
    let c = c_contiguous(&shape, &strides, &suboffsets, itemsize);
    let f = f_contiguous(&shape, &strides, &suboffsets, itemsize);
    match order {
        'C' => value::bool_value(c),
        'F' => value::bool_value(f),
        _ => value::bool_value(c || f),
    }
}

fn is_contiguous(args: &[i64]) -> i64 {
    let obj = args.first().copied().unwrap_or(NONE);
    let Some(order) = args.get(1).copied() else {
        return type_error("is_contiguous() missing the order argument");
    };
    let Ok(order) = order_argument(order) else {
        return NONE;
    };
    order_contiguous(obj, order)
}

fn contiguous_copy(obj: i64) -> i64 {
    if is_ndarray(obj) {
        tobytes(obj)
    } else if let Some(bytes) = value::as_bytes(obj) {
        value::bytes_new(bytes.to_vec())
    } else {
        type_error("object does not implement the buffer protocol")
    }
}

fn py_buffer_to_contiguous(args: &[i64]) -> i64 {
    let obj = args.first().copied().unwrap_or(NONE);
    let Some(order_value) = args.get(1).copied() else {
        return type_error("py_buffer_to_contiguous() missing the order argument");
    };
    let Ok(_order) = order_argument(order_value) else {
        return NONE;
    };
    if args.get(2).is_none() {
        return type_error("py_buffer_to_contiguous() missing the flags argument");
    }
    contiguous_copy(obj)
}

fn get_contiguous(args: &[i64]) -> i64 {
    let obj = args.first().copied().unwrap_or(NONE);
    if args.get(1).is_none() {
        return type_error("get_contiguous() missing the flags argument");
    }
    let Some(order_value) = args.get(2).copied() else {
        return type_error("get_contiguous() missing the order argument");
    };
    let Ok(_order) = order_argument(order_value) else {
        return NONE;
    };
    contiguous_copy(obj)
}

fn cmp_contig(args: &[i64]) -> i64 {
    let left = args.first().copied().unwrap_or(NONE);
    let right = args.get(1).copied().unwrap_or(NONE);
    if !is_exporter(left) {
        return type_error("cmp_contig: first argument does not implement the buffer protocol");
    }
    if !is_exporter(right) {
        return type_error("cmp_contig: second argument does not implement the buffer protocol");
    }
    let left_bytes = if is_ndarray(left) {
        let bytes = tobytes(left);
        if exc::pending() {
            return NONE;
        }
        value::as_bytes(bytes).map(|bytes| bytes.to_vec()).unwrap_or_default()
    } else {
        value::as_bytes(left).map(|bytes| bytes.to_vec()).unwrap_or_default()
    };
    let right_bytes = if is_ndarray(right) {
        let bytes = tobytes(right);
        if exc::pending() {
            return NONE;
        }
        value::as_bytes(bytes).map(|bytes| bytes.to_vec()).unwrap_or_default()
    } else {
        value::as_bytes(right).map(|bytes| bytes.to_vec()).unwrap_or_default()
    };
    value::bool_value(left_bytes == right_bytes)
}

fn get_pointer(args: &[i64]) -> i64 {
    let obj = args.first().copied().unwrap_or(NONE);
    if !is_ndarray(obj) {
        return type_error("get_pointer: first argument must be an ndarray");
    }
    let Some(items) = args.get(1).copied() else {
        return type_error("get_pointer: missing the index sequence");
    };
    let Some(items) = value::as_list(items).or_else(|| value::as_tuple(items)) else {
        return type_error("get_pointer: index must be a list or a tuple");
    };
    let mut indices = Vec::with_capacity(items.len());
    for &item in items {
        match value::as_integer(item) {
            Some(index) => indices.push(index),
            None if value::to_bigint(item).is_some() => {
                return overflow_error("get_pointer: index does not fit in a C ssize_t")
            }
            None => return type_error("get_pointer: indices must be integers"),
        }
    }
    let shape = shape_of(obj);
    if indices.len() != shape.len() {
        return value_error("get_pointer: invalid number of indices");
    }
    match indexed_position(obj, &indices) {
        Ok(position) => read_item(obj, position),
        Err(error) => error,
    }
}

// -------------------------------------------------------------------------
// Dispatch
// -------------------------------------------------------------------------

fn construct_ndarray(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let instance = new_ndarray();
    let instance_root = heap::root_values(std::slice::from_ref(&instance));
    let mut call_args = Vec::with_capacity(args.len() + 1);
    call_args.push(instance);
    call_args.extend_from_slice(args);
    let result = init_impl(&call_args, keywords);
    if exc::pending() {
        heap::release_roots(instance_root);
        return result;
    }
    heap::release_roots(instance_root);
    instance
}

pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "ndarray" => ndarray_class(),
        "Py_buffer" => pybuffer_class(),
        "ND_MAX_NDIM" => ops::int_result(ND_MAX_NDIM),
        "ND_VAREXPORT" => ops::int_result(ND_VAREXPORT),
        "ND_WRITABLE" => ops::int_result(ND_WRITABLE),
        "ND_FORTRAN" => ops::int_result(ND_FORTRAN),
        "ND_SCALAR" => ops::int_result(ND_SCALAR),
        "ND_PIL" => ops::int_result(ND_PIL),
        "ND_REDIRECT" => ops::int_result(ND_REDIRECT),
        "ND_GETBUF_FAIL" => ops::int_result(ND_GETBUF_FAIL),
        "ND_GETBUF_UNDEFINED" => ops::int_result(ND_GETBUF_UNDEFINED),
        "PyBUF_SIMPLE" => ops::int_result(PYBUF_SIMPLE),
        "PyBUF_WRITABLE" => ops::int_result(PYBUF_WRITABLE),
        "PyBUF_FORMAT" => ops::int_result(PYBUF_FORMAT),
        "PyBUF_ND" => ops::int_result(PYBUF_ND),
        "PyBUF_STRIDES" => ops::int_result(PYBUF_STRIDES),
        "PyBUF_INDIRECT" => ops::int_result(PYBUF_INDIRECT),
        "PyBUF_C_CONTIGUOUS" => ops::int_result(PYBUF_C_CONTIGUOUS),
        "PyBUF_F_CONTIGUOUS" => ops::int_result(PYBUF_F_CONTIGUOUS),
        "PyBUF_ANY_CONTIGUOUS" => ops::int_result(PYBUF_ANY_CONTIGUOUS),
        "PyBUF_FULL" => ops::int_result(PYBUF_FULL),
        "PyBUF_FULL_RO" => ops::int_result(PYBUF_FULL_RO),
        "PyBUF_RECORDS" => ops::int_result(PYBUF_RECORDS),
        "PyBUF_RECORDS_RO" => ops::int_result(PYBUF_RECORDS_RO),
        "PyBUF_STRIDED" => ops::int_result(PYBUF_STRIDED),
        "PyBUF_STRIDED_RO" => ops::int_result(PYBUF_STRIDED_RO),
        "PyBUF_CONTIG" => ops::int_result(PYBUF_CONTIG),
        "PyBUF_CONTIG_RO" => ops::int_result(PYBUF_CONTIG_RO),
        "PyBUF_READ" => ops::int_result(PYBUF_READ),
        "PyBUF_WRITE" => ops::int_result(PYBUF_WRITE),
        "BUFFER_MAX_SIZE" => ops::int_result(BUFFER_MAX_SIZE),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_testbuffer' has no attribute '{name}'"),
        ),
    }
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        // Constructor fallback for a module-call lowering of
        // `_testbuffer.ndarray(...)`. The normal path is the class value bound
        // by `constant`, whose `__init__` method runs this same code.
        "ndarray" => construct_ndarray(args, &[]),
        "ndarray.__init__" | "__init__" => init_impl(args, &[]),
        "ndarray.tolist" => match method_self(args) {
            Some(obj) => tolist(obj),
            None => type_error("tolist() missing self"),
        },
        "ndarray.tobytes" => match method_self(args) {
            Some(obj) => tobytes(obj),
            None => type_error("tobytes() missing self"),
        },
        "ndarray.push" => ndarray_push(args, &[]),
        "ndarray.pop" => ndarray_pop(args),
        "ndarray.add_suboffsets" => ndarray_add_suboffsets(args),
        "ndarray.memoryview_from_buffer" => ndarray_memoryview_from_buffer(args),
        "ndarray.getbuffer" => {
            let obj = args.first().copied().unwrap_or(NONE);
            let flags = args
                .get(1)
                .copied()
                .and_then(value::as_integer)
                .unwrap_or(PYBUF_FULL_RO);
            buffer_from(obj, flags)
        }
        "ndarray.releasebuffer" => release_buffer(args.get(1).copied().unwrap_or(NONE)),
        "ndarray.__buffer__" => {
            let obj = args.first().copied().unwrap_or(NONE);
            let flags = args
                .get(1)
                .copied()
                .and_then(value::as_integer)
                .unwrap_or(PYBUF_FULL_RO);
            buffer_from(obj, flags)
        }
        "ndarray.__release_buffer__" => release_buffer(args.get(1).copied().unwrap_or(NONE)),
        "ndarray.__getitem__" => match (method_self(args), args.get(1).copied()) {
            (Some(obj), Some(key)) => getitem(obj, key),
            _ => type_error("__getitem__() missing arguments"),
        },
        "ndarray.__setitem__" => match (
            method_self(args),
            args.get(1).copied(),
            args.get(2).copied(),
        ) {
            (Some(obj), Some(key), Some(new_value)) => setitem(obj, key, new_value),
            _ => type_error("__setitem__() missing arguments"),
        },
        "ndarray.__len__" => match method_self(args) {
            Some(obj) => {
                let shape = shape_of(obj);
                ops::int_result(shape.first().copied().unwrap_or(0).max(0))
            }
            None => type_error("__len__() missing self"),
        },
        "ndarray.__repr__" => match method_self(args) {
            Some(obj) => value::str_alloc(format!(
                "<ndarray shape={:?} format={:?}>",
                shape_of(obj),
                format_attr(obj)
            )),
            None => type_error("__repr__() missing self"),
        },
        "ndarray.__eq__" => {
            let left = args.first().copied().unwrap_or(NONE);
            let right = args.get(1).copied().unwrap_or(NONE);
            if !is_ndarray(right) {
                return value::NOT_IMPLEMENTED;
            }
            let left_bytes = tobytes(left);
            if exc::pending() {
                return NONE;
            }
            let right_bytes = tobytes(right);
            if exc::pending() {
                return NONE;
            }
            let same_structure = shape_of(left) == shape_of(right)
                && format_attr(left) == format_attr(right)
                && itemsize_of(left) == itemsize_of(right);
            value::bool_value(
                same_structure
                    && value::as_bytes(left_bytes) == value::as_bytes(right_bytes),
            )
        }
        "ndarray.__hash__" => {
            let obj = args.first().copied().unwrap_or(NONE);
            if bool_attr(obj, ATTR_WRITABLE) {
                return value_error("cannot hash writable ndarray object");
            }
            let bytes = tobytes(obj);
            if exc::pending() {
                return NONE;
            }
            crate::builtins::hash(bytes)
        }
        "Py_buffer.release" => release_buffer(args.first().copied().unwrap_or(NONE)),
        "Py_buffer.buf" | "Py_buffer.ptr" => not_implemented(
            "a garbage-collected buffer has no stable machine address to expose to Python",
        ),
        "Py_buffer.__repr__" => {
            let obj = args.first().copied().unwrap_or(NONE);
            value::str_alloc(format!(
                "<Py_buffer len={} itemsize={} readonly={}>",
                int_attr(obj, "len"),
                int_attr(obj, "itemsize"),
                bool_attr(obj, "readonly"),
            ))
        }
        "getbuffer" => {
            let obj = args.first().copied().unwrap_or(NONE);
            let flags = args
                .get(1)
                .copied()
                .and_then(value::as_integer)
                .unwrap_or(PYBUF_FULL_RO);
            buffer_from(obj, flags)
        }
        "releasebuffer" => release_buffer(args.first().copied().unwrap_or(NONE)),
        "get_sizeof_void_p" => get_sizeof_void_p(),
        "slice_indices" => slice_indices(args),
        "get_pointer" => get_pointer(args),
        "is_contiguous" => is_contiguous(args),
        "cmp_contig" => cmp_contig(args),
        "get_contiguous" => get_contiguous(args),
        "py_buffer_to_contiguous" => py_buffer_to_contiguous(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_testbuffer' has no attribute '{function}'"),
        ),
    }
}

pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        "ndarray" => construct_ndarray(args, keywords),
        "ndarray.__init__" | "__init__" => init_impl(args, keywords),
        "ndarray.push" => ndarray_push(args, keywords),
        _ if keywords.is_empty() => call(function, args),
        _ => type_error(&format!(
            "{function}() got an unexpected keyword argument '{}'",
            keywords[0].0
        )),
    }
}
