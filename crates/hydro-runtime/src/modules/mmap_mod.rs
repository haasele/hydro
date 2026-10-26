//! `mmap`: memory-mapped files and anonymous mappings.
//!
//! The implementation wraps the POSIX `mmap`/`munmap`/`msync`/`ftruncate`
//! primitives through `libc` and mirrors CPython 3.14's
//! `Modules/mmapmodule.c` (Unix branch) argument handling and error texts:
//!
//! * `mmap(fileno, length, flags=MAP_SHARED, prot=PROT_READ|PROT_WRITE,
//!   access=ACCESS_DEFAULT, offset=0, trackfd=True)` accepts an integer file
//!   descriptor or any object with `fileno()`. `length=0` maps the rest of
//!   the file; a length past the end of the file is rejected, exactly like
//!   CPython. `fileno=-1` maps anonymous memory (`MAP_ANONYMOUS` is added to
//!   the flags). With `trackfd=True` (the default) the mapping owns a
//!   duplicated descriptor so it survives the caller closing the original.
//! * `ACCESS_READ` maps `PROT_READ`/`MAP_SHARED`, `ACCESS_WRITE` maps
//!   `PROT_READ|PROT_WRITE`/`MAP_SHARED` and `ACCESS_COPY` maps
//!   `PROT_READ|PROT_WRITE`/`MAP_PRIVATE`; `ACCESS_DEFAULT` keeps the
//!   explicit `flags`/`prot` and derives its write mode from `prot`.
//! * `read`/`read_byte`/`readline`/`write`/`write_byte`/`seek`/`tell`/
//!   `size`/`flush`/`resize`/`move`/`find`/`rfind`/`close` and the
//!   `__getitem__`/`__setitem__` slice protocol behave like CPython,
//!   including `find`/`rfind` defaulting `start` to the current position,
//!   `flush` returning early for read-only mappings, resize using
//!   `ftruncate` plus a remap, and idempotent `close`.
//!
//! ## Safety
//!
//! Every access is bounds-checked against the mapped length before a slice is
//! formed or a byte is written, so no access can run past the mapping. The
//! mapping's memory lives outside the Hydro heap and is released by `close()`
//! (or when the process exits). Hydro has no finaliser hook for native
//! instances, so a mapping the program abandons without closing stays
//! reserved until exit; the registry is deliberately not keyed to the heap
//! generation, because a reset in one thread must never unmap memory owned by
//! another. The registry is keyed by an integer handle stored in a private
//! instance attribute; instances never hold raw pointers.
//!
//! On non-Unix hosts the constructor raises `OSError` instead of pretending
//! to map memory.

use std::collections::HashMap;
use std::os::raw::c_int;
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::core::globals;
use crate::core::methods;
use crate::exc::{
    self, ATTRIBUTE_ERROR, INDEX_ERROR, OS_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::value::{self, NONE};

// `mmap` access modes.
const ACCESS_DEFAULT: i32 = 0;
const ACCESS_READ: i32 = 1;
const ACCESS_WRITE: i32 = 2;
const ACCESS_COPY: i32 = 3;

#[cfg(unix)]
const MAP_SHARED: c_int = libc::MAP_SHARED;
#[cfg(not(unix))]
const MAP_SHARED: c_int = 1;

#[cfg(unix)]
const MAP_PRIVATE: c_int = libc::MAP_PRIVATE;
#[cfg(not(unix))]
const MAP_PRIVATE: c_int = 2;

#[cfg(target_os = "linux")]
const MAP_ANONYMOUS: c_int = libc::MAP_ANONYMOUS;
#[cfg(all(unix, not(target_os = "linux")))]
const MAP_ANONYMOUS: c_int = libc::MAP_ANON;
#[cfg(not(unix))]
const MAP_ANONYMOUS: c_int = 0x20;

#[cfg(unix)]
const PROT_READ: c_int = libc::PROT_READ;
#[cfg(not(unix))]
const PROT_READ: c_int = 1;

#[cfg(unix)]
const PROT_WRITE: c_int = libc::PROT_WRITE;
#[cfg(not(unix))]
const PROT_WRITE: c_int = 2;

/// Default `prot`: `PROT_READ | PROT_WRITE`.
const DEFAULT_PROT: c_int = PROT_READ | PROT_WRITE;

/// Private instance attribute holding the registry handle.
const HANDLE_ATTR: &str = "_mmap_handle";

// -------------------------------------------------------------------------
// Errors and argument helpers
// -------------------------------------------------------------------------

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn overflow_error(message: &str) -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, message)
}

fn closed_error() -> i64 {
    value_error("mmap closed or invalid")
}

/// A mapping created without `PROT_READ` cannot be read from without faulting
/// the process, so every read path reports an exception instead.
fn readability_error() -> i64 {
    type_error("mmap cannot be read: PROT_READ is not set")
}

fn os_error(message: &str) -> i64 {
    exc::raise_with_message(OS_ERROR, message)
}

/// Raises `OSError` in CPython's `[Errno N] message` form.
fn io_error(error: std::io::Error) -> i64 {
    let text = error.to_string();
    let detail = text.split(" (os error").next().unwrap_or(text.as_str());
    match error.raw_os_error() {
        Some(code) => os_error(&format!("[Errno {code}] {detail}")),
        None => os_error(detail),
    }
}

/// Reads a positional or keyword slot; omitted optional slots arrive as `None`.
fn argument(args: &[i64], index: usize) -> i64 {
    args.get(index).copied().unwrap_or(NONE)
}

fn int_argument(value_arg: i64, name: &str, default: i64) -> Result<i64, i64> {
    if value_arg == NONE {
        return Ok(default);
    }
    value::as_integer(value_arg).ok_or_else(|| {
        type_error(&format!(
            "argument '{name}' must be an integer, not '{}'",
            value::type_name(value_arg)
        ))
    })
}

fn required_int(value_arg: i64, name: &str) -> Result<i64, i64> {
    if value_arg == NONE {
        return Err(type_error(&format!(
            "mmap() missing required argument: '{name}'"
        )));
    }
    int_argument(value_arg, name, 0)
}

fn bytes_argument(value_arg: i64, name: &str) -> Result<Vec<u8>, i64> {
    match value::as_bytes(value_arg) {
        Some(bytes) => Ok(bytes.to_vec()),
        None => Err(type_error(&format!(
            "argument '{name}' must be a bytes-like object, not '{}'",
            value::type_name(value_arg)
        ))),
    }
}

fn flag_argument(value_arg: i64, default: bool) -> bool {
    if value_arg == NONE {
        default
    } else {
        value::truthy(value_arg)
    }
}

fn encode_int(n: i64) -> i64 {
    value::encode_int(n).unwrap_or(NONE)
}

/// `fileno` may be an integer or an object exposing `fileno()`.
fn fileno_of(value_arg: i64) -> Result<i32, i64> {
    if let Some(raw) = value::as_integer(value_arg) {
        if raw < -1 {
            return Err(os_error("[Errno 9] Bad file descriptor"));
        }
        if raw > i32::MAX as i64 {
            return Err(overflow_error("fileno is out of range"));
        }
        return Ok(raw as i32);
    }
    let method = methods::getattr(value_arg, b"fileno".as_ptr(), b"fileno".len());
    if exc::pending() {
        exc::clear();
        return Err(type_error(
            "fileno must be an integer or an object with a fileno() method",
        ));
    }
    let result = crate::builtins::call(method, 0, std::ptr::null());
    if exc::pending() {
        return Err(NONE);
    }
    match value::as_integer(result) {
        Some(raw) if (-1..=i32::MAX as i64).contains(&raw) => Ok(raw as i32),
        _ => Err(type_error("fileno() must return an integer")),
    }
}

// -------------------------------------------------------------------------
// Page and mapping primitives
// -------------------------------------------------------------------------

#[cfg(unix)]
fn page_size() -> usize {
    // SAFETY: `sysconf` reads a process-wide constant.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as usize
    } else {
        4096
    }
}

#[cfg(not(unix))]
fn page_size() -> usize {
    4096
}

#[cfg(unix)]
fn map_region(
    fd: i32,
    length: usize,
    prot: c_int,
    flags: c_int,
    offset: usize,
) -> Result<*mut u8, i64> {
    // SAFETY: the descriptor and offset come from validated arguments; the
    // kernel returns `MAP_FAILED` instead of exposing an invalid region.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            prot,
            flags,
            fd,
            offset as libc::off_t,
        )
    };
    if ptr == libc::MAP_FAILED {
        Err(io_error(std::io::Error::last_os_error()))
    } else {
        Ok(ptr as *mut u8)
    }
}

#[cfg(not(unix))]
fn map_region(
    _fd: i32,
    _length: usize,
    _prot: c_int,
    _flags: c_int,
    _offset: usize,
) -> Result<*mut u8, i64> {
    Err(os_error("mmap is not supported on this platform"))
}

fn unmap_region(ptr: *mut u8, length: usize) {
    #[cfg(unix)]
    {
        if !ptr.is_null() && length > 0 {
            // SAFETY: `ptr`/`length` describe a live mapping owned by the
            // registry entry being dropped.
            unsafe { libc::munmap(ptr as *mut libc::c_void, length) };
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (ptr, length);
    }
}

#[cfg(unix)]
fn sync_region(ptr: *mut u8, length: usize) -> Result<(), i64> {
    if length == 0 {
        return Ok(());
    }
    // SAFETY: the range was bounds-checked against the live mapping.
    if unsafe { libc::msync(ptr as *mut libc::c_void, length, libc::MS_SYNC) } != 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_region(_ptr: *mut u8, _length: usize) -> Result<(), i64> {
    Err(os_error("mmap is not supported on this platform"))
}

#[cfg(unix)]
fn truncate_fd(fd: i32, size: usize) -> Result<(), i64> {
    // SAFETY: `fd` is an owned descriptor and `size` was bounds-checked.
    if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn truncate_fd(_fd: i32, _size: usize) -> Result<(), i64> {
    Err(os_error("mmap is not supported on this platform"))
}

/// File size and regular-file flag of `fd`.
#[cfg(unix)]
fn file_status(fd: i32) -> Result<(usize, bool), i64> {
    // SAFETY: a zeroed `stat` is a valid output buffer for `fstat`.
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` comes from validated user input; `fstat` only writes to
    // `status`.
    if unsafe { libc::fstat(fd, &mut status) } != 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    let size = if status.st_size > 0 {
        status.st_size as usize
    } else {
        0
    };
    let regular = status.st_mode & libc::S_IFMT == libc::S_IFREG;
    Ok((size, regular))
}

#[cfg(not(unix))]
fn file_status(_fd: i32) -> Result<(usize, bool), i64> {
    Err(os_error("mmap is not supported on this platform"))
}

#[cfg(unix)]
fn duplicate_fd(fd: i32) -> Result<i32, i64> {
    // SAFETY: `fd` is a readable descriptor validated by the caller.
    let copy = unsafe { libc::dup(fd) };
    if copy < 0 {
        Err(io_error(std::io::Error::last_os_error()))
    } else {
        Ok(copy)
    }
}

#[cfg(not(unix))]
fn duplicate_fd(_fd: i32) -> Result<i32, i64> {
    Err(os_error("mmap is not supported on this platform"))
}

fn close_fd(fd: i32) {
    #[cfg(unix)]
    {
        if fd >= 0 {
            // SAFETY: the descriptor is owned by the mapping being dropped.
            unsafe { libc::close(fd) };
        }
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
    }
}

// -------------------------------------------------------------------------
// Mapping state and registry
// -------------------------------------------------------------------------

struct Mapping {
    ptr: *mut u8,
    len: usize,
    access: i32,
    /// Owned (duplicated) descriptor, or -1 when the mapping does not own one.
    fd: i32,
    offset: usize,
    pos: usize,
    flags: c_int,
    prot: c_int,
    trackfd: bool,
}

// SAFETY: the raw pointer is never handed outside the registry: every access
// happens while the registry mutex is held.
unsafe impl Send for Mapping {}

impl Mapping {
    fn valid(&self) -> bool {
        !self.ptr.is_null()
    }

    fn writable(&self) -> bool {
        self.access != ACCESS_READ && self.prot & PROT_WRITE != 0
    }

    fn readable(&self) -> bool {
        self.prot & PROT_READ != 0
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `valid()` was checked by the caller and the region has
        // exactly `len` readable bytes.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: the caller checked `valid()` and write access; `len` bounds
        // the region.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unmap_region(self.ptr, self.len);
        self.ptr = std::ptr::null_mut();
        if self.fd >= 0 {
            close_fd(self.fd);
            self.fd = -1;
        }
    }
}

struct Registry {
    next_handle: u64,
    mappings: HashMap<u64, Mapping>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
    Mutex::new(Registry {
        next_handle: 1,
        mappings: HashMap::new(),
    })
});

/// Locks the mapping registry.
fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn register(mapping: Mapping) -> u64 {
    let mut registry = registry();
    let handle = registry.next_handle;
    registry.next_handle = handle.wrapping_add(1).max(1);
    registry.mappings.insert(handle, mapping);
    handle
}

fn handle_of(instance: i64) -> Result<u64, i64> {
    match value::instance_attr(instance, HANDLE_ATTR).and_then(value::as_integer) {
        Some(handle) if handle > 0 => Ok(handle as u64),
        _ => Err(closed_error()),
    }
}

fn attach_handle(instance: i64, handle: u64) {
    value::instance_setattr(instance, HANDLE_ATTR.to_string(), encode_int(handle as i64));
}

/// Runs `body` on the live mapping of `instance`. A closed mapping raises
/// `ValueError("mmap closed or invalid")`.
fn with_mapping<R>(
    instance: i64,
    body: impl FnOnce(&mut Mapping) -> Result<R, i64>,
) -> Result<R, i64> {
    let handle = handle_of(instance)?;
    let mut registry = registry();
    match registry.mappings.get_mut(&handle) {
        Some(mapping) if mapping.valid() => body(mapping),
        _ => Err(closed_error()),
    }
}

/// Builds an instance and installs its mapping.
fn instance_with(mapping: Mapping) -> i64 {
    let handle = register(mapping);
    let class = mmap_class();
    let instance = value::instance_new(class);
    attach_handle(instance, handle);
    value::instance_setattr(instance, "closed".to_string(), value::FALSE);
    instance
}

// -------------------------------------------------------------------------
// Class
// -------------------------------------------------------------------------

fn native(function: &'static str) -> i64 {
    value::native_function_new("mmap", function, usize::MAX)
}

/// The cached `mmap.mmap` class.
fn mmap_class() -> i64 {
    if let Some(class) = globals::registry_get("mmap.mmap") {
        return class;
    }
    let methods = vec![
        ("__init__".to_string(), native("_mmap_init")),
        ("close".to_string(), native("_mmap_close")),
        ("read".to_string(), native("_mmap_read")),
        ("read_byte".to_string(), native("_mmap_read_byte")),
        ("readline".to_string(), native("_mmap_readline")),
        ("write".to_string(), native("_mmap_write")),
        ("write_byte".to_string(), native("_mmap_write_byte")),
        ("seek".to_string(), native("_mmap_seek")),
        ("seekable".to_string(), native("_mmap_seekable")),
        ("tell".to_string(), native("_mmap_tell")),
        ("size".to_string(), native("_mmap_size")),
        ("flush".to_string(), native("_mmap_flush")),
        ("resize".to_string(), native("_mmap_resize")),
        ("move".to_string(), native("_mmap_move")),
        ("find".to_string(), native("_mmap_find")),
        ("rfind".to_string(), native("_mmap_rfind")),
        ("__getitem__".to_string(), native("_mmap_getitem")),
        ("__setitem__".to_string(), native("_mmap_setitem")),
        ("__len__".to_string(), native("_mmap_len")),
        ("__enter__".to_string(), native("_mmap_enter")),
        ("__exit__".to_string(), native("_mmap_exit")),
        ("__repr__".to_string(), native("_mmap_repr")),
    ];
    let class = value::class_new("mmap".to_string(), NONE, methods);
    globals::registry_set("mmap.mmap", class);
    class
}

// -------------------------------------------------------------------------
// Construction
// -------------------------------------------------------------------------

/// Parses the constructor arguments and creates the mapping.
fn mapping_from_args(args: &[i64]) -> Result<Mapping, i64> {
    if args.len() < 2 {
        return Err(type_error("mmap() missing required argument: 'length'"));
    }
    let fd = fileno_of(argument(args, 0))?;
    let length = required_int(argument(args, 1), "length")?;
    if length < 0 {
        return Err(overflow_error("memory mapped length must be positive"));
    }
    let flags = int_argument(argument(args, 2), "flags", MAP_SHARED as i64)?;
    let prot = int_argument(argument(args, 3), "prot", DEFAULT_PROT as i64)?;
    let access = int_argument(argument(args, 4), "access", ACCESS_DEFAULT as i64)?;
    let offset = int_argument(argument(args, 5), "offset", 0)?;
    let trackfd = flag_argument(argument(args, 6), true);
    if offset < 0 {
        return Err(overflow_error("memory mapped offset must be positive"));
    }
    if !(ACCESS_DEFAULT as i64..=ACCESS_COPY as i64).contains(&access) {
        return Err(value_error("mmap invalid access parameter."));
    }
    if access != ACCESS_DEFAULT as i64
        && (flags != MAP_SHARED as i64 || prot != DEFAULT_PROT as i64)
    {
        return Err(value_error(
            "mmap can't specify both access and flags, prot.",
        ));
    }
    let access = match access as i32 {
        ACCESS_READ => ACCESS_READ,
        ACCESS_WRITE => ACCESS_WRITE,
        ACCESS_COPY => ACCESS_COPY,
        _ => {
            // `ACCESS_DEFAULT`: derive the mode from `prot`.
            if prot & (PROT_READ as i64) != 0 && prot & (PROT_WRITE as i64) != 0 {
                ACCESS_DEFAULT
            } else if prot & (PROT_WRITE as i64) != 0 {
                ACCESS_WRITE
            } else {
                ACCESS_READ
            }
        }
    };
    let (flags, prot) = match access {
        ACCESS_READ => (MAP_SHARED, PROT_READ),
        ACCESS_WRITE => (MAP_SHARED, DEFAULT_PROT),
        ACCESS_COPY => (MAP_PRIVATE, DEFAULT_PROT),
        _ => (flags as c_int, prot as c_int),
    };
    let offset = usize::try_from(offset)
        .map_err(|_| overflow_error("memory mapped offset must be positive"))?;
    let requested = usize::try_from(length)
        .map_err(|_| overflow_error("memory mapped length must be positive"))?;

    let (map_len, map_fd) = if fd < 0 {
        // Anonymous mapping; CPython adds `MAP_ANONYMOUS` to the flags.
        let flags = flags | MAP_ANONYMOUS;
        let ptr = map_region(-1, requested, prot, flags, offset)?;
        let mapping = Mapping {
            ptr,
            len: requested,
            access,
            fd: -1,
            offset,
            pos: 0,
            flags,
            prot,
            trackfd,
        };
        return Ok(mapping);
    } else {
        let (size, regular) = file_status(fd)?;
        let map_len = if requested == 0 {
            if regular {
                if size == 0 {
                    return Err(value_error("cannot mmap an empty file"));
                }
                if offset >= size {
                    return Err(value_error("mmap offset is greater than file size"));
                }
                size - offset
            } else {
                return Err(value_error("cannot mmap an empty file"));
            }
        } else {
            if regular && (offset > size || size - offset < requested) {
                return Err(value_error("mmap length is greater than file size"));
            }
            requested
        };
        let owned = if trackfd { duplicate_fd(fd)? } else { -1 };
        (map_len, if owned >= 0 { owned } else { fd })
    };

    let ptr = match map_region(map_fd, map_len, prot, flags, offset) {
        Ok(ptr) => ptr,
        Err(error) => {
            if map_fd != fd {
                close_fd(map_fd);
            }
            return Err(error);
        }
    };
    Ok(Mapping {
        ptr,
        len: map_len,
        access,
        fd: if map_fd == fd { -1 } else { map_fd },
        offset,
        pos: 0,
        flags,
        prot,
        trackfd,
    })
}

// -------------------------------------------------------------------------
// Method bodies
// -------------------------------------------------------------------------

fn close_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let Some(handle) = value::instance_attr(instance, HANDLE_ATTR).and_then(value::as_integer)
    else {
        return NONE;
    };
    if handle <= 0 {
        return NONE;
    }
    {
        let mut registry = registry();
        registry.mappings.remove(&(handle as u64));
    }
    value::instance_setattr(instance, "closed".to_string(), value::TRUE);
    NONE
}

fn read_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let count = match int_argument(argument(args, 1), "n", -1) {
        Ok(count) => count,
        Err(error) => return error,
    };
    let outcome = with_mapping(instance, |mapping| {
        if !mapping.readable() {
            return Err(readability_error());
        }
        let remaining = mapping.len.saturating_sub(mapping.pos);
        let take = if count < 0 || count as u128 > remaining as u128 {
            remaining
        } else {
            count as usize
        };
        let start = mapping.pos;
        let data = mapping.bytes()[start..start + take].to_vec();
        mapping.pos = start + take;
        Ok(data)
    });
    match outcome {
        Ok(data) => value::bytes_new(data),
        Err(error) => error,
    }
}

fn read_byte_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let outcome = with_mapping(instance, |mapping| {
        if !mapping.readable() {
            return Err(readability_error());
        }
        if mapping.pos >= mapping.len {
            return Err(value_error("read byte out of range"));
        }
        let byte = mapping.bytes()[mapping.pos];
        mapping.pos += 1;
        Ok(i64::from(byte))
    });
    match outcome {
        Ok(byte) => encode_int(byte),
        Err(error) => error,
    }
}

fn readline_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let outcome = with_mapping(instance, |mapping| {
        if !mapping.readable() {
            return Err(readability_error());
        }
        let start = mapping.pos;
        let rest = &mapping.bytes()[start..];
        let end = match rest.iter().position(|&byte| byte == b'\n') {
            Some(offset) => start + offset + 1,
            None => mapping.len,
        };
        let data = mapping.bytes()[start..end].to_vec();
        mapping.pos = end;
        Ok(data)
    });
    match outcome {
        Ok(data) => value::bytes_new(data),
        Err(error) => error,
    }
}

fn write_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let data = match bytes_argument(argument(args, 1), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let outcome = with_mapping(instance, |mapping| {
        if !mapping.writable() {
            return Err(type_error("mmap can't modify a readonly memory map."));
        }
        if mapping.pos > mapping.len || mapping.len - mapping.pos < data.len() {
            return Err(value_error("data out of range"));
        }
        let start = mapping.pos;
        let end = start + data.len();
        mapping.bytes_mut()[start..end].copy_from_slice(&data);
        mapping.pos = end;
        Ok(end - start)
    });
    match outcome {
        Ok(written) => encode_int(written as i64),
        Err(error) => error,
    }
}

fn write_byte_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let byte = match required_int(argument(args, 1), "byte") {
        Ok(byte) => byte,
        Err(error) => return error,
    };
    if !(0..=255).contains(&byte) {
        return overflow_error("byte must be in range(0, 256)");
    }
    let outcome = with_mapping(instance, |mapping| {
        if !mapping.writable() {
            return Err(type_error("mmap can't modify a readonly memory map."));
        }
        if mapping.pos >= mapping.len {
            return Err(value_error("write byte out of range"));
        }
        let position = mapping.pos;
        mapping.bytes_mut()[position] = byte as u8;
        mapping.pos += 1;
        Ok(())
    });
    match outcome {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

fn seek_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let position = match required_int(argument(args, 1), "pos") {
        Ok(position) => position,
        Err(error) => return error,
    };
    let whence = match int_argument(argument(args, 2), "whence", 0) {
        Ok(whence) => whence,
        Err(error) => return error,
    };
    let outcome = with_mapping(instance, |mapping| {
        let base = match whence {
            0 => 0_i128,
            1 => mapping.pos as i128,
            2 => mapping.len as i128,
            _ => return Err(value_error("unknown seek type")),
        };
        let target = base + position as i128;
        if target < 0 || target > mapping.len as i128 {
            return Err(value_error("seek out of range"));
        }
        mapping.pos = target as usize;
        Ok(target as i64)
    });
    match outcome {
        Ok(target) => encode_int(target),
        Err(error) => error,
    }
}

fn seekable_method(_args: &[i64]) -> i64 {
    value::TRUE
}

fn tell_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    match with_mapping(instance, |mapping| Ok(mapping.pos as i64)) {
        Ok(position) => encode_int(position),
        Err(error) => error,
    }
}

fn size_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let outcome = with_mapping(instance, |mapping| {
        if mapping.fd >= 0 {
            let (size, _) = file_status(mapping.fd)?;
            Ok(size as i64)
        } else {
            Ok(mapping.len as i64)
        }
    });
    match outcome {
        Ok(size) => encode_int(size),
        Err(error) => error,
    }
}

fn flush_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let offset = match int_argument(argument(args, 1), "offset", 0) {
        Ok(offset) => offset,
        Err(error) => return error,
    };
    let outcome = with_mapping(instance, |mapping| {
        let size = match int_argument(argument(args, 2), "size", mapping.len as i64) {
            Ok(size) => size,
            Err(error) => return Err(error),
        };
        if size < 0 || offset < 0 || (mapping.len as i128) - (offset as i128) < size as i128 {
            return Err(value_error("flush values out of range"));
        }
        if mapping.access == ACCESS_READ || mapping.access == ACCESS_COPY {
            return Ok(());
        }
        let offset = offset as usize;
        // SAFETY: the range was bounds-checked against the mapping.
        let start = unsafe { mapping.ptr.add(offset) };
        sync_region(start, size as usize)
    });
    match outcome {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

fn resize_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let new_size = match required_int(argument(args, 1), "new_size") {
        Ok(new_size) => new_size,
        Err(error) => return error,
    };
    let outcome = with_mapping(instance, |mapping| {
        if !mapping.trackfd {
            return Err(value_error("mmap can't resize with trackfd=False."));
        }
        if mapping.access != ACCESS_WRITE && mapping.access != ACCESS_DEFAULT {
            return Err(type_error(
                "mmap can't resize a readonly or copy-on-write memory map.",
            ));
        }
        let new_size = match usize::try_from(new_size) {
            Ok(new_size) => new_size,
            Err(_) => return Err(value_error("new size out of range")),
        };
        if new_size == 0 {
            return Err(value_error("cannot mmap an empty file"));
        }
        if mapping.fd < 0 && !mapping.readable() {
            return Err(readability_error());
        }
        if mapping.fd >= 0 {
            let required = mapping
                .offset
                .checked_add(new_size)
                .ok_or_else(|| value_error("new size out of range"))?;
            truncate_fd(mapping.fd, required)?;
        }
        // Map the new region before releasing the old one: a failure leaves
        // the mapping intact instead of silently dropping it.
        match map_region(
            if mapping.fd >= 0 { mapping.fd } else { -1 },
            new_size,
            mapping.prot,
            mapping.flags,
            mapping.offset,
        ) {
            Ok(ptr) => {
                if mapping.fd < 0 {
                    // Anonymous growth: preserve the old contents.
                    let keep = mapping.len.min(new_size);
                    // SAFETY: both regions are live and `keep` is within both.
                    unsafe { std::ptr::copy(mapping.ptr, ptr, keep) };
                }
                unmap_region(mapping.ptr, mapping.len);
                mapping.ptr = ptr;
                mapping.len = new_size;
                if mapping.pos > mapping.len {
                    mapping.pos = mapping.len;
                }
                Ok(())
            }
            Err(error) => {
                unmap_region(mapping.ptr, mapping.len);
                mapping.ptr = std::ptr::null_mut();
                mapping.len = 0;
                Err(error)
            }
        }
    });
    match outcome {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

fn move_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let dest = match required_int(argument(args, 1), "dest") {
        Ok(dest) => dest,
        Err(error) => return error,
    };
    let src = match required_int(argument(args, 2), "src") {
        Ok(src) => src,
        Err(error) => return error,
    };
    let count = match required_int(argument(args, 3), "count") {
        Ok(count) => count,
        Err(error) => return error,
    };
    let outcome = with_mapping(instance, |mapping| {
        if !mapping.writable() {
            return Err(type_error("mmap can't modify a readonly memory map."));
        }
        if !mapping.readable() {
            return Err(readability_error());
        }
        let len = mapping.len as i64;
        if dest < 0 || src < 0 || count < 0 || len - dest < count || len - src < count {
            return Err(value_error("source, destination, or count out of range"));
        }
        let count = count as usize;
        // SAFETY: both ranges were bounds-checked; `copy` handles overlap.
        unsafe {
            std::ptr::copy(
                mapping.ptr.add(src as usize),
                mapping.ptr.add(dest as usize),
                count,
            )
        };
        Ok(())
    });
    match outcome {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

fn search(haystack: &[u8], needle: &[u8], base: usize, reverse: bool) -> i64 {
    if needle.is_empty() {
        return if reverse {
            (base + haystack.len()) as i64
        } else {
            base as i64
        };
    }
    if needle.len() > haystack.len() {
        return -1;
    }
    let last = haystack.len() - needle.len();
    let found = if reverse {
        (0..=last)
            .rev()
            .find(|&index| &haystack[index..index + needle.len()] == needle)
    } else {
        (0..=last).find(|&index| &haystack[index..index + needle.len()] == needle)
    };
    match found {
        Some(index) => (base + index) as i64,
        None => -1,
    }
}

fn find_method(args: &[i64], reverse: bool) -> i64 {
    let instance = argument(args, 0);
    let needle = match bytes_argument(argument(args, 1), "sub") {
        Ok(needle) => needle,
        Err(error) => return error,
    };
    let start_arg = argument(args, 2);
    let end_arg = argument(args, 3);
    let outcome = with_mapping(instance, |mapping| {
        if !mapping.readable() {
            return Err(readability_error());
        }
        let len = mapping.len as i64;
        // CPython's `find`/`rfind` default `start` to the current position.
        let mut start = if start_arg == NONE {
            mapping.pos as i64
        } else {
            int_argument(start_arg, "start", 0)?
        };
        let mut end = if end_arg == NONE {
            len
        } else {
            int_argument(end_arg, "end", 0)?
        };
        if start < 0 {
            start = start.saturating_add(len);
        }
        start = start.clamp(0, len);
        if end < 0 {
            end = end.saturating_add(len);
        }
        end = end.clamp(0, len);
        if end < start {
            return Ok(-1);
        }
        let haystack = &mapping.bytes()[start as usize..end as usize];
        Ok(search(haystack, &needle, start as usize, reverse))
    });
    match outcome {
        Ok(index) => encode_int(index),
        Err(error) => error,
    }
}

fn slice_attr(index: i64, name: &str) -> Result<Option<i64>, i64> {
    let raw = value::instance_attr(index, name).unwrap_or(NONE);
    if raw == NONE {
        return Ok(None);
    }
    value::as_integer(raw).map(Some).ok_or_else(|| {
        type_error("slice indices must be integers or None or have an __index__ method")
    })
}

/// Python's `PySlice_AdjustIndices`: `(start, stop, step)` clamped to `len`.
fn normalize_slice(index: i64, len: usize) -> Result<(i64, i64, i64), i64> {
    let start = slice_attr(index, "start")?;
    let stop = slice_attr(index, "stop")?;
    let step = slice_attr(index, "step")?.unwrap_or(1);
    if step == 0 {
        return Err(value_error("slice step cannot be zero"));
    }
    let len_i = len as i64;
    let (lower, upper) = if step > 0 {
        (0, len_i)
    } else {
        (-1, len_i - 1)
    };
    let clamp = |value: i64| {
        if value < 0 {
            value.saturating_add(len_i).max(lower)
        } else {
            value.min(upper)
        }
    };
    let start = match start {
        Some(value) => clamp(value),
        None if step > 0 => 0,
        None => len_i - 1,
    };
    let stop = match stop {
        Some(value) => clamp(value),
        None if step > 0 => len_i,
        None => -1,
    };
    Ok((start, stop, step))
}

fn slice_length(start: i64, stop: i64, step: i64) -> usize {
    let start = i128::from(start);
    let stop = i128::from(stop);
    let step = i128::from(step);
    let count = if step > 0 {
        if stop > start {
            (stop - start + step - 1) / step
        } else {
            0
        }
    } else if start > stop {
        (start - stop - step - 1) / -step
    } else {
        0
    };
    count.max(0) as usize
}

fn getitem_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let index = argument(args, 1);
    if value::instance_class_name(index) == Some("slice") {
        let outcome = with_mapping(instance, |mapping| {
            if !mapping.readable() {
                return Err(readability_error());
            }
            let (start, stop, step) = normalize_slice(index, mapping.len)?;
            let bytes = mapping.bytes();
            let mut out = Vec::with_capacity(slice_length(start, stop, step));
            let stop = i128::from(stop);
            let step = i128::from(step);
            let mut cursor = i128::from(start);
            if step > 0 {
                while cursor < stop {
                    out.push(bytes[cursor as usize]);
                    cursor += step;
                }
            } else {
                while cursor > stop {
                    out.push(bytes[cursor as usize]);
                    cursor += step;
                }
            }
            Ok(out)
        });
        return match outcome {
            Ok(out) => value::bytes_new(out),
            Err(error) => error,
        };
    }
    let Some(raw) = value::as_integer(index) else {
        return type_error("mmap indices must be integers");
    };
    let outcome = with_mapping(instance, |mapping| {
        if !mapping.readable() {
            return Err(readability_error());
        }
        let len = mapping.len as i128;
        let raw = i128::from(raw);
        let position = if raw < 0 { raw + len } else { raw };
        if position < 0 || position >= len {
            return Err(exc::raise_with_message(
                INDEX_ERROR,
                "mmap index out of range",
            ));
        }
        Ok(i64::from(mapping.bytes()[position as usize]))
    });
    match outcome {
        Ok(byte) => encode_int(byte),
        Err(error) => error,
    }
}

fn setitem_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let index = argument(args, 1);
    let new_value = argument(args, 2);
    let outcome = with_mapping(instance, |mapping| {
        if !mapping.writable() {
            return Err(type_error("mmap can't modify a readonly memory map."));
        }
        if value::instance_class_name(index) == Some("slice") {
            let data = match value::as_bytes(new_value) {
                Some(data) => data.to_vec(),
                None => {
                    return Err(type_error(
                        "mmap slice assignment expects a bytes-like object",
                    ))
                }
            };
            let (start, stop, step) = normalize_slice(index, mapping.len)?;
            let wanted = slice_length(start, stop, step);
            if data.len() != wanted {
                return Err(exc::raise_with_message(
                    INDEX_ERROR,
                    "mmap slice assignment is wrong size",
                ));
            }
            let stop = i128::from(stop);
            let step = i128::from(step);
            let mut cursor = i128::from(start);
            let bytes = mapping.bytes_mut();
            let mut source = 0;
            if step > 0 {
                while cursor < stop {
                    bytes[cursor as usize] = data[source];
                    cursor += step;
                    source += 1;
                }
            } else {
                while cursor > stop {
                    bytes[cursor as usize] = data[source];
                    cursor += step;
                    source += 1;
                }
            }
            return Ok(());
        }
        let Some(raw) = value::as_integer(index) else {
            return Err(type_error("mmap indices must be integers"));
        };
        let Some(byte) = value::as_integer(new_value) else {
            return Err(type_error("mmap item value must be an int"));
        };
        if !(0..=255).contains(&byte) {
            return Err(value_error("mmap item value must be in range(0, 256)"));
        }
        let len = mapping.len as i128;
        let raw = i128::from(raw);
        let position = if raw < 0 { raw + len } else { raw };
        if position < 0 || position >= len {
            return Err(exc::raise_with_message(
                INDEX_ERROR,
                "mmap index out of range",
            ));
        }
        mapping.bytes_mut()[position as usize] = byte as u8;
        Ok(())
    });
    match outcome {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

fn len_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    match with_mapping(instance, |mapping| Ok(mapping.len as i64)) {
        Ok(len) => encode_int(len),
        Err(error) => error,
    }
}

fn enter_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    match with_mapping(instance, |_| Ok(())) {
        Ok(()) => instance,
        Err(error) => error,
    }
}

fn exit_method(args: &[i64]) -> i64 {
    close_method(args)
}

fn access_name(access: i32) -> &'static str {
    match access {
        ACCESS_READ => "ACCESS_READ",
        ACCESS_WRITE => "ACCESS_WRITE",
        ACCESS_COPY => "ACCESS_COPY",
        _ => "ACCESS_DEFAULT",
    }
}

fn repr_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let text = match value::instance_attr(instance, HANDLE_ATTR).and_then(value::as_integer) {
        Some(handle) if handle > 0 => {
            let registry = registry();
            match registry.mappings.get(&(handle as u64)) {
                Some(mapping) if mapping.valid() => format!(
                    "<mmap.mmap closed=False, access={}, length={}, pos={}, offset={}>",
                    access_name(mapping.access),
                    mapping.len,
                    mapping.pos,
                    mapping.offset
                ),
                _ => "<mmap.mmap closed=True>".to_string(),
            }
        }
        _ => "<mmap.mmap closed=True>".to_string(),
    };
    value::str_alloc(text)
}

/// `mmap(...)`: constructs and returns the mapping object.
fn new_mapping(args: &[i64]) -> i64 {
    match mapping_from_args(args) {
        Ok(mapping) => instance_with(mapping),
        Err(error) => error,
    }
}

/// `mmap.mmap.__init__(self, ...)`: the class-call path.
fn init_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let rest = if args.len() > 1 { &args[1..] } else { &[][..] };
    match mapping_from_args(rest) {
        Ok(mapping) => {
            let handle = register(mapping);
            attach_handle(instance, handle);
            value::instance_setattr(instance, "closed".to_string(), value::FALSE);
            NONE
        }
        Err(error) => error,
    }
}

// -------------------------------------------------------------------------
// Module surface
// -------------------------------------------------------------------------

/// `mmap.mmap` as a class value (so `isinstance(m, mmap.mmap)` works).
pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    match (module, name) {
        ("mmap", "mmap") => Some(mmap_class()),
        _ => None,
    }
}

/// Materialises a module constant.
pub(crate) fn constant(name: &str) -> i64 {
    let int = |value: i64| value::encode_int(value).unwrap_or(NONE);
    match name {
        "ACCESS_DEFAULT" => int(i64::from(ACCESS_DEFAULT)),
        "ACCESS_READ" => int(i64::from(ACCESS_READ)),
        "ACCESS_WRITE" => int(i64::from(ACCESS_WRITE)),
        "ACCESS_COPY" => int(i64::from(ACCESS_COPY)),
        "ALLOCATIONGRANULARITY" | "PAGESIZE" => int(page_size() as i64),
        "MAP_SHARED" => int(i64::from(MAP_SHARED)),
        "MAP_PRIVATE" => int(i64::from(MAP_PRIVATE)),
        "MAP_ANONYMOUS" => int(i64::from(MAP_ANONYMOUS)),
        "PROT_READ" => int(i64::from(PROT_READ)),
        "PROT_WRITE" => int(i64::from(PROT_WRITE)),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'mmap' has no attribute '{name}'"),
        ),
    }
}

/// Dispatches `mmap.*` calls, the constructor and the native method bodies.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "mmap" => new_mapping(args),
        "_mmap_init" => init_method(args),
        "_mmap_close" => close_method(args),
        "_mmap_read" => read_method(args),
        "_mmap_read_byte" => read_byte_method(args),
        "_mmap_readline" => readline_method(args),
        "_mmap_write" => write_method(args),
        "_mmap_write_byte" => write_byte_method(args),
        "_mmap_seek" => seek_method(args),
        "_mmap_seekable" => seekable_method(args),
        "_mmap_tell" => tell_method(args),
        "_mmap_size" => size_method(args),
        "_mmap_flush" => flush_method(args),
        "_mmap_resize" => resize_method(args),
        "_mmap_move" => move_method(args),
        "_mmap_find" => find_method(args, false),
        "_mmap_rfind" => find_method(args, true),
        "_mmap_getitem" => getitem_method(args),
        "_mmap_setitem" => setitem_method(args),
        "_mmap_len" => len_method(args),
        "_mmap_enter" => enter_method(args),
        "_mmap_exit" => exit_method(args),
        "_mmap_repr" => repr_method(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'mmap' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable integer")
    }

    fn bytes(data: &[u8]) -> i64 {
        value::bytes_new(data.to_vec())
    }

    fn data(value_arg: i64) -> Vec<u8> {
        value::as_bytes(value_arg).expect("bytes result").to_vec()
    }

    fn number(value_arg: i64) -> i64 {
        value::as_integer(value_arg).expect("integer result")
    }

    fn call(function: &str, args: &[i64]) -> i64 {
        super::call(function, args)
    }

    fn slice(start: i64, stop: i64, step: i64) -> i64 {
        crate::builtins::slice_new(&[start, stop, step])
    }

    #[test]
    fn anonymous_mapping_round_trips() {
        exc::init();
        let flags = int(i64::from(MAP_PRIVATE | MAP_ANONYMOUS));
        let object = call("mmap", &[int(-1), int(8), flags]);
        assert!(!exc::pending());
        assert_eq!(number(call("_mmap_write", &[object, bytes(b"abc")])), 3);
        assert_eq!(number(call("_mmap_seek", &[object, int(0)])), 0);
        assert_eq!(data(call("_mmap_read", &[object, int(3)])), b"abc".to_vec());
        assert_eq!(number(call("_mmap_len", &[object])), 8);
        call("_mmap_close", &[object]);
        assert!(!exc::pending());
        // Closing twice is a no-op, like CPython.
        call("_mmap_close", &[object]);
        assert!(!exc::pending());
        call("_mmap_read", &[object, int(1)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn slice_reads_honour_the_step() {
        exc::init();
        let flags = int(i64::from(MAP_PRIVATE | MAP_ANONYMOUS));
        let object = call("mmap", &[int(-1), int(6), flags]);
        call("_mmap_write", &[object, bytes(b"abcdef")]);
        assert_eq!(
            data(call(
                "_mmap_getitem",
                &[object, slice(int(0), NONE, int(2))]
            )),
            b"ace".to_vec()
        );
        assert_eq!(
            data(call(
                "_mmap_getitem",
                &[object, slice(int(4), NONE, int(-2))]
            )),
            b"eca".to_vec()
        );
        assert_eq!(number(call("_mmap_getitem", &[object, int(-1)])), 102);
        call("_mmap_getitem", &[object, int(6)]);
        assert_eq!(exc::pending_tag(), INDEX_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn readonly_mappings_reject_writes() {
        exc::init();
        let flags = int(i64::from(MAP_PRIVATE | MAP_ANONYMOUS));
        let object = call("mmap", &[int(-1), int(4), flags, int(i64::from(PROT_READ))]);
        assert!(!exc::pending());
        call("_mmap_write", &[object, bytes(b"x")]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }
}
