//! The CPython 3.14 buffer protocol.
//!
//! Exporters implement `bf_getbuffer`/`bf_releasebuffer` through their type's
//! `tp_as_buffer` suite. This host provides:
//!
//! - a read-only, contiguous, one-dimensional exporter for `bytes`
//!   ([`BYTES_BUFFER_PROCS`]);
//! - a writable/read-only, contiguous, one-dimensional exporter for
//!   `bytearray` ([`BYTEARRAY_BUFFER_PROCS`]); C writes are committed back to
//!   the object when the view is released;
//! - `PyObject_CheckBuffer`, `PyObject_GetBuffer`, `PyBuffer_Release` and
//!   `PyBuffer_FillInfo` with CPython 3.14 semantics;
//! - validation of every field an extension exporter fills in. Writable,
//!   read-only, contiguous, strided, one-dimensional and multi-dimensional
//!   views are all accepted when internally consistent; a view whose `shape`,
//!   `len`, `itemsize`, `readonly`, `strides` or `suboffsets` disagree with
//!   the request or with each other is rejected with a `BufferError` and the
//!   exporter's `bf_releasebuffer` is invoked before the (zeroed) view is
//!   returned. A malformed view therefore never escapes to the caller.
//!
//! `Py_buffer` itself is the stable-ABI structure; its size and field offsets
//! are asserted at compile time below.

#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(private_interfaces)]

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::LazyLock;

use super::types::{type_name, type_of};
use super::{
    bytes_c_data, bytes_data, dec_ref, inc_ref, kind_mut, set_error, GilGuard, ShimKind, ShimObject,
};

/// Mirrors `Py_buffer` from CPython 3.14 on x86_64 (`pybuffer.h`).
#[repr(C)]
#[allow(dead_code)]
pub(crate) struct Py_buffer {
    pub(crate) buf: *mut c_void,
    pub(crate) obj: *mut ShimObject,
    pub(crate) len: isize,
    pub(crate) itemsize: isize,
    pub(crate) readonly: c_int,
    pub(crate) ndim: c_int,
    pub(crate) format: *mut c_char,
    pub(crate) shape: *mut isize,
    pub(crate) strides: *mut isize,
    pub(crate) suboffsets: *mut isize,
    pub(crate) internal: *mut c_void,
}

// CPython 3.14 x86_64: eleven fields, 80 bytes, no padding surprises.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<Py_buffer>() == 80);
    assert!(std::mem::offset_of!(Py_buffer, buf) == 0);
    assert!(std::mem::offset_of!(Py_buffer, obj) == 8);
    assert!(std::mem::offset_of!(Py_buffer, len) == 16);
    assert!(std::mem::offset_of!(Py_buffer, itemsize) == 24);
    assert!(std::mem::offset_of!(Py_buffer, readonly) == 32);
    assert!(std::mem::offset_of!(Py_buffer, ndim) == 36);
    assert!(std::mem::offset_of!(Py_buffer, format) == 40);
    assert!(std::mem::offset_of!(Py_buffer, shape) == 48);
    assert!(std::mem::offset_of!(Py_buffer, strides) == 56);
    assert!(std::mem::offset_of!(Py_buffer, suboffsets) == 64);
    assert!(std::mem::offset_of!(Py_buffer, internal) == 72);
};

/// Mirrors `PyBufferProcs` (`getbufferproc`, `releasebufferproc`).
#[repr(C)]
pub(crate) struct PyBufferProcs {
    pub(crate) bf_getbuffer: usize,
    pub(crate) bf_releasebuffer: usize,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<PyBufferProcs>() == 16);
    assert!(std::mem::offset_of!(PyBufferProcs, bf_getbuffer) == 0);
    assert!(std::mem::offset_of!(PyBufferProcs, bf_releasebuffer) == 8);
};

/// Buffer request flags from CPython's `pybuffer.h`.
pub(crate) const PYBUF_SIMPLE: c_int = 0;
pub(crate) const PYBUF_WRITABLE: c_int = 0x0001;
pub(crate) const PYBUF_FORMAT: c_int = 0x0004;
pub(crate) const PYBUF_ND: c_int = 0x0008;
pub(crate) const PYBUF_STRIDES: c_int = 0x0010 | PYBUF_ND;
pub(crate) const PYBUF_C_CONTIGUOUS: c_int = 0x0020 | PYBUF_STRIDES;
pub(crate) const PYBUF_F_CONTIGUOUS: c_int = 0x0040 | PYBUF_STRIDES;
pub(crate) const PYBUF_ANY_CONTIGUOUS: c_int = 0x0080 | PYBUF_STRIDES;
pub(crate) const PYBUF_INDIRECT: c_int = 0x0100 | PYBUF_STRIDES;
pub(crate) const PYBUF_READ: c_int = 0x100;
pub(crate) const PYBUF_WRITE: c_int = 0x200;

/// Every bit this host understands. Unknown bits are rejected rather than
/// ignored, because ignoring them would hide an unsupported request.
const PYBUF_KNOWN: c_int = PYBUF_WRITABLE
    | PYBUF_FORMAT
    | PYBUF_ND
    | PYBUF_C_CONTIGUOUS
    | PYBUF_F_CONTIGUOUS
    | PYBUF_ANY_CONTIGUOUS
    | PYBUF_INDIRECT;

/// `PyBUF_MAX_NDIM`.
const PYBUF_MAX_NDIM: c_int = 64;

/// The `bytes` type's buffer suite.
///
/// `LazyLock` keeps the function-pointer cast out of const evaluation while
/// still giving the type a stable address for the life of the process.
pub(crate) static BYTES_BUFFER_PROCS: LazyLock<PyBufferProcs> = LazyLock::new(|| PyBufferProcs {
    bf_getbuffer: bytes_getbuffer as *const () as usize,
    bf_releasebuffer: 0,
});

/// The `bytearray` type's buffer suite: one-dimensional, contiguous and
/// writable, backed by the object's NUL-terminated byte storage.
///
/// Writes from C land in the object's `c_data` buffer (the same storage
/// `PyByteArray_AsString` exposes). `bf_releasebuffer` copies those bytes back
/// into the bytearray's logical `data`, so a view's lifetime defines when C
/// mutations become visible to Hydro code. The object is referenced by the
/// view (`PyBuffer_FillInfo`), so its storage cannot be freed while the view is
/// alive; a bytearray never resizes in place in this host.
pub(crate) static BYTEARRAY_BUFFER_PROCS: LazyLock<PyBufferProcs> =
    LazyLock::new(|| PyBufferProcs {
        bf_getbuffer: bytearray_getbuffer as *const () as usize,
        bf_releasebuffer: bytearray_releasebuffer as *const () as usize,
    });

fn zero_view(view: *mut Py_buffer) {
    // SAFETY: `view` is either NULL (checked by callers) or points at a
    // caller-owned `Py_buffer`; all-zero is a valid bit pattern (null
    // pointers, zero integers).
    unsafe { ptr::write_bytes(view as *mut u8, 0, std::mem::size_of::<Py_buffer>()) };
}

/// `bytes` exporter: one-dimensional, contiguous and read-only.
///
/// `PyBuffer_FillInfo` implements the field layout, including the CPython
/// trick of pointing `shape` at `len` and `strides` at `itemsize`.
pub(crate) extern "C" fn bytes_getbuffer(
    object: *mut ShimObject,
    view: *mut Py_buffer,
    flags: c_int,
) -> c_int {
    let data = match bytes_data(object) {
        Ok(data) => data,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    let len = match isize::try_from(data.len()) {
        Ok(len) => len,
        Err(_) => {
            set_error("OverflowError: bytes buffer is too large");
            return -1;
        }
    };
    // `c_data` is `data` plus a trailing NUL, so even an empty bytes object
    // exposes a real pointer (CPython guarantees `buf` is dereferenceable for
    // zero-length views as well).
    let pointer = match bytes_c_data(object) {
        Ok(data) => data.as_ptr(),
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    PyBuffer_FillInfo(view, object, pointer as *mut c_void, len, 1, flags)
}

/// `bytearray` exporter: one-dimensional, contiguous, writable.
///
/// The view is read-only only when the caller did not request
/// `PyBUF_WRITABLE`, matching CPython's bytearray exporter. `buf` points at
/// the object's NUL-terminated storage, so a zero-length bytearray still
/// exposes a dereferenceable pointer.
pub(crate) extern "C" fn bytearray_getbuffer(
    object: *mut ShimObject,
    view: *mut Py_buffer,
    flags: c_int,
) -> c_int {
    if object.is_null() {
        set_error("TypeError: expected a bytearray object, got NULL");
        return -1;
    }
    // SAFETY: `kind_ref` validates the handle; taking the payload mutably is
    // sound under the emulated GIL, which the buffer protocol entry points
    // hold.
    let (length, pointer) = match unsafe { kind_mut(object) } {
        ShimKind::ByteArray { c_data, .. } => {
            let length = c_data.len().saturating_sub(1);
            (length, c_data.as_mut_ptr())
        }
        _ => {
            set_error("TypeError: expected a bytearray object");
            return -1;
        }
    };
    let length = match isize::try_from(length) {
        Ok(length) => length,
        Err(_) => {
            set_error("OverflowError: bytearray buffer is too large");
            return -1;
        }
    };
    let readonly = (flags & PYBUF_WRITABLE == 0) as c_int;
    PyBuffer_FillInfo(
        view,
        object,
        pointer as *mut c_void,
        length,
        readonly,
        flags,
    )
}

/// `bytearray` release hook: commits the bytes C wrote through the view.
///
/// Only a view whose `buf` is this object's own storage is committed; an
/// exporter cannot alias a different address through this hook. The logical
/// `data` and the NUL-terminated `c_data` are refreshed together so later
/// Hydro reads, `PyByteArray_AsString` and subsequent views all observe the
/// same content.
pub(crate) extern "C" fn bytearray_releasebuffer(object: *mut ShimObject, view: *mut Py_buffer) {
    if object.is_null() || view.is_null() {
        return;
    }
    // SAFETY: the view was filled by `bytearray_getbuffer`, which checked the
    // object. The buffer protocol guarantees `len >= 0` and a live `buf`.
    let (length, buffer) = unsafe { ((*view).len, (*view).buf) };
    if length <= 0 || buffer.is_null() {
        return;
    }
    let length = length as usize;
    // SAFETY: `kind_ref` validates the handle; the object is kept alive by the
    // reference the view holds, and the emulated GIL is held by
    // `PyBuffer_Release`.
    let kind = unsafe { kind_mut(object) };
    let ShimKind::ByteArray { data, c_data } = kind else {
        return;
    };
    if buffer as *const u8 != c_data.as_ptr() || length > c_data.len().saturating_sub(1) {
        return;
    }
    if data.len() != length {
        // The host never resizes a bytearray in place, but a corrupt view must
        // not make the two buffers disagree.
        data.resize(length, 0);
    }
    data.copy_from_slice(&c_data[..length]);
    c_data[..length].copy_from_slice(data);
    if c_data.len() != length + 1 {
        c_data.clear();
        c_data.extend_from_slice(data);
        c_data.push(0);
    }
}

/// `PyObject_CheckBuffer`: 1 when the type exports a buffer.
#[no_mangle]
pub extern "C" fn PyObject_CheckBuffer(object: *mut ShimObject) -> c_int {
    let type_object = type_of(object);
    if type_object.is_null() {
        return 0;
    }
    // SAFETY: live type object.
    unsafe {
        let procs = (*type_object).tp_as_buffer;
        (!procs.is_null() && (*procs).bf_getbuffer != 0) as c_int
    }
}

/// Reads the type's buffer suite.
fn buffer_procs(object: *mut ShimObject) -> Result<*mut PyBufferProcs, String> {
    let type_object = type_of(object);
    if type_object.is_null() {
        return Err("TypeError: a bytes-like object is required, got NULL".to_string());
    }
    // SAFETY: live type object.
    let procs = unsafe { (*type_object).tp_as_buffer };
    if procs.is_null() {
        return Err(format!(
            "TypeError: a bytes-like object is required, not '{}'",
            type_name(type_object)
        ));
    }
    // SAFETY: `procs` belongs to the live type.
    let getbuffer = unsafe { (*procs).bf_getbuffer };
    if getbuffer == 0 {
        return Err(format!(
            "TypeError: a bytes-like object is required, not '{}'",
            type_name(type_object)
        ));
    }
    Ok(procs)
}

/// `PyBuffer_FillInfo`: fills a simple one-dimensional view.
#[no_mangle]
pub extern "C" fn PyBuffer_FillInfo(
    view: *mut Py_buffer,
    object: *mut ShimObject,
    buf: *mut c_void,
    len: isize,
    readonly: c_int,
    flags: c_int,
) -> c_int {
    if view.is_null() {
        set_error("BufferError: PyBuffer_FillInfo: view is NULL");
        return -1;
    }
    if flags != PYBUF_SIMPLE && (flags == PYBUF_READ || flags == PYBUF_WRITE) {
        set_error("SystemError: PyBuffer_FillInfo: invalid request flags");
        return -1;
    }
    if readonly != 0 && readonly != 1 {
        set_error("BufferError: readonly must be 0 or 1");
        return -1;
    }
    if flags & PYBUF_WRITABLE != 0 && readonly != 0 {
        set_error("BufferError: Object is not writable.");
        return -1;
    }
    if len < 0 {
        set_error("BufferError: negative buffer length");
        return -1;
    }
    if buf.is_null() && len != 0 {
        set_error("BufferError: NULL buffer with a non-zero length");
        return -1;
    }
    if !object.is_null() {
        inc_ref(object);
    }
    // SAFETY: `view` is non-NULL and caller-owned; the caller guarantees at
    // least `sizeof(Py_buffer)` writable bytes.
    unsafe {
        let view = &mut *view;
        view.obj = object;
        view.buf = buf;
        view.len = len;
        view.readonly = readonly;
        view.itemsize = 1;
        view.format = if flags & PYBUF_FORMAT != 0 {
            c"B".as_ptr() as *mut c_char
        } else {
            ptr::null_mut()
        };
        view.ndim = 1;
        view.suboffsets = ptr::null_mut();
        view.internal = ptr::null_mut();
        // CPython points `shape` at `len` and `strides` at `itemsize` for a
        // simple view. Derive those addresses only after `len`/`itemsize` hold
        // their final values; the writings below touch disjoint fields.
        let length_pointer: *mut isize = ptr::addr_of_mut!(view.len);
        let itemsize_pointer: *mut isize = ptr::addr_of_mut!(view.itemsize);
        view.shape = if flags & PYBUF_ND != 0 {
            length_pointer
        } else {
            ptr::null_mut()
        };
        view.strides = if flags & PYBUF_STRIDES != 0 {
            itemsize_pointer
        } else {
            ptr::null_mut()
        };
    }
    0
}

/// Checks a view returned by an exporter.
///
/// Every field that participates in memory access is validated:
///
/// - `obj` must be the exporting object, `len >= 0`, `itemsize >= 1`,
///   `ndim` in `0..=64`, `readonly` in `{0, 1}`;
/// - a non-empty view needs a non-NULL `buf`;
/// - `shape`, when present, must have non-negative extents whose product times
///   `itemsize` equals `len` (checked, no overflow);
/// - `strides`, when present, must have one entry per dimension, no element
///   may overflow, and a requested C/F contiguity flag must actually hold;
/// - `suboffsets`, when present, must be in range and non-decreasing
///   (indirect views);
/// - a requested `PyBUF_WRITABLE` view must report `readonly == 0`.
fn validate_view(
    object: *mut ShimObject,
    view: *const Py_buffer,
    flags: c_int,
) -> Result<(), String> {
    // SAFETY: `view` is the caller's live Py_buffer, filled by the exporter.
    let view = unsafe { &*view };
    if view.obj != object {
        return Err("BufferError: exporter did not set view->obj".to_string());
    }
    if view.len < 0 {
        return Err("BufferError: negative buffer length".to_string());
    }
    if view.itemsize < 1 {
        return Err("BufferError: buffer itemsize must be positive".to_string());
    }
    if view.readonly != 0 && view.readonly != 1 {
        return Err("BufferError: buffer readonly must be 0 or 1".to_string());
    }
    if flags & PYBUF_WRITABLE != 0 && view.readonly != 0 {
        return Err("BufferError: buffer is not writable".to_string());
    }
    if !(0..=PYBUF_MAX_NDIM).contains(&view.ndim) {
        return Err(format!(
            "BufferError: buffer ndim {} is outside 0..={PYBUF_MAX_NDIM}",
            view.ndim
        ));
    }
    if view.len > 0 && view.buf.is_null() {
        return Err("BufferError: buffer is empty but view->buf is NULL".to_string());
    }
    let ndim = view.ndim as usize;
    if ndim == 0 {
        if view.len != view.itemsize {
            return Err(
                "BufferError: a zero-dimensional buffer must have len == itemsize".to_string(),
            );
        }
    } else if view.shape.is_null() {
        // Legacy simple view: ndim == 1 with no shape matches
        // PyBuffer_FillInfo; anything else cannot be validated.
        if ndim != 1 {
            return Err("BufferError: multi-dimensional buffer has no shape array".to_string());
        }
    } else {
        // SAFETY: the exporter promises `shape` has `ndim` entries.
        let mut total: usize = 1;
        for dimension in 0..ndim {
            let extent = unsafe { *view.shape.add(dimension) };
            if extent < 0 {
                return Err("BufferError: negative buffer dimension".to_string());
            }
            total = match total.checked_mul(extent as usize) {
                Some(total) => total,
                None => return Err("BufferError: buffer shape overflows".to_string()),
            };
        }
        let expected = match total.checked_mul(view.itemsize as usize) {
            Some(expected) => expected,
            None => return Err("BufferError: buffer length overflows".to_string()),
        };
        if expected != view.len as usize {
            return Err(format!(
                "BufferError: buffer length {} does not match shape (expected {expected})",
                view.len
            ));
        }
        if !view.strides.is_null() {
            // SAFETY: the exporter promises one stride per dimension.
            let strides: Vec<isize> = (0..ndim)
                .map(|dimension| unsafe { *view.strides.add(dimension) })
                .collect();
            // CPython's contiguity rules: dimensions of extent 0 or 1 do not
            // constrain the stride; every other dimension must follow the
            // itemsize chain exactly.
            let mut c_contiguous = true;
            let mut expected = view.itemsize;
            for (dimension, &stride) in strides.iter().enumerate().rev() {
                let extent = unsafe { *view.shape.add(dimension) };
                if extent > 1 && stride != expected {
                    c_contiguous = false;
                    break;
                }
                expected = expected.saturating_mul(extent.max(1));
            }
            let mut f_contiguous = true;
            let mut expected = view.itemsize;
            for (dimension, &stride) in strides.iter().enumerate() {
                let extent = unsafe { *view.shape.add(dimension) };
                if extent > 1 && stride != expected {
                    f_contiguous = false;
                    break;
                }
                expected = expected.saturating_mul(extent.max(1));
            }
            // The request constants are composite (`PyBUF_C_CONTIGUOUS`
            // already contains `PyBUF_ND | PyBUF_STRIDES`), so compare all
            // their bits. Testing for a non-zero intersection would make a
            // plain `PyBUF_STRIDES` request demand both orders.
            if flags & PYBUF_C_CONTIGUOUS == PYBUF_C_CONTIGUOUS && !c_contiguous {
                return Err(
                    "BufferError: PyBUF_C_CONTIGUOUS requested but strides are not C-contiguous"
                        .to_string(),
                );
            }
            if flags & PYBUF_F_CONTIGUOUS == PYBUF_F_CONTIGUOUS && !f_contiguous {
                return Err(
                    "BufferError: PyBUF_F_CONTIGUOUS requested but strides are not \
                     F-contiguous"
                        .to_string(),
                );
            }
            if flags & PYBUF_ANY_CONTIGUOUS == PYBUF_ANY_CONTIGUOUS
                && !c_contiguous
                && !f_contiguous
            {
                return Err(
                    "BufferError: PyBUF_ANY_CONTIGUOUS requested but strides are not contiguous"
                        .to_string(),
                );
            }
        }
    }
    if !view.suboffsets.is_null() {
        if ndim == 0 {
            return Err("BufferError: a zero-dimensional buffer has suboffsets".to_string());
        }
        let mut previous: isize = -1;
        for dimension in 0..ndim {
            let offset = unsafe { *view.suboffsets.add(dimension) };
            if !(-1..view.ndim as isize).contains(&offset) {
                return Err(format!(
                    "BufferError: suboffset {offset} is outside the buffer dimensions"
                ));
            }
            if offset < previous {
                return Err("BufferError: suboffsets are not sorted".to_string());
            }
            previous = offset;
        }
    }
    Ok(())
}

/// Releases a view after a validation failure, without reporting an error from
/// the exporter's cleanup.
fn release_after_failure(procs: *mut PyBufferProcs, object: *mut ShimObject, view: *mut Py_buffer) {
    // SAFETY: `procs` belongs to the object's live type.
    let release = unsafe { (*procs).bf_releasebuffer };
    if release != 0 {
        let release: unsafe extern "C" fn(*mut ShimObject, *mut Py_buffer) =
            unsafe { std::mem::transmute(release as *const c_void) };
        let _gripper = GilGuard::new();
        // SAFETY: `bf_releasebuffer` has this signature.
        unsafe { release(object, view) };
    }
    // The exporter took a reference in `view.obj`; `PyBuffer_Release` would
    // normally drop it, but this view is being discarded.
    let held = unsafe { (*view).obj };
    if !held.is_null() {
        dec_ref(held);
    }
}

/// `PyObject_GetBuffer`: obtains a validated view over `object`.
#[no_mangle]
pub extern "C" fn PyObject_GetBuffer(
    object: *mut ShimObject,
    view: *mut Py_buffer,
    flags: c_int,
) -> c_int {
    let _gripper = GilGuard::new();
    if view.is_null() {
        set_error("SystemError: PyObject_GetBuffer: view is NULL");
        return -1;
    }
    if flags != PYBUF_SIMPLE && (flags == PYBUF_READ || flags == PYBUF_WRITE) {
        set_error("SystemError: PyObject_GetBuffer: invalid request flags");
        return -1;
    }
    if flags & !PYBUF_KNOWN != 0 {
        set_error(format!(
            "BufferError: unsupported buffer request flags {:#x}",
            flags & !PYBUF_KNOWN
        ));
        return -1;
    }
    let procs = match buffer_procs(object) {
        Ok(procs) => procs,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    zero_view(view);
    // SAFETY: `procs` belongs to a live type and `bf_getbuffer` was checked
    // non-NULL; `view` is caller-owned storage.
    let getbuffer: unsafe extern "C" fn(*mut ShimObject, *mut Py_buffer, c_int) -> c_int =
        unsafe { std::mem::transmute((*procs).bf_getbuffer as *const c_void) };
    let status = unsafe { getbuffer(object, view, flags) };
    if status != 0 {
        // A failed exporter must not publish a view; release any reference it
        // managed to take before failing.
        let held = unsafe { (*view).obj };
        if !held.is_null() {
            dec_ref(held);
        }
        zero_view(view);
        if !super::error_pending() {
            set_error("BufferError: exporter failed to fill the buffer");
        }
        return -1;
    }
    if let Err(error) = validate_view(object, view, flags) {
        release_after_failure(procs, object, view);
        zero_view(view);
        set_error(error);
        return -1;
    }
    0
}

/// `PyBuffer_Release`: releases a view obtained from `PyObject_GetBuffer`.
#[no_mangle]
pub extern "C" fn PyBuffer_Release(view: *mut Py_buffer) {
    if view.is_null() {
        return;
    }
    let _gripper = GilGuard::new();
    // SAFETY: `view` is the caller's live Py_buffer.
    let object = unsafe { (*view).obj };
    if !object.is_null() {
        let type_object = type_of(object);
        let procs = if type_object.is_null() {
            ptr::null_mut()
        } else {
            // SAFETY: `type_object` is live.
            unsafe { (*type_object).tp_as_buffer }
        };
        if !procs.is_null() {
            // SAFETY: `procs` belongs to the object's type.
            let release = unsafe { (*procs).bf_releasebuffer };
            if release != 0 {
                let release: unsafe extern "C" fn(*mut ShimObject, *mut Py_buffer) =
                    unsafe { std::mem::transmute(release as *const c_void) };
                // SAFETY: `bf_releasebuffer` has this signature and the view
                // is still valid.
                unsafe { release(object, view) };
            }
        }
        dec_ref(object);
    }
    zero_view(view);
}
