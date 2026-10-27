//! Native implementations for the builtin modules declared in
//! `hydro_ir::stdlib`.
//!
//! The central spec is the single source of truth for names, arities and
//! result types. `hydro_module_call` dispatches a direct `module.function(...)`
//! call, `hydro_module_get` materialises constants and native function values,
//! and [`call_native`] handles calls through a native function value
//! (including `map`/`filter`/`functools.reduce` callbacks).
//!
//! All fallible paths report a pending exception and return the `None` dummy
//! value; no user input can panic.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hydro_ir::stdlib;
use hydro_ir::{PYTHON_VERSION, PYTHON_VERSION_STRING};
use num_bigint::{BigInt, Sign};
use num_traits::{Signed, ToPrimitive};

use crate::core::globals;

pub(crate) mod abc;
pub(crate) mod argparse;
pub(crate) mod ast;
pub(crate) mod asyncio;
pub(crate) mod atexit;
pub(crate) mod base64;
pub(crate) mod binascii;
pub(crate) mod bisect;
pub(crate) mod builtins;
pub(crate) mod codecs;
pub(crate) mod collections;
pub(crate) mod collections_abc;
pub(crate) mod colorsys;
pub(crate) mod contextvars;
pub(crate) mod ctypes;
pub(crate) mod datetime;
pub(crate) mod decimal;
pub(crate) mod r#enum;
pub(crate) mod errno;
pub(crate) mod extension;
pub(crate) mod fnmatch;
pub(crate) mod fractions;
pub(crate) mod functools;
pub(crate) mod future;
pub(crate) mod gc;
pub(crate) mod getopt;
pub(crate) mod heapq;
pub(crate) mod imp;
pub(crate) mod importlib;
pub(crate) mod importlib_machinery;
pub(crate) mod importlib_metadata;
pub(crate) mod importlib_resources;
pub(crate) mod importlib_util;
pub(crate) mod inspect;
pub(crate) mod itertools;
pub(crate) mod keyword;
pub(crate) mod locale;
pub(crate) mod marshal;
pub(crate) mod mimetypes;
pub(crate) mod mmap_mod;
pub(crate) mod opcode;
pub(crate) mod pytest;
pub(crate) mod quopri;
pub(crate) mod re;
pub(crate) mod select;
pub(crate) mod shutil;
pub(crate) mod site;
pub(crate) mod socket;
pub(crate) mod stat;
pub(crate) mod string;
pub(crate) mod struct_mod;
pub(crate) mod subprocess;
pub(crate) mod sysconfig;
pub(crate) mod tempfile;
pub(crate) mod textwrap;
pub(crate) mod timeit;
pub(crate) mod tokenize;
pub(crate) mod types;
pub(crate) mod typing;
pub(crate) mod unicodedata;
pub(crate) mod unittest;
pub(crate) mod warnings;
pub(crate) mod weakref;
pub(crate) mod zlib_mod;
pub(crate) mod zoneinfo;

use crate::container;
use crate::exc::{
    self, ATTRIBUTE_ERROR, INDEX_ERROR, OS_ERROR, OVERFLOW_ERROR, RECURSION_ERROR, RUNTIME_ERROR,
    STOP_ITERATION, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::iter;
use crate::ops;
use crate::process;
use crate::serialize;
use crate::value::{self, Num, FALSE, NONE, TRUE};

/// Maximum length accepted for an ABI name buffer (`isize::MAX` keeps the
/// slice representable).
const MAX_NAME_LEN: usize = isize::MAX as usize;

/// Maximum argument count whose byte length still fits in `isize`.
const MAX_ARGS: usize = MAX_NAME_LEN / std::mem::size_of::<i64>();

/// Borrows a UTF-8 name from the ABI buffer without allocating. `NULL`, empty,
/// over-long and non-UTF-8 buffers decode to `None`; those are error paths,
/// where callers fall back to [`decode_name`] to preserve the historical lossy
/// rendering.
fn name_str<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if ptr.is_null() || len == 0 || len > MAX_NAME_LEN {
        return None;
    }
    // SAFETY: ABI contract: `ptr..ptr+len` is readable; length checked above.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).ok()
}

/// Copies a UTF-8 name from the ABI buffer. `NULL`, empty and over-long
/// buffers decode to the empty string, which then fails the module lookup.
/// Only used on error paths; hot paths borrow through [`name_str`].
fn decode_name(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() || len == 0 || len > MAX_NAME_LEN {
        return String::new();
    }
    // SAFETY: ABI contract: `ptr..ptr+len` is readable; length checked above.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// One resolved native call. Names identify the call-site buffers by
/// pointer/length pair; the byte comparison in [`NativeCache::lookup`]
/// rechecks the contents so a recycled pointer with different text cannot
/// produce a false hit.
#[derive(Clone, Copy)]
struct NativeCacheEntry {
    module_ptr: usize,
    module_len: usize,
    function_ptr: usize,
    function_len: usize,
    /// Interned from `ModuleSpec::name`; never allocated per call.
    module: &'static str,
    /// Interned from `FunctionSpec::name`; never allocated per call.
    function: &'static str,
    spec: &'static stdlib::FunctionSpec,
}

const NATIVE_CACHE_SIZE: usize = 8;

/// Bounded FIFO cache of resolved native calls. Resolving a name scans the
/// central spec table; repeated `module.function(...)` call sites instead pay
/// a pointer/length probe plus a byte comparison and reuse the interned names.
struct NativeCache {
    entries: [Option<NativeCacheEntry>; NATIVE_CACHE_SIZE],
    next: usize,
    #[cfg(test)]
    hits: usize,
}

thread_local! {
    static NATIVE_CACHE: RefCell<NativeCache> = const {
        RefCell::new(NativeCache {
            entries: [None; NATIVE_CACHE_SIZE],
            next: 0,
            #[cfg(test)]
            hits: 0,
        })
    };
}

impl NativeCache {
    fn lookup(
        &mut self,
        module_ptr: usize,
        module_len: usize,
        module: &str,
        function_ptr: usize,
        function_len: usize,
        function: &str,
    ) -> Option<NativeCacheEntry> {
        let entry = self.entries.iter().flatten().copied().find(|entry| {
            entry.module_ptr == module_ptr
                && entry.module_len == module_len
                && entry.function_ptr == function_ptr
                && entry.function_len == function_len
                && entry.module.as_bytes() == module.as_bytes()
                && entry.function.as_bytes() == function.as_bytes()
        });
        #[cfg(test)]
        if entry.is_some() {
            self.hits += 1;
        }
        entry
    }

    fn insert(&mut self, entry: NativeCacheEntry) {
        self.entries[self.next] = Some(entry);
        self.next += 1;
        if self.next == NATIVE_CACHE_SIZE {
            self.next = 0;
        }
    }
}

/// Arity-only spec for a native method value (`OrderedDict.move_to_end`,
/// `_singledispatchmethod_get.__init__`) that is not a Python-visible module
/// function but whose `FunctionBody::Native` name must resolve.
const fn native_member(
    name: &'static str,
    min_args: usize,
    max_args: usize,
) -> stdlib::FunctionSpec {
    stdlib::FunctionSpec {
        name,
        min_args,
        max_args,
        ret: hydro_ir::Type::Any,
        params: &[],
        dynamic_keywords: false,
    }
}

/// Runtime-only module functions that the static spec table predates.
///
/// They live on the same modules and use the same arity validation as the
/// static table; the frontend has an identical table so call sites bind the
/// parameters by name. `types.CellType` is exposed as a callable (its value is
/// the cached native class) and `builtins.eval`/`exec` are the builtin
/// implementations.
static SUPPLEMENTAL_FUNCTIONS: &[(&str, stdlib::FunctionSpec)] = &[
    (
        "builtins.eval",
        stdlib::FunctionSpec {
            name: "eval",
            min_args: 1,
            max_args: 3,
            ret: hydro_ir::Type::Any,
            params: &["source", "globals", "locals"],
            dynamic_keywords: false,
        },
    ),
    (
        "builtins.exec",
        stdlib::FunctionSpec {
            name: "exec",
            min_args: 1,
            max_args: 3,
            ret: hydro_ir::Type::Any,
            params: &["source", "globals", "locals"],
            dynamic_keywords: false,
        },
    ),
    (
        "types.CellType",
        stdlib::FunctionSpec {
            name: "CellType",
            min_args: 0,
            max_args: 1,
            ret: hydro_ir::Type::Any,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    // Native methods of `_struct.Struct`. The native values carry these
    // dotted names (`FunctionBody::Native`), so the central table must know
    // them even though they are not Python-visible module functions.
    (
        "_struct.Struct.__init__",
        stdlib::FunctionSpec {
            name: "Struct.__init__",
            min_args: 2,
            max_args: 2,
            ret: hydro_ir::Type::None,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "_struct.Struct.pack",
        stdlib::FunctionSpec {
            name: "Struct.pack",
            min_args: 1,
            max_args: usize::MAX,
            ret: hydro_ir::Type::Bytes,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "_struct.Struct.pack_into",
        stdlib::FunctionSpec {
            name: "Struct.pack_into",
            min_args: 3,
            max_args: usize::MAX,
            ret: hydro_ir::Type::None,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "_struct.Struct.unpack",
        stdlib::FunctionSpec {
            name: "Struct.unpack",
            min_args: 2,
            max_args: 2,
            ret: hydro_ir::Type::Tuple,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "_struct.Struct.unpack_from",
        stdlib::FunctionSpec {
            name: "Struct.unpack_from",
            min_args: 2,
            max_args: 3,
            ret: hydro_ir::Type::Tuple,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "_struct.Struct.iter_unpack",
        stdlib::FunctionSpec {
            name: "Struct.iter_unpack",
            min_args: 2,
            max_args: 2,
            ret: hydro_ir::Type::Any,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    // Native methods of `abc.ABCMeta`. A metaclass method is reached through a
    // class (`Number.register(int)`), so the class is the first argument and
    // the name is not a Python-visible module member.
    (
        "abc.ABCMeta.register",
        stdlib::FunctionSpec {
            name: "ABCMeta.register",
            min_args: 2,
            max_args: 2,
            ret: hydro_ir::Type::Any,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "abc.ABCMeta.__instancecheck__",
        stdlib::FunctionSpec {
            name: "ABCMeta.__instancecheck__",
            min_args: 2,
            max_args: 2,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "abc.ABCMeta.__subclasscheck__",
        stdlib::FunctionSpec {
            name: "ABCMeta.__subclasscheck__",
            min_args: 2,
            max_args: 2,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "abc.ABCMeta.__abstractmethods__",
        stdlib::FunctionSpec {
            name: "ABCMeta.__abstractmethods__",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Set,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    // Native methods of the in-memory streams. They are not Python-visible
    // module members; the class namespace holds the function values and the
    // central table validates their arity (the receiver counts as the first
    // argument, like any bound method).
    (
        "io.StringIO.write",
        stdlib::FunctionSpec {
            name: "StringIO.write",
            min_args: 2,
            max_args: 2,
            ret: hydro_ir::Type::Int,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.read",
        stdlib::FunctionSpec {
            name: "StringIO.read",
            min_args: 1,
            max_args: 2,
            ret: hydro_ir::Type::Str,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.readline",
        stdlib::FunctionSpec {
            name: "StringIO.readline",
            min_args: 1,
            max_args: 2,
            ret: hydro_ir::Type::Str,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.getvalue",
        stdlib::FunctionSpec {
            name: "StringIO.getvalue",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Str,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.seek",
        stdlib::FunctionSpec {
            name: "StringIO.seek",
            min_args: 2,
            max_args: 3,
            ret: hydro_ir::Type::Int,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.tell",
        stdlib::FunctionSpec {
            name: "StringIO.tell",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Int,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.close",
        stdlib::FunctionSpec {
            name: "StringIO.close",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::None,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.flush",
        stdlib::FunctionSpec {
            name: "StringIO.flush",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::None,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.readable",
        stdlib::FunctionSpec {
            name: "StringIO.readable",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.writable",
        stdlib::FunctionSpec {
            name: "StringIO.writable",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.seekable",
        stdlib::FunctionSpec {
            name: "StringIO.seekable",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.__enter__",
        stdlib::FunctionSpec {
            name: "StringIO.__enter__",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Any,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.StringIO.__exit__",
        stdlib::FunctionSpec {
            name: "StringIO.__exit__",
            min_args: 4,
            max_args: 4,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.write",
        stdlib::FunctionSpec {
            name: "BytesIO.write",
            min_args: 2,
            max_args: 2,
            ret: hydro_ir::Type::Int,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.read",
        stdlib::FunctionSpec {
            name: "BytesIO.read",
            min_args: 1,
            max_args: 2,
            ret: hydro_ir::Type::Bytes,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.readline",
        stdlib::FunctionSpec {
            name: "BytesIO.readline",
            min_args: 1,
            max_args: 2,
            ret: hydro_ir::Type::Bytes,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.getvalue",
        stdlib::FunctionSpec {
            name: "BytesIO.getvalue",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bytes,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.seek",
        stdlib::FunctionSpec {
            name: "BytesIO.seek",
            min_args: 2,
            max_args: 3,
            ret: hydro_ir::Type::Int,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.tell",
        stdlib::FunctionSpec {
            name: "BytesIO.tell",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Int,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.close",
        stdlib::FunctionSpec {
            name: "BytesIO.close",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::None,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.flush",
        stdlib::FunctionSpec {
            name: "BytesIO.flush",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::None,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.readable",
        stdlib::FunctionSpec {
            name: "BytesIO.readable",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.writable",
        stdlib::FunctionSpec {
            name: "BytesIO.writable",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.seekable",
        stdlib::FunctionSpec {
            name: "BytesIO.seekable",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.__enter__",
        stdlib::FunctionSpec {
            name: "BytesIO.__enter__",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Any,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.BytesIO.__exit__",
        stdlib::FunctionSpec {
            name: "BytesIO.__exit__",
            min_args: 4,
            max_args: 4,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    // Native methods of `io.TextIOWrapper`. The wrapper is a class instance,
    // so its method values dispatch through this table like the in-memory
    // stream methods above.
    (
        "io.TextIOWrapper.read",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.read",
            min_args: 1,
            max_args: 2,
            ret: hydro_ir::Type::Str,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.readline",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.readline",
            min_args: 1,
            max_args: 2,
            ret: hydro_ir::Type::Str,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.readlines",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.readlines",
            min_args: 1,
            max_args: 2,
            ret: hydro_ir::Type::List,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.write",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.write",
            min_args: 2,
            max_args: 2,
            ret: hydro_ir::Type::Int,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.writelines",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.writelines",
            min_args: 2,
            max_args: 2,
            ret: hydro_ir::Type::None,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.close",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.close",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::None,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.flush",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.flush",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::None,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.seek",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.seek",
            min_args: 2,
            max_args: 3,
            ret: hydro_ir::Type::Int,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.tell",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.tell",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Int,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.readable",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.readable",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.writable",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.writable",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.seekable",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.seekable",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.isatty",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.isatty",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.detach",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.detach",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Any,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.__enter__",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.__enter__",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Any,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.__exit__",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.__exit__",
            min_args: 4,
            max_args: 4,
            ret: hydro_ir::Type::Bool,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.__iter__",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.__iter__",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Any,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    (
        "io.TextIOWrapper.__next__",
        stdlib::FunctionSpec {
            name: "TextIOWrapper.__next__",
            min_args: 1,
            max_args: 1,
            ret: hydro_ir::Type::Str,
            params: &[],
            dynamic_keywords: false,
        },
    ),
    // Native methods of `collections.OrderedDict`. The values carry
    // `FunctionBody::Native` names like `OrderedDict.move_to_end`, so this
    // table must resolve them even though they are not module functions.
    (
        "collections.OrderedDict.__init__",
        native_member("OrderedDict.__init__", 1, usize::MAX),
    ),
    (
        "collections.OrderedDict.fromkeys",
        native_member("OrderedDict.fromkeys", 2, 3),
    ),
    (
        "collections.OrderedDict.__getitem__",
        native_member("OrderedDict.__getitem__", 2, 2),
    ),
    (
        "collections.OrderedDict.__setitem__",
        native_member("OrderedDict.__setitem__", 3, 3),
    ),
    (
        "collections.OrderedDict.__delitem__",
        native_member("OrderedDict.__delitem__", 2, 2),
    ),
    (
        "collections.OrderedDict.__iter__",
        native_member("OrderedDict.__iter__", 1, 1),
    ),
    (
        "collections.OrderedDict.__len__",
        native_member("OrderedDict.__len__", 1, 1),
    ),
    (
        "collections.OrderedDict.__contains__",
        native_member("OrderedDict.__contains__", 2, 2),
    ),
    (
        "collections.OrderedDict.__eq__",
        native_member("OrderedDict.__eq__", 2, 2),
    ),
    (
        "collections.OrderedDict.__repr__",
        native_member("OrderedDict.__repr__", 1, 1),
    ),
    (
        "collections.OrderedDict.get",
        native_member("OrderedDict.get", 2, 3),
    ),
    (
        "collections.OrderedDict.keys",
        native_member("OrderedDict.keys", 1, 1),
    ),
    (
        "collections.OrderedDict.values",
        native_member("OrderedDict.values", 1, 1),
    ),
    (
        "collections.OrderedDict.items",
        native_member("OrderedDict.items", 1, 1),
    ),
    (
        "collections.OrderedDict.move_to_end",
        native_member("OrderedDict.move_to_end", 2, 3),
    ),
    (
        "collections.OrderedDict.popitem",
        native_member("OrderedDict.popitem", 1, 2),
    ),
    (
        "collections.OrderedDict.pop",
        native_member("OrderedDict.pop", 2, 3),
    ),
    (
        "collections.OrderedDict.update",
        native_member("OrderedDict.update", 1, 2),
    ),
    (
        "collections.OrderedDict.setdefault",
        native_member("OrderedDict.setdefault", 2, 3),
    ),
    (
        "collections.OrderedDict.copy",
        native_member("OrderedDict.copy", 1, 1),
    ),
    (
        "collections.OrderedDict.clear",
        native_member("OrderedDict.clear", 1, 1),
    ),
    // Native methods of the `functools._singledispatchmethod_get` class.
    (
        "functools._singledispatchmethod_get.__init__",
        native_member("_singledispatchmethod_get.__init__", 4, usize::MAX),
    ),
    (
        "functools._singledispatchmethod_get.__repr__",
        native_member("_singledispatchmethod_get.__repr__", 1, 1),
    ),
];

/// Resolves a [`SUPPLEMENTAL_FUNCTIONS`] entry by module and member name.
fn supplemental_function(module: &str, function: &str) -> Option<&'static stdlib::FunctionSpec> {
    SUPPLEMENTAL_FUNCTIONS
        .iter()
        .find(|(qualified, _)| {
            qualified
                .split_once('.')
                .is_some_and(|(owner, name)| owner == module && name == function)
        })
        .map(|(_, spec)| spec)
}

/// Resolves a native call without the cache. The spec table owns the names,
/// so they are interned as `&'static str` and error rendering never allocates
/// on the hit path.
fn resolve_native(
    module: &str,
    function: &str,
) -> Option<(&'static str, &'static str, &'static stdlib::FunctionSpec)> {
    let module_name = stdlib::module(module)?.name;
    if let Some(spec) = supplemental_function(module, function) {
        return Some((module_name, spec.name, spec));
    }
    // The native `inspect` classes expose methods whose qualified names are
    // not module functions; the module owns their specs.
    if module == "inspect" {
        if let Some(spec) = inspect::function_spec(function) {
            return Some((module_name, spec.name, spec));
        }
    }
    let spec = stdlib::function(module, function)?;
    Some((module_name, spec.name, spec))
}

/// Resolves `module.function` through the per-thread cache, falling back to a
/// spec-table scan on a miss. Empty names (invalid ABI buffers) are never
/// cached and simply fail the lookup.
fn native_spec(
    module_ptr: usize,
    module_len: usize,
    module: &str,
    function_ptr: usize,
    function_len: usize,
    function: &str,
) -> Option<(&'static str, &'static str, &'static stdlib::FunctionSpec)> {
    if module_len == 0 || function_len == 0 {
        return None;
    }
    let cached = NATIVE_CACHE.try_with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(entry) = cache.lookup(
            module_ptr,
            module_len,
            module,
            function_ptr,
            function_len,
            function,
        ) {
            return Some((entry.module, entry.function, entry.spec));
        }
        let resolved = resolve_native(module, function)?;
        cache.insert(NativeCacheEntry {
            module_ptr,
            module_len,
            function_ptr,
            function_len,
            module: resolved.0,
            function: resolved.1,
            spec: resolved.2,
        });
        Some(resolved)
    });
    match cached {
        Ok(resolved) => resolved,
        // The thread-local is unavailable only while thread state is being
        // torn down; resolve directly rather than panic.
        Err(_) => resolve_native(module, function),
    }
}

fn missing_attribute(module: &str, name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '{module}' has no attribute '{name}'"),
    )
}

fn failed() -> bool {
    exc::pending()
}

fn encode_int(n: i64) -> i64 {
    value::encode_int(n)
        .unwrap_or_else(|| exc::raise_with_message(OVERFLOW_ERROR, "integer overflow"))
}

// ABI entry helpers

/// `hydro_module_call`: direct `module.function(args...)` call.
pub(crate) fn module_call(
    module_ptr: *const u8,
    module_len: usize,
    function_ptr: *const u8,
    function_len: usize,
    argc: usize,
    argv: *const i64,
) -> i64 {
    if let (Some(module), Some(function)) = (
        name_str(module_ptr, module_len),
        name_str(function_ptr, function_len),
    ) {
        return call_native_at(
            module_ptr as usize,
            module_len,
            module,
            function_ptr as usize,
            function_len,
            function,
            argc,
            argv,
        );
    }
    // Invalid buffer or non-UTF-8 name: decode lossily so the resulting
    // error message matches the historical rendering.
    let module = decode_name(module_ptr, module_len);
    let function = decode_name(function_ptr, function_len);
    call_native(&module, &function, argc, argv)
}

/// `hydro_module_call_kw`: `module.function(args..., name=value...)` for a spec
/// that declares dynamic keywords (`functools.partial`).
///
/// `kw_argv` holds `2 * kwargc` tagged values in `[name, value, ...]` order.
/// A spec without dynamic keywords rejects the call instead of dropping the
/// names, mirroring the compile-time diagnostic for ordinary module functions.
#[allow(clippy::too_many_arguments)]
pub(crate) fn module_call_kw(
    module_ptr: *const u8,
    module_len: usize,
    function_ptr: *const u8,
    function_len: usize,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    if kwargc == 0 {
        return module_call(
            module_ptr,
            module_len,
            function_ptr,
            function_len,
            argc,
            argv,
        );
    }
    if let (Some(module), Some(function)) = (
        name_str(module_ptr, module_len),
        name_str(function_ptr, function_len),
    ) {
        return call_native_kw_at(
            module_ptr as usize,
            module_len,
            module,
            function_ptr as usize,
            function_len,
            function,
            argc,
            argv,
            kwargc,
            kw_argv,
        );
    }
    let module = decode_name(module_ptr, module_len);
    let function = decode_name(function_ptr, function_len);
    call_native_kw_at(
        module.as_ptr() as usize,
        module.len(),
        &module,
        function.as_ptr() as usize,
        function.len(),
        &function,
        argc,
        argv,
        kwargc,
        kw_argv,
    )
}

/// `hydro_module_get`: returns a constant value or a native function object.
pub(crate) fn module_get(
    module_ptr: *const u8,
    module_len: usize,
    name_ptr: *const u8,
    name_len: usize,
) -> i64 {
    if let (Some(module), Some(name)) = (
        name_str(module_ptr, module_len),
        name_str(name_ptr, name_len),
    ) {
        return module_get_named(module, name);
    }
    // Invalid buffer or non-UTF-8 name: decode lossily so the resulting
    // error message matches the historical rendering.
    let module = decode_name(module_ptr, module_len);
    let name = decode_name(name_ptr, name_len);
    module_get_named(&module, &name)
}

/// Resolves `module.name` for a module value receiver (`getattr(module, name)`
/// in `crate::methods`/`crate::builtins`).
pub(crate) fn module_get_by_name(module: &str, name: &str) -> i64 {
    module_get_named(module, name)
}

fn module_get_named(module: &str, name: &str) -> i64 {
    // Every module value knows its import name, including user modules that
    // have no native spec.
    if name == "__name__" {
        return value::str_alloc(module.to_string());
    }
    // `module.__dict__` is the module namespace mapping. Compiled user
    // modules take the live `globals()` view; native modules are enumerated
    // from the central spec. `vars(module)` shares this mapping.
    if name == "__dict__" {
        return module_dict(module);
    }
    // Native modules have no Python source, but they still expose a spec, a
    // loader and a package name. The spec is cached so repeated reads share
    // identity, matching `module.__spec__` in CPython.
    if stdlib::module(module).is_some() {
        match name {
            "__package__" => return value::str_alloc(native_package(module)),
            "__spec__" => return native_module_spec(module),
            "__loader__" => return importlib_machinery::builtin_importer_class(),
            _ => {}
        }
    }
    // `errno` attributes are platform-defined names resolved on demand; the
    // static spec table cannot enumerate them.
    if module == "errno" {
        return errno::lookup(name);
    }
    if module == "unittest" && name == "TestCase" {
        return unittest::test_case();
    }
    if module == "pytest" && name == "mark" {
        return pytest::marks();
    }
    // `builtins.object` is CPython's universal base class, not a function.
    if module == "builtins" && name == "object" {
        return value::object_class();
    }
    // `io.StringIO`/`io.BytesIO` are declared as functions so calls lower as
    // module calls, but the runtime value is the cached class: identity,
    // `isinstance` and method binding all need a class, not a constructor
    // function.
    if matches!(module, "io" | "_io") {
        match name {
            "StringIO" => return string_io_class(),
            "BytesIO" => return bytes_io_class(),
            "TextIOWrapper" => return text_io_wrapper_class(),
            _ => {}
        }
    }
    // `_collections` is the accelerator alias of `collections`; both names
    // must expose the same `defaultdict`/`OrderedDict` objects.
    if matches!(module, "collections" | "_collections") {
        match name {
            "defaultdict" => return collections::defaultdict_function(),
            // Declared as a function so calls lower as module calls, but the
            // value is the cached class: `isinstance` and method binding need
            // a class, exactly like `io.StringIO`.
            "OrderedDict" => return collections::ordereddict_class(),
            _ => {}
        }
    }
    // `functools._singledispatchmethod_get` is a class value because
    // `inspect.isroutine` uses it as an `isinstance` target.
    if module == "functools" && name == "_singledispatchmethod_get" {
        return functools::singledispatchmethod_get_class();
    }
    // `functools.partialmethod` is declared as a function so calls lower as
    // module calls, but the value itself is the real descriptor class:
    // `isinstance(pm, functools.partialmethod)` and descriptor access need it.
    if module == "functools" && name == "partialmethod" {
        return functools::partialmethod_class();
    }
    // `functools.cached_property` is the same shape: declared as a function so
    // calls lower as module calls, but the value is the real descriptor class
    // (`isinstance`, class access and `__get__` need it).
    if module == "functools" && name == "cached_property" {
        return functools::cached_property_class();
    }
    // AST node/operator names are declared as functions so `ast.Name(...)`
    // lowers as a module call, but the value itself is a real class:
    // `isinstance(node, ast.Name)` and `class T(ast.NodeTransformer)` need the
    // class, not the constructor function.
    if let Some(class) = ast::class_value(module, name) {
        return class;
    }
    if let Some(class) = typing::class_value(module, name) {
        return class;
    }
    if let Some(class) = types::class_value(module, name) {
        return class;
    }
    if let Some(class) = inspect::class_value(module, name) {
        return class;
    }
    // `_struct.Struct` and `_struct.error` are declared as functions/constants
    // so calls lower as module calls; the runtime values are real classes (the
    // class protocol and `isinstance` need them).
    if let Some(class) = struct_mod::class_value(module, name) {
        return class;
    }
    // `mmap.mmap` must be a class value (identity/`isinstance`/method binding),
    // and `zlib.error` is exposed the same way.
    if let Some(class) = mmap_mod::class_value(module, name) {
        return class;
    }
    if let Some(class) = zlib_mod::class_value(module, name) {
        return class;
    }
    // A nested native module (`asyncio.coroutines`, `os.path`, ...) is an
    // attribute of its parent package. Resolve it to the exact `sys.modules`
    // entry so `import asyncio.coroutines` followed by
    // `asyncio.coroutines._is_coroutine` keeps module identity.
    let nested = format!("{module}.{name}");
    if stdlib::module(&nested).is_some() {
        return registry_lookup(&nested).unwrap_or_else(|| value::module_value(nested));
    }
    if let Some(spec) = stdlib::function(module, name) {
        let module_name = stdlib::module(module).map(|m| m.name).unwrap_or("<module>");
        return value::native_function_new(module_name, spec.name, spec.max_args);
    }
    if stdlib::constant(module, name).is_some() {
        return constant_value(module, name);
    }
    // Runtime-only members (`builtins.eval`, `builtins.exec`,
    // `types.CellType`) have no central spec entry but still exist on their
    // modules, matching `module.__dict__` below.
    if let Some(value) = supplemental_value(module, name) {
        return value;
    }
    // Compiled user modules keep their attributes in the globals side store.
    // Native modules win above, so a user module named like a stdlib module
    // cannot shadow native attributes. The `globals()` view is consulted for
    // names bound dynamically through `globals()[name] = value`.
    if let Some(value) = globals::lookup(module, name) {
        return value;
    }
    missing_attribute(module, name)
}

/// `module.__package__` for a native module: a module with registered
/// submodules is a package (its own package), otherwise it lives in its parent
/// and a top-level module reports `""`, like CPython.
fn native_package(module: &str) -> String {
    let prefix = format!("{module}.");
    if stdlib::MODULES
        .iter()
        .any(|spec| spec.name.starts_with(&prefix))
    {
        return module.to_string();
    }
    match module.rsplit_once('.') {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    }
}

/// Cached `ModuleSpec` of a native module.
fn native_module_spec(module: &str) -> i64 {
    let key = format!("module.spec:{module}");
    if let Some(spec) = globals::registry_get(&key) {
        return spec;
    }
    let spec = importlib_util::native_spec(module);
    globals::registry_set(&key, spec);
    spec
}

/// Values of the runtime-only module members. `builtins.eval`/`exec` are the
/// builtin function objects; `types.CellType` is the cached native class (a
/// class value is callable and works as an `isinstance` target).
fn supplemental_value(module: &str, name: &str) -> Option<i64> {
    match (module, name) {
        ("builtins", "eval") => Some(value::builtin_function_new(hydro_ir::Builtin::Eval)),
        ("builtins", "exec") => Some(value::builtin_function_new(hydro_ir::Builtin::Exec)),
        ("types", "CellType") => Some(types::cell_type_class()),
        _ => None,
    }
}

/// `module.__dict__` / `vars(module)`: the module's namespace mapping.
///
/// A compiled user module (`globals()` view) returns the live mapping; a
/// native module gets one cached dict holding `__name__`, every function and
/// every constant from the central spec. An unknown, never-loaded module
/// resolves to its (empty) globals view instead of a fabricated spec.
pub(crate) fn module_dict(module: &str) -> i64 {
    if stdlib::module(module).is_none() {
        return globals::view(module);
    }
    let cache_key = format!("module.dict:{module}");
    if let Some(cached) = globals::registry_get(&cache_key) {
        return cached;
    }
    let dict = value::dict_new(Vec::new());
    globals::registry_set(&cache_key, dict);
    let roots = crate::heap::root_values(&[dict]);
    insert_namespace_entry(dict, "__name__", value::str_alloc(module.to_string()));
    let Some(spec) = stdlib::module(module) else {
        crate::heap::release_roots(roots);
        return dict;
    };
    let mut names: Vec<&'static str> = spec.functions.iter().map(|f| f.name).collect();
    names.extend(spec.constants.iter().map(|c| c.name));
    match module {
        "builtins" => {
            names.push("eval");
            names.push("exec");
        }
        "types" => names.push("CellType"),
        _ => {}
    }
    for name in names {
        let item = module_get_named(module, name);
        if exc::pending() {
            // A spec entry that cannot be materialised (none today, but the
            // table is shared) must not silently become `None`: propagate so
            // the caller sees the real failure.
            crate::heap::release_roots(roots);
            return NONE;
        }
        insert_namespace_entry(dict, name, item);
    }
    crate::heap::release_roots(roots);
    dict
}

/// Inserts one `(name, value)` pair into a namespace dict, rooting the freshly
/// allocated key until the dict owns it.
fn insert_namespace_entry(dict: i64, name: &str, item: i64) {
    let key = value::str_alloc(name.to_string());
    let roots = crate::heap::root_values(&[key, item]);
    container::dict_set(dict, key, item);
    crate::heap::release_roots(roots);
}

/// Calls a native function by name, validating it against the central spec.
/// Shared by `hydro_module_call` and calls through native function values.
pub(crate) fn call_native(module: &str, function: &str, argc: usize, argv: *const i64) -> i64 {
    // `FunctionBody::Native` stores `&'static str`, so the pointer identity
    // used as the cache key is stable; the byte recheck still guards any
    // caller that passes an ordinary borrow.
    call_native_at(
        module.as_ptr() as usize,
        module.len(),
        module,
        function.as_ptr() as usize,
        function.len(),
        function,
        argc,
        argv,
    )
}

#[allow(clippy::too_many_arguments)]
fn call_native_at(
    module_ptr: usize,
    module_len: usize,
    module: &str,
    function_ptr: usize,
    function_len: usize,
    function: &str,
    argc: usize,
    argv: *const i64,
) -> i64 {
    let Some((module_name, function_name, spec)) = native_spec(
        module_ptr,
        module_len,
        module,
        function_ptr,
        function_len,
        function,
    ) else {
        return missing_attribute(module, function);
    };
    if argc < spec.min_args || argc > spec.max_args {
        let detail = if spec.max_args == usize::MAX {
            format!("at least {} arguments", spec.min_args)
        } else if spec.min_args == spec.max_args {
            format!("{} arguments", spec.max_args)
        } else {
            format!("{} to {} arguments", spec.min_args, spec.max_args)
        };
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{module_name}.{function_name}() takes {detail} ({argc} given)"),
        );
    }
    if argc > MAX_ARGS {
        return exc::raise_with_message(TYPE_ERROR, "argument count is out of range");
    }
    let args: &[i64] = if argc == 0 {
        &[]
    } else if argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("function call received {argc} argument(s) without an argument buffer"),
        );
    } else {
        // SAFETY: ABI contract: `argv..argv+argc` is readable; length checked above.
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    dispatch(module_name, function_name, args)
}

/// Keyword variant of [`call_native`] for a native function value.
///
/// `functools.partial` and the `contextlib.contextmanager` wrapper forward
/// call-site keywords to a native target; the central dispatch rejects the
/// names unless the spec declares dynamic keyword support.
pub(crate) fn call_native_kw(
    module: &str,
    function: &str,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    call_native_kw_at(
        module.as_ptr() as usize,
        module.len(),
        module,
        function.as_ptr() as usize,
        function.len(),
        function,
        argc,
        argv,
        kwargc,
        kw_argv,
    )
}

/// Whether the native function keeps arbitrary keyword names for its callee
/// (`dynamic_keywords` in the central spec). Callers use this to decide
/// between the positional and keyword dispatch paths.
pub(crate) fn native_accepts_keywords(module: &str, function: &str) -> bool {
    resolve_native(module, function)
        .map(|(_, _, spec)| spec.dynamic_keywords)
        .unwrap_or(false)
}

/// Keyword variant of [`call_native_at`]: validates the spec's arity, decodes
/// the keyword pairs, moves the fixed leading parameters supplied by name into
/// their positional slots and hands the rest to the keyword-aware dispatcher.
#[allow(clippy::too_many_arguments)]
fn call_native_kw_at(
    module_ptr: usize,
    module_len: usize,
    module: &str,
    function_ptr: usize,
    function_len: usize,
    function: &str,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    let Some((module_name, function_name, spec)) = native_spec(
        module_ptr,
        module_len,
        module,
        function_ptr,
        function_len,
        function,
    ) else {
        return missing_attribute(module, function);
    };
    if !spec.dynamic_keywords {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{module_name}.{function_name}() does not accept keyword arguments"),
        );
    }
    if argc > MAX_ARGS || kwargc > MAX_ARGS / 2 {
        return exc::raise_with_message(TYPE_ERROR, "argument count is out of range");
    }
    if (argc > 0 && argv.is_null()) || kw_argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "function call received arguments without an argument buffer",
        );
    }
    // SAFETY: ABI contract: caller passes readable buffers; length checked above.
    let mut args: Vec<i64> = if argc == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
    };
    let words = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
    let mut keywords: Vec<(String, i64)> = Vec::with_capacity(kwargc);
    for pair in words.chunks(2) {
        let Some(name) = value::as_str(pair[0]) else {
            return exc::raise_with_message(TYPE_ERROR, "keywords must be strings");
        };
        keywords.push((name.to_string(), pair[1]));
    }
    // A fixed leading parameter may be passed by name; it takes its positional
    // slot so the dispatcher sees the usual layout.
    for (index, param) in spec.params.iter().enumerate() {
        if args.len() > index {
            continue;
        }
        if let Some(position) = keywords
            .iter()
            .position(|(name, _)| name.as_str() == *param)
        {
            let (_, value) = keywords.remove(position);
            args.push(value);
        }
    }
    if args.len() < spec.min_args || args.len() > spec.max_args {
        let detail = if spec.max_args == usize::MAX {
            format!("at least {} arguments", spec.min_args)
        } else if spec.min_args == spec.max_args {
            format!("{} arguments", spec.max_args)
        } else {
            format!("{} to {} arguments", spec.min_args, spec.max_args)
        };
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{module_name}.{function_name}() takes {detail} ({} given)",
                args.len()
            ),
        );
    }
    dispatch_kw(module_name, function_name, &args, keywords)
}

/// Keyword-aware dispatch; `functools.partial` and `dataclasses.replace` bind
/// names.
fn dispatch_kw(module: &str, function: &str, args: &[i64], keywords: Vec<(String, i64)>) -> i64 {
    match module {
        "math" => math_call_kw(function, args, keywords),
        "functools" | "_functools" => functools_call_kw(function, args, keywords),
        "collections" | "_collections" => collections::call_kw(function, args, &keywords),
        "dataclasses" => dataclasses_call_kw(function, args, keywords),
        "argparse" => argparse::call_kw(function, args, &keywords),
        "socket" => socket::call_kw(function, args, keywords),
        "ssl" => socket::ssl_call_kw(function, args, keywords),
        "select" => select::call_kw(function, args, keywords),
        "pytest" => pytest::call_kw(function, args, keywords),
        "warnings" | "_warnings" => warnings::call_kw(function, args, &keywords),
        "typing" => typing::call_kw(function, args, &keywords),
        "inspect" => inspect::call_kw(function, args, &keywords),
        "ast" => ast::call_kw(function, args, &keywords),
        "contextlib" => contextlib_call_kw(function, args, &keywords),
        "types" => types::call_kw(function, args, &keywords),
        "os.path" => os_path_call_kw(function, args, &keywords),
        "itertools" => itertools_call_kw(function, args, keywords),
        "importlib.metadata" => importlib_metadata::call_kw(function, args, &keywords),
        _ => exc::raise_with_message(
            TYPE_ERROR,
            &format!("{module}.{function}() does not accept keyword arguments"),
        ),
    }
}

/// Materialises an iterable into a list, keeping the growing list rooted so
/// elements stay traceable while later steps allocate.
fn itertools_collect(iterable: i64) -> Result<i64, i64> {
    let iterator = iter::iter_of(iterable);
    if exc::pending() {
        return Err(NONE);
    }
    let result = value::list_new(Vec::new());
    let roots = heap::root_values(&[result]);
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            heap::release_roots(roots);
            return Err(NONE);
        }
        if status == 0 {
            break;
        }
        container::list_append(result, item);
        if exc::pending() {
            heap::release_roots(roots);
            return Err(NONE);
        }
    }
    heap::release_roots(roots);
    Ok(result)
}

fn itertools_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "permutations" => itertools_permutations(args),
        "combinations" => itertools_combinations(args),
        "combinations_with_replacement" => itertools_combinations_with_replacement(args),
        "product" => itertools_product(args, 1),
        "takewhile" => itertools_takewhile(args),
        _ => itertools::call(function, args),
    }
}

/// Keyword-aware `itertools` dispatch: `product` is the only combinator with
/// a keyword-only parameter (`repeat`).
fn itertools_call_kw(function: &str, args: &[i64], keywords: Vec<(String, i64)>) -> i64 {
    if function != "product" {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("itertools.{function}() does not accept keyword arguments"),
        );
    }
    let mut repeat = 1i64;
    for (name, argument) in keywords {
        if name != "repeat" {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("product() got an unexpected keyword argument '{name}'"),
            );
        }
        match value::as_integer(argument) {
            Some(value) => repeat = value,
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "integer argument expected, got {}",
                        value::type_name(argument)
                    ),
                )
            }
        }
    }
    itertools_product(args, repeat)
}

/// `permutations(iterable, r=None)`: every length-`r` arrangement of the input
/// positions, in the lexicographic order `itertools.permutations` yields.
///
/// The index/cycle scheme is CPython's documented recipe: `indices` always
/// holds `n` positions ordered by their current selection priority, so the
/// first `r` entries are the emitted selection, and `cycles` counts how many
/// swaps remain for each of those slots. Rotating the tail reselects a fresh
/// combination once a slot is exhausted, which is what keeps `r < n` results
/// lexicographic instead of cycling through the initial `r` positions only.
fn itertools_permutations(args: &[i64]) -> i64 {
    let source = match itertools_collect(args[0]) {
        Ok(source) => source,
        Err(error) => return error,
    };
    let roots = heap::root_values(&[source]);
    let items: Vec<i64> = value::as_list(source)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let n = items.len();
    let r = match args.get(1).copied() {
        None | Some(NONE) => n,
        Some(argument) => match value::as_integer(argument) {
            Some(r) if r >= 0 => usize::try_from(r).unwrap_or(usize::MAX),
            Some(_) => {
                heap::release_roots(roots);
                return exc::raise_with_message(VALUE_ERROR, "r must be non-negative");
            }
            None => {
                heap::release_roots(roots);
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "integer argument expected, got {}",
                        value::type_name(argument)
                    ),
                );
            }
        },
    };
    let result = value::list_new(Vec::new());
    if r > n {
        // CPython yields nothing when `r` exceeds the pool; no tuple is built.
        heap::release_roots(roots);
        return result;
    }
    let result_roots = heap::root_values(&[result]);
    // `indices` holds every pool position, so a slot that runs out of swaps can
    // be refilled from the tail; only its first `r` entries are emitted.
    let mut indices: Vec<usize> = (0..n).collect();
    let mut cycles: Vec<usize> = (0..r).map(|offset| n - offset).collect();
    loop {
        let tuple = value::tuple_new(
            indices[..r]
                .iter()
                .map(|index| items[*index])
                .collect::<Vec<i64>>(),
        );
        container::list_append(result, tuple);
        // Advance to the next arrangement. Exhausting the leftmost slot ends
        // the sequence; `r == 0` emits the single empty tuple above and then
        // finds no slot to advance.
        let mut advanced = false;
        for slot in (0..r).rev() {
            cycles[slot] -= 1;
            if cycles[slot] == 0 {
                // Every rotation of this slot is used up: rotate the tail so
                // the slot's position takes the next pool value.
                indices[slot..].rotate_left(1);
                cycles[slot] = n - slot;
            } else {
                let swap = n - cycles[slot];
                indices.swap(slot, swap);
                advanced = true;
                break;
            }
        }
        if !advanced {
            break;
        }
    }
    heap::release_roots(result_roots);
    heap::release_roots(roots);
    result
}

/// `combinations(iterable, r)`: the length-`r` subsets of the input positions,
/// in the lexicographic order `itertools.combinations` yields.
fn itertools_combinations(args: &[i64]) -> i64 {
    itertools_combinations_impl(args, false)
}

/// `combinations_with_replacement(iterable, r)`: the length-`r` selections that
/// may reuse a position, in lexicographic order.
fn itertools_combinations_with_replacement(args: &[i64]) -> i64 {
    itertools_combinations_impl(args, true)
}

/// Shared implementation of the two combination combinators.
///
/// `indices` holds the pool positions currently selected. Emitting is simply
/// reading them; advancing finds the rightmost slot that still has room and
/// refills the tail with the smallest legal positions, which is exactly the
/// lexicographic successor of the current selection. Without replacement a
/// slot `i` stops at `n - r + i` (it must leave room for the slots after it),
/// with replacement at `n - 1` (the tail repeats the advanced position).
fn itertools_combinations_impl(args: &[i64], replacement: bool) -> i64 {
    let source = match itertools_collect(args[0]) {
        Ok(source) => source,
        Err(error) => return error,
    };
    let roots = heap::root_values(&[source]);
    let items: Vec<i64> = value::as_list(source)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let n = items.len();
    let r = match value::as_integer(args[1]) {
        Some(r) if r >= 0 => usize::try_from(r).unwrap_or(usize::MAX),
        Some(_) => {
            heap::release_roots(roots);
            return exc::raise_with_message(VALUE_ERROR, "r must be non-negative");
        }
        None => {
            heap::release_roots(roots);
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "integer argument expected, got {}",
                    value::type_name(args[1])
                ),
            );
        }
    };
    let result = value::list_new(Vec::new());
    // Without replacement `r > n` yields nothing; with replacement the empty
    // pool is the only pool that cannot supply `r` positions.
    if (r > n && !replacement) || (n == 0 && r > 0) {
        heap::release_roots(roots);
        return result;
    }
    let result_roots = heap::root_values(&[result]);
    // `r == 0` starts (and ends) with the empty selection below, emitting the
    // single empty tuple CPython yields.
    let mut indices: Vec<usize> = if replacement {
        vec![0; r]
    } else {
        (0..r).collect()
    };
    loop {
        let tuple = value::tuple_new(
            indices
                .iter()
                .map(|index| items[*index])
                .collect::<Vec<i64>>(),
        );
        container::list_append(result, tuple);
        let mut advanced = false;
        for slot in (0..r).rev() {
            if replacement {
                if indices[slot] + 1 < n {
                    indices[slot] += 1;
                    let start = indices[slot];
                    for tail in &mut indices[slot + 1..] {
                        *tail = start;
                    }
                    advanced = true;
                    break;
                }
            } else if indices[slot] < n - r + slot {
                indices[slot] += 1;
                let start = indices[slot] + 1;
                for (offset, tail) in indices[slot + 1..].iter_mut().enumerate() {
                    *tail = start + offset;
                }
                advanced = true;
                break;
            }
        }
        if !advanced {
            break;
        }
    }
    heap::release_roots(result_roots);
    heap::release_roots(roots);
    result
}

/// `product(*iterables, repeat=1)`: the Cartesian product, with the rightmost
/// pool varying fastest (the order `itertools.product` yields).
fn itertools_product(args: &[i64], repeat: i64) -> i64 {
    if repeat < 0 {
        return exc::raise_with_message(VALUE_ERROR, "repeat argument cannot be negative");
    }
    let repeat = usize::try_from(repeat).unwrap_or(usize::MAX);
    let mut sources = Vec::with_capacity(args.len());
    let mut source_roots = Vec::with_capacity(args.len());
    for &argument in args {
        match itertools_collect(argument) {
            Ok(source) => {
                // Each source must stay reachable while the next iterable is
                // materialised (which allocates and may collect).
                source_roots.push(heap::root_values(&[source]));
                sources.push(source);
            }
            Err(error) => {
                for start in source_roots.into_iter().rev() {
                    heap::release_roots(start);
                }
                return error;
            }
        }
    }
    let pools: Vec<Vec<i64>> = sources
        .iter()
        .map(|&source| {
            value::as_list(source)
                .map(|items| items.to_vec())
                .unwrap_or_default()
        })
        .collect();
    let result = value::list_new(Vec::new());
    let result_roots = heap::root_values(&[result]);
    let count = pools.len().saturating_mul(repeat);
    let empty_pool = pools.iter().any(|pool| pool.is_empty());
    if count == 0 {
        // `product()` and `product(..., repeat=0)` both yield one empty tuple.
        let tuple = value::tuple_new(Vec::new());
        container::list_append(result, tuple);
    } else if !empty_pool {
        let mut indices = vec![0usize; count];
        loop {
            let tuple = value::tuple_new(
                indices
                    .iter()
                    .enumerate()
                    .map(|(slot, index)| {
                        let pool = &pools[slot % pools.len()];
                        pool[*index]
                    })
                    .collect(),
            );
            container::list_append(result, tuple);
            let mut slot = count;
            let mut done = false;
            while slot > 0 {
                slot -= 1;
                let length = pools[slot % pools.len()].len();
                indices[slot] += 1;
                if indices[slot] < length {
                    break;
                }
                indices[slot] = 0;
                if slot == 0 {
                    done = true;
                }
            }
            if done {
                break;
            }
        }
    }
    heap::release_roots(result_roots);
    for start in source_roots.into_iter().rev() {
        heap::release_roots(start);
    }
    result
}

/// `takewhile(predicate, iterable)`: the longest prefix the predicate accepts.
fn itertools_takewhile(args: &[i64]) -> i64 {
    let predicate = args[0];
    let iterator = iter::iter_of(args[1]);
    if exc::pending() {
        return NONE;
    }
    let result = value::list_new(Vec::new());
    let roots = heap::root_values(&[result]);
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
        if status == 0 {
            break;
        }
        let keep = crate::builtins::call(predicate, 1, [item].as_ptr());
        if exc::pending() {
            heap::release_roots(roots);
            return keep;
        }
        if !value::truthy(keep) {
            break;
        }
        container::list_append(result, item);
    }
    heap::release_roots(roots);
    result
}

fn dispatch(module: &str, function: &str, args: &[i64]) -> i64 {
    match module {
        "builtins" => builtins::call(function, args),
        "math" => math_call(function, args),
        "functools" | "_functools" => functools_call(function, args),
        "operator" | "_operator" => operator_call(function, args),
        "itertools" => itertools_call(function, args),
        "ast" => ast::call(function, args),
        "fractions" => fractions::call(function, args),
        "importlib" => importlib::call(function, args),
        "importlib.util" => importlib_util::call(function, args),
        "importlib.machinery" => importlib_machinery::call(function, args),
        "importlib.metadata" => importlib_metadata::call(function, args),
        "importlib.resources" => importlib_resources::call(function, args),
        "types" => types::call(function, args),
        "inspect" => inspect::call(function, args),
        "bisect" | "_bisect" => bisect::call(function, args),
        "heapq" | "_heapq" => heapq::call(function, args),
        "colorsys" => colorsys::call(function, args),
        "keyword" => keyword::call(function, args),
        "stat" | "_stat" => stat::call(function, args),
        "time" => time_call(function, args),
        "_thread" => thread_call(function, args),
        "rich.status" => rich_status_call(function, args),
        "threading" => threading_call(function, args),
        "queue" => queue_call(function, args),
        "multiprocessing" => multiprocessing_call(function, args),
        "pickle" => pickle_call(function, args),
        "html" => html_call(function, args),
        "json" => json_call(function, args),
        "csv" => csv_call(function, args),
        "io" | "_io" => io_call(function, args),
        "os" => os_call(function, args),
        "sys" => sys_call(function, args),
        "os.path" => os_path_call(function, args),
        "random" => random_call(function, args),
        "pathlib" => pathlib_call(function, args),
        "platform" => platform_call(function),
        "statistics" => statistics_call(function, args),
        "argparse" => argparse::call(function, args),
        "_collections" => collections::call(function, args),
        "collections" => collections::call(function, args),
        "re" => re::call(function, args),
        "socket" => socket::call(function, args),
        "select" => select::call(function, args),
        "selectors" => select::selectors_call(function, args),
        "ssl" => socket::ssl_call(function, args),
        "asyncio" => asyncio::call(function, args),
        "unittest" => unittest::call(function, args),
        "pytest" => pytest::call(function, args),
        "binascii" => binascii::call(function, args),
        "base64" => base64::call(function, args),
        "quopri" => quopri::call(function, args),
        "fnmatch" => fnmatch::call(function, args),
        "getopt" => getopt::call(function, args),
        "mimetypes" => mimetypes::call(function, args),
        "mmap" => mmap_mod::call(function, args),
        "zlib" => zlib_mod::call(function, args),
        "subprocess" => subprocess::call(function, args),
        "tempfile" => tempfile::call(function, args),
        "shutil" => shutil::call(function, args),
        "textwrap" => textwrap::call(function, args),
        "timeit" => timeit::call(function, args),
        "datetime" => datetime::call(function, args),
        "dataclasses" => dataclasses_call(function, args),
        "contextlib" => contextlib_call(function, args),
        "typing" => typing::call(function, args),
        "abc" => abc::call(function, args),
        "_opcode" => opcode::call(function, args),
        "enum" => r#enum::call(function, args),
        "_imp" => imp::call(function, args),
        "_codecs" => codecs::call(function, args),
        "codecs" => codecs::call(function, args),
        "_string" => string::call(function, args),
        "_weakref" | "weakref" => weakref::call(function, args),
        "warnings" | "_warnings" => warnings::call(function, args),
        "marshal" => marshal::call(function, args),
        "_tokenize" => tokenize::call(function, args),
        "token" => tokenize::token_call(function, args),
        "_collections_abc" | "collections.abc" => collections_abc::call(function, args),
        "gc" => gc::call(function, args),
        "atexit" => atexit::call(function, args),
        "site" => site::call(function, args),
        "ctypes" | "_ctypes" => ctypes::call(function, args),
        "locale" => locale::call(function, args),
        "sysconfig" => sysconfig::call(function, args),
        "zoneinfo" => zoneinfo::call(function, args),
        "decimal" | "_decimal" => decimal::call(function, args),
        "_struct" | "struct" => struct_mod::call(function, args),
        "unicodedata" => unicodedata::call(function, args),
        "_contextvars" | "contextvars" => contextvars::call(function, args),
        _ => missing_attribute(module, function),
    }
}

// Runtime module registry
//
// `sys.modules` and `sys.path` are ordinary Hydro values, but repeated reads
// must return the same object: libraries compare identities, and
// `sys.modules[name] is imported_module` is observable. Both are cached in the
// globals side store under the internal registry pseudo-module, so they are
// traced while alive and `globals::reset()` drops them together with every
// user module during `hydro_runtime_init`. No new ABI symbol is involved.

/// Registry keys in the globals side store.
const SYS_MODULES_CACHE: &str = "sys.modules";
const SYS_PATH_CACHE: &str = "sys.path";
/// `sys.path_importer_cache`: a real dict, cached for stable identity.
const SYS_PATH_IMPORTER_CACHE: &str = "sys.path_importer_cache";

/// `sys.modules`: native modules from [`stdlib::MODULES`] plus every compiled
/// user module discovered in the globals store, in deterministic order.
///
/// The returned dict is cached for stable identity. Each read adds user
/// modules that stored their first global after the previous read, so a
/// module body executed later still appears without rebuilding the dict.
fn sys_modules() -> i64 {
    if let Some(cached) = globals::registry_get(SYS_MODULES_CACHE) {
        sync_user_modules(cached);
        return cached;
    }
    // Publish the empty dict before filling it: the registry entry is a root,
    // so every key and module value stays reachable across allocations.
    let modules = value::dict_new(Vec::new());
    globals::registry_set(SYS_MODULES_CACHE, modules);
    let mut seen: HashSet<&'static str> = HashSet::new();
    for spec in stdlib::MODULES {
        if !seen.insert(spec.name) {
            continue;
        }
        let key = value::str_alloc(spec.name.to_string());
        let roots = crate::heap::root_values(&[key]);
        let module = value::module_value(spec.name.to_string());
        container::dict_set(modules, key, module);
        crate::heap::release_roots(roots);
    }
    sync_user_modules(modules);
    modules
}

/// Inserts user modules discovered in the globals store into `modules`.
///
/// Native names win because they were inserted first; `module_names` is
/// sorted, so new user entries are appended deterministically. Once a module
/// is registered it stays for the rest of the run (like CPython's
/// `sys.modules`); only [`globals::reset`] clears the registry.
fn sync_user_modules(modules: i64) {
    for name in globals::module_names() {
        if registry_contains(modules, &name) {
            continue;
        }
        let key = value::str_alloc(name.clone());
        let roots = crate::heap::root_values(&[key]);
        let module = value::module_value(name);
        container::dict_set(modules, key, module);
        crate::heap::release_roots(roots);
    }
}

/// Whether the registry dict already maps `name`, without rebuilding it.
fn registry_contains(modules: i64, name: &str) -> bool {
    value::as_dict(modules).is_some_and(|entries| {
        entries
            .iter()
            .any(|(key, _)| value::as_str(*key) == Some(name))
    })
}

/// Looks up a loaded module by name in the runtime registry.
///
/// Native modules and user modules discovered in the globals store both live
/// in `sys.modules`, so importers and attribute access share one identity.
pub(crate) fn registry_lookup(name: &str) -> Option<i64> {
    let modules = sys_modules();
    value::as_dict(modules)?
        .iter()
        .find_map(|(key, value)| (value::as_str(*key) == Some(name)).then_some(*value))
}

/// Module value of the nested `collections.abc` module, shared by the
/// `collections.abc` and `collections.abc.abc` constant spellings so both
/// resolve to the exact `sys.modules` entry.
fn collections_abc_module() -> i64 {
    registry_lookup("collections.abc")
        .unwrap_or_else(|| value::module_value("collections.abc".to_string()))
}

/// `sys.path`: the documented search path, as a cached `list[str]`.
///
/// Mutating the returned list is observation-only: no runtime importer
/// consults it yet, so writes do not change module resolution.
fn sys_path() -> i64 {
    if let Some(cached) = globals::registry_get(SYS_PATH_CACHE) {
        return cached;
    }
    // Publish the empty list before appending so the cache entry roots the
    // elements allocated below.
    let path = value::list_new(Vec::new());
    globals::registry_set(SYS_PATH_CACHE, path);
    for dir in search_path() {
        let item = value::str_alloc(dir);
        container::list_append(path, item);
    }
    path
}

/// `sys.path_importer_cache`: the process-wide finder cache.
///
/// CPython populates this dict with the finder for every `sys.path` entry; this
/// runtime has no per-entry finder objects, so native modules and path entries
/// stay absent and user code reading the mapping sees an empty (but real and
/// mutable) dict. The dict is cached in the globals side store, so repeated
/// reads return the same object until `globals::reset()` clears the run.
fn sys_path_importer_cache() -> i64 {
    if let Some(cached) = globals::registry_get(SYS_PATH_IMPORTER_CACHE) {
        return cached;
    }
    let cache = value::dict_new(Vec::new());
    globals::registry_set(SYS_PATH_IMPORTER_CACHE, cache);
    cache
}

/// Builds `sys.path` from the environment.
///
/// Order: `HYDRO_SITE_PACKAGES`, the site-packages directories of
/// `VIRTUAL_ENV` then `CONDA_PREFIX`, `PYTHONPATH`, and finally
/// `HYDRO_STDLIB`. Empty, missing and non-directory entries are dropped and
/// duplicates are kept once, so the result is deterministic for a given
/// environment.
pub(crate) fn search_path() -> Vec<String> {
    search_path_from(
        std::env::var_os("HYDRO_SITE_PACKAGES").as_deref(),
        std::env::var_os("PYTHONPATH").as_deref(),
        std::env::var_os("VIRTUAL_ENV").as_deref(),
        std::env::var_os("CONDA_PREFIX").as_deref(),
        std::env::var_os("HYDRO_STDLIB").as_deref(),
    )
}

/// [`search_path`] with explicit environment values, for tests.
fn search_path_from(
    hydro_site_packages: Option<&OsStr>,
    pythonpath: Option<&OsStr>,
    virtual_env: Option<&OsStr>,
    conda_prefix: Option<&OsStr>,
    hydro_stdlib: Option<&OsStr>,
) -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(list) = hydro_site_packages {
        dirs.extend(std::env::split_paths(list));
    }
    for prefix in [virtual_env, conda_prefix].into_iter().flatten() {
        if !prefix.is_empty() {
            dirs.extend(prefix_site_packages(Path::new(prefix)));
        }
    }
    if let Some(list) = pythonpath {
        dirs.extend(std::env::split_paths(list));
    }
    if let Some(stdlib) = hydro_stdlib {
        dirs.push(PathBuf::from(stdlib));
    }
    let mut seen = HashSet::new();
    dirs.into_iter()
        .filter(|dir| {
            if dir.as_os_str().is_empty() || !dir.is_dir() {
                return false;
            }
            let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
            seen.insert(key)
        })
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect()
}

/// Site-packages directories of an environment prefix, in deterministic
/// order: the Windows `<prefix>/Lib/site-packages` layout first, then every
/// `<prefix>/lib/python*/site-packages` sorted lexicographically.
fn prefix_site_packages(prefix: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let windows = prefix.join("Lib").join("site-packages");
    if windows.is_dir() {
        dirs.push(windows);
    }
    if let Ok(entries) = std::fs::read_dir(prefix.join("lib")) {
        let mut posix: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("python"))
            .map(|entry| entry.path().join("site-packages"))
            .filter(|dir| dir.is_dir())
            .collect();
        posix.sort();
        dirs.extend(posix);
    }
    dirs
}

/// `sys.base_prefix`: the Hydro installation prefix.
///
/// `HYDRO_STDLIB` names the bundled stdlib directory, so its parent is the
/// installation root; without it the executable's directory is used. The
/// result is always a string (possibly empty) rather than a failure.
pub(crate) fn base_prefix() -> String {
    let from_stdlib = std::env::var_os("HYDRO_STDLIB")
        .as_deref()
        .map(Path::new)
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    let prefix = from_stdlib.or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
    });
    prefix
        .map(|prefix| prefix.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `sys.prefix`: the active virtual environment, else the installation
/// prefix.
pub(crate) fn installation_prefix() -> String {
    for variable in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        if let Some(prefix) = std::env::var_os(variable) {
            if !prefix.is_empty() {
                return PathBuf::from(prefix).to_string_lossy().into_owned();
            }
        }
    }
    base_prefix()
}

/// `sys.builtin_module_names`: the native module names, in table order.
fn builtin_module_names() -> i64 {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    // Root each name until the tuple owns it: allocating the next name may
    // trigger a collection.
    let mut roots = Vec::new();
    for spec in stdlib::MODULES {
        if !seen.insert(spec.name) {
            continue;
        }
        let name = value::str_alloc(spec.name.to_string());
        roots.push(crate::heap::root_values(&[name]));
        names.push(name);
    }
    let names = value::tuple_new(names);
    for start in roots.into_iter().rev() {
        crate::heap::release_roots(start);
    }
    names
}

// Constants

fn constant_value(module: &str, name: &str) -> i64 {
    match (module, name) {
        ("__future__", name) => future::constant(name),
        ("token", name) => match tokenize::token_constant(name) {
            Some(constant) => constant,
            None => missing_attribute("token", name),
        },
        // The real CPython byte-order marks. `BOM_LE`/`BOM_BE` are the UTF-16
        // marks and `BOM` follows the target endianness, like `Lib/codecs.py`.
        ("codecs", "BOM_UTF8") => value::bytes_new(vec![0xEF, 0xBB, 0xBF]),
        ("codecs", "BOM_UTF16_LE") | ("codecs", "BOM_LE") => value::bytes_new(vec![0xFF, 0xFE]),
        ("codecs", "BOM_UTF16_BE") | ("codecs", "BOM_BE") => value::bytes_new(vec![0xFE, 0xFF]),
        ("codecs", "BOM_UTF32_LE") => value::bytes_new(vec![0xFF, 0xFE, 0x00, 0x00]),
        ("codecs", "BOM_UTF32_BE") => value::bytes_new(vec![0x00, 0x00, 0xFE, 0xFF]),
        ("codecs", "BOM") => {
            if cfg!(target_endian = "little") {
                value::bytes_new(vec![0xFF, 0xFE])
            } else {
                value::bytes_new(vec![0xFE, 0xFF])
            }
        }
        ("types", name) => types::constant(name),
        ("functools" | "_functools", name) => functools::constant(name),
        ("inspect", name) => inspect::constant(name),
        ("math", "pi") => value::float_new(std::f64::consts::PI),
        ("math", "e") => value::float_new(std::f64::consts::E),
        ("math", "tau") => value::float_new(std::f64::consts::TAU),
        ("math", "inf") => value::float_new(f64::INFINITY),
        ("math", "nan") => value::float_new(f64::NAN),
        ("sys", "argv") => argv_list(),
        // Runtime module registry (identity-stable for repeated reads).
        ("sys", "path") => sys_path(),
        ("sys", "path_importer_cache") => sys_path_importer_cache(),
        ("sys", "modules") => sys_modules(),
        ("sys", "builtin_module_names") => builtin_module_names(),
        // `prefix` follows the active virtual environment; `base_prefix` and
        // `exec_prefix` describe the installation itself (they coincide here,
        // as they do in an unmodified CPython installation).
        ("sys", "prefix") => value::str_alloc(installation_prefix()),
        ("sys", "base_prefix") => value::str_alloc(base_prefix()),
        ("sys", "exec_prefix") => value::str_alloc(base_prefix()),
        ("sys", "executable") => value::str_alloc(
            std::env::current_exe()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        ("sys", "platform") => value::str_from_bytes(platform().as_bytes()),
        // Target endianness, as CPython reports it (the compiler and runtime
        // always share one target).
        ("sys", "byteorder") => value::str_from_bytes(if cfg!(target_endian = "little") {
            b"little"
        } else {
            b"big"
        }),
        ("sys", "version") => value::str_alloc(format!(
            "{PYTHON_VERSION_STRING} (Hydro {})",
            env!("CARGO_PKG_VERSION")
        )),
        ("sys", "version_info") => value::tuple_new(vec![
            encode_int(i64::from(PYTHON_VERSION.0)),
            encode_int(i64::from(PYTHON_VERSION.1)),
            encode_int(i64::from(PYTHON_VERSION.2)),
            value::str_from_bytes(b"final"),
            encode_int(0),
        ]),
        ("sys", "hydro_version") => value::str_from_bytes(env!("CARGO_PKG_VERSION").as_bytes()),
        ("sys", "hydro_version_info") => value::tuple_new(vec![
            encode_int(0),
            encode_int(1),
            encode_int(0),
            value::str_from_bytes(b"dev"),
            encode_int(0),
        ]),
        // `i64::MAX` does not fit into the 61-bit tagged integer; the
        // documented value is `2^60 - 1` (`value::MAX_INT`).
        ("sys", "maxsize") => encode_int(value::MAX_INT),
        // Last uncaught exception (`sys.last_exc`/`last_type`/`last_value`/
        // `last_traceback`). CPython sets these only while an interactive
        // interpreter reports an exception; Hydro compiles and runs whole
        // programs, so no value outlives an uncaught exception and the names
        // are `None` instead of a fabricated exception. `dis` reads
        // `last_exc` behind `hasattr` and follows the same "no last
        // traceback" path it takes in CPython script mode.
        ("sys", "last_exc" | "last_type" | "last_value" | "last_traceback") => NONE,
        // Fresh native streams over the standard descriptors on every
        // resolution; `__std*__` are the original streams, which this runtime
        // never rebinds, so both spellings resolve alike. They hold no Hydro
        // children, so no rooting is required.
        ("sys", "stdin") | ("sys", "__stdin__") => value::std_stream_new(0),
        ("sys", "stdout") | ("sys", "__stdout__") => value::std_stream_new(1),
        ("sys", "stderr") | ("sys", "__stderr__") => value::std_stream_new(2),
        ("os", "name") => {
            let name = if cfg!(windows) { "nt" } else { "posix" };
            value::str_from_bytes(name.as_bytes())
        }
        ("os", "sep") => {
            let sep = if cfg!(windows) { "\\" } else { "/" };
            value::str_from_bytes(sep.as_bytes())
        }
        ("os", "environ") => environ_dict(),
        // `os.PathLike` is the abstract base of path objects; Hydro path values
        // are plain strings (`pathlib.Path` included), so the class carries no
        // members and exists for attribute/import compatibility.
        ("io" | "_io", name @ ("IOBase" | "TextIOBase" | "BufferedIOBase")) => {
            value::class_new(name.to_string(), NONE, Vec::new())
        }
        ("os", "PathLike") => value::class_new("PathLike".to_string(), NONE, Vec::new()),
        ("os", "devnull") => {
            let devnull = if cfg!(windows) { "nul" } else { "/dev/null" };
            value::str_from_bytes(devnull.as_bytes())
        }
        ("os", "F_OK") => encode_int(libc::F_OK as i64),
        ("os", "X_OK") => encode_int(libc::X_OK as i64),
        ("os", "W_OK") => encode_int(libc::W_OK as i64),
        ("os", "R_OK") => encode_int(libc::R_OK as i64),
        // `file_control` flags for `os.open`: the libc bit values (identical
        // on every supported Unix; Windows has no `os.open` here).
        ("os", "O_RDONLY") => encode_int(libc::O_RDONLY as i64),
        ("os", "O_WRONLY") => encode_int(libc::O_WRONLY as i64),
        ("os", "O_RDWR") => encode_int(libc::O_RDWR as i64),
        ("os", "O_CREAT") => encode_int(libc::O_CREAT as i64),
        ("os", "O_TRUNC") => encode_int(libc::O_TRUNC as i64),
        ("os", "O_APPEND") => encode_int(libc::O_APPEND as i64),
        ("os", "O_CLOEXEC") => encode_int(libc::O_CLOEXEC as i64),
        // `threading.local` is an empty base class: it exists so thread-local
        // subclasses (`class X(threading.local)`) and `local()` calls work.
        // Hydro stores attributes per instance, not per thread; the documented
        // limitation is that concurrent instances share nothing, which is the
        // usual pattern for console/render state.
        ("threading", "local") => value::class_new("local".to_string(), NONE, Vec::new()),
        // Help formatters are marker classes; `ArgumentParser` reads the
        // class name to select layout details. `SUPPRESS` is CPython's
        // sentinel string.
        (
            "argparse",
            name @ ("RawTextHelpFormatter"
            | "RawDescriptionHelpFormatter"
            | "ArgumentDefaultsHelpFormatter"
            | "MetavarTypeHelpFormatter"
            | "HelpFormatter"
            | "Action"
            | "BooleanOptionalAction"
            | "SUPPRESS"),
        ) => argparse::constant(name),
        ("asyncio", "CancelledError") => exc::exception_class(exc::CANCELLED_ERROR),
        ("asyncio.coroutines", "_is_coroutine") => asyncio::coroutines_constant("_is_coroutine")
            .unwrap_or_else(|| missing_attribute("asyncio.coroutines", "_is_coroutine")),
        ("socket", name) => socket::constant(name),
        ("select", name) => select::constant(name),
        ("selectors", name) => select::selectors_constant(name),
        ("ssl", name) => socket::ssl_constant(name),
        ("subprocess", "PIPE") => encode_int(subprocess::PIPE),
        ("subprocess", "STDOUT") => encode_int(subprocess::STDOUT),
        ("subprocess", "DEVNULL") => encode_int(subprocess::DEVNULL),
        ("keyword", "kwlist") => keyword::kwlist(),
        ("keyword", "softkwlist") => keyword::softkwlist(),
        ("stat" | "_stat", name) => stat::constant(name),
        ("re", name) => re::constant(name),
        ("mimetypes", name) => mimetypes::constant(name),
        ("mmap", name) => mmap_mod::constant(name),
        ("zlib", name) => zlib_mod::constant(name),
        ("datetime", name) => datetime::constant(name),
        ("typing", name) => typing::constant(name),
        ("abc", name) => abc::constant(name),
        ("enum", name) => r#enum::constant(name),
        ("builtins", name) => builtins::constant(name),
        ("_opcode", name) => opcode::constant(name),
        ("_weakref" | "weakref", name) => weakref::constant(name),
        ("warnings" | "_warnings", name) => warnings::constant(name),
        ("marshal", name) => marshal::constant(name),
        // `importlib`'s nested modules are real native modules; expose the
        // exact `sys.modules` entry so identity holds.
        ("importlib", "util" | "machinery" | "metadata" | "resources") => {
            registry_lookup(&format!("importlib.{name}"))
                .unwrap_or_else(|| value::module_value(format!("importlib.{name}")))
        }
        ("importlib.util", name) => importlib_util::constant(name),
        ("importlib.machinery", name) => importlib_machinery::constant(name),
        ("importlib.metadata", name) => importlib_metadata::constant(name),
        ("importlib.resources", name) => importlib_resources::constant(name),
        // `collections.abc` is a nested module. These constants expose the
        // module value so `collections.abc.Mapping` resolves through the
        // runtime module (the frontend does not model general nested-module
        // chains yet). `import collections` reaches the first spelling and
        // `import collections.abc` the second; the specific arm must precede
        // the module-wide constant table below.
        ("collections" | "collections.abc", "abc") => collections_abc_module(),
        ("_collections_abc" | "collections.abc", name) => collections_abc::constant(name),
        ("site", name) => site::constant(name),
        ("ctypes" | "_ctypes", name) => ctypes::constant(name),
        ("locale", name) => locale::constant(name),
        ("sysconfig", name) => sysconfig::constant(name),
        ("zoneinfo", name) => zoneinfo::constant(name),
        ("decimal" | "_decimal", name) => decimal::constant(name),
        ("_struct" | "struct", "error") => struct_mod::error_value(),
        ("unicodedata", "unidata_version") => {
            value::str_alloc(unicodedata::UNIDATA_VERSION.to_string())
        }
        ("_contextvars" | "contextvars", name) => contextvars::constant(name),
        // The `@dataclass` decorator is expanded by the frontend; the runtime
        // value only has to exist for the import.
        ("dataclasses", "dataclass") => NONE,
        _ => missing_attribute(module, name),
    }
}

/// `contextlib.nullcontext(enter_result=None)`: CPython's no-op context
/// manager as a native class instance. `__enter__` returns `enter_result`
/// (read back from the instance, so the same manager can be reused) and
/// `__exit__` returns `False`, which keeps exceptions propagating.
///
/// `contextlib.suppress` returns a native context manager too. The subset does
/// not suppress exceptions; its `__exit__` returns `None` and the normal block
/// path is supported.
///
/// `contextlib.contextmanager(func)` returns a `functools.partial`-shaped
/// callable that binds `func` to `_contextmanager_call`. Calling it invokes
/// `func` with the call arguments (keywords included) and wraps the resulting
/// generator in a `_GeneratorContextManager`. `__enter__` advances the
/// generator once; `__exit__` throws the block's exception into it (or calls
/// `next` on the normal path) and reports whether the exception was
/// suppressed, following CPython's `_GeneratorContextManager` rules.
fn contextlib_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "suppress" => {
            let methods = vec![
                (
                    "__enter__".to_string(),
                    value::native_function_new("contextlib", "_suppress_enter", 1),
                ),
                (
                    "__exit__".to_string(),
                    value::native_function_new("contextlib", "_suppress_exit", 4),
                ),
            ];
            let class = value::class_new("suppress".to_string(), NONE, methods);
            let _ = args;
            value::instance_new(class)
        }
        "_suppress_enter" | "_suppress_exit" => NONE,
        "nullcontext" => {
            let methods = vec![
                (
                    "__enter__".to_string(),
                    value::native_function_new("contextlib", "_nullcontext_enter", 1),
                ),
                (
                    "__exit__".to_string(),
                    value::native_function_new("contextlib", "_nullcontext_exit", 4),
                ),
            ];
            let class = value::class_new("nullcontext".to_string(), NONE, methods);
            let instance = value::instance_new(class);
            let enter_result = args.first().copied().unwrap_or(NONE);
            value::instance_setattr(instance, "enter_result".to_string(), enter_result);
            instance
        }
        "_nullcontext_enter" => value::instance_attr(args[0], "enter_result").unwrap_or(NONE),
        // Returning false keeps the exception raised by the block.
        "_nullcontext_exit" => value::bool_value(false),
        // The decorator keeps `func` as the partial's bound argument; the
        // wrapper value does not need its own class.
        "contextmanager" => {
            let Some(&function) = args.first() else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "contextlib.contextmanager() missing required argument 'func'",
                );
            };
            let roots = heap::root_values(&[function]);
            let wrapper =
                value::native_function_new("contextlib", "_contextmanager_call", usize::MAX);
            let wrapper_roots = heap::root_values(&[wrapper]);
            let result = value::partial_new(wrapper, vec![function]);
            heap::release_roots(wrapper_roots);
            heap::release_roots(roots);
            result
        }
        "_contextmanager_call" => match args.split_first() {
            Some((&function, rest)) => contextmanager_wrap(function, rest, &[]),
            None => exc::raise_with_message(
                TYPE_ERROR,
                "contextlib._contextmanager_call() missing the decorated function",
            ),
        },
        "_contextmanager_enter" => contextmanager_enter(args[0]),
        "_contextmanager_exit" => contextmanager_exit(args[0], args[1], args[2], args[3]),
        _ => missing_attribute("contextlib", function),
    }
}

/// Keyword-aware `contextlib` dispatch: `_contextmanager_call` forwards the
/// call-site keywords to the decorated generator function.
fn contextlib_call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        "_contextmanager_call" => match args.split_first() {
            Some((&decorated, rest)) => contextmanager_wrap(decorated, rest, keywords),
            None => exc::raise_with_message(
                TYPE_ERROR,
                "contextlib._contextmanager_call() missing the decorated function",
            ),
        },
        _ => exc::raise_with_message(
            TYPE_ERROR,
            &format!("contextlib.{function}() does not accept keyword arguments"),
        ),
    }
}

/// Calls the decorated generator function and wraps the resulting generator.
///
/// The call may raise (a normal function used as a context manager can fail
/// before returning), in which case the exception propagates unchanged. A
/// non-generator result is wrapped anyway; `__enter__` reports the `TypeError`
/// at the point CPython does.
fn contextmanager_wrap(function: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut rooted: Vec<i64> = Vec::with_capacity(args.len() + keywords.len() + 1);
    rooted.push(function);
    rooted.extend_from_slice(args);
    rooted.extend(keywords.iter().map(|(_, value)| *value));
    let roots = heap::root_values(&rooted);
    let generator = if keywords.is_empty() {
        crate::core::builtins::call(function, args.len(), args.as_ptr())
    } else {
        let mut pairs: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
        for (name, value) in keywords {
            pairs.push(value::str_alloc(name.clone()));
            pairs.push(*value);
        }
        crate::core::builtins::call_kw(
            function,
            args.len(),
            args.as_ptr(),
            keywords.len(),
            pairs.as_ptr(),
        )
    };
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let generator_roots = heap::root_values(&[generator]);
    let class = context_manager_class();
    let manager = value::instance_new(class);
    value::instance_setattr(manager, "gen".to_string(), generator);
    heap::release_roots(generator_roots);
    heap::release_roots(roots);
    manager
}

/// The cached `_GeneratorContextManager` class returned by the
/// `contextmanager` decorator.
fn context_manager_class() -> i64 {
    const KEY: &str = "contextlib._GeneratorContextManager.class";
    if let Some(cached) = crate::core::globals::registry_get(KEY) {
        return cached;
    }
    let enter = value::native_function_new("contextlib", "_contextmanager_enter", 1);
    let exit = value::native_function_new("contextlib", "_contextmanager_exit", 4);
    let methods = vec![
        ("__enter__".to_string(), enter),
        ("__exit__".to_string(), exit),
    ];
    let method_roots = heap::root_values(&[enter, exit]);
    let class = value::class_new("_GeneratorContextManager".to_string(), NONE, methods);
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    crate::core::globals::registry_set(KEY, class);
    heap::release_roots(class_roots);
    heap::release_roots(method_roots);
    class
}

/// The generator bound to a `_GeneratorContextManager`, or a `TypeError` when
/// the decorated function returned something else.
fn contextmanager_generator(manager: i64) -> Result<i64, i64> {
    let generator = value::instance_attr(manager, "gen").unwrap_or(NONE);
    if crate::fiber::kind(generator) == Some(crate::fiber::FiberKind::Generator) {
        Ok(generator)
    } else {
        Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "'{}' object is not a generator",
                value::type_name(generator)
            ),
        ))
    }
}

/// `_GeneratorContextManager.__enter__`: resume the generator once.
///
/// A generator that stops before yielding reports CPython's
/// `RuntimeError("generator didn't yield")`; an exception raised by the body
/// propagates unchanged.
fn contextmanager_enter(manager: i64) -> i64 {
    let generator = match contextmanager_generator(manager) {
        Ok(generator) => generator,
        Err(err) => return err,
    };
    if crate::fiber::is_done(generator) {
        if crate::fiber::has_error(generator) {
            return crate::fiber::raise_error(generator);
        }
        return exc::raise_with_message(RUNTIME_ERROR, "generator didn't yield");
    }
    match crate::fiber::resume_value(generator) {
        Some(true) => {
            if crate::fiber::has_error(generator) {
                crate::fiber::raise_error(generator)
            } else {
                exc::raise_with_message(RUNTIME_ERROR, "generator didn't yield")
            }
        }
        Some(false) => crate::fiber::yielded_value(generator),
        None => exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "'{}' object is not a generator",
                value::type_name(generator)
            ),
        ),
    }
}

/// `_GeneratorContextManager.__exit__(exc_type, exc, tb)`.
///
/// Without an exception, resume the generator once: finishing normally returns
/// `False` (no suppression) and a second yield reports
/// `RuntimeError("generator didn't stop")`. With an exception, throw it into
/// the generator: a generator that stops after handling it returns `True`
/// (suppress), one that lets the same exception escape returns `False`, and
/// one that yields again reports `RuntimeError("generator didn't stop after
/// throw()")`. A different escaping exception propagates unchanged. The
/// error paths close the generator so its `finally` blocks run.
fn contextmanager_exit(manager: i64, exc_type: i64, exception: i64, traceback: i64) -> i64 {
    let _ = (exc_type, traceback);
    let generator = match contextmanager_generator(manager) {
        Ok(generator) => generator,
        Err(err) => return err,
    };
    if exception == NONE {
        if crate::fiber::is_done(generator) {
            if crate::fiber::has_error(generator) {
                return crate::fiber::raise_error(generator);
            }
            return FALSE;
        }
        return match crate::fiber::resume_value(generator) {
            Some(true) => {
                if crate::fiber::has_error(generator) {
                    crate::fiber::raise_error(generator);
                    NONE
                } else {
                    FALSE
                }
            }
            Some(false) => {
                contextmanager_close(generator);
                exc::raise_with_message(RUNTIME_ERROR, "generator didn't stop")
            }
            None => exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "'{}' object is not a generator",
                    value::type_name(generator)
                ),
            ),
        };
    }
    // Throwing into a finished generator re-raises the exception in CPython,
    // which makes `__exit__` report it as unsuppressed.
    if crate::fiber::is_done(generator) {
        return FALSE;
    }
    let _ = crate::fiber::throw_value(generator, exception);
    if exc::pending() {
        let pending = exc::pending_value();
        if same_exception(pending, exception) {
            let _ = exc::clear();
            return FALSE;
        }
        if exc::pending_tag() == STOP_ITERATION as i64 {
            // The generator handled the exception and returned.
            let _ = exc::clear();
            return TRUE;
        }
        // A different exception escapes the generator; keep it pending.
        return NONE;
    }
    not_stopping_after_throw(generator)
}

/// Reports `RuntimeError("generator didn't stop after throw()")` for a
/// generator that yielded again after the thrown exception, closing it first
/// so its cleanup runs.
fn not_stopping_after_throw(generator: i64) -> i64 {
    contextmanager_close(generator);
    exc::raise_with_message(RUNTIME_ERROR, "generator didn't stop after throw()")
}

/// Closes a generator, discarding the `close()` outcome so the caller's own
/// error is the one reported.
fn contextmanager_close(generator: i64) {
    if crate::fiber::is_done(generator) {
        return;
    }
    let _ = crate::fiber::close_value(generator);
    if exc::pending() {
        let _ = exc::clear();
    }
}

/// Whether the exception a generator let escape is the one thrown into it.
///
/// Re-raising copies the payload, so pointer identity alone is not enough;
/// the payload comparison recovers the CPython `exc is value` decision for an
/// exception that propagated unchanged. `StopIteration` never compares equal
/// by payload, because that tag is how a handler that stops is reported.
fn same_exception(a: i64, b: i64) -> bool {
    if a == b {
        return true;
    }
    let (Some(left), Some(right)) = (value::exception_data(a), value::exception_data(b)) else {
        return false;
    };
    left.tag != STOP_ITERATION
        && left.tag == right.tag
        && left.msg == right.msg
        && left.args == right.args
}

/// `dataclasses` functions. The frontend reads `field(...)` from the class-body
/// AST to synthesize `__init__`; executing the class body still evaluates the
/// call, so it returns a marker namespace carrying both settings.
///
/// `replace(obj, **changes)` copies the instance; the positional form binds no
/// changes, the keyword form goes through [`dataclasses_call_kw`].
fn dataclasses_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "field" => {
            // Omitted slots arrive as `None` (the frontend fills them).
            let default = args.first().copied().unwrap_or(NONE);
            let default_factory = args.get(1).copied().unwrap_or(NONE);
            value::namespace_new(vec![
                ("default".to_string(), default),
                ("default_factory".to_string(), default_factory),
            ])
        }
        "replace" => dataclasses_replace(args[0], Vec::new()),
        _ => missing_attribute("dataclasses", function),
    }
}

/// Keyword-aware `dataclasses` dispatch: only `replace` binds names.
fn dataclasses_call_kw(function: &str, args: &[i64], keywords: Vec<(String, i64)>) -> i64 {
    match function {
        "replace" => dataclasses_replace(args[0], keywords),
        _ => exc::raise_with_message(
            TYPE_ERROR,
            &format!("dataclasses.{function}() does not accept keyword arguments"),
        ),
    }
}

/// `dataclasses.replace(obj, **changes)`: shallow copy of an instance's
/// attribute table with the named slots replaced or added.
fn dataclasses_replace(instance: i64, changes: Vec<(String, i64)>) -> i64 {
    match value::instance_replace(instance, changes) {
        Some(copy) => copy,
        None => exc::raise_with_message(
            TYPE_ERROR,
            "replace() should be called on dataclass instances",
        ),
    }
}

/// Fresh list for `sys.argv`; each string is appended through the list so a
/// collection during a later allocation cannot free an earlier entry.
fn argv_list() -> i64 {
    let values = crate::argv();
    let list = value::list_new(Vec::with_capacity(values.len()));
    for argument in values {
        let item = value::str_alloc(argument);
        container::list_append(list, item);
    }
    list
}

/// Fresh dict for `os.environ`. Keys and values are inserted as they are
/// allocated; lossy conversion keeps non-UTF-8 entries from panicking.
fn environ_dict() -> i64 {
    let dict = value::dict_new(Vec::new());
    for (name, contents) in std::env::vars_os() {
        let key = value::str_alloc(name.to_string_lossy().into_owned());
        let item = value::str_alloc(contents.to_string_lossy().into_owned());
        if let Some(mut entries) = value::dict_pairs_mut(dict) {
            entries.set(key, item);
        }
        crate::heap::write_barrier_edge(dict, key);
        crate::heap::write_barrier_edge(dict, item);
    }
    dict
}

fn platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    }
}

// random

/// Global generator behind the module-level `random` functions. Seeded from
/// the environment on first use.
static GLOBAL_RANDOM: LazyLock<value::RandomState> = LazyLock::new(value::RandomState::new);

fn random_type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Converts a `random` seed (`int`, `float` or `None`) to 64 bits.
fn seed_u64(v: i64) -> Result<u64, i64> {
    if let Some(integer) = value::as_integer(v) {
        return Ok(integer as u64);
    }
    if let Some(float) = value::as_float(v) {
        return Ok(float.to_bits());
    }
    Err(random_type_error(
        "The only supported seed types are: int, float, None",
    ))
}

/// Integer argument of `randrange`/`randint`; non-integers report a pending
/// `TypeError`.
fn random_int_arg(v: i64) -> Result<i64, i64> {
    value::as_integer(v).ok_or_else(|| {
        random_type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(v)
        ))
    })
}

fn random_empty_range(start: i64, stop: i64, step: i64) -> i64 {
    exc::raise_with_message(
        VALUE_ERROR,
        &format!("empty range for randrange() ({start}, {stop}, {step})"),
    )
}

/// Draws a uniform offset in `0..width` for a width that may equal `2^64`.
fn random_offset(rng: &value::RandomState, width: i128) -> u64 {
    if width > u64::MAX as i128 {
        // The only representable width above `u64::MAX` is `2^64`; every
        // 64-bit draw covers it exactly once.
        rng.next_u64()
    } else {
        rng.below(width as u64)
    }
}

fn random_randint(rng: &value::RandomState, args: &[i64]) -> i64 {
    let low = match random_int_arg(args[0]) {
        Ok(v) => v,
        Err(err) => return err,
    };
    let high = match random_int_arg(args[1]) {
        Ok(v) => v,
        Err(err) => return err,
    };
    if low > high {
        return random_empty_range(low, high, 1);
    }
    let width = high as i128 - low as i128 + 1;
    let offset = random_offset(rng, width) as i128;
    encode_int((low as i128 + offset) as i64)
}

fn random_randrange(rng: &value::RandomState, args: &[i64]) -> i64 {
    let mut values = Vec::with_capacity(args.len());
    for &arg in args {
        match random_int_arg(arg) {
            Ok(v) => values.push(v),
            Err(err) => return err,
        }
    }
    let (start, stop, step) = match values[..] {
        [stop] => (0, stop, 1),
        [start, stop] => (start, stop, 1),
        [start, stop, step] => (start, stop, step),
        _ => return random_type_error("randrange() received an invalid argument count"),
    };
    if step == 0 {
        return exc::raise_with_message(VALUE_ERROR, "randrange() arg 3 must not be zero");
    }
    let start = start as i128;
    let stop = stop as i128;
    let step = step as i128;
    let width = if step > 0 {
        (stop - start + step - 1) / step
    } else {
        (stop - start + step + 1) / step
    };
    if width <= 0 {
        return random_empty_range(start as i64, stop as i64, step as i64);
    }
    let offset = random_offset(rng, width) as i128;
    encode_int((start + step * offset) as i64)
}

fn random_choice(rng: &value::RandomState, sequence: i64) -> i64 {
    if let Some(items) = value::as_list(sequence) {
        if items.is_empty() {
            return exc::raise_with_message(INDEX_ERROR, "Cannot choose from an empty sequence");
        }
        let index = rng.below(items.len() as u64) as usize;
        return items[index];
    }
    if let Some(items) = value::as_tuple(sequence) {
        if items.is_empty() {
            return exc::raise_with_message(INDEX_ERROR, "Cannot choose from an empty sequence");
        }
        let index = rng.below(items.len() as u64) as usize;
        return items[index];
    }
    if let Some(text) = value::as_str(sequence) {
        let count = text.chars().count();
        if count == 0 {
            return exc::raise_with_message(INDEX_ERROR, "Cannot choose from an empty sequence");
        }
        let index = rng.below(count as u64) as usize;
        let picked = text.chars().nth(index).unwrap_or('\0').to_string();
        return value::str_alloc(picked);
    }
    if let Some(bytes) = value::as_bytes(sequence) {
        if bytes.is_empty() {
            return exc::raise_with_message(INDEX_ERROR, "Cannot choose from an empty sequence");
        }
        let index = rng.below(bytes.len() as u64) as usize;
        return encode_int(bytes[index] as i64);
    }
    random_type_error(&format!(
        "choice() argument must be a sequence, not {}",
        value::type_name(sequence)
    ))
}

fn random_shuffle(rng: &value::RandomState, sequence: i64) -> i64 {
    let Some(mut items) = value::list_items_mut(sequence) else {
        return random_type_error(&format!(
            "shuffle() argument must be a list, not {}",
            value::type_name(sequence)
        ));
    };
    let len = items.len();
    for index in (1..len).rev() {
        let other = rng.below(index as u64 + 1) as usize;
        items.swap(index, other);
    }
    NONE
}

fn random_uniform(rng: &value::RandomState, args: &[i64]) -> i64 {
    let low = match number(args[0]) {
        Ok(v) => v,
        Err(err) => return err,
    };
    let high = match number(args[1]) {
        Ok(v) => v,
        Err(err) => return err,
    };
    value::float_new(low + (high - low) * rng.next_f64())
}

/// Mirrors `ops::MAX_BIGINT_BITS` (8 Mibit, ~1 MiB of magnitude) so a random
/// draw cannot allocate integers the rest of the runtime refuses to produce.
const MAX_GETRANDBITS: i64 = 1 << 23;

fn random_getrandbits(rng: &value::RandomState, arg: i64) -> i64 {
    let bits = match random_int_arg(arg) {
        Ok(v) => v,
        Err(err) => return err,
    };
    if bits < 0 {
        return exc::raise_with_message(VALUE_ERROR, "number of bits must be non-negative");
    }
    if bits == 0 {
        return encode_int(0);
    }
    if bits > MAX_GETRANDBITS {
        return exc::raise_with_message(
            OVERFLOW_ERROR,
            "getrandbits() result exceeds the maximum integer size",
        );
    }
    // Draw independent 64-bit SplitMix64 words and assemble little-endian
    // 32-bit limbs; the final limb is masked to the exact requested width.
    let mut digits = Vec::with_capacity((bits as usize).div_ceil(32));
    let mut remaining = bits as usize;
    while remaining > 0 {
        let draw = rng.next_u64();
        let take = remaining.min(64);
        let chunk = if take == 64 {
            draw
        } else {
            draw & ((1u64 << take) - 1)
        };
        digits.push(chunk as u32);
        if take > 32 {
            digits.push((chunk >> 32) as u32);
        }
        remaining -= take;
    }
    value::bigint_new(BigInt::from_slice(Sign::Plus, &digits))
}

/// Shared implementation of the module-level functions and `Random` methods.
/// `seed` without an argument and `seed(None)` reseed from entropy.
fn random_operation(rng: &value::RandomState, function: &str, args: &[i64]) -> i64 {
    match function {
        "seed" => {
            match args.first().copied() {
                None => rng.reset_entropy(),
                Some(seed) if seed == NONE => rng.reset_entropy(),
                Some(seed) => match seed_u64(seed) {
                    Ok(seed) => rng.reset(seed),
                    Err(err) => return err,
                },
            }
            NONE
        }
        "random" => value::float_new(rng.next_f64()),
        "randint" => random_randint(rng, args),
        "randrange" => random_randrange(rng, args),
        "choice" => random_choice(rng, args[0]),
        "uniform" => random_uniform(rng, args),
        "shuffle" => random_shuffle(rng, args[0]),
        "getrandbits" => random_getrandbits(rng, args[0]),
        _ => missing_attribute("random", function),
    }
}

fn random_call(function: &str, args: &[i64]) -> i64 {
    if function == "Random" {
        let seed = match args.first().copied() {
            None => None,
            Some(seed) if seed == NONE => None,
            Some(seed) => match seed_u64(seed) {
                Ok(seed) => Some(seed),
                Err(err) => return err,
            },
        };
        return value::random_new(seed);
    }
    random_operation(&GLOBAL_RANDOM, function, args)
}

/// Arity check for `Random` methods (the module-level functions are checked
/// against the central spec before dispatch).
fn check_random_arity(method: &str, argc: usize, min: usize, max: usize) -> Option<i64> {
    if (min..=max).contains(&argc) {
        return None;
    }
    let detail = if min == max {
        format!("{min}")
    } else if argc < min {
        format!("at least {min}")
    } else {
        format!("at most {max}")
    };
    Some(random_type_error(&format!(
        "{method}() takes {detail} argument(s) ({argc} given)"
    )))
}

/// Dynamic `Random` method dispatch (`rng.seed(...)` etc.).
pub(crate) fn random_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(rng) = value::as_random(obj) else {
        return random_type_error("invalid Random receiver");
    };
    let (min, max) = match method {
        "seed" => (0, 1),
        "random" => (0, 0),
        "randint" => (2, 2),
        "randrange" => (1, 3),
        "choice" => (1, 1),
        "uniform" => (2, 2),
        "shuffle" => (1, 1),
        "getrandbits" => (1, 1),
        _ => {
            return exc::raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("'Random' object has no attribute '{method}'"),
            )
        }
    };
    if let Some(err) = check_random_arity(method, args.len(), min, max) {
        return err;
    }
    random_operation(rng, method, args)
}

// pathlib

/// `pathlib.Path(path=None)`, `pathlib.cwd()` and `pathlib.home()`.
///
/// A keyword call arrives with `None` in every omitted slot (the frontend
/// fills the gaps), so `None` selects the documented default.
fn pathlib_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "Path" => {
            let arg = args.first().copied().unwrap_or(NONE);
            if arg == NONE {
                return value::path_new(".".to_string());
            }
            if let Some(text) = value::as_str(arg) {
                return value::path_new(text.to_string());
            }
            if let Some(state) = value::as_path(arg) {
                return value::path_new(state.path().to_string());
            }
            exc::raise_with_message(
                TYPE_ERROR,
                &format!("Path() argument must be str, not {}", value::type_name(arg)),
            )
        }
        "cwd" => match std::env::current_dir() {
            Ok(path) => value::path_new(path.to_string_lossy().into_owned()),
            Err(error) => exc::raise_with_message(OS_ERROR, &format!("cwd: {error}")),
        },
        // `HOME` may be unset in minimal environments; CPython raises
        // `RuntimeError`, but the documented fallback here is `"."`.
        "home" => {
            let home = std::env::var_os("HOME")
                .map(|path| path.to_string_lossy().into_owned())
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| ".".to_string());
            value::path_new(home)
        }
        _ => missing_attribute("pathlib", function),
    }
}

// platform

/// Reads a kernel pseudo-file, trimming the trailing newline. Missing files
/// yield the empty string (portable fallback).
fn read_trimmed(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

fn platform_system() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        "windows" => "Windows",
        other => other,
    }
}

fn platform_machine() -> &'static str {
    std::env::consts::ARCH
}

fn platform_release() -> String {
    if cfg!(target_os = "linux") {
        read_trimmed("/proc/sys/kernel/osrelease")
    } else {
        String::new()
    }
}

fn platform_version() -> String {
    if cfg!(target_os = "linux") {
        read_trimmed("/proc/sys/kernel/version")
    } else {
        String::new()
    }
}

fn platform_node() -> String {
    if let Ok(node) = std::env::var("HOSTNAME") {
        if !node.is_empty() {
            return node;
        }
    }
    if cfg!(target_os = "linux") {
        let node = read_trimmed("/proc/sys/kernel/hostname");
        if !node.is_empty() {
            return node;
        }
    }
    String::new()
}

fn platform_call(function: &str) -> i64 {
    match function {
        "system" => value::str_from_bytes(platform_system().as_bytes()),
        "machine" => value::str_from_bytes(platform_machine().as_bytes()),
        "release" => value::str_alloc(platform_release()),
        "version" => value::str_alloc(platform_version()),
        "node" => value::str_alloc(platform_node()),
        "platform" => value::str_alloc(format!(
            "{}-{}-{}",
            platform_system(),
            platform_release(),
            platform_machine()
        )),
        "python_version" => value::str_from_bytes(PYTHON_VERSION_STRING.as_bytes()),
        "python_implementation" => value::str_from_bytes(b"Hydro"),
        "processor" => value::str_from_bytes(platform_machine().as_bytes()),
        _ => missing_attribute("platform", function),
    }
}

// statistics

fn statistics_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn statistic_number(v: i64) -> Result<f64, i64> {
    match value::num_of(v) {
        Some(Num::Int(integer)) => Ok(integer as f64),
        Some(Num::Float(float)) => Ok(float),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("data must be numeric, not {}", value::type_name(v)),
        )),
    }
}

/// Materialises the numeric view of a `list`, `tuple`, `range` or `bytes`.
fn statistics_values(data: i64) -> Result<Vec<f64>, i64> {
    if let Some(items) = value::as_list(data) {
        return items.iter().map(|&item| statistic_number(item)).collect();
    }
    if let Some(items) = value::as_tuple(data) {
        return items.iter().map(|&item| statistic_number(item)).collect();
    }
    if let Some(bytes) = value::as_bytes(data) {
        return Ok(bytes.iter().map(|&byte| byte as f64).collect());
    }
    if let Some((start, stop, step)) = value::as_range(data) {
        let mut values = Vec::new();
        let mut current = start as i128;
        let stop = stop as i128;
        let step = step as i128;
        while (step > 0 && current < stop) || (step < 0 && current > stop) {
            values.push(current as f64);
            current += step;
        }
        return Ok(values);
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "data must be a list, tuple, range or bytes, not {}",
            value::type_name(data)
        ),
    ))
}

fn statistics_mean(data: i64, function: &str) -> i64 {
    let values = match statistics_values(data) {
        Ok(values) => values,
        Err(err) => return err,
    };
    if values.is_empty() {
        return statistics_error(&format!("{function} requires at least one data point"));
    }
    let sum: f64 = values.iter().sum();
    value::float_new(sum / values.len() as f64)
}

fn statistics_median(data: i64) -> i64 {
    let mut values = match statistics_values(data) {
        Ok(values) => values,
        Err(err) => return err,
    };
    if values.is_empty() {
        return statistics_error("median requires at least one data point");
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let middle = values.len() / 2;
    let median = if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    };
    value::float_new(median)
}

fn statistics_deviation(data: i64, sample: bool) -> i64 {
    let function = if sample { "stdev" } else { "pstdev" };
    let values = match statistics_values(data) {
        Ok(values) => values,
        Err(err) => return err,
    };
    if values.is_empty() {
        return statistics_error(&format!("{function} requires at least one data point"));
    }
    if sample && values.len() < 2 {
        return statistics_error("stdev requires at least two data points");
    }
    let count = values.len() as f64;
    let mean = values.iter().sum::<f64>() / count;
    let squared: f64 = values.iter().map(|value| (*value - mean).powi(2)).sum();
    let divisor = if sample { count - 1.0 } else { count };
    value::float_new((squared / divisor).sqrt())
}

fn statistics_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "mean" => statistics_mean(args[0], "mean"),
        "fmean" => statistics_mean(args[0], "fmean"),
        "median" => statistics_median(args[0]),
        "pstdev" => statistics_deviation(args[0], false),
        "stdev" => statistics_deviation(args[0], true),
        _ => missing_attribute("statistics", function),
    }
}

// math

fn domain_error() -> i64 {
    exc::raise_with_message(VALUE_ERROR, "math domain error")
}

fn range_error() -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, "math range error")
}

/// Numeric view of a `math` argument: int, bool and float are accepted.
fn number(v: i64) -> Result<f64, i64> {
    match value::num_of(v) {
        Some(Num::Int(i)) => Ok(i as f64),
        Some(Num::Float(f)) => Ok(f),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("must be real number, not {}", value::type_name(v)),
        )),
    }
}

fn math_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "sqrt" => match number(args[0]) {
            Ok(x) if x < 0.0 => domain_error(),
            Ok(x) => value::float_new(x.sqrt()),
            Err(err) => err,
        },
        "sin" | "cos" | "tan" => match number(args[0]) {
            // CPython rejects infinite inputs for the trigonometric family.
            Ok(x) if x.is_infinite() => domain_error(),
            Ok(x) => {
                let result = match function {
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    _ => x.tan(),
                };
                value::float_new(result)
            }
            Err(err) => err,
        },
        "asin" | "acos" => match number(args[0]) {
            Ok(x) => {
                if !x.is_nan() && !(-1.0..=1.0).contains(&x) {
                    domain_error()
                } else if function == "asin" {
                    value::float_new(x.asin())
                } else {
                    value::float_new(x.acos())
                }
            }
            Err(err) => err,
        },
        "atan" => match number(args[0]) {
            Ok(x) => value::float_new(x.atan()),
            Err(err) => err,
        },
        "exp" => match number(args[0]) {
            Ok(x) => {
                let result = x.exp();
                if result.is_infinite() && x.is_finite() {
                    range_error()
                } else {
                    value::float_new(result)
                }
            }
            Err(err) => err,
        },
        // `log2(x)` shares `log`/`log10`'s domain rules: a non-positive input
        // raises `ValueError: math domain error`, NaN propagates and infinity
        // maps to infinity.
        "log" | "log10" | "log2" => match number(args[0]) {
            Ok(x) if !x.is_nan() && x <= 0.0 => domain_error(),
            Ok(x) => {
                let result = match function {
                    "log" => x.ln(),
                    "log10" => x.log10(),
                    _ => x.log2(),
                };
                value::float_new(result)
            }
            Err(err) => err,
        },
        "pow" => {
            let base = match number(args[0]) {
                Ok(value) => value,
                Err(err) => return err,
            };
            let exponent = match number(args[1]) {
                Ok(value) => value,
                Err(err) => return err,
            };
            if base == 0.0 && exponent < 0.0 {
                return domain_error();
            }
            if base < 0.0 && exponent.is_finite() && exponent.fract() != 0.0 {
                return domain_error();
            }
            let result = base.powf(exponent);
            if result.is_infinite() && base.is_finite() && exponent.is_finite() {
                range_error()
            } else {
                value::float_new(result)
            }
        }
        "fabs" => match number(args[0]) {
            Ok(x) => value::float_new(x.abs()),
            Err(err) => err,
        },
        "floor" | "ceil" | "trunc" => {
            // Integers are already integral: return the same exact value.
            if value::is_bigint(args[0]) {
                return args[0];
            }
            if let Some(n) = value::as_integer(args[0]) {
                return ops::int_result(n);
            }
            match number(args[0]) {
                Ok(x) => float_to_int(function, x),
                Err(err) => err,
            }
        }
        // `gcd(*integers)`: CPython accepts any number of integers and
        // returns 0 for none. Floats are rejected even when integral.
        "gcd" => {
            let mut result = BigInt::from(0);
            for arg in args {
                let Some(n) = value::to_bigint(*arg) else {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "'{}' object cannot be interpreted as an integer",
                            value::type_name(*arg)
                        ),
                    );
                };
                result = gcd_bigint(result, n);
            }
            value::bigint_new(result)
        }
        // `prod(iterable, *, start=1)`: the number protocol decides the
        // result type, so the start value (or the integer 1) is multiplied
        // by each element. `start` is the only keyword.
        "prod" => math_prod(args[0], ops::int_result(1)),
        _ => missing_attribute("math", function),
    }
}

/// Keyword-aware `math` dispatch: `prod` binds its keyword-only `start`.
fn math_call_kw(function: &str, args: &[i64], keywords: Vec<(String, i64)>) -> i64 {
    match function {
        "prod" => {
            let mut start = ops::int_result(1);
            let mut bound = false;
            for (name, value) in keywords {
                if name != "start" {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("prod() got an unexpected keyword argument '{name}'"),
                    );
                }
                if bound {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "prod() got multiple values for keyword argument 'start'",
                    );
                }
                bound = true;
                start = value;
            }
            math_prod(args[0], start)
        }
        _ => exc::raise_with_message(
            TYPE_ERROR,
            &format!("math.{function}() does not accept keyword arguments"),
        ),
    }
}

/// `math.prod(iterable, *, start=1)`: multiplies every element into `start`
/// using Python's number protocol. Tagged ints stay exact and promote to
/// `BigInt` on overflow, floats promote the running product to `float`, and
/// any type with `__mul__`/`__rmul__` (or sequence repetition) follows the
/// same rules as `*`. An empty iterable yields `start`.
fn math_prod(iterable: i64, start: i64) -> i64 {
    let iterator = iter::iter_new(iterable);
    if failed() {
        return NONE;
    }
    let mut product = start;
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if failed() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        product = ops::binop(ops::OP_MUL, product, item);
        if failed() {
            return NONE;
        }
    }
    product
}

/// Greatest common divisor over exact magnitudes (`math.gcd` accepts
/// negatives and integers beyond the tagged range).
fn gcd_bigint(a: BigInt, b: BigInt) -> BigInt {
    let mut a = a.abs();
    let mut b = b.abs();
    while b.sign() != Sign::NoSign {
        let remainder = a % &b;
        a = b;
        b = remainder;
    }
    a
}

/// Converts an integral float to a tagged int, matching CPython's
/// `math.floor`/`ceil`/`trunc` error for non-finite inputs.
fn float_to_int(function: &str, x: f64) -> i64 {
    if x.is_nan() {
        return exc::raise_with_message(OVERFLOW_ERROR, "cannot convert float NaN to integer");
    }
    if x.is_infinite() {
        return exc::raise_with_message(OVERFLOW_ERROR, "cannot convert float infinity to integer");
    }
    let rounded = match function {
        "floor" => x.floor(),
        "ceil" => x.ceil(),
        _ => x.trunc(),
    };
    if rounded < i64::MIN as f64 || rounded > i64::MAX as f64 {
        return exc::raise_with_message(OVERFLOW_ERROR, "integer overflow");
    }
    ops::int_result(rounded as i64)
}

/// `hydro_math_floor_sqrt_int`: fallback for the codegen fast path of
/// `math.floor(math.sqrt(x))` with a statically `int` `x`.
///
/// The generated code handles non-negative tagged ints inline; this helper
/// covers the remaining values (negatives raise the `math.sqrt` domain
/// `ValueError`, bools and small `BigInt`s keep their numeric conversion, and
/// values the generic path cannot convert keep its exact error).
pub(crate) fn floor_sqrt_int(v: i64) -> i64 {
    let Some(n) = value::as_integer(v) else {
        let root = math_call("sqrt", &[v]);
        if failed() {
            return root;
        }
        return math_call("floor", &[root]);
    };
    if n < 0 {
        return domain_error();
    }
    // `sqrt(n) < 2^31` for every `i64`, so the result always fits a tagged
    // immediate and never overflows the conversion.
    ops::int_result((n as f64).sqrt().floor() as i64)
}

// functools

/// Dispatches a native `functools` function.
///
/// `lru_cache` is accepted as a decorator factory returning an identity
/// callable; caching is not implemented, so the decorated function stays
/// inert. `wraps(wrapped)` is real: it returns the decorator that applies
/// `update_wrapper` (see [`functools::wraps_decorator`]), so the wrapper gets
/// the wrapped function's metadata and its `__wrapped__` back-reference.
///
/// `partial(f, *args, **kwargs)` stores the keyword values by name; applying
/// them happens when the partial is called (see
/// `value::partial_new_with_keywords`).
///
/// `cached_property(func)` builds the real descriptor defined in
/// [`functools::cached_property_class`]; the module form and the class value
/// (`module_get` returns the class) construct the same state.
fn functools_call(function: &str, args: &[i64]) -> i64 {
    if let Some(result) = functools::call(function, args) {
        return result;
    }
    match function {
        "reduce" => reduce(args),
        // `@cache` is an identity decorator: the function itself is returned.
        "cache" => args[0],
        "lru_cache" => value::identity_function_new("lru_cache.<locals>.decorator"),
        "wraps" => functools::wraps_decorator(args[0], args.get(1).copied(), args.get(2).copied()),
        "partial" => value::partial_new(args[0], args[1..].to_vec()),
        _ => missing_attribute("functools", function),
    }
}

/// Keyword-aware `functools` dispatch: `partial` forwards names,
/// `partialmethod` stores them and `cached_property` binds `func`.
fn functools_call_kw(function: &str, args: &[i64], keywords: Vec<(String, i64)>) -> i64 {
    match function {
        "partial" => value::partial_new_with_keywords(args[0], args[1..].to_vec(), keywords),
        "partialmethod" => functools::new_partialmethod(args, &keywords),
        "cached_property" => functools::new_cached_property(args, &keywords),
        _ => exc::raise_with_message(
            TYPE_ERROR,
            &format!("functools.{function}() does not accept keyword arguments"),
        ),
    }
}

/// `functools.reduce(function, iterable[, initial])`.
fn reduce(args: &[i64]) -> i64 {
    let function = args[0];
    if value::as_function(function).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "reduce() arg 1 must be a function");
    }
    let iterator = iter::iter_new(args[1]);
    if failed() {
        return NONE;
    }

    let mut accumulator;
    if args.len() == 3 {
        accumulator = args[2];
    } else {
        let mut first = NONE;
        let status = iter::iter_next(iterator, &mut first);
        if failed() {
            return NONE;
        }
        if status == 0 {
            return exc::raise_with_message(
                TYPE_ERROR,
                "reduce() of empty iterable with no initial value",
            );
        }
        accumulator = first;
    }

    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if failed() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let call_args = [accumulator, item];
        let next = crate::builtins::call(function, 2, call_args.as_ptr());
        if failed() {
            return NONE;
        }
        accumulator = next;
    }
    accumulator
}

// operator

/// The result of `operator.index` when `v` is already an exact integer:
/// tagged `int` and heap `BigInt` stay as they are, while `bool` becomes the
/// corresponding `int` (`True.__index__()` is `1`, not `True`), matching
/// CPython's `PyNumber_Index`, which copies bools to exact ints.
fn exact_index_value(v: i64) -> Option<i64> {
    if value::is_int(v) || value::is_bigint(v) {
        return Some(v);
    }
    match v {
        TRUE => Some(ops::int_result(1)),
        FALSE => Some(ops::int_result(0)),
        _ => None,
    }
}

/// `operator.index(a)`: CPython's `PyNumber_Index`.
///
/// Exact integers are returned unchanged (bools as `int`); user objects are
/// asked for `__index__`, whose result must itself be an integer. Everything
/// else raises the `TypeError` CPython reports.
fn operator_index(a: i64) -> i64 {
    if let Some(index) = exact_index_value(a) {
        return index;
    }
    if let Some(result) = ops::instance_method_call(a, "__index__", &[a]) {
        if exc::pending() {
            return NONE;
        }
        if let Some(index) = exact_index_value(result) {
            return index;
        }
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "__index__ returned non-int (type {})",
                value::type_name_of(result)
            ),
        );
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name_of(a)
        ),
    )
}

fn operator_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "attrgetter" => {
            let mut names = Vec::with_capacity(args.len());
            for &arg in args {
                let Some(name) = value::as_str(arg) else {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "attribute name must be a string, not {}",
                            value::type_name(arg)
                        ),
                    );
                };
                names.push(name.to_string());
            }
            return value::attribute_getter_new(names);
        }
        "itemgetter" => return value::item_getter_new(args.to_vec()),
        "index" => return operator_index(args[0]),
        _ => {}
    }
    let binary = match function {
        "add" => Some(ops::OP_ADD),
        "sub" => Some(ops::OP_SUB),
        "mul" => Some(ops::OP_MUL),
        "truediv" => Some(ops::OP_DIV),
        "floordiv" => Some(ops::OP_FLOOR_DIV),
        "mod" => Some(ops::OP_MOD),
        "pow" => Some(ops::OP_POW),
        "eq" => Some(ops::OP_EQ),
        "ne" => Some(ops::OP_NE),
        "lt" => Some(ops::OP_LT),
        "le" => Some(ops::OP_LE),
        "gt" => Some(ops::OP_GT),
        "ge" => Some(ops::OP_GE),
        "or_" => Some(ops::OP_BIT_OR),
        _ => None,
    };
    if let Some(op) = binary {
        return ops::binop(op, args[0], args[1]);
    }
    match function {
        "neg" => ops::unop(ops::OP_NEG, args[0]),
        "not_" => ops::unop(ops::OP_NOT, args[0]),
        "truth" => value::bool_value(value::truthy(args[0])),
        _ => missing_attribute("operator", function),
    }
}

// time

/// Epoch for `perf_counter`/`monotonic` (and their `_ns` variants), relative
/// to the first time-module call in this process.
static CLOCK_BASE: OnceLock<Instant> = OnceLock::new();

fn clock_base() -> &'static Instant {
    CLOCK_BASE.get_or_init(Instant::now)
}

fn monotonic_seconds() -> f64 {
    clock_base().elapsed().as_secs_f64()
}

fn monotonic_nanos() -> i64 {
    nanos_value(clock_base().elapsed().as_nanos())
}

fn wall_clock() -> Duration {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration,
        Err(err) => err.duration(),
    }
}

fn wall_clock_nanos() -> i64 {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH);
    match since_epoch {
        Ok(duration) => nanos_value(duration.as_nanos()),
        // A pre-1970 clock: represent as a negative nanosecond count.
        Err(err) => value::bigint_new(-BigInt::from(err.duration().as_nanos())),
    }
}

/// Encodes an exact nanosecond count, promoting past `MAX_INT` instead of
/// clamping.
fn nanos_value(nanos: u128) -> i64 {
    value::bigint_new(BigInt::from(nanos))
}

fn time_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "time" => value::float_new(wall_clock().as_secs_f64()),
        "perf_counter" | "monotonic" => value::float_new(monotonic_seconds()),
        "time_ns" => wall_clock_nanos(),
        "perf_counter_ns" | "monotonic_ns" => monotonic_nanos(),
        "sleep" => match number(args[0]) {
            Ok(seconds) => {
                if seconds.is_nan() || seconds < 0.0 {
                    exc::raise_with_message(VALUE_ERROR, "sleep length must be non-negative")
                } else if !seconds.is_finite() || seconds >= u64::MAX as f64 {
                    exc::raise_with_message(OVERFLOW_ERROR, "sleep length is too large")
                } else {
                    // Park for the collector while sleeping; no runtime lock
                    // is held across the block.
                    heap::blocking_enter();
                    std::thread::sleep(Duration::from_secs_f64(seconds));
                    heap::blocking_leave();
                    NONE
                }
            }
            Err(err) => err,
        },
        _ => missing_attribute("time", function),
    }
}

// _thread (no GIL)
//
// `start_new_thread` switches the process to the shared heap (see
// `crate::heap`) before the new thread exists. Threads then run in parallel:
// there is no GIL, so a list/dict that two threads mutate must be protected by
// a user lock. Runtime internals (heap registry, thread registry, exception
// state) are either mutex-protected or thread-local.
//
// Locking rules that prevent deadlocks:
// * the heap mutex is never held while user code runs and never taken by
//   `join`, lock acquisition or `time.sleep`;
// * the thread registry lock is released before `join` blocks;
// * blocking operations park the calling thread for the stop-the-world
//   collector (`heap::blocking_enter` / `blocking_leave`), which never holds
//   a runtime lock across the block.

/// Registry of spawned threads that have not been joined yet.
///
/// The lock is held only for map operations. `join` removes its handle and
/// releases the lock before blocking. Threads that are never joined leave a
/// finished `JoinHandle` here until the process exits; that is an inert,
/// small value, and a later `join` still reports a clean result.
static THREADS: LazyLock<Mutex<HashMap<i64, JoinHandle<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Locks the thread registry, ignoring poisoning (the map stays consistent
/// because no user code runs under the lock).
fn lock_registry() -> MutexGuard<'static, HashMap<i64, JoinHandle<()>>> {
    THREADS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records that a spawned thread has left its closure.
///
/// A `Drop` guard (rather than a plain call at the end) keeps the live-thread
/// count, the Python-visible `finished` flag and the collector registration
/// correct even if the thread unwinds with a Rust panic;
/// `heap::leave_thread` marks the registration exited and wakes a waiting
/// collector.
struct ThreadExit {
    finished: Option<Arc<AtomicBool>>,
}

impl Drop for ThreadExit {
    fn drop(&mut self) {
        if let Some(finished) = &self.finished {
            finished.store(true, Ordering::Release);
        }
        heap::leave_thread();
    }
}

/// Drops the join handles inherited by a forked child. The threads they
/// referred to exist only in the parent, so the child must not join them.
pub(crate) fn after_fork_child() {
    lock_registry().clear();
}

fn thread_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "start_new_thread" => start_new_thread(args[0], args[1]),
        "join" => join_thread(args[0]),
        "get_ident" => encode_int(heap::current_thread_id()),
        "allocate_lock" => value::lock_new(),
        _ => missing_attribute("_thread", function),
    }
}

/// Validates the callable/arity pair shared by `_thread.start_new_thread` and
/// `threading.Thread`. Returns the `TypeError` message on mismatch.
fn spawn_arity_error(function: i64, argc: usize) -> Option<String> {
    let func = value::as_function(function)?;
    // `arity` is exact for generated wrappers. Native module functions store
    // their upper bound there, so consult the central spec for the lower
    // bound as well.
    match &func.body {
        value::FunctionBody::Wrapper { .. } | value::FunctionBody::Host { .. } => (func.arity
            != argc)
            .then(|| format!("function takes {} arguments ({argc} given)", func.arity)),
        // Getter and identity callables also have an exact arity; a partial
        // validates the combined argument list when it calls its target.
        value::FunctionBody::AttributeGetter { .. }
        | value::FunctionBody::ItemGetter { .. }
        | value::FunctionBody::Identity => (func.arity != argc)
            .then(|| format!("function takes {} arguments ({argc} given)", func.arity)),
        value::FunctionBody::Partial { .. } | value::FunctionBody::BoundMethod { .. } => None,
        value::FunctionBody::Native { module, name } => {
            stdlib::function(module, name).and_then(|spec| {
                if argc >= spec.min_args && argc <= spec.max_args {
                    return None;
                }
                let detail = if spec.max_args == usize::MAX {
                    format!("at least {}", spec.min_args)
                } else if spec.min_args == spec.max_args {
                    format!("{} arguments", spec.max_args)
                } else {
                    format!("{} to {} arguments", spec.min_args, spec.max_args)
                };
                Some(format!("{module}.{name}() takes {detail} ({argc} given)"))
            })
        }
        // Builtins validate their own arity inside the call dispatcher.
        value::FunctionBody::Builtin(_) => None,
    }
}

/// Spawns an OS thread that calls `function` with `argv`.
///
/// Shared by `_thread.start_new_thread` and `threading.Thread.start`. The
/// heap is migrated to shared mode before the child exists, the child
/// registers itself (with the callable and arguments as explicit GC roots)
/// before running user code, and the returned id keys the join registry.
/// `finished` is set by the child just before it leaves the runtime, allowing
/// `Thread.is_alive` to observe termination.
pub(crate) fn spawn_thread(
    function: i64,
    argv: Vec<i64>,
    finished: Option<Arc<AtomicBool>>,
) -> Result<i64, String> {
    // All objects created so far (including `function` and every argument)
    // move into the shared heap before a second thread can observe them.
    heap::enable_shared();

    let id = heap::new_thread_id();
    let argc = argv.len();
    // Stable argument buffer: the boxed slice lives in the closure until the
    // thread has finished. Its tagged contents are not on either thread's
    // native stack, so they are registered as explicit roots below.
    let argv: Box<[i64]> = argv.into_boxed_slice();
    // The callable and the argument values are only reachable from this Rust
    // closure until the generated wrapper receives them; neither thread's
    // native stack necessarily holds them. Register them as explicit roots of
    // the child together with its GC registration.
    let mut roots = Vec::with_capacity(argc + 1);
    roots.push(function);
    roots.extend_from_slice(&argv);
    // Count the thread before it starts so the sweep can see the upcoming
    // thread (the collector proper waits on the GC registration table).
    heap::enter_thread();
    let (registered_tx, registered_rx) = std::sync::mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name(format!("hydro-thread-{id}"))
        .spawn(move || {
            heap::register_thread(id, roots);
            // Wake the spawner: the child is now visible to the collector
            // with stack bounds and roots before it reaches its first
            // safepoint.
            let _ = registered_tx.send(());
            heap::safepoint();
            let _exit = ThreadExit { finished };
            // SAFETY: `argv` outlives the call and `argc` matches its length; the callee
            // reads `argc` values (NULL for zero arguments, which it never dereferences).
            let _ = crate::builtins::call(function, argc, argv.as_ptr());
            // An exception cannot cross the thread boundary: report it like
            // an uncaught exception, as CPython's `_thread` does.
            if exc::pending() {
                let _ = exc::uncaught();
            }
        });
    match spawned {
        Ok(handle) => {
            // Wait until the child registered itself (and thereby installed
            // its roots). Until then the collector cannot sweep the closure
            // arguments.
            let _ = registered_rx.recv();
            lock_registry().insert(id, handle);
            Ok(id)
        }
        Err(_) => {
            // The child never ran, so only the pre-counted live thread is
            // rolled back; the spawner's own registration stays intact.
            heap::leave_unstarted_thread();
            Err("can't start new thread".to_string())
        }
    }
}

/// `_thread.start_new_thread(fn, args)`.
///
/// Validates the callable and the argument sequence (arity is checked before
/// spawning so callers see a `TypeError` instead of a traceback on stderr),
/// then delegates to [`spawn_thread`]. Returns the thread id as a tagged
/// integer.
fn start_new_thread(function: i64, args_value: i64) -> i64 {
    if value::as_function(function).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "start_new_thread() argument 1 must be callable, not {}",
                value::type_name(function)
            ),
        );
    }
    let argv: Vec<i64> = if let Some(items) = value::as_list(args_value) {
        items.to_vec()
    } else if let Some(items) = value::as_tuple(args_value) {
        items.to_vec()
    } else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "start_new_thread() argument 2 must be a tuple, not {}",
                value::type_name(args_value)
            ),
        );
    };
    if let Some(message) = spawn_arity_error(function, argv.len()) {
        return exc::raise_with_message(TYPE_ERROR, &message);
    }
    match spawn_thread(function, argv, None) {
        Ok(id) => encode_int(id),
        Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
    }
}

/// Joins a spawned thread by id, parking for the collector while it blocks.
/// The handle is removed from the registry; a second join is an error.
pub(crate) fn join_spawned(id: i64) -> Result<(), String> {
    let handle = lock_registry().remove(&id);
    let Some(handle) = handle else {
        return Err("invalid thread id".to_string());
    };
    // The registry lock is released; park for the collector while the join
    // blocks, then wait for a running collection before returning to user
    // code.
    heap::blocking_enter();
    let result = handle.join();
    heap::blocking_leave();
    match result {
        Ok(()) => Ok(()),
        Err(_) => Err("thread terminated with an exception".to_string()),
    }
}

/// `_thread.join(id)` — Hydro extension; CPython's `_thread` has no join.
///
/// Unknown and already-joined ids raise `RuntimeError`.
fn join_thread(handle_value: i64) -> i64 {
    let Some(id) = value::as_integer(handle_value) else {
        return exc::raise_with_message(RUNTIME_ERROR, "invalid thread id");
    };
    match join_spawned(id) {
        Ok(()) => NONE,
        Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
    }
}

// threading / queue / multiprocessing / pickle

/// Fallback for constructor slots the frontend omitted (`None` means default).
fn optional(args: &[i64], index: usize) -> i64 {
    args.get(index).copied().unwrap_or(NONE)
}

/// Validates a callable constructor argument.
fn callable_argument(context: &str, value_arg: i64) -> Result<i64, i64> {
    if value::as_function(value_arg).is_some() {
        Ok(value_arg)
    } else {
        Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{context} 'target' must be callable, not {}",
                value::type_name(value_arg)
            ),
        ))
    }
}

/// `args=None` becomes an empty argument list; tuples and lists are copied.
fn argument_sequence(context: &str, value_arg: i64) -> Result<Vec<i64>, i64> {
    if value_arg == NONE {
        return Ok(Vec::new());
    }
    if let Some(items) = value::as_list(value_arg) {
        return Ok(items.to_vec());
    }
    if let Some(items) = value::as_tuple(value_arg) {
        return Ok(items.to_vec());
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "{context} 'args' must be a tuple or list, not {}",
            value::type_name(value_arg)
        ),
    ))
}

/// `name=None` selects the next default name (`Thread-N` / `Process-N`).
fn thread_name(
    context: &str,
    value_arg: i64,
    prefix: &str,
    next: &AtomicUsize,
) -> Result<String, i64> {
    if value_arg == NONE {
        return Ok(format!("{prefix}-{}", next.fetch_add(1, Ordering::Relaxed)));
    }
    match value::as_str(value_arg) {
        Some(name) => Ok(name.to_string()),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{context} 'name' must be a str, not {}",
                value::type_name(value_arg)
            ),
        )),
    }
}

/// `maxsize <= 0` (and `None`) mean unbounded.
fn maxsize_argument(value_arg: i64) -> Result<usize, i64> {
    if value_arg == NONE {
        return Ok(0);
    }
    let Some(maxsize) = value::as_integer(value_arg) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "'{}' object cannot be interpreted as an integer",
                value::type_name(value_arg)
            ),
        ));
    };
    if maxsize <= 0 {
        return Ok(0);
    }
    Ok(usize::try_from(maxsize).unwrap_or(usize::MAX))
}

fn default_thread_counter() -> &'static AtomicUsize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    &NEXT
}

fn default_process_counter() -> &'static AtomicUsize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    &NEXT
}

/// Base class used when a user subclasses `threading.Thread`.
/// The subclass keeps normal user-class storage while `start`/`join`/
/// `is_alive` delegate to a native Thread object stored on the instance.
pub(crate) fn threading_thread_base() -> i64 {
    let methods = vec![
        (
            "__init__".to_string(),
            value::native_function_new("threading", "_threadbase_init", 4),
        ),
        (
            "start".to_string(),
            value::native_function_new("threading", "_threadbase_start", 1),
        ),
        (
            "join".to_string(),
            value::native_function_new("threading", "_threadbase_join", 2),
        ),
        (
            "is_alive".to_string(),
            value::native_function_new("threading", "_threadbase_is_alive", 1),
        ),
        (
            "run".to_string(),
            value::native_function_new("threading", "_threadbase_run", 1),
        ),
    ];
    value::class_new("Thread".to_string(), NONE, methods)
}

fn threadbase_native(instance: i64) -> Option<i64> {
    value::instance_attr(instance, "_hydro_thread")
}

fn threadbase_start(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "Thread.start() missing self");
    };
    if threadbase_native(instance).is_some() {
        return exc::raise_with_message(RUNTIME_ERROR, "threads can only be started once");
    }
    let run = crate::core::methods::getattr(instance, b"run".as_ptr(), 3);
    if exc::pending() {
        return NONE;
    }
    let thread = value::thread_new(run, Vec::new(), "Thread".to_string());
    let result = crate::core::methods::call(thread, b"start".as_ptr(), 5, 0, std::ptr::null());
    if exc::pending() {
        return result;
    }
    value::instance_setattr(instance, "_hydro_thread".to_string(), thread);
    NONE
}

fn threadbase_join(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "Thread.join() missing self");
    };
    let Some(thread) = threadbase_native(instance) else {
        return NONE;
    };
    crate::core::methods::call(
        thread,
        b"join".as_ptr(),
        4,
        args.len().saturating_sub(1),
        args[1..].as_ptr(),
    )
}

fn threadbase_is_alive(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "Thread.is_alive() missing self");
    };
    let Some(thread) = threadbase_native(instance) else {
        return value::FALSE;
    };
    crate::core::methods::call(thread, b"is_alive".as_ptr(), 8, 0, std::ptr::null())
}

/// Minimal native `rich.status` implementation.
///
/// `Status` is materialized as a user instance whose methods are native
/// functions. The object stores the status text and the console; `with`
/// protocol and update/start/stop are the parts used by report-style tools.
fn rich_status_class() -> i64 {
    let methods = vec![
        (
            "__enter__".to_string(),
            value::native_function_new("rich.status", "_status_enter", 1),
        ),
        (
            "__exit__".to_string(),
            value::native_function_new("rich.status", "_status_exit", 4),
        ),
        (
            "start".to_string(),
            value::native_function_new("rich.status", "_status_start", 1),
        ),
        (
            "stop".to_string(),
            value::native_function_new("rich.status", "_status_stop", 1),
        ),
        (
            "update".to_string(),
            value::native_function_new("rich.status", "_status_update", 3),
        ),
    ];
    value::class_new("Status".to_string(), NONE, methods)
}

fn rich_status_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "Status" => {
            let instance = value::instance_new(rich_status_class());
            if let Some(&status) = args.first() {
                value::instance_setattr(instance, "status".to_string(), status);
            }
            if let Some(&console) = args.get(1) {
                value::instance_setattr(instance, "console".to_string(), console);
            }
            instance
        }
        "_status_enter" => args.first().copied().unwrap_or(NONE),
        "_status_exit" | "_status_start" | "_status_stop" | "_status_update" => NONE,
        _ => missing_attribute("rich.status", function),
    }
}

fn threading_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "Thread" => {
            let target = match callable_argument("Thread()", optional(args, 0)) {
                Ok(target) => target,
                Err(err) => return err,
            };
            let argv = match argument_sequence("Thread()", optional(args, 1)) {
                Ok(argv) => argv,
                Err(err) => return err,
            };
            let name = match thread_name(
                "Thread()",
                optional(args, 2),
                "Thread",
                default_thread_counter(),
            ) {
                Ok(name) => name,
                Err(err) => return err,
            };
            value::thread_new(target, argv, name)
        }
        "Lock" => value::lock_new(),
        "RLock" => value::lock_new_reentrant(),
        "Event" => value::event_new(),
        "get_ident" => encode_int(heap::current_thread_id()),
        "active_count" => encode_int(heap::active_threads() as i64),
        "_threadbase_init" => NONE,
        "_threadbase_run" => NONE,
        "_threadbase_start" => threadbase_start(args),
        "_threadbase_join" => threadbase_join(args),
        "_threadbase_is_alive" => threadbase_is_alive(args),
        _ => missing_attribute("threading", function),
    }
}

fn queue_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "Queue" => match maxsize_argument(optional(args, 0)) {
            Ok(maxsize) => value::queue_new(maxsize),
            Err(err) => err,
        },
        _ => missing_attribute("queue", function),
    }
}

/// Validates `Pool(processes)`: `None` selects the CPU count, any other value
/// must be a positive integer.
fn pool_processes(value_arg: i64) -> Result<usize, i64> {
    if value_arg == NONE {
        return Ok(crate::pool::default_processes());
    }
    let Some(processes) = value::as_integer(value_arg) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "Pool() 'processes' must be an integer, not {}",
                value::type_name(value_arg)
            ),
        ));
    };
    if processes <= 0 {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            "Number of processes must be at least 1",
        ));
    }
    Ok(usize::try_from(processes).unwrap_or(usize::MAX))
}

fn multiprocessing_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "Process" => {
            let target = match callable_argument("Process()", optional(args, 0)) {
                Ok(target) => target,
                Err(err) => return err,
            };
            let argv = match argument_sequence("Process()", optional(args, 1)) {
                Ok(argv) => argv,
                Err(err) => return err,
            };
            let name = match thread_name(
                "Process()",
                optional(args, 2),
                "Process",
                default_process_counter(),
            ) {
                Ok(name) => name,
                Err(err) => return err,
            };
            value::process_new(target, argv, name)
        }
        "Queue" => match maxsize_argument(optional(args, 0)) {
            Ok(maxsize) => match process::new_queue(maxsize) {
                Ok(queue) => queue,
                Err(message) => {
                    exc::raise_with_message(RUNTIME_ERROR, &format!("Queue(): {message}"))
                }
            },
            Err(err) => err,
        },
        "Pool" => {
            let processes = match pool_processes(optional(args, 0)) {
                Ok(processes) => processes,
                Err(err) => return err,
            };
            match crate::pool::new_pool(processes) {
                Ok(pool) => pool,
                Err(message) => {
                    exc::raise_with_message(RUNTIME_ERROR, &format!("Pool(): {message}"))
                }
            }
        }
        "cpu_count" => encode_int(cpu_count()),
        "get_start_method" => value::str_alloc(process::get_start_method()),
        "set_start_method" => {
            let Some(method) = value::as_str(args[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "set_start_method() argument must be str, not {}",
                        value::type_name(args[0])
                    ),
                );
            };
            match process::set_start_method(method) {
                Ok(()) => NONE,
                Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
            }
        }
        _ => missing_attribute("multiprocessing", function),
    }
}

fn pickle_error(error: serialize::PickleError, loading: bool) -> i64 {
    let message = if loading {
        error.loads_message()
    } else {
        error.dumps_message()
    };
    let tag = match error {
        serialize::PickleError::Unsupported(_) => TYPE_ERROR,
        serialize::PickleError::TooDeep | serialize::PickleError::Corrupt => VALUE_ERROR,
    };
    exc::raise_with_message(tag, &message)
}

/// Highest protocol version CPython defines. HYD1 output is the same for
/// every version, but an out-of-range request is rejected instead of being
/// silently accepted.
const PICKLE_HIGHEST_PROTOCOL: i64 = 5;

/// Validates `pickle`'s `protocol` argument. `None` selects the default and
/// negative integers select the highest protocol, like CPython; anything above
/// [`PICKLE_HIGHEST_PROTOCOL`] is a `ValueError` and non-integers are a
/// `TypeError`.
fn pickle_check_protocol(argument: i64) -> Result<(), i64> {
    if argument == NONE {
        return Ok(());
    }
    let Some(protocol) = value::to_bigint(argument) else {
        let ty = value::type_name_of(argument);
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("protocol must be an integer or None, not {ty}"),
        ));
    };
    if protocol > BigInt::from(PICKLE_HIGHEST_PROTOCOL) {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            &format!("pickle protocol must be <= {}", PICKLE_HIGHEST_PROTOCOL),
        ));
    }
    Ok(())
}

/// `pickle.dumps(obj, protocol=None)` / `pickle.dump(obj, file, protocol=None,
/// *, fix_imports=True, buffer_callback=None)`.
///
/// `dumps` returns the HYD1 payload as one hexadecimal string and `dump`
/// writes that same text to a file object: UTF-8 bytes into binary files, the
/// `str` into text files. `fix_imports` and `buffer_callback` are accepted and
/// ignored because HYD1 stores class references inline and has no out-of-band
/// buffers.
fn pickle_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "dumps" => {
            if let Err(raised) = pickle_check_protocol(args.get(1).copied().unwrap_or(NONE)) {
                return raised;
            }
            match serialize::encode(args[0], false) {
                Ok(bytes) => value::str_alloc(serialize::to_hex(&bytes)),
                Err(error) => pickle_error(error, false),
            }
        }
        "dump" => {
            if let Err(raised) = pickle_check_protocol(args.get(2).copied().unwrap_or(NONE)) {
                return raised;
            }
            let file_value = args[1];
            let Some(file) = value::as_file(file_value) else {
                let ty = value::type_name_of(file_value);
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("file must be a writable file object, not {ty}"),
                );
            };
            let bytes = match serialize::encode(args[0], false) {
                Ok(bytes) => bytes,
                Err(error) => return pickle_error(error, false),
            };
            // HYD1 is one hexadecimal string: binary files receive its UTF-8
            // bytes, text files receive the `str` itself.
            let payload = serialize::to_hex(&bytes);
            let written = if file.binary() {
                file.write_bytes(payload.as_bytes())
            } else {
                file.write(&payload)
            };
            match written {
                Ok(_) => NONE,
                Err(error) => value::raise_file_error(error),
            }
        }
        "loads" => {
            let Some(text) = value::as_str(args[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "loads() argument must be str, not {}",
                        value::type_name(args[0])
                    ),
                );
            };
            let Some(bytes) = serialize::from_hex(text) else {
                return exc::raise_with_message(VALUE_ERROR, "invalid pickle data");
            };
            match serialize::decode(&bytes, false) {
                Ok(decoded) => decoded,
                Err(error) => pickle_error(error, true),
            }
        }
        _ => missing_attribute("pickle", function),
    }
}

// csv

fn csv_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "reader" => crate::csv::reader_new(args[0]),
        "writer" => crate::csv::writer_new(args[0]),
        "DictReader" => crate::csv::dict_reader_new(args[0]),
        "DictWriter" => crate::csv::dict_writer_new(args[0], args[1]),
        _ => missing_attribute("csv", function),
    }
}

// html
fn html_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "escape" => {
            let Some(text) = args.first().and_then(|value| value::as_str(*value)) else {
                return exc::raise_with_message(TYPE_ERROR, "escape() argument must be str");
            };
            let quote = args.get(1).is_none_or(|value| value::truthy(*value));
            let mut out = String::with_capacity(text.len());
            for character in text.chars() {
                match character {
                    '&' => out.push_str("&amp;"),
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '"' if quote => out.push_str("&quot;"),
                    '\'' if quote => out.push_str("&#x27;"),
                    other => out.push(other),
                }
            }
            value::str_alloc(out)
        }
        "unescape" => {
            let Some(text) = args.first().and_then(|value| value::as_str(*value)) else {
                return exc::raise_with_message(TYPE_ERROR, "unescape() argument must be str");
            };
            value::str_alloc(
                text.replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&#x27;", "'"),
            )
        }
        _ => missing_attribute("html", function),
    }
}

// json

/// Maximum container nesting accepted by both the serializer and the parser.
/// Bounds recursion on the native stack; cycles and over-deep documents are
/// reported instead of overflowing.
const MAX_JSON_DEPTH: usize = 128;

fn serialize_error(v: i64) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "Object of type {} is not JSON serializable",
            value::type_name(v)
        ),
    )
}

fn circular_error() -> i64 {
    exc::raise_with_message(VALUE_ERROR, "Circular reference detected")
}

fn json_parse_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

pub(crate) fn json_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "dumps" => json_dumps(args),
        "loads" => json_loads(args[0]),
        "dump" => json_dump(args),
        "load" => json_load(args[0]),
        _ => missing_attribute("json", function),
    }
}

/// Upper bound for `indent`: a pathological level must not exhaust memory.
const MAX_JSON_INDENT: usize = 256;

/// Formatting options of `json.dumps`/`json.dump`.
///
/// The frontend reorders keyword arguments into positional slots and pads the
/// omitted ones with `None`, so every option is `None`-able and trailing
/// `None` values keep the CPython defaults.
struct JsonOptions {
    indent: Option<usize>,
    ensure_ascii: bool,
    sort_keys: bool,
    item_separator: String,
    key_separator: String,
}

impl JsonOptions {
    fn defaults() -> JsonOptions {
        JsonOptions {
            indent: None,
            ensure_ascii: true,
            sort_keys: false,
            item_separator: ", ".to_string(),
            key_separator: ": ".to_string(),
        }
    }
}

fn json_option_type_error(function: &str, option: &str, expected: &str, received: i64) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "{function}() argument '{option}' must be {expected}, not {}",
            value::type_name(received)
        ),
    )
}

/// Decodes `indent`, `ensure_ascii`, `separators` and `sort_keys` starting at
/// `offset` in `args`. Absent or `None` slots select the CPython defaults.
fn json_options(args: &[i64], offset: usize, function: &str) -> Result<JsonOptions, i64> {
    let mut options = JsonOptions::defaults();
    if let Some(&indent) = args.get(offset) {
        if indent != NONE {
            let Some(level) = value::as_integer(indent) else {
                return Err(json_option_type_error(
                    function,
                    "indent",
                    "an integer or None",
                    indent,
                ));
            };
            options.indent = Some(usize::try_from(level).unwrap_or(0).min(MAX_JSON_INDENT));
        }
    }
    if let Some(&ensure_ascii) = args.get(offset + 1) {
        if ensure_ascii != NONE {
            options.ensure_ascii = value::truthy(ensure_ascii);
        }
    }
    if let Some(&separators) = args.get(offset + 2) {
        if separators != NONE {
            let Some(items) = value::as_tuple(separators).or_else(|| value::as_list(separators))
            else {
                return Err(json_option_type_error(
                    function,
                    "separators",
                    "a tuple",
                    separators,
                ));
            };
            let (Some(item_separator), Some(key_separator)) = (
                items.first().and_then(|item| value::as_str(*item)),
                items.get(1).and_then(|item| value::as_str(*item)),
            ) else {
                return Err(exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{function}() argument 'separators' must be a tuple of two strings"),
                ));
            };
            options.item_separator = item_separator.to_string();
            options.key_separator = key_separator.to_string();
        }
    }
    if let Some(&sort_keys) = args.get(offset + 3) {
        if sort_keys != NONE {
            options.sort_keys = value::truthy(sort_keys);
        }
    }
    Ok(options)
}

/// Validates the `file` argument of `json.dump`/`json.load`.
fn json_file(file: i64, function: &str) -> Result<&'static value::FileState, i64> {
    match value::as_file(file) {
        Some(state) => Ok(state),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{function}() argument 'file' must be a file object, not {}",
                value::type_name(file)
            ),
        )),
    }
}

/// `json.dump(obj, file, indent=None, ensure_ascii=True, separators=None,
/// sort_keys=False)`: serialises like [`json_dumps`] and writes the result
/// with a single `FileState::write` call.
fn json_dump(args: &[i64]) -> i64 {
    let state = match json_file(args[1], "dump") {
        Ok(state) => state,
        Err(err) => return err,
    };
    let options = match json_options(args, 2, "dump") {
        Ok(options) => options,
        Err(err) => return err,
    };
    let mut out = String::new();
    let mut stack: Vec<i64> = Vec::new();
    if let Err(err) = dumps_value(args[0], 0, &mut stack, &mut out, &options) {
        return err;
    }
    match state.write(&out) {
        Ok(_) => NONE,
        Err(error) => value::raise_file_error(error),
    }
}

/// `json.load(file)`: reads the remaining file text and parses it with the
/// same parser as [`json_loads`].
fn json_load(file: i64) -> i64 {
    let state = match json_file(file, "load") {
        Ok(state) => state,
        Err(err) => return err,
    };
    match state.read(None) {
        Ok(source) => parse_json(source),
        Err(error) => value::raise_file_error(error),
    }
}

/// `json.dumps(obj, indent=None, ensure_ascii=True, separators=None,
/// sort_keys=False)`: CPython defaults are `", "`/`": "` separators, insertion
/// order and `ensure_ascii=True`.
fn json_dumps(args: &[i64]) -> i64 {
    let options = match json_options(args, 1, "dumps") {
        Ok(options) => options,
        Err(err) => return err,
    };
    let mut out = String::new();
    let mut stack: Vec<i64> = Vec::new();
    match dumps_value(args[0], 0, &mut stack, &mut out, &options) {
        Ok(()) => value::str_alloc(out),
        Err(err) => err,
    }
}

/// Appends `width * depth` spaces; `indent` is already clamped by
/// [`json_options`], so the repeated push stays bounded.
fn write_indent(out: &mut String, width: usize, depth: usize) {
    for _ in 0..width.saturating_mul(depth) {
        out.push(' ');
    }
}

/// Appends the JSON form of `v`. Only heap objects created through
/// [`dumps_value`]'s errors can trigger a collection here, so the borrows into
/// containers taken below are never read again after an allocation.
fn dumps_value(
    v: i64,
    depth: usize,
    stack: &mut Vec<i64>,
    out: &mut String,
    options: &JsonOptions,
) -> Result<(), i64> {
    if v == NONE {
        out.push_str("null");
        return Ok(());
    }
    if v == TRUE {
        out.push_str("true");
        return Ok(());
    }
    if v == FALSE {
        out.push_str("false");
        return Ok(());
    }
    if let Some(n) = value::as_integer(v) {
        out.push_str(&n.to_string());
        return Ok(());
    }
    if let Some(n) = value::as_bigint(v) {
        // Arbitrary-precision integers keep their exact decimal form.
        out.push_str(&n.to_string());
        return Ok(());
    }
    if let Some(s) = value::as_str(v) {
        write_json_string(s, options.ensure_ascii, out);
        return Ok(());
    }
    if let Some(f) = value::as_float(v) {
        out.push_str(&json_float(f));
        return Ok(());
    }
    if let Some(items) = value::as_list(v).or_else(|| value::as_tuple(v)) {
        if depth >= MAX_JSON_DEPTH || stack.contains(&v) {
            return Err(circular_error());
        }
        stack.push(v);
        out.push('[');
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                if options.indent.is_some() {
                    out.push(',');
                } else {
                    out.push_str(&options.item_separator);
                }
            }
            if let Some(level) = options.indent {
                out.push('\n');
                write_indent(out, level, depth + 1);
            }
            dumps_value(*item, depth + 1, stack, out, options)?;
        }
        if let Some(level) = options.indent {
            if !items.is_empty() {
                out.push('\n');
                write_indent(out, level, depth);
            }
        }
        out.push(']');
        stack.pop();
        return Ok(());
    }
    if let Some(pairs) = value::as_dict(v) {
        if depth >= MAX_JSON_DEPTH || stack.contains(&v) {
            return Err(circular_error());
        }
        // Render the keys once; `sort_keys` then only reorders this table.
        // Rendering allocates Rust strings (never Hydro objects), so the values
        // stay reachable through the dict while the table is built.
        let mut entries: Vec<(String, i64)> = Vec::with_capacity(pairs.len());
        for (key, item) in pairs.iter() {
            entries.push((json_key(*key, options.ensure_ascii)?, *item));
        }
        if options.sort_keys {
            entries.sort_by(|left, right| left.0.cmp(&right.0));
        }
        stack.push(v);
        out.push('{');
        for (index, (key, item)) in entries.iter().enumerate() {
            if index > 0 {
                if options.indent.is_some() {
                    out.push(',');
                } else {
                    out.push_str(&options.item_separator);
                }
            }
            if let Some(level) = options.indent {
                out.push('\n');
                write_indent(out, level, depth + 1);
            }
            out.push_str(key);
            out.push_str(&options.key_separator);
            dumps_value(*item, depth + 1, stack, out, options)?;
        }
        if let Some(level) = options.indent {
            if !entries.is_empty() {
                out.push('\n');
                write_indent(out, level, depth);
            }
        }
        out.push('}');
        stack.pop();
        return Ok(());
    }
    Err(serialize_error(v))
}

/// Renders a dict key. Scalars are accepted and always emitted as a quoted
/// JSON string; other types raise CPython's key `TypeError`.
fn json_key(key: i64, ensure_ascii: bool) -> Result<String, i64> {
    if let Some(s) = value::as_str(key) {
        let mut out = String::new();
        write_json_string(s, ensure_ascii, &mut out);
        return Ok(out);
    }
    let scalar = if key == NONE {
        Some("null".to_string())
    } else if key == TRUE {
        Some("true".to_string())
    } else if key == FALSE {
        Some("false".to_string())
    } else if let Some(n) = value::as_integer(key) {
        Some(n.to_string())
    } else if let Some(n) = value::as_bigint(key) {
        Some(n.to_string())
    } else {
        value::as_float(key).map(json_float)
    };
    match scalar {
        Some(text) => {
            let mut out = String::with_capacity(text.len() + 2);
            out.push('"');
            out.push_str(&text);
            out.push('"');
            Ok(out)
        }
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "keys must be str, int, float, bool or None, not {}",
                value::type_name(key)
            ),
        )),
    }
}

/// CPython's json float rendering: shortest round-tripping representation
/// with `NaN`/`Infinity` spellings.
fn json_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }
    value::format_float(f)
}

/// Appends `s` as a JSON string. With `ensure_ascii` everything outside
/// printable ASCII is written as `\uXXXX` (surrogate pairs above the BMP);
/// without it non-ASCII text stays raw while control characters keep their
/// escapes.
fn write_json_string(s: &str, ensure_ascii: bool, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => write_unicode_escape(ch as u32, out),
            ch if ensure_ascii && (ch as u32) >= 0x7f => write_unicode_escape(ch as u32, out),
            ch => out.push(ch),
        }
    }
    out.push('"');
}

fn write_unicode_escape(code: u32, out: &mut String) {
    use std::fmt::Write as _;
    if code <= 0xffff {
        // Writing into a String cannot fail.
        let _ = write!(out, "\\u{code:04x}");
        return;
    }
    let rest = code - 0x10000;
    let high = 0xd800 + (rest >> 10);
    let low = 0xdc00 + (rest & 0x3ff);
    let _ = write!(out, "\\u{high:04x}\\u{low:04x}");
}

/// `json.loads(text)`: parses a `str` into runtime values. Containers are
/// allocated empty first and held in tagged locals, then filled incrementally,
/// so a collection between two elements still traces the partial container.
fn json_loads(v: i64) -> i64 {
    let Some(text) = value::as_str(v) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "the JSON object must be str, bytes or bytearray, not {}",
                value::type_name(v)
            ),
        );
    };
    // Copy the input before the first allocation: the parser must not depend
    // on a reference into a heap object while new objects are being created.
    parse_json(text.to_string())
}

/// Parses a complete JSON document (shared by `loads` and file `load`).
fn parse_json(source: String) -> i64 {
    let mut parser = JsonParser { source, pos: 0 };
    match parser.parse_document() {
        Ok(result) => result,
        Err(err) => err,
    }
}

struct JsonParser {
    source: String,
    pos: usize,
}

impl JsonParser {
    fn bytes(&self) -> &[u8] {
        self.source.as_bytes()
    }

    /// Current byte, or `None` at end of input.
    fn byte(&self) -> Option<u8> {
        self.bytes().get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.byte(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn consume(&mut self, word: &str) -> bool {
        // `pos` is always on a char boundary and `word` is ASCII.
        if self.source[self.pos..].starts_with(word) {
            self.pos += word.len();
            true
        } else {
            false
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), i64> {
        if self.consume(word) {
            Ok(())
        } else {
            Err(json_parse_error("Expecting value"))
        }
    }

    fn parse_document(&mut self) -> Result<i64, i64> {
        self.skip_whitespace();
        if self.byte().is_none() {
            return Err(json_parse_error("Expecting value"));
        }
        let result = self.parse_value(0)?;
        self.skip_whitespace();
        if self.pos < self.bytes().len() {
            return Err(json_parse_error("Extra data"));
        }
        Ok(result)
    }

    fn parse_value(&mut self, depth: usize) -> Result<i64, i64> {
        self.skip_whitespace();
        match self.byte() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => {
                let text = self.parse_string()?;
                Ok(value::str_alloc(text))
            }
            Some(b't') => {
                self.literal("true")?;
                Ok(TRUE)
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(FALSE)
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(NONE)
            }
            Some(b'N') => {
                self.literal("NaN")?;
                Ok(value::float_new(f64::NAN))
            }
            Some(b'I') => {
                self.literal("Infinity")?;
                Ok(value::float_new(f64::INFINITY))
            }
            Some(b'-') if self.source[self.pos..].starts_with("-Infinity") => {
                self.pos += "-Infinity".len();
                Ok(value::float_new(f64::NEG_INFINITY))
            }
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(json_parse_error("Expecting value")),
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<i64, i64> {
        if depth >= MAX_JSON_DEPTH {
            return Err(json_parse_error("Maximum nesting depth exceeded"));
        }
        self.pos += 1; // '['
        let array = value::list_new(Vec::new());
        self.skip_whitespace();
        if self.byte() == Some(b']') {
            self.pos += 1;
            return Ok(array);
        }
        loop {
            let item = self.parse_value(depth + 1)?;
            if let Some(mut items) = value::list_items_mut(array) {
                items.push(item);
                crate::heap::write_barrier_edge(array, item);
            }
            self.skip_whitespace();
            match self.byte() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(array);
                }
                _ => return Err(json_parse_error("Expecting ',' delimiter")),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<i64, i64> {
        if depth >= MAX_JSON_DEPTH {
            return Err(json_parse_error("Maximum nesting depth exceeded"));
        }
        self.pos += 1; // '{'
        let object = value::dict_new(Vec::new());
        self.skip_whitespace();
        if self.byte() == Some(b'}') {
            self.pos += 1;
            return Ok(object);
        }
        loop {
            self.skip_whitespace();
            if self.byte() != Some(b'"') {
                return Err(json_parse_error(
                    "Expecting property name enclosed in double quotes",
                ));
            }
            let raw_key = self.parse_string()?;
            // Root the key in a tagged local while the value is parsed.
            let key = value::str_alloc(raw_key);
            self.skip_whitespace();
            if self.byte() != Some(b':') {
                return Err(json_parse_error("Expecting ':' delimiter"));
            }
            self.pos += 1;
            let item = self.parse_value(depth + 1)?;
            if let Some(mut pairs) = value::dict_pairs_mut(object) {
                crate::container::dict_insert_barrier(object, &mut pairs, key, item);
            }
            self.skip_whitespace();
            match self.byte() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(object);
                }
                _ => return Err(json_parse_error("Expecting ',' delimiter")),
            }
        }
    }

    fn parse_number(&mut self) -> Result<i64, i64> {
        let start = self.pos;
        if self.byte() == Some(b'-') {
            self.pos += 1;
        }
        match self.byte() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.byte(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(json_parse_error("Expecting value")),
        }
        let mut is_float = false;
        if self.byte() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            let digits = self.pos;
            while matches!(self.byte(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == digits {
                return Err(json_parse_error("Expecting value"));
            }
        }
        if matches!(self.byte(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.byte(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let digits = self.pos;
            while matches!(self.byte(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == digits {
                return Err(json_parse_error("Expecting value"));
            }
        }
        let text = &self.source[start..self.pos];
        if is_float {
            match text.parse::<f64>() {
                Ok(f) => Ok(value::float_new(f)),
                Err(_) => Err(json_parse_error("Expecting value")),
            }
        } else {
            match text.parse::<num_bigint::BigInt>() {
                // `bigint_new` normalizes values inside the tagged range to
                // immediates, so small integers keep the fast path.
                Ok(n) => Ok(value::bigint_new(n)),
                Err(_) => Err(json_parse_error("Expecting value")),
            }
        }
    }

    /// Parses a quoted string. `self.pos` must point at the opening quote.
    fn parse_string(&mut self) -> Result<String, i64> {
        self.pos += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(byte) = self.byte() else {
                return Err(json_parse_error("Unterminated string starting at"));
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    return match String::from_utf8(out) {
                        Ok(text) => Ok(text),
                        Err(_) => Err(json_parse_error("Invalid UTF-8 sequence")),
                    };
                }
                b'\\' => {
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                }
                0x00..=0x1f => {
                    return Err(json_parse_error("Invalid control character at"));
                }
                _ => {
                    // Copy one complete UTF-8 scalar verbatim. The input is a
                    // Rust `String`, so `pos` is always on a char boundary.
                    let ch = self.source[self.pos..].chars().next().unwrap_or('\u{fffd}');
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    /// Handles the escape sequence after a backslash (`self.pos` is past it).
    fn parse_escape(&mut self, out: &mut Vec<u8>) -> Result<(), i64> {
        let Some(byte) = self.byte() else {
            return Err(json_parse_error("Unterminated string starting at"));
        };
        self.pos += 1;
        match byte {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let Some(code) = self.parse_hex4() else {
                    return Err(json_parse_error("Invalid \\uXXXX escape"));
                };
                let scalar = if (0xd800..=0xdbff).contains(&code) {
                    // A high surrogate must be followed by a low surrogate;
                    // lone and invalid pairs become U+FFFD because Rust strings
                    // cannot store surrogate code points.
                    let saved = self.pos;
                    if self.consume("\\u") {
                        match self.parse_hex4() {
                            Some(low) if (0xdc00..=0xdfff).contains(&low) => {
                                0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00)
                            }
                            _ => {
                                self.pos = saved;
                                0xfffd
                            }
                        }
                    } else {
                        0xfffd
                    }
                } else if (0xdc00..=0xdfff).contains(&code) {
                    0xfffd
                } else {
                    code
                };
                push_scalar(out, scalar);
            }
            _ => return Err(json_parse_error("Invalid \\escape")),
        }
        Ok(())
    }

    /// Reads exactly four hex digits, leaving `pos` untouched on failure.
    fn parse_hex4(&mut self) -> Option<u32> {
        let bytes = self.bytes();
        if self.pos + 4 > bytes.len() {
            return None;
        }
        let mut code = 0u32;
        for index in 0..4 {
            let digit = (bytes[self.pos + index] as char).to_digit(16)?;
            code = code * 16 + digit;
        }
        self.pos += 4;
        Some(code)
    }
}

fn push_scalar(out: &mut Vec<u8>, scalar: u32) {
    let ch = char::from_u32(scalar).unwrap_or('\u{fffd}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

// os.path

/// Borrows a `str` or `Path` argument for an `os.path` function.
///
/// On failure a `TypeError` is pending and `None` is returned, so callers
/// only have to return the `None` dummy value.
fn os_path_arg(function: &str, arg: i64) -> Option<&'static str> {
    if let Some(path) = value::as_path_like(arg) {
        return Some(path);
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "{}() argument must be a path string, not {}",
            function,
            value::type_name(arg)
        ),
    );
    None
}

/// Lexically normalises an absolute path: drops `.` components and resolves
/// `..` without touching the filesystem (like `os.path.normpath`).
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            std::path::Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            std::path::Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// `posixpath.basename` semantics: the text after the last separator,
/// including the empty result for a trailing separator.
fn path_basename(path: &str) -> String {
    match path.rfind(std::path::MAIN_SEPARATOR) {
        Some(index) => path[index + 1..].to_string(),
        None => path.to_string(),
    }
}

/// `posixpath.dirname` semantics: the text up to the last separator with
/// trailing separators removed, except for a pure root.
fn path_dirname(path: &str) -> String {
    let separator = std::path::MAIN_SEPARATOR;
    let Some(index) = path.rfind(separator) else {
        return String::new();
    };
    let head = &path[..=index];
    if head.chars().all(|ch| ch == separator) {
        return head.to_string();
    }
    head.trim_end_matches(separator).to_string()
}

/// Splits a trailing extension. A leading dot in the file name is part of the
/// name, and a trailing dot is an (empty) extension, like `os.path.splitext`.
fn path_splitext(path: &str) -> (String, String) {
    let separator = std::path::MAIN_SEPARATOR;
    let start = path.rfind(separator).map_or(0, |index| index + 1);
    let Some(dot) = path[start..].rfind('.') else {
        return (path.to_string(), String::new());
    };
    if dot == 0 {
        return (path.to_string(), String::new());
    }
    let cut = start + dot;
    (path[..cut].to_string(), path[cut..].to_string())
}

fn os_path_join(args: &[i64]) -> i64 {
    let mut joined = String::new();
    for arg in args {
        let Some(part) = os_path_arg("join", *arg) else {
            return NONE;
        };
        if std::path::Path::new(part).is_absolute() {
            joined.clear();
            joined.push_str(part);
        } else if joined.is_empty() {
            joined.push_str(part);
        } else {
            if !joined.ends_with(std::path::MAIN_SEPARATOR) {
                joined.push(std::path::MAIN_SEPARATOR);
            }
            joined.push_str(part);
        }
    }
    value::str_alloc(joined)
}

fn os_path_predicate(function: &str, value: i64, accept: fn(&std::fs::Metadata) -> bool) -> i64 {
    let Some(path) = os_path_arg(function, value) else {
        return NONE;
    };
    match std::fs::metadata(path) {
        Ok(metadata) if accept(&metadata) => TRUE,
        Ok(_) => FALSE,
        Err(_) => FALSE,
    }
}

/// `os.path.isabs(path)`: whether the path is absolute under POSIX rules
/// (a leading separator). A `TypeError` is pending for non-path arguments.
fn os_path_isabs(value: i64) -> i64 {
    let Some(path) = os_path_arg("isabs", value) else {
        return NONE;
    };
    if std::path::Path::new(path).is_absolute() {
        TRUE
    } else {
        FALSE
    }
}

fn os_path_abspath(value: i64) -> i64 {
    let Some(path) = os_path_arg("abspath", value) else {
        return NONE;
    };
    let path = std::path::Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) if path.as_os_str().is_empty() => cwd,
            Ok(cwd) => cwd.join(path),
            Err(error) => {
                return exc::raise_with_message(OS_ERROR, &format!("abspath: {error}"));
            }
        }
    };
    value::str_alloc(normalize_path(&absolute).to_string_lossy().into_owned())
}

fn os_path_split_pair(value: i64) -> i64 {
    let Some(path) = os_path_arg("split", value) else {
        return NONE;
    };
    let head = value::str_alloc(path_dirname(path));
    let tail = value::str_alloc(path_basename(path));
    value::tuple_new(vec![head, tail])
}

fn os_path_splitext_pair(value: i64) -> i64 {
    let Some(path) = os_path_arg("splitext", value) else {
        return NONE;
    };
    let (root, extension) = path_splitext(path);
    let root = value::str_alloc(root);
    let extension = value::str_alloc(extension);
    value::tuple_new(vec![root, extension])
}

/// `os.path.normcase(path)`: on POSIX this is the identity (CPython's
/// `posixpath.normcase` returns the path unchanged).
fn os_path_normcase(value: i64) -> i64 {
    match os_path_arg("normcase", value) {
        Some(path) => value::str_alloc(path.to_string()),
        None => NONE,
    }
}

/// One component while resolving symlinks.
enum RealpathStep {
    Root,
    Parent,
    Normal(std::ffi::OsString),
}

/// Resolves `path` component by component, following symlinks. Returns the
/// resolved path plus whether a symlink loop (or over-deep expansion) was
/// detected; the non-strict caller keeps the partial result in that case.
fn resolve_symlinks(path: &Path) -> (PathBuf, bool) {
    let mut resolved = PathBuf::new();
    let mut steps: VecDeque<RealpathStep> = VecDeque::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => {
                steps.push_back(RealpathStep::Normal(prefix.as_os_str().to_owned()))
            }
            std::path::Component::RootDir => steps.push_back(RealpathStep::Root),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => steps.push_back(RealpathStep::Parent),
            std::path::Component::Normal(part) => {
                steps.push_back(RealpathStep::Normal(part.to_owned()))
            }
        }
    }
    let mut hops = 0usize;
    while let Some(step) = steps.pop_front() {
        match step {
            RealpathStep::Root => {
                resolved.clear();
                resolved.push(std::path::MAIN_SEPARATOR.to_string());
            }
            RealpathStep::Parent => {
                resolved.pop();
            }
            RealpathStep::Normal(name) => {
                let candidate = resolved.join(&name);
                match std::fs::read_link(&candidate) {
                    Ok(target) => {
                        hops += 1;
                        if hops > 40 {
                            // Non-strict callers keep the resolved prefix;
                            // adding the loop component keeps the spelling
                            // deterministic.
                            resolved.push(name);
                            return (resolved, true);
                        }
                        let mut substituted: VecDeque<RealpathStep> = VecDeque::new();
                        if target.is_absolute() {
                            resolved.clear();
                            resolved.push(std::path::MAIN_SEPARATOR.to_string());
                        }
                        for component in target.components() {
                            match component {
                                std::path::Component::Prefix(prefix) => substituted
                                    .push_back(RealpathStep::Normal(prefix.as_os_str().to_owned())),
                                std::path::Component::RootDir => {
                                    substituted.push_back(RealpathStep::Root)
                                }
                                std::path::Component::CurDir => {}
                                std::path::Component::ParentDir => {
                                    substituted.push_back(RealpathStep::Parent)
                                }
                                std::path::Component::Normal(part) => {
                                    substituted.push_back(RealpathStep::Normal(part.to_owned()))
                                }
                            }
                        }
                        while let Some(step) = steps.pop_front() {
                            substituted.push_back(step);
                        }
                        steps = substituted;
                    }
                    Err(_) => resolved.push(name),
                }
            }
        }
    }
    (resolved, false)
}

/// `os.path.realpath(filename, *, strict=False)`: resolves symlinks and
/// normalises the result. A missing path is returned as an absolute path when
/// `strict` is false, and raises `OSError` when it is true, like CPython.
fn os_path_realpath(value: i64, strict: bool) -> i64 {
    let Some(path) = os_path_arg("realpath", value) else {
        return NONE;
    };
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) if path.as_os_str().is_empty() => cwd,
            Ok(cwd) => cwd.join(path),
            Err(error) => {
                return exc::raise_with_message(OS_ERROR, &format!("realpath: {error}"));
            }
        }
    };
    let (resolved, looped) = resolve_symlinks(&absolute);
    let resolved = normalize_path(&resolved);
    if strict {
        if looped {
            return exc::raise_with_message(
                OS_ERROR,
                "realpath: too many levels of symbolic links",
            );
        }
        if !resolved.exists() {
            return exc::raise_with_message(
                OS_ERROR,
                &format!(
                    "realpath: no such file or directory: {}",
                    resolved.to_string_lossy()
                ),
            );
        }
    }
    value::str_alloc(resolved.to_string_lossy().into_owned())
}

/// Keyword-aware `os.path` dispatch: `realpath` binds `strict`.
fn os_path_call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if function != "realpath" {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("os.path.{function}() does not accept keyword arguments"),
        );
    }
    let mut filename = args.first().copied();
    let mut strict = false;
    if args.len() > 1 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "realpath() takes at most 2 arguments ({} given)",
                args.len()
            ),
        );
    }
    for (name, item) in keywords {
        match name.as_str() {
            "filename" => {
                if filename.is_some() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "realpath() got multiple values for argument 'filename'",
                    );
                }
                filename = Some(*item);
            }
            "strict" => strict = value::truthy(*item),
            other => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("realpath() got an unexpected keyword argument '{other}'"),
                )
            }
        }
    }
    match filename {
        Some(value_arg) => os_path_realpath(value_arg, strict),
        None => exc::raise_with_message(
            TYPE_ERROR,
            "realpath() missing required argument 'filename'",
        ),
    }
}

/// `os.path`: pure string manipulation plus filesystem predicates.
fn os_path_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "join" => os_path_join(args),
        "basename" => match os_path_arg("basename", args[0]) {
            Some(path) => value::str_alloc(path_basename(path)),
            None => NONE,
        },
        "dirname" => match os_path_arg("dirname", args[0]) {
            Some(path) => value::str_alloc(path_dirname(path)),
            None => NONE,
        },
        "exists" => os_path_predicate("exists", args[0], |_| true),
        "isfile" => os_path_predicate("isfile", args[0], |metadata| metadata.is_file()),
        "isdir" => os_path_predicate("isdir", args[0], |metadata| metadata.is_dir()),
        "isabs" => os_path_isabs(args[0]),
        "abspath" => os_path_abspath(args[0]),
        "splitext" => os_path_splitext_pair(args[0]),
        "split" => os_path_split_pair(args[0]),
        "normcase" => os_path_normcase(args[0]),
        "realpath" => os_path_realpath(args[0], args.get(1).is_some_and(|v| value::truthy(*v))),
        _ => missing_attribute("os.path", function),
    }
}

// os

/// `os.stat(path, *, dir_fd=None, follow_symlinks=True)`: the metadata of
/// `path` as an `os.stat_result`-shaped namespace.
///
/// The frontend reorders keyword spellings into the positional slots (padding
/// omitted ones with `None`), so `args` may hold up to three entries. The
/// default follows symlinks (`std::fs::metadata`); `follow_symlinks` false
/// inspects the link itself. `dir_fd` has no runtime representation, so only
/// `None` is accepted. Failures raise the CPython OSError subclass matching the
/// `io::ErrorKind`, worded as `[Errno N] message: 'path'`.
fn os_stat(args: &[i64]) -> i64 {
    let Some(path_value) = args.first().copied() else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "stat() missing required argument 'path' (pos 1)",
        );
    };
    if let Some(dir_fd) = args.get(1).copied() {
        if dir_fd != NONE {
            return exc::raise_with_message(
                exc::NOT_IMPLEMENTED_ERROR,
                "os.stat: dir_fd is not supported",
            );
        }
    }
    let follow_symlinks = match args.get(2).copied() {
        None | Some(NONE) => true,
        Some(flag) => value::truthy(flag),
    };
    let Some(path) = value::as_path_like(path_value) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "stat() argument must be a path string, not {}",
                value::type_name(path_value)
            ),
        );
    };
    let result = if follow_symlinks {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    };
    match result {
        Ok(metadata) => stat_result(&metadata),
        Err(error) => os_stat_error(error, path),
    }
}

/// Appends one `os.stat_result` field to `namespace`.
///
/// The field value is rooted across the mutation so a collection triggered
/// while the container lock is held cannot free it; the namespace itself is
/// rooted by the caller until every field is stored.
fn stat_field(namespace: i64, name: &str, field: i64) {
    let roots = crate::heap::root_values(&[field]);
    if let Some(mut state) = value::namespace_pairs_mut(namespace) {
        state.pairs.push((name.to_string(), field));
    }
    crate::heap::write_barrier_edge(namespace, field);
    crate::heap::release_roots(roots);
}

/// Encodes a metadata integer, promoting past the tagged-integer range.
fn stat_int(number: i128) -> i64 {
    match i64::try_from(number) {
        Ok(small) => {
            value::encode_int(small).unwrap_or_else(|| value::bigint_new(BigInt::from(small)))
        }
        Err(_) => value::bigint_new(BigInt::from(number)),
    }
}

/// Builds the `os.stat_result` namespace. The `st_*_ns` names are the exact
/// integer aliases of the float second fields, like CPython's `stat_result`.
#[cfg(unix)]
fn stat_result(metadata: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    let atime_ns = i128::from(metadata.atime()) * 1_000_000_000 + i128::from(metadata.atime_nsec());
    let mtime_ns = i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec());
    let ctime_ns = i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec());
    let seconds = |nanos: i128| value::float_new(nanos as f64 / 1_000_000_000.0);
    let namespace = value::namespace_new(Vec::new());
    let roots = crate::heap::root_values(&[namespace]);
    stat_field(namespace, "st_mode", stat_int(i128::from(metadata.mode())));
    stat_field(namespace, "st_ino", stat_int(i128::from(metadata.ino())));
    stat_field(namespace, "st_dev", stat_int(i128::from(metadata.dev())));
    stat_field(
        namespace,
        "st_nlink",
        stat_int(i128::from(metadata.nlink())),
    );
    stat_field(namespace, "st_uid", stat_int(i128::from(metadata.uid())));
    stat_field(namespace, "st_gid", stat_int(i128::from(metadata.gid())));
    stat_field(namespace, "st_size", stat_int(i128::from(metadata.size())));
    stat_field(namespace, "st_atime", seconds(atime_ns));
    stat_field(namespace, "st_mtime", seconds(mtime_ns));
    stat_field(namespace, "st_ctime", seconds(ctime_ns));
    stat_field(namespace, "st_atime_ns", stat_int(atime_ns));
    stat_field(namespace, "st_mtime_ns", stat_int(mtime_ns));
    stat_field(namespace, "st_ctime_ns", stat_int(ctime_ns));
    stat_field(namespace, "st_rdev", stat_int(i128::from(metadata.rdev())));
    stat_field(
        namespace,
        "st_blksize",
        stat_int(i128::from(metadata.blksize())),
    );
    stat_field(
        namespace,
        "st_blocks",
        stat_int(i128::from(metadata.blocks())),
    );
    crate::heap::release_roots(roots);
    namespace
}

/// Non-Unix `os.stat_result`: the portable metadata fields, with zero for the
/// POSIX-only identities.
#[cfg(not(unix))]
fn stat_result(metadata: &std::fs::Metadata) -> i64 {
    use std::time::UNIX_EPOCH;
    let seconds = |time: std::io::Result<SystemTime>| {
        time.ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0.0, |duration| duration.as_secs_f64())
    };
    let atime = seconds(metadata.accessed());
    let mtime = seconds(metadata.modified());
    let ctime = seconds(metadata.created());
    let mode = if metadata.is_dir() {
        0o040755
    } else {
        0o100644
    };
    let nanos = |seconds: f64| stat_int((seconds * 1_000_000_000.0) as i128);
    let namespace = value::namespace_new(Vec::new());
    let roots = crate::heap::root_values(&[namespace]);
    stat_field(namespace, "st_mode", encode_int(mode));
    stat_field(namespace, "st_ino", encode_int(0));
    stat_field(namespace, "st_dev", encode_int(0));
    stat_field(namespace, "st_nlink", encode_int(1));
    stat_field(namespace, "st_uid", encode_int(0));
    stat_field(namespace, "st_gid", encode_int(0));
    stat_field(namespace, "st_size", stat_int(i128::from(metadata.len())));
    stat_field(namespace, "st_atime", value::float_new(atime));
    stat_field(namespace, "st_mtime", value::float_new(mtime));
    stat_field(namespace, "st_ctime", value::float_new(ctime));
    stat_field(namespace, "st_atime_ns", nanos(atime));
    stat_field(namespace, "st_mtime_ns", nanos(mtime));
    stat_field(namespace, "st_ctime_ns", nanos(ctime));
    stat_field(namespace, "st_rdev", encode_int(0));
    stat_field(namespace, "st_blksize", encode_int(4096));
    stat_field(namespace, "st_blocks", encode_int(0));
    crate::heap::release_roots(roots);
    namespace
}

/// Raises the OSError subclass matching `error` with CPython's
/// `[Errno N] message: 'path'` wording.
fn os_stat_error(error: std::io::Error, path: &str) -> i64 {
    let tag = match error.kind() {
        std::io::ErrorKind::NotFound => exc::FILE_NOT_FOUND_ERROR,
        std::io::ErrorKind::PermissionDenied => exc::PERMISSION_ERROR,
        _ => OS_ERROR,
    };
    let text = error.to_string();
    let detail = text.split(" (os error").next().unwrap_or(text.as_str());
    match error.raw_os_error() {
        Some(code) => exc::raise_with_message(tag, &format!("[Errno {code}] {detail}: '{path}'")),
        None => exc::raise_with_message(tag, &format!("{detail}: '{path}'")),
    }
}

fn os_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "getpid" => encode_int(i64::from(std::process::id())),
        "getppid" => encode_int(parent_pid()),
        "cpu_count" => encode_int(cpu_count()),
        "getcwd" => match std::env::current_dir() {
            Ok(path) => value::str_alloc(path.to_string_lossy().into_owned()),
            Err(err) => {
                exc::raise_with_message(crate::exc::RUNTIME_ERROR, &format!("getcwd: {err}"))
            }
        },
        "getenv" => getenv(args),
        "fork" => match process::fork() {
            Ok(0) => {
                // The child only has the calling thread; rebuild the runtime
                // bookkeeping before returning 0 to user code.
                process::child_after_fork();
                encode_int(0)
            }
            Ok(pid) => encode_int(pid),
            Err(message) => exc::raise_with_message(RUNTIME_ERROR, &format!("fork: {message}")),
        },
        "waitpid" => {
            let Some(pid) = value::as_integer(args[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("pid must be an integer, not {}", value::type_name(args[0])),
                );
            };
            let Some(options) = value::as_integer(args[1]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "options must be an integer, not {}",
                        value::type_name(args[1])
                    ),
                );
            };
            match process::os_waitpid(pid, options) {
                Ok((reaped, status)) => {
                    let reaped = encode_int(reaped);
                    let status = encode_int(status);
                    if failed() {
                        return NONE;
                    }
                    value::tuple_new(vec![reaped, status])
                }
                Err(message) => {
                    exc::raise_with_message(RUNTIME_ERROR, &format!("waitpid: {message}"))
                }
            }
        }
        "_exit" => {
            let Some(code) = value::as_integer(args[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "_exit() argument must be an integer, not {}",
                        value::type_name(args[0])
                    ),
                );
            };
            process::exit_now(code as i32)
        }
        "remove" => {
            let Some(path) = value::as_path_like(args[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "remove() argument must be a path string, not {}",
                        value::type_name(args[0])
                    ),
                );
            };
            match std::fs::remove_file(path) {
                Ok(()) => NONE,
                Err(error) => exc::raise_with_message(
                    crate::exc::OS_ERROR,
                    &format!("remove: {error}: '{}'", path),
                ),
            }
        }
        "rename" => {
            let (Some(from), Some(to)) =
                (value::as_path_like(args[0]), value::as_path_like(args[1]))
            else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "rename() arguments must be path strings",
                );
            };
            match std::fs::rename(from, to) {
                Ok(()) => NONE,
                Err(error) => {
                    exc::raise_with_message(crate::exc::OS_ERROR, &format!("rename: {error}"))
                }
            }
        }
        "access" => os_access(args),
        "fspath" => os_fspath(args[0]),
        "stat" => os_stat(args),
        "sysconf" => os_sysconf(args),
        "getloadavg" => os_getloadavg(),
        "uname" => os_uname(),
        "open" => os_open(args),
        "dup2" => os_dup2(args),
        "get_terminal_size" => os_get_terminal_size(args),
        _ => missing_attribute("os", function),
    }
}

/// Configuration names accepted by `os.sysconf`. `None` means the name is
/// unknown to this runtime.
#[cfg(target_os = "linux")]
fn sysconf_number(name: &str) -> Option<i64> {
    let number = match name {
        "SC_PAGE_SIZE" | "SC_PAGESIZE" => libc::_SC_PAGESIZE,
        "SC_PHYS_PAGES" => libc::_SC_PHYS_PAGES,
        "SC_AVPHYS_PAGES" => libc::_SC_AVPHYS_PAGES,
        "SC_NPROCESSORS_ONLN" => libc::_SC_NPROCESSORS_ONLN,
        "SC_NPROCESSORS_CONF" => libc::_SC_NPROCESSORS_CONF,
        "SC_CLK_TCK" => libc::_SC_CLK_TCK,
        "SC_ARG_MAX" => libc::_SC_ARG_MAX,
        "SC_OPEN_MAX" => libc::_SC_OPEN_MAX,
        _ => return None,
    };
    Some(i64::from(number))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn sysconf_number(name: &str) -> Option<i64> {
    match name {
        "SC_PAGE_SIZE" | "SC_PAGESIZE" => Some(i64::from(libc::_SC_PAGESIZE)),
        _ => None,
    }
}

/// `os.sysconf(name)`: accepts an integer configuration number (like CPython)
/// or one of the common `SC_*` names.
#[cfg(unix)]
fn os_sysconf(args: &[i64]) -> i64 {
    let argument = args[0];
    let number = match value::as_integer(argument) {
        Some(number) => number,
        None => match value::as_str(argument) {
            Some(name) => match sysconf_number(name) {
                Some(number) => number,
                None => {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("unrecognized configuration name: {name}"),
                    )
                }
            },
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "integer argument expected, got {}",
                        value::type_name(argument)
                    ),
                )
            }
        },
    };
    // SAFETY: `sysconf` only reads the configuration number; every value is
    // valid input and an invalid one reports -1 with `errno`.
    let result = unsafe { libc::sysconf(number as libc::c_int) };
    if result < 0 {
        return exc::raise_with_message(OS_ERROR, "invalid configuration name");
    }
    encode_int(result as i64)
}

#[cfg(not(unix))]
fn os_sysconf(_args: &[i64]) -> i64 {
    exc::raise_with_message(
        RUNTIME_ERROR,
        "os.sysconf is not supported on this platform",
    )
}

/// `os.getloadavg()`: the three 1/5/15-minute load averages.
#[cfg(unix)]
fn os_getloadavg() -> i64 {
    let mut averages = [0.0f64; 3];
    // SAFETY: the pointer is valid for three `f64` values.
    let count = unsafe { libc::getloadavg(averages.as_mut_ptr(), 3) };
    if count < 0 {
        return exc::raise_with_message(OS_ERROR, "getloadavg() failed");
    }
    let count = (count as usize).min(averages.len());
    let values: Vec<i64> = averages[..count]
        .iter()
        .map(|average| value::float_new(*average))
        .collect();
    value::tuple_new(values)
}

#[cfg(not(unix))]
fn os_getloadavg() -> i64 {
    exc::raise_with_message(
        RUNTIME_ERROR,
        "os.getloadavg is not supported on this platform",
    )
}

/// `os.uname()` on POSIX: the five identity strings from `uname(2)`.
#[cfg(unix)]
fn os_uname() -> i64 {
    let mut info: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `uname` initialises every field of the provided `utsname`.
    if unsafe { libc::uname(&mut info) } != 0 {
        return os_error_from_io("uname", std::io::Error::last_os_error(), None);
    }
    let field = |bytes: &[libc::c_char]| -> String {
        // SAFETY: `uname` NUL-terminates each field.
        unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    uname_result([
        ("sysname", field(&info.sysname)),
        ("nodename", field(&info.nodename)),
        ("release", field(&info.release)),
        ("version", field(&info.version)),
        ("machine", field(&info.machine)),
    ])
}

/// `os.uname()` elsewhere: the portable platform helpers.
#[cfg(not(unix))]
fn os_uname() -> i64 {
    uname_result([
        ("sysname", platform_system().to_string()),
        ("nodename", platform_node()),
        ("release", platform_release()),
        ("version", platform_version()),
        ("machine", platform_machine().to_string()),
    ])
}

/// Builds the `os.uname()` value: a `uname_result`-shaped namedtuple instance
/// with CPython's five fields.
///
/// `typing.NamedTuple` instances expose their fields as attributes and iterate
/// in field order, so `os.uname().release` and
/// `system, node, release, version, machine = os.uname()` both work. The
/// `__len__`/`__getitem__` methods make the value index like the tuple
/// subclass CPython returns.
fn uname_result(fields: [(&str, String); 5]) -> i64 {
    // The marker base is what makes an ordinary class instance iterate in
    // attribute order (`value::namedtuple_values`).
    let marker = value::class_new("NamedTuple".to_string(), NONE, Vec::new());
    let roots = crate::heap::root_values(&[marker]);
    let methods = vec![
        (
            "__len__".to_string(),
            value::function_new(uname_len as *const () as usize, 1, "__len__".to_string()),
        ),
        (
            "__getitem__".to_string(),
            value::function_new(
                uname_getitem as *const () as usize,
                2,
                "__getitem__".to_string(),
            ),
        ),
    ];
    let class = value::class_new("uname_result".to_string(), marker, methods);
    crate::heap::root_values(&[class]);
    let result = value::instance_new(class);
    crate::heap::root_values(&[result]);
    for (name, text) in fields {
        let field = value::str_alloc(text);
        value::instance_setattr(result, name.to_string(), field);
    }
    crate::heap::release_roots(roots);
    result
}

/// Field order of `uname_result`, matching CPython's `posix.uname_result`.
const UNAME_FIELDS: [&str; 5] = ["sysname", "nodename", "release", "version", "machine"];

/// `uname_result.__len__`: always five fields.
extern "C" fn uname_len(_argv: *const i64) -> i64 {
    // 5 always fits the tagged-integer range.
    value::encode_int(UNAME_FIELDS.len() as i64).expect("five is a tagged int")
}

/// `uname_result.__getitem__`: positional access and negative indices.
extern "C" fn uname_getitem(argv: *const i64) -> i64 {
    // SAFETY: a method call passes `(self, index)` in that order.
    let object = unsafe { *argv };
    let index_value = unsafe { *argv.add(1) };
    let Some(index) = value::as_integer(index_value) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "uname_result indices must be integers, not slices",
        );
    };
    let index = if index < 0 {
        index + UNAME_FIELDS.len() as i64
    } else {
        index
    };
    let Some(name) = usize::try_from(index)
        .ok()
        .and_then(|position| UNAME_FIELDS.get(position).copied())
    else {
        return exc::raise_with_message(INDEX_ERROR, "uname_result index out of range");
    };
    value::instance_attr(object, name).unwrap_or(NONE)
}

/// `os.get_terminal_size(fd=1)`: the `columns`/`lines` of the terminal behind
/// `fd`, as a namespace. Raises `OSError` when `fd` is not a terminal.
#[cfg(unix)]
fn os_get_terminal_size(args: &[i64]) -> i64 {
    let fd = match args.first() {
        None => 1,
        Some(arg) => match value::as_integer(*arg) {
            Some(fd) => fd,
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "get_terminal_size() argument must be an integer, not {}",
                        value::type_name(*arg)
                    ),
                )
            }
        },
    };
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: `TIOCGWINSZ` writes one `winsize` through the valid pointer.
    let result = unsafe {
        libc::ioctl(
            fd as libc::c_int,
            libc::TIOCGWINSZ,
            &mut size as *mut libc::winsize,
        )
    };
    if result != 0 {
        return os_error_from_io("get_terminal_size", std::io::Error::last_os_error(), None);
    }
    value::namespace_new(vec![
        ("columns".to_string(), encode_int(i64::from(size.ws_col))),
        ("lines".to_string(), encode_int(i64::from(size.ws_row))),
    ])
}

#[cfg(not(unix))]
fn os_get_terminal_size(_args: &[i64]) -> i64 {
    exc::raise_with_message(
        RUNTIME_ERROR,
        "os.get_terminal_size is not supported on this platform",
    )
}

/// `os.open(path, flags, mode=0o777)`: opens a file and returns the new
/// descriptor. The caller owns it; `os.close` is not part of this slice.
fn os_open(args: &[i64]) -> i64 {
    let Some(path) = value::as_path_like(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "open() argument must be a path string, not {}",
                value::type_name(args[0])
            ),
        );
    };
    let Some(flags) = value::as_integer(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "open() flags must be an integer, not {}",
                value::type_name(args[1])
            ),
        );
    };
    let mode = match args.get(2) {
        None => 0o777,
        Some(arg) => match value::as_integer(*arg) {
            Some(mode) => mode,
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "open() mode must be an integer, not {}",
                        value::type_name(*arg)
                    ),
                )
            }
        },
    };
    let Ok(c_path) = CString::new(path) else {
        return exc::raise_with_message(VALUE_ERROR, "embedded null byte");
    };
    // SAFETY: `c_path` is NUL-terminated and both flags values are integers;
    // the variadic mode is only read when `O_CREAT` is set.
    let fd = unsafe { libc::open(c_path.as_ptr(), flags as libc::c_int, mode as libc::c_uint) };
    if fd < 0 {
        return os_error_from_io("open", std::io::Error::last_os_error(), Some(path));
    }
    encode_int(i64::from(fd))
}

/// `os.dup2(fd, fd2)`: duplicates `fd` onto `fd2` and returns `fd2`.
fn os_dup2(args: &[i64]) -> i64 {
    let (Some(fd), Some(target)) = (value::as_integer(args[0]), value::as_integer(args[1])) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "dup2() arguments must be file descriptors (integers)",
        );
    };
    // SAFETY: both arguments are plain descriptors; the kernel validates them.
    let result = unsafe { libc::dup2(fd as libc::c_int, target as libc::c_int) };
    if result < 0 {
        return os_error_from_io("dup2", std::io::Error::last_os_error(), None);
    }
    encode_int(i64::from(result))
}

/// Raises `OSError` in CPython's `[Errno N] message[: 'path']` form and
/// returns the `None` dummy value.
fn os_error_from_io(function: &str, error: std::io::Error, path: Option<&str>) -> i64 {
    let text = error.to_string();
    let detail = text.split(" (os error").next().unwrap_or(text.as_str());
    let suffix = match path {
        Some(path) => format!(": '{path}'"),
        None => String::new(),
    };
    match error.raw_os_error() {
        Some(code) => exc::raise_with_message(
            OS_ERROR,
            &format!("{function}: [Errno {code}] {detail}{suffix}"),
        ),
        None => exc::raise_with_message(OS_ERROR, &format!("{function}: {detail}{suffix}")),
    }
}

/// `os.access(path, mode)`: Linux permission bits, no exception on failure.
fn os_access(args: &[i64]) -> i64 {
    let Some(path) = value::as_path_like(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "access() argument must be a path string, not {}",
                value::type_name(args[0])
            ),
        );
    };
    let Some(mode) = value::as_integer(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "access() mode must be an integer, not {}",
                value::type_name(args[1])
            ),
        );
    };
    // F_OK / X_OK / W_OK / R_OK only; any other mode is not supported.
    let mode = match mode {
        0 => libc::F_OK,
        1 => libc::X_OK,
        2 => libc::W_OK,
        4 => libc::R_OK,
        _ => return FALSE,
    };
    let Ok(path) = std::ffi::CString::new(path) else {
        return FALSE;
    };
    // SAFETY: `path` is a valid NUL-terminated C string and `mode` is one of
    // the `access(2)` flags above.
    let accessible = unsafe { libc::access(path.as_ptr(), mode) } == 0;
    if accessible {
        TRUE
    } else {
        FALSE
    }
}

/// `os.fspath(path)`: `str` and `bytes` pass through unchanged; any other
/// value must be path-like, and its `__fspath__()` result (str or bytes) is
/// returned. `pathlib.Path` carries its text in a native payload rather than a
/// `__fspath__` method value, so it is answered directly.
fn os_fspath(path: i64) -> i64 {
    if value::as_str(path).is_some() || value::as_bytes(path).is_some() {
        return path;
    }
    if let Some(state) = value::as_path(path) {
        return value::str_alloc(state.path().to_string());
    }
    if let Some(instance) = value::as_instance(path) {
        if let Some(value::ClassEntry::Method(kind, function)) =
            value::class_lookup(instance.class, "__fspath__")
        {
            // Special methods go through the normal descriptor protocol: an
            // ordinary or `@property` method binds the instance, a
            // `@classmethod` binds the class, and a `@staticmethod` binds
            // nothing.
            let argv: Vec<i64> = match kind {
                hydro_ir::MethodKind::Normal | hydro_ir::MethodKind::Property => vec![path],
                hydro_ir::MethodKind::Classmethod => vec![instance.class],
                hydro_ir::MethodKind::Staticmethod => Vec::new(),
            };
            let result = crate::core::builtins::call(function, argv.len(), argv.as_ptr());
            if exc::pending() {
                return NONE;
            }
            if value::as_str(result).is_some() || value::as_bytes(result).is_some() {
                return result;
            }
            let ty = value::type_name_of(result);
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("expected __fspath__() to return str or bytes, not {ty}"),
            );
        }
    }
    let ty = value::type_name_of(path);
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("expected str, bytes or os.PathLike object, not {ty}"),
    )
}

// sys

fn sys_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "exit" => sys_exit(args),
        "exc_info" => exc::exc_info(),
        "excepthook" => sys_excepthook(args),
        "getrecursionlimit" => sys_getrecursionlimit(),
        "setrecursionlimit" => sys_setrecursionlimit(args),
        "_getframemodulename" => sys_getframemodulename(args),
        "_getframe" => sys_getframe(args),
        _ => missing_attribute("sys", function),
    }
}

/// CPython's default recursion limit.
const DEFAULT_RECURSION_LIMIT: usize = 1000;

/// Process-wide recursion limit, read by `sys.getrecursionlimit` and written
/// by `sys.setrecursionlimit`. It is a real process-global setting (repeated
/// calls observe the stored value), matching CPython's model where every
/// thread shares one limit.
static RECURSION_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_RECURSION_LIMIT);

/// The current process-wide recursion limit.
pub(crate) fn recursion_limit() -> usize {
    RECURSION_LIMIT.load(Ordering::Relaxed)
}

/// Encodes a `usize` limit as a Python integer (tagged when it fits, a bigint
/// otherwise).
fn recursion_limit_value(limit: usize) -> i64 {
    match i64::try_from(limit).ok().map(encode_int) {
        Some(value) => value,
        None => value::bigint_new(BigInt::from(limit)),
    }
}

fn sys_getrecursionlimit() -> i64 {
    recursion_limit_value(recursion_limit())
}

/// `sys.setrecursionlimit(limit)`: validates the new limit like CPython and
/// stores it for the process. The limit must be an integer greater than zero
/// (and above the current recursion depth, which is 0 here).
fn sys_setrecursionlimit(args: &[i64]) -> i64 {
    let limit = args.first().copied().unwrap_or(NONE);
    let Some(limit_value) = value::to_bigint(limit) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "'{}' object cannot be interpreted as an integer",
                value::type_name(limit)
            ),
        );
    };
    let Some(limit) = limit_value.to_usize() else {
        return exc::raise_with_message(
            RECURSION_ERROR,
            &format!(
                "cannot set the recursion limit to {limit_value} at the recursion depth 0: \
                 the limit is too low"
            ),
        );
    };
    if limit == 0 {
        return exc::raise_with_message(
            RECURSION_ERROR,
            "cannot set the recursion limit to 0 at the recursion depth 0: the limit is too low",
        );
    }
    RECURSION_LIMIT.store(limit, Ordering::Relaxed);
    NONE
}

/// `sys._getframemodulename(depth=0)`.
///
/// Hydro has no reified frame objects, so a caller's module name cannot be
/// observed. `None` is the truthful answer (CPython also returns `None` when
/// the stack is shallower than `depth`); an invalid depth is still rejected
/// with CPython's `TypeError` instead of being ignored.
fn sys_getframemodulename(args: &[i64]) -> i64 {
    if let Some(&depth) = args.first() {
        if let Err(raised) = frame_depth(depth) {
            return raised;
        }
    }
    NONE
}

/// Validates the optional `depth` argument of the `sys` frame APIs.
///
/// CPython converts the value to a C `int`: an integer that does not fit is an
/// `OverflowError`, any other type a `TypeError` naming its type.
fn frame_depth(depth: i64) -> Result<i64, i64> {
    if let Some(depth) = value::as_integer(depth) {
        return Ok(depth);
    }
    if value::to_bigint(depth).is_some() {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "Python int too large to convert to C int",
        ));
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(depth)
        ),
    ))
}

/// `sys._getframe(depth=0)`.
///
/// A frame object cannot be represented, so instead of fabricating one the
/// call fails exactly like CPython when the requested depth lies beyond the
/// available stack: `ValueError: call stack is not deep enough`. The runtime
/// keeps no Python stack frames, so every depth is beyond it.
fn sys_getframe(args: &[i64]) -> i64 {
    if let Some(&depth) = args.first() {
        if let Err(raised) = frame_depth(depth) {
            return raised;
        }
    }
    exc::raise_with_message(VALUE_ERROR, "call stack is not deep enough")
}

/// `sys.excepthook(type, value, traceback)`: prints the exception's traceback
/// to stderr and returns `None`; `type=None` prints nothing.
fn sys_excepthook(args: &[i64]) -> i64 {
    let exc_type = args.first().copied().unwrap_or(NONE);
    let value = args.get(1).copied().unwrap_or(NONE);
    let traceback = args.get(2).copied().unwrap_or(NONE);
    exc::excepthook(exc_type, value, traceback)
}

/// `sys.exit(code=None)`: raises `SystemExit`, which `try/except
/// SystemExit` can catch. Uncaught, `None` exits 0, an integer exits with
/// that status and a string is reported on stderr with status 1.
fn sys_exit(args: &[i64]) -> i64 {
    let code = args.first().copied().unwrap_or(NONE);
    exc::raise_exit(code)
}

// io

/// `io.open(file, mode="r", encoding=None, newline=None, errors=None)`.
///
/// UTF-8 text mode (`r`, `w`, `a`, `x`, optionally with `t`) and binary mode
/// (the same letters with `b`) are implemented. A keyword call arrives with
/// `None` in every omitted slot (the frontend fills the gaps), so `None` means
/// "use the default" for every optional argument. Universal-newline
/// translation is not implemented: `newline` is accepted and ignored for text
/// mode, as are `errors`. Binary mode rejects `encoding`, `newline` and
/// `errors` with `ValueError`, like CPython.
fn io_call(function: &str, args: &[i64]) -> i64 {
    if let Some(method) = function.strip_prefix("StringIO.") {
        return string_io_method(method, args);
    }
    if let Some(method) = function.strip_prefix("BytesIO.") {
        return bytes_io_method(method, args);
    }
    if let Some(method) = function.strip_prefix("TextIOWrapper.") {
        return text_io_wrapper_method(method, args);
    }
    match function {
        "open" => io_open(args),
        "StringIO" => string_io_new(args),
        "BytesIO" => bytes_io_new(args),
        "TextIOWrapper" => text_io_wrapper_new(args),
        _ => missing_attribute("io", function),
    }
}

/// The cached `io.StringIO` / `io.BytesIO` classes. Native instances compare
/// by class identity, so every access must return the same class value.
fn in_memory_class(name: &'static str, methods: Vec<(String, i64)>) -> i64 {
    let key = format!("io.{name}");
    if let Some(cached) = globals::registry_get(&key) {
        return cached;
    }
    // The method table holds freshly allocated native functions; root them
    // while the class object itself is being allocated and registered.
    let method_values: Vec<i64> = methods.iter().map(|(_, value)| *value).collect();
    let method_roots = heap::root_values(&method_values);
    let class = value::class_new(name.to_string(), NONE, methods);
    let class_roots = heap::root_values(&[class]);
    globals::registry_set(&key, class);
    heap::release_roots(class_roots);
    heap::release_roots(method_roots);
    class
}

fn string_io_class() -> i64 {
    in_memory_class(
        "StringIO",
        vec![
            (
                "write".to_string(),
                value::native_function_new("io", "StringIO.write", 2),
            ),
            (
                "read".to_string(),
                value::native_function_new("io", "StringIO.read", 2),
            ),
            (
                "readline".to_string(),
                value::native_function_new("io", "StringIO.readline", 2),
            ),
            (
                "getvalue".to_string(),
                value::native_function_new("io", "StringIO.getvalue", 1),
            ),
            (
                "seek".to_string(),
                value::native_function_new("io", "StringIO.seek", 3),
            ),
            (
                "tell".to_string(),
                value::native_function_new("io", "StringIO.tell", 1),
            ),
            (
                "close".to_string(),
                value::native_function_new("io", "StringIO.close", 1),
            ),
            (
                "flush".to_string(),
                value::native_function_new("io", "StringIO.flush", 1),
            ),
            (
                "readable".to_string(),
                value::native_function_new("io", "StringIO.readable", 1),
            ),
            (
                "writable".to_string(),
                value::native_function_new("io", "StringIO.writable", 1),
            ),
            (
                "seekable".to_string(),
                value::native_function_new("io", "StringIO.seekable", 1),
            ),
            (
                "__enter__".to_string(),
                value::native_function_new("io", "StringIO.__enter__", 1),
            ),
            (
                "__exit__".to_string(),
                value::native_function_new("io", "StringIO.__exit__", 4),
            ),
        ],
    )
}

fn bytes_io_class() -> i64 {
    in_memory_class(
        "BytesIO",
        vec![
            (
                "write".to_string(),
                value::native_function_new("io", "BytesIO.write", 2),
            ),
            (
                "read".to_string(),
                value::native_function_new("io", "BytesIO.read", 2),
            ),
            (
                "readline".to_string(),
                value::native_function_new("io", "BytesIO.readline", 2),
            ),
            (
                "getvalue".to_string(),
                value::native_function_new("io", "BytesIO.getvalue", 1),
            ),
            (
                "seek".to_string(),
                value::native_function_new("io", "BytesIO.seek", 3),
            ),
            (
                "tell".to_string(),
                value::native_function_new("io", "BytesIO.tell", 1),
            ),
            (
                "close".to_string(),
                value::native_function_new("io", "BytesIO.close", 1),
            ),
            (
                "flush".to_string(),
                value::native_function_new("io", "BytesIO.flush", 1),
            ),
            (
                "readable".to_string(),
                value::native_function_new("io", "BytesIO.readable", 1),
            ),
            (
                "writable".to_string(),
                value::native_function_new("io", "BytesIO.writable", 1),
            ),
            (
                "seekable".to_string(),
                value::native_function_new("io", "BytesIO.seekable", 1),
            ),
            (
                "__enter__".to_string(),
                value::native_function_new("io", "BytesIO.__enter__", 1),
            ),
            (
                "__exit__".to_string(),
                value::native_function_new("io", "BytesIO.__exit__", 4),
            ),
        ],
    )
}

const IO_DATA: &str = "_hydro_io_data";
const IO_POS: &str = "_hydro_io_pos";
const IO_CLOSED: &str = "closed";

/// Raises `ValueError` when an in-memory stream has been closed.
fn io_open_check(instance: i64) -> Option<i64> {
    match value::instance_attr(instance, IO_CLOSED) {
        Some(closed) if closed == TRUE => Some(exc::raise_with_message(
            VALUE_ERROR,
            "I/O operation on closed file.",
        )),
        _ => None,
    }
}

fn io_store(instance: i64, name: &str, value: i64) {
    let roots = heap::root_values(&[value]);
    value::instance_setattr(instance, name.to_string(), value);
    heap::release_roots(roots);
}

fn io_data(instance: i64) -> i64 {
    value::instance_attr(instance, IO_DATA).unwrap_or(NONE)
}

fn io_pos(instance: i64) -> usize {
    value::instance_attr(instance, IO_POS)
        .and_then(value::as_integer)
        .and_then(|pos| usize::try_from(pos).ok())
        .unwrap_or(0)
}

fn io_set_pos(instance: i64, pos: usize) {
    io_store(instance, IO_POS, ops::int_result(pos as i64));
}

fn io_size_arg(value: i64) -> Result<i64, i64> {
    match value::as_integer(value) {
        Some(size) => Ok(size),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "argument should be integer or None, not {}",
                value::type_name(value)
            ),
        )),
    }
}

fn io_seek(instance: i64, args: &[i64], length: usize) -> i64 {
    if let Some(error) = io_open_check(instance) {
        return error;
    }
    let offset = match args.get(1).copied() {
        Some(value) => match value::as_integer(value) {
            Some(offset) => offset,
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("integer argument expected, got {}", value::type_name(value)),
                )
            }
        },
        None => {
            return exc::raise_with_message(TYPE_ERROR, "seek() missing required argument 'pos'")
        }
    };
    let whence = match args.get(2).copied() {
        None | Some(NONE) => 0,
        Some(value) => match value::as_integer(value) {
            Some(whence) => whence,
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("integer argument expected, got {}", value::type_name(value)),
                )
            }
        },
    };
    let base = match whence {
        0 => 0,
        1 => io_pos(instance) as i64,
        2 => length as i64,
        other => {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("invalid whence ({other}, should be 0, 1 or 2)"),
            )
        }
    };
    let target = base.saturating_add(offset);
    if target < 0 {
        return exc::raise_with_message(VALUE_ERROR, &format!("negative seek position {target}"));
    }
    io_set_pos(instance, target as usize);
    ops::int_result(target)
}

fn io_tell(instance: i64) -> i64 {
    if let Some(error) = io_open_check(instance) {
        return error;
    }
    ops::int_result(io_pos(instance) as i64)
}

fn io_close(instance: i64) -> i64 {
    io_store(instance, IO_CLOSED, TRUE);
    NONE
}

fn io_enter(instance: i64) -> i64 {
    if let Some(error) = io_open_check(instance) {
        return error;
    }
    instance
}

fn io_exit(instance: i64) -> i64 {
    io_close(instance);
    FALSE
}

fn io_probe(instance: i64) -> i64 {
    if let Some(error) = io_open_check(instance) {
        return error;
    }
    TRUE
}

/// `io.StringIO(initial_value="", newline="\n")`.
fn string_io_new(args: &[i64]) -> i64 {
    let instance = value::instance_new(string_io_class());
    string_io_init(instance, args.first().copied())
}

fn string_io_init(instance: i64, initial: Option<i64>) -> i64 {
    let roots = heap::root_values(&[instance]);
    let result = string_io_init_attrs(instance, initial);
    heap::release_roots(roots);
    result
}

fn string_io_init_attrs(instance: i64, initial: Option<i64>) -> i64 {
    let initial = match initial {
        None | Some(NONE) => String::new(),
        Some(value) => match value::as_str(value) {
            Some(text) => text.to_string(),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "initial_value should be a string, not {}",
                        value::type_name(value)
                    ),
                )
            }
        },
    };
    io_store(instance, IO_DATA, value::str_from_bytes(initial.as_bytes()));
    io_set_pos(instance, 0);
    io_store(instance, IO_CLOSED, FALSE);
    instance
}

fn string_io_method(method: &str, args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, &format!("StringIO.{method}() missing self"));
    };
    match method {
        "write" => {
            if let Some(error) = io_open_check(instance) {
                return error;
            }
            let Some(value) = args.get(1).copied() else {
                return exc::raise_with_message(TYPE_ERROR, "write() missing required argument");
            };
            let Some(text) = value::as_str(value) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "string argument expected, got '{}'",
                        value::type_name(value)
                    ),
                );
            };
            let text = text.to_string();
            let mut chars: Vec<char> = value::as_str(io_data(instance))
                .unwrap_or_default()
                .chars()
                .collect();
            let pos = io_pos(instance);
            while chars.len() < pos {
                chars.push('\0');
            }
            let written = text.chars().count();
            for (index, ch) in text.chars().enumerate() {
                let at = pos + index;
                if at < chars.len() {
                    chars[at] = ch;
                } else {
                    chars.push(ch);
                }
            }
            io_store(
                instance,
                IO_DATA,
                value::str_alloc(chars.into_iter().collect()),
            );
            io_set_pos(instance, pos + written);
            ops::int_result(written as i64)
        }
        "read" | "readline" => {
            if let Some(error) = io_open_check(instance) {
                return error;
            }
            let data: Vec<char> = value::as_str(io_data(instance))
                .unwrap_or_default()
                .chars()
                .collect();
            let pos = io_pos(instance).min(data.len());
            let limit = match args.get(1).copied() {
                None | Some(NONE) => -1,
                Some(value) => match io_size_arg(value) {
                    Ok(size) => size,
                    Err(error) => return error,
                },
            };
            let end = if method == "readline" {
                let mut end = data.len();
                if limit >= 0 {
                    end = end.min(pos.saturating_add(limit as usize));
                }
                match data[pos..end].iter().position(|ch| *ch == '\n') {
                    Some(offset) => pos + offset + 1,
                    None => end,
                }
            } else if limit < 0 {
                data.len()
            } else {
                data.len().min(pos.saturating_add(limit as usize))
            };
            let text: String = data[pos..end].iter().collect();
            io_set_pos(instance, end);
            value::str_alloc(text)
        }
        "getvalue" => {
            if let Some(error) = io_open_check(instance) {
                return error;
            }
            match value::instance_attr(instance, IO_DATA) {
                Some(data) => data,
                None => value::str_alloc(String::new()),
            }
        }
        "seek" => {
            let length = value::as_str(io_data(instance))
                .unwrap_or_default()
                .chars()
                .count();
            io_seek(instance, args, length)
        }
        "tell" => io_tell(instance),
        "close" => io_close(instance),
        "flush" => match io_open_check(instance) {
            Some(error) => error,
            None => NONE,
        },
        "readable" | "writable" | "seekable" => io_probe(instance),
        "__enter__" => io_enter(instance),
        "__exit__" => io_exit(instance),
        _ => missing_attribute("io", method),
    }
}

/// `io.BytesIO(initial_bytes=b"")`.
fn bytes_io_new(args: &[i64]) -> i64 {
    let instance = value::instance_new(bytes_io_class());
    bytes_io_init(instance, args.first().copied())
}

fn bytes_io_init(instance: i64, initial: Option<i64>) -> i64 {
    let roots = heap::root_values(&[instance]);
    let result = bytes_io_init_attrs(instance, initial);
    heap::release_roots(roots);
    result
}

fn bytes_io_init_attrs(instance: i64, initial: Option<i64>) -> i64 {
    let initial = match initial {
        None | Some(NONE) => Vec::new(),
        Some(value) => match value::as_bytes(value) {
            Some(bytes) => bytes.to_vec(),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "a bytes-like object is required, not '{}'",
                        value::type_name(value)
                    ),
                )
            }
        },
    };
    io_store(instance, IO_DATA, value::bytes_new(initial));
    io_set_pos(instance, 0);
    io_store(instance, IO_CLOSED, FALSE);
    instance
}

fn bytes_io_method(method: &str, args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, &format!("BytesIO.{method}() missing self"));
    };
    match method {
        "write" => {
            if let Some(error) = io_open_check(instance) {
                return error;
            }
            let Some(value) = args.get(1).copied() else {
                return exc::raise_with_message(TYPE_ERROR, "write() missing required argument");
            };
            let Some(data) = value::as_bytes(value) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "a bytes-like object is required, not '{}'",
                        value::type_name(value)
                    ),
                );
            };
            let data = data.to_vec();
            let mut bytes: Vec<u8> = value::as_bytes(io_data(instance)).unwrap_or(&[]).to_vec();
            let pos = io_pos(instance);
            while bytes.len() < pos {
                bytes.push(0);
            }
            for (index, byte) in data.iter().enumerate() {
                let at = pos + index;
                if at < bytes.len() {
                    bytes[at] = *byte;
                } else {
                    bytes.push(*byte);
                }
            }
            let written = data.len();
            io_store(instance, IO_DATA, value::bytes_new(bytes));
            io_set_pos(instance, pos + written);
            ops::int_result(written as i64)
        }
        "read" | "readline" => {
            if let Some(error) = io_open_check(instance) {
                return error;
            }
            let data: Vec<u8> = value::as_bytes(io_data(instance)).unwrap_or(&[]).to_vec();
            let pos = io_pos(instance).min(data.len());
            let limit = match args.get(1).copied() {
                None | Some(NONE) => -1,
                Some(value) => match io_size_arg(value) {
                    Ok(size) => size,
                    Err(error) => return error,
                },
            };
            let end = if method == "readline" {
                let mut end = data.len();
                if limit >= 0 {
                    end = end.min(pos.saturating_add(limit as usize));
                }
                match data[pos..end].iter().position(|byte| *byte == b'\n') {
                    Some(offset) => pos + offset + 1,
                    None => end,
                }
            } else if limit < 0 {
                data.len()
            } else {
                data.len().min(pos.saturating_add(limit as usize))
            };
            let slice = data[pos..end].to_vec();
            io_set_pos(instance, end);
            value::bytes_new(slice)
        }
        "getvalue" => {
            if let Some(error) = io_open_check(instance) {
                return error;
            }
            match value::instance_attr(instance, IO_DATA) {
                Some(data) => data,
                None => value::bytes_new(Vec::new()),
            }
        }
        "seek" => {
            let length = value::as_bytes(io_data(instance)).unwrap_or(&[]).len();
            io_seek(instance, args, length)
        }
        "tell" => io_tell(instance),
        "close" => io_close(instance),
        "flush" => match io_open_check(instance) {
            Some(error) => error,
            None => NONE,
        },
        "readable" | "writable" | "seekable" => io_probe(instance),
        "__enter__" => io_enter(instance),
        "__exit__" => io_exit(instance),
        _ => missing_attribute("io", method),
    }
}

const TW_BUFFER: &str = "_hydro_tw_buffer";
const TW_ERRORS: &str = "_hydro_tw_errors";
const TW_NEWLINE: &str = "_hydro_tw_newline";
const TW_SEEN: &str = "_hydro_tw_seen";

/// The cached `io.TextIOWrapper` class. Instances are ordinary native class
/// instances; the wrapped file object lives in an instance attribute, so the
/// wrapper's lifetime keeps the buffer reachable.
fn text_io_wrapper_class() -> i64 {
    let native = |name: &'static str, arity: usize| value::native_function_new("io", name, arity);
    in_memory_class(
        "TextIOWrapper",
        vec![
            ("read".to_string(), native("TextIOWrapper.read", 2)),
            ("readline".to_string(), native("TextIOWrapper.readline", 2)),
            (
                "readlines".to_string(),
                native("TextIOWrapper.readlines", 2),
            ),
            ("write".to_string(), native("TextIOWrapper.write", 2)),
            (
                "writelines".to_string(),
                native("TextIOWrapper.writelines", 2),
            ),
            ("close".to_string(), native("TextIOWrapper.close", 1)),
            ("flush".to_string(), native("TextIOWrapper.flush", 1)),
            ("seek".to_string(), native("TextIOWrapper.seek", 3)),
            ("tell".to_string(), native("TextIOWrapper.tell", 1)),
            ("readable".to_string(), native("TextIOWrapper.readable", 1)),
            ("writable".to_string(), native("TextIOWrapper.writable", 1)),
            ("seekable".to_string(), native("TextIOWrapper.seekable", 1)),
            ("isatty".to_string(), native("TextIOWrapper.isatty", 1)),
            ("detach".to_string(), native("TextIOWrapper.detach", 1)),
            (
                "__enter__".to_string(),
                native("TextIOWrapper.__enter__", 1),
            ),
            ("__exit__".to_string(), native("TextIOWrapper.__exit__", 4)),
            ("__iter__".to_string(), native("TextIOWrapper.__iter__", 1)),
            ("__next__".to_string(), native("TextIOWrapper.__next__", 1)),
        ],
    )
}

/// Snapshot of the remaining buffer content and the position it started at.
struct TwRead {
    /// Decoded text (never newline-translated).
    text: String,
    /// Character position the read started at (bytes for a binary buffer).
    start: usize,
    /// Byte length of `text` in the buffer encoding (binary buffers only).
    bytes: usize,
    /// True when the wrapped object stores raw bytes.
    binary: bool,
}

fn tw_buffer(instance: i64) -> Option<i64> {
    value::instance_attr(instance, TW_BUFFER).filter(|buffer| *buffer != NONE)
}

fn tw_file(instance: i64) -> Option<&'static value::FileState> {
    value::as_file(tw_buffer(instance)?)
}

fn tw_newline(instance: i64) -> Option<String> {
    match value::instance_attr(instance, TW_NEWLINE) {
        Some(value) if value != NONE => value::as_str(value).map(str::to_string),
        _ => None,
    }
}

fn tw_errors(instance: i64) -> i64 {
    value::instance_attr(instance, TW_ERRORS).unwrap_or(NONE)
}

/// Raises `ValueError` when the wrapper or its buffer is closed.
fn tw_check_open(instance: i64) -> Option<i64> {
    let closed = value::instance_attr(instance, "closed") == Some(TRUE);
    let buffer = tw_buffer(instance);
    let buffer_closed = match buffer.and_then(value::as_file) {
        Some(file) => file.closed(),
        None => true,
    };
    if closed || buffer_closed {
        return Some(exc::raise_with_message(
            VALUE_ERROR,
            "I/O operation on closed file.",
        ));
    }
    None
}

/// `io.TextIOWrapper(buffer, encoding=None, errors=None, newline=None,
/// line_buffering=False, write_through=False)`.
fn text_io_wrapper_new(args: &[i64]) -> i64 {
    let Some(&buffer) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "TextIOWrapper() missing buffer argument");
    };
    let Some(file) = value::as_file(buffer) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "TextIOWrapper() buffer must be a file object, not {}",
                value::type_name(buffer)
            ),
        );
    };
    let encoding = match args.get(1).copied() {
        None | Some(NONE) => "utf-8".to_string(),
        Some(value) => {
            let Some(name) = value::as_str(value) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "TextIOWrapper() encoding must be str or None, not {}",
                        value::type_name(value)
                    ),
                );
            };
            let normalized = name.to_ascii_lowercase();
            if normalized != "utf-8" && normalized != "utf8" {
                return exc::raise_with_message(
                    VALUE_ERROR,
                    &format!("unsupported encoding: '{name}'"),
                );
            }
            "utf-8".to_string()
        }
    };
    let errors = match args.get(2).copied() {
        None | Some(NONE) => value::str_from_bytes(b"strict"),
        Some(value) => match value::as_str(value) {
            Some(_) => value,
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "TextIOWrapper() errors must be str or None, not {}",
                        value::type_name(value)
                    ),
                )
            }
        },
    };
    let newline = match args.get(3).copied() {
        None | Some(NONE) => NONE,
        Some(value) => {
            let Some(text) = value::as_str(value) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "TextIOWrapper() newline must be str or None, not {}",
                        value::type_name(value)
                    ),
                );
            };
            match text {
                "" | "\n" | "\r" | "\r\n" => value,
                _ => {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("illegal newline value: {text}"),
                    )
                }
            }
        }
    };
    let line_buffering = args.get(4).copied().is_some_and(value::truthy);
    let write_through = args.get(5).copied().is_some_and(value::truthy);
    // Snapshot the buffer's metadata before allocating the instance.
    let name = file.path().to_string();
    let mode = file.mode();
    let binary = file.binary();
    let instance = value::instance_new(text_io_wrapper_class());
    let roots = heap::root_values(&[instance, buffer]);
    io_store(instance, TW_BUFFER, buffer);
    io_store(instance, "buffer", buffer);
    io_store(instance, TW_ERRORS, errors);
    io_store(instance, TW_NEWLINE, newline);
    io_store(instance, TW_SEEN, encode_int(0));
    io_store(instance, "closed", FALSE);
    io_store(instance, "name", value::str_from_bytes(name.as_bytes()));
    io_store(
        instance,
        "mode",
        value::str_alloc(format!("{mode}{}", if binary { "b" } else { "" })),
    );
    io_store(
        instance,
        "encoding",
        value::str_from_bytes(encoding.as_bytes()),
    );
    io_store(instance, "errors", errors);
    io_store(instance, "newlines", NONE);
    io_store(
        instance,
        "line_buffering",
        value::bool_value(line_buffering),
    );
    io_store(instance, "write_through", value::bool_value(write_through));
    heap::release_roots(roots);
    instance
}

/// Decodes raw buffer bytes through the existing native UTF-8 codec so the
/// `strict`/`ignore`/`replace` (and registered) error handlers are the ones
/// `codecs` already implements.
fn tw_decode(data: &[u8], errors: i64) -> Result<String, i64> {
    let bytes = value::bytes_new(data.to_vec());
    let args = [bytes, errors, TRUE];
    let decoded = codecs::call("utf_8_decode", &args);
    if exc::pending() {
        return Err(NONE);
    }
    let Some(items) = value::as_tuple(decoded) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            "UTF-8 decoder returned a non-tuple",
        ));
    };
    match items.first().copied().and_then(value::as_str) {
        Some(text) => Ok(text.to_string()),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            "UTF-8 decoder returned a non-string",
        )),
    }
}

/// Reads the rest of the buffer from the current position. The cursor is left
/// at end-of-file; callers reposition it once they know the consumed prefix.
fn tw_read_remaining(instance: i64) -> Result<TwRead, i64> {
    if let Some(error) = tw_check_open(instance) {
        return Err(error);
    }
    let Some(file) = tw_file(instance) else {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            "I/O operation on closed file.",
        ));
    };
    let start = match file.tell() {
        Ok(position) => position,
        Err(error) => return Err(value::raise_file_error(error)),
    };
    let binary = file.binary();
    if binary {
        let data = match file.read_bytes(None) {
            Ok(data) => data,
            Err(error) => return Err(value::raise_file_error(error)),
        };
        let bytes = data.len();
        let text = tw_decode(&data, tw_errors(instance))?;
        Ok(TwRead {
            text,
            start,
            bytes,
            binary,
        })
    } else {
        match file.read(None) {
            Ok(text) => Ok(TwRead {
                text,
                start,
                bytes: 0,
                binary,
            }),
            Err(error) => Err(value::raise_file_error(error)),
        }
    }
}

/// Repositions the buffer to `start + consumed` so an under-read does not
/// discard unread data.
fn tw_reposition(instance: i64, start: usize, consumed: usize) -> Option<i64> {
    let file = tw_file(instance)?;
    let target = i64::try_from(start.saturating_add(consumed)).unwrap_or(value::MAX_INT);
    match file.seek(target, 0) {
        Ok(()) => None,
        Err(error) => Some(value::raise_file_error(error)),
    }
}

fn tw_consumed(read: &TwRead, prefix: &str, chars: usize) -> usize {
    if !read.binary {
        return chars;
    }
    if chars == read.text.chars().count() {
        read.bytes
    } else {
        // The buffer encoding is UTF-8, so re-encoding the consumed prefix
        // yields its byte length for valid input.
        prefix.len()
    }
}

/// Translates universal-newline input for the text layer: `newline=None`
/// folds `\r\n` and `\r` to `\n`, while `newline=''` and an explicit newline
/// leave the data untouched (CPython returns it untranslated too).
fn tw_translate_read(text: &str, newline: Option<&str>) -> String {
    if newline.is_some() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(character);
        }
    }
    out
}

/// Applies the write-side newline translation: `None` uses the platform line
/// separator, `''`/`'\n'` keep `\n`, and explicit `'\r'`/`'\r\n'` replace it.
fn tw_translate_write(text: &str, newline: Option<&str>) -> String {
    let replacement = match newline {
        None => {
            if cfg!(windows) {
                "\r\n"
            } else {
                "\n"
            }
        }
        Some("") | Some("\n") => "\n",
        Some("\r") => "\r",
        Some("\r\n") => "\r\n",
        // Validated by the constructor.
        Some(_) => "\n",
    };
    text.replace('\n', replacement)
}

/// Index one past the last character of the first line in `chars`.
fn tw_line_end(chars: &[char], newline: Option<&str>) -> usize {
    match newline {
        None | Some("") => {
            for (index, character) in chars.iter().enumerate() {
                if *character == '\n' {
                    return index + 1;
                }
                if *character == '\r' {
                    return if chars.get(index + 1) == Some(&'\n') {
                        index + 2
                    } else {
                        index + 1
                    };
                }
            }
            chars.len()
        }
        Some(separator) => {
            let target: Vec<char> = separator.chars().collect();
            if target.is_empty() {
                return chars.len();
            }
            for index in 0..chars.len() {
                if index + target.len() <= chars.len()
                    && &chars[index..index + target.len()] == target.as_slice()
                {
                    return index + target.len();
                }
            }
            chars.len()
        }
    }
}

/// Updates the `newlines` attribute with the newline kinds observed so far.
fn tw_record_newlines(instance: i64, text: &str) {
    // CPython tracks the observed line endings only in universal-newline
    // modes (`newline=None` or `newline=''`); with an explicit terminator the
    // attribute stays `None`.
    if let Some(separator) = tw_newline(instance) {
        if !separator.is_empty() {
            return;
        }
    }
    let mut seen = value::instance_attr(instance, TW_SEEN)
        .and_then(value::as_integer)
        .unwrap_or(0);
    let before = seen;
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\r' => {
                if chars.get(index + 1) == Some(&'\n') {
                    seen |= 4;
                    index += 1;
                } else {
                    seen |= 2;
                }
            }
            '\n' => seen |= 1,
            _ => {}
        }
        index += 1;
    }
    if seen == before {
        return;
    }
    let mut names: Vec<&str> = Vec::new();
    if seen & 4 != 0 {
        names.push("\r\n");
    }
    if seen & 2 != 0 {
        names.push("\r");
    }
    if seen & 1 != 0 {
        names.push("\n");
    }
    let newlines = match names.as_slice() {
        [] => NONE,
        [only] => value::str_from_bytes(only.as_bytes()),
        names => value::tuple_new(
            names
                .iter()
                .map(|name| value::str_from_bytes(name.as_bytes()))
                .collect(),
        ),
    };
    io_store(instance, TW_SEEN, encode_int(seen));
    io_store(instance, "newlines", newlines);
}

fn tw_limit(args: &[i64], index: usize) -> Result<i64, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(-1),
        Some(value) => match value::as_integer(value) {
            Some(limit) => Ok(limit),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "argument should be integer or None, not {}",
                    value::type_name(value)
                ),
            )),
        },
    }
}

/// `TextIOWrapper.read(size=-1)`.
fn tw_read(instance: i64, limit: i64) -> i64 {
    let read = match tw_read_remaining(instance) {
        Ok(read) => read,
        Err(error) => return error,
    };
    let total = read.text.chars().count();
    let take = if limit < 0 {
        total
    } else {
        usize::try_from(limit).unwrap_or(usize::MAX).min(total)
    };
    let prefix: String = read.text.chars().take(take).collect();
    let consumed = tw_consumed(&read, &prefix, take);
    if let Some(error) = tw_reposition(instance, read.start, consumed) {
        return error;
    }
    tw_record_newlines(instance, &prefix);
    let newline = tw_newline(instance);
    value::str_alloc(tw_translate_read(&prefix, newline.as_deref()))
}

/// `TextIOWrapper.readline(size=-1)`.
fn tw_readline(instance: i64, limit: i64) -> i64 {
    let read = match tw_read_remaining(instance) {
        Ok(read) => read,
        Err(error) => return error,
    };
    let total = read.text.chars().count();
    let cap = if limit < 0 {
        total
    } else {
        usize::try_from(limit).unwrap_or(usize::MAX).min(total)
    };
    let chars: Vec<char> = read.text.chars().take(cap).collect();
    let newline = tw_newline(instance);
    let end = tw_line_end(&chars, newline.as_deref());
    let prefix: String = chars[..end].iter().collect();
    let consumed = tw_consumed(&read, &prefix, end);
    if let Some(error) = tw_reposition(instance, read.start, consumed) {
        return error;
    }
    tw_record_newlines(instance, &prefix);
    value::str_alloc(tw_translate_read(&prefix, newline.as_deref()))
}

/// `TextIOWrapper.readlines(hint=-1)`.
fn tw_readlines(instance: i64, hint: i64) -> i64 {
    let result = value::list_new(Vec::new());
    let roots = heap::root_values(&[result]);
    let mut total = 0usize;
    loop {
        let line = tw_readline(instance, -1);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
        let Some(text) = value::as_str(line) else {
            heap::release_roots(roots);
            return NONE;
        };
        if text.is_empty() {
            break;
        }
        total = total.saturating_add(text.chars().count());
        container::list_append(result, line);
        if hint > 0 && total >= usize::try_from(hint).unwrap_or(usize::MAX) {
            break;
        }
    }
    heap::release_roots(roots);
    result
}

/// `TextIOWrapper.write(text)`.
fn tw_write(instance: i64, argument: i64) -> i64 {
    if let Some(error) = tw_check_open(instance) {
        return error;
    }
    let Some(text) = value::as_str(argument) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "write() argument must be str, not {}",
                value::type_name(argument)
            ),
        );
    };
    let text = text.to_string();
    let newline = tw_newline(instance);
    let translated = tw_translate_write(&text, newline.as_deref());
    let Some(file) = tw_file(instance) else {
        return exc::raise_with_message(VALUE_ERROR, "I/O operation on closed file.");
    };
    let written = if file.binary() {
        file.write_bytes(translated.as_bytes())
    } else {
        file.write(&translated)
    };
    match written {
        Ok(_) => encode_int(text.chars().count() as i64),
        Err(error) => value::raise_file_error(error),
    }
}

/// `TextIOWrapper.writelines(lines)`.
fn tw_writelines(instance: i64, argument: i64) -> i64 {
    let items = value::as_list(argument)
        .or_else(|| value::as_tuple(argument))
        .or_else(|| value::as_set(argument));
    let Some(items) = items else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "writelines() argument must be an iterable of strings, not {}",
                value::type_name(argument)
            ),
        );
    };
    let items = items.to_vec();
    for item in items {
        let result = tw_write(instance, item);
        if exc::pending() {
            return result;
        }
    }
    NONE
}

fn tw_close(instance: i64) -> i64 {
    if value::instance_attr(instance, "closed") == Some(TRUE) {
        return NONE;
    }
    if let Some(file) = tw_file(instance) {
        file.close();
    }
    io_store(instance, "closed", TRUE);
    NONE
}

/// Dispatches `io.TextIOWrapper.<method>`; the receiver is `args[0]`.
fn text_io_wrapper_method(method: &str, args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("TextIOWrapper.{method}() missing self"),
        );
    };
    match method {
        "read" => {
            let limit = match tw_limit(args, 1) {
                Ok(limit) => limit,
                Err(error) => return error,
            };
            tw_read(instance, limit)
        }
        "readline" => {
            let limit = match tw_limit(args, 1) {
                Ok(limit) => limit,
                Err(error) => return error,
            };
            tw_readline(instance, limit)
        }
        "readlines" => {
            let hint = match tw_limit(args, 1) {
                Ok(hint) => hint,
                Err(error) => return error,
            };
            tw_readlines(instance, hint)
        }
        "write" => {
            let Some(argument) = args.get(1).copied() else {
                return exc::raise_with_message(TYPE_ERROR, "write() missing required argument");
            };
            tw_write(instance, argument)
        }
        "writelines" => {
            let Some(argument) = args.get(1).copied() else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "writelines() missing required argument",
                );
            };
            tw_writelines(instance, argument)
        }
        "close" => tw_close(instance),
        "flush" => {
            if let Some(error) = tw_check_open(instance) {
                return error;
            }
            match tw_file(instance).map(|file| file.flush()) {
                Some(Ok(())) => NONE,
                Some(Err(error)) => value::raise_file_error(error),
                None => exc::raise_with_message(VALUE_ERROR, "I/O operation on closed file."),
            }
        }
        "seek" => {
            if let Some(error) = tw_check_open(instance) {
                return error;
            }
            let Some(&offset) = args.get(1) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "seek() missing required argument 'pos'",
                );
            };
            let Some(offset) = value::as_integer(offset) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "integer argument expected, got {}",
                        value::type_name(offset)
                    ),
                );
            };
            let whence = match args.get(2).copied() {
                None | Some(NONE) => 0,
                Some(value) => match value::as_integer(value) {
                    Some(whence) => whence,
                    None => {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!("integer argument expected, got {}", value::type_name(value)),
                        )
                    }
                },
            };
            let Some(file) = tw_file(instance) else {
                return exc::raise_with_message(VALUE_ERROR, "I/O operation on closed file.");
            };
            match file.seek(offset, whence) {
                Ok(()) => match file.tell() {
                    Ok(position) => encode_int(i64::try_from(position).unwrap_or(value::MAX_INT)),
                    Err(error) => value::raise_file_error(error),
                },
                Err(error) => value::raise_file_error(error),
            }
        }
        "tell" => {
            if let Some(error) = tw_check_open(instance) {
                return error;
            }
            match tw_file(instance).map(|file| file.tell()) {
                Some(Ok(position)) => encode_int(i64::try_from(position).unwrap_or(value::MAX_INT)),
                Some(Err(error)) => value::raise_file_error(error),
                None => exc::raise_with_message(VALUE_ERROR, "I/O operation on closed file."),
            }
        }
        "readable" | "writable" | "seekable" => {
            if let Some(error) = tw_check_open(instance) {
                return error;
            }
            let Some(file) = tw_file(instance) else {
                return exc::raise_with_message(VALUE_ERROR, "I/O operation on closed file.");
            };
            let mode = file.mode();
            value::bool_value(match method {
                "readable" => mode == 'r',
                "writable" => mode != 'r',
                _ => true,
            })
        }
        "isatty" => {
            if let Some(error) = tw_check_open(instance) {
                return error;
            }
            FALSE
        }
        "detach" => {
            let Some(buffer) = tw_buffer(instance) else {
                return exc::raise_with_message(VALUE_ERROR, "underlying buffer has been detached");
            };
            io_store(instance, TW_BUFFER, NONE);
            io_store(instance, "buffer", NONE);
            io_store(instance, "closed", TRUE);
            buffer
        }
        "__enter__" => {
            if let Some(error) = tw_check_open(instance) {
                return error;
            }
            instance
        }
        "__exit__" => {
            tw_close(instance);
            FALSE
        }
        "__iter__" => {
            if let Some(error) = tw_check_open(instance) {
                return error;
            }
            instance
        }
        "__next__" => {
            let line = tw_readline(instance, -1);
            if exc::pending() {
                return line;
            }
            match value::as_str(line) {
                Some("") => exc::raise_with_message(exc::STOP_ITERATION, ""),
                _ => line,
            }
        }
        _ => missing_attribute("io", method),
    }
}

pub(crate) fn io_open(args: &[i64]) -> i64 {
    let Some(path) = value::as_str(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "open() argument 'file' must be str, not {}",
                value::type_name(args[0])
            ),
        );
    };
    let path = path.to_string();

    let mode_text = match args.get(1).copied() {
        None | Some(NONE) => "r".to_string(),
        Some(mode) => match value::as_str(mode) {
            Some(text) => text.to_string(),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "open() argument 'mode' must be str, not {}",
                        value::type_name(mode)
                    ),
                )
            }
        },
    };
    let mode = match parse_mode(&mode_text) {
        Ok(mode) => mode,
        Err(message) => return exc::raise_with_message(VALUE_ERROR, &message),
    };

    if mode.binary {
        // CPython rejects every text-only argument in binary mode instead of
        // ignoring it, so a caller cannot assume an encoding was applied.
        for (slot, name, article) in [
            (2usize, "encoding", "an"),
            (3usize, "newline", "a"),
            (4usize, "errors", "an"),
        ] {
            if let Some(&arg) = args.get(slot) {
                if arg != NONE {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("binary mode doesn't take {article} {name} argument"),
                    );
                }
            }
        }
    } else {
        if let Some(arg) = args.get(2).copied() {
            if arg != NONE {
                match value::as_str(arg) {
                    Some(encoding) => {
                        let normalized = encoding.to_ascii_lowercase();
                        if normalized != "utf-8" && normalized != "utf8" {
                            return exc::raise_with_message(
                                VALUE_ERROR,
                                &format!("unsupported encoding: '{encoding}'"),
                            );
                        }
                    }
                    None => {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!(
                                "open() argument 'encoding' must be str or None, not {}",
                                value::type_name(arg)
                            ),
                        )
                    }
                }
            }
        }
        // `newline` and `errors` are accepted for API compatibility; text mode
        // reads and writes the file unchanged, so they only need type checking.
        for (slot, name) in [(3usize, "newline"), (4usize, "errors")] {
            if let Some(&arg) = args.get(slot) {
                if arg != NONE && value::as_str(arg).is_none() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "open() argument '{name}' must be str or None, not {}",
                            value::type_name(arg)
                        ),
                    );
                }
            }
        }
    }

    match (mode.base, mode.binary) {
        ('r', binary) => match std::fs::read(&path) {
            Ok(content) if binary => value::file_new_read_bytes(path, content),
            Ok(content) => match String::from_utf8(content) {
                Ok(text) => value::file_new_read(path, text),
                Err(error) => exc::raise_with_message(
                    VALUE_ERROR,
                    &format!("invalid UTF-8 in input: '{path}': {error}"),
                ),
            },
            Err(error) => os_failure(&path, &error),
        },
        (base, binary) => {
            let opened = match base {
                'w' => std::fs::File::create(&path),
                'a' => std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&path),
                'x' => std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path),
                // `parse_mode` only returns one of the four letters above.
                _ => {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("invalid mode: '{mode_text}'"),
                    )
                }
            };
            match opened {
                Ok(file) => value::file_new_write(path, base, file, binary),
                Err(error) => os_failure(&path, &error),
            }
        }
    }
}

/// Raises `OSError` for a failed `open`, naming the path.
fn os_failure(path: &str, error: &std::io::Error) -> i64 {
    exc::raise_with_message(OS_ERROR, &format!("{error}: '{path}'"))
}

/// Parsed `io.open` mode string.
struct OpenMode {
    /// One of `r`, `w`, `a`, `x`.
    base: char,
    /// `true` for `b`, `false` for text (the default and explicit `t`).
    binary: bool,
}

/// Validates a mode string and returns its base letter plus binary flag.
///
/// Exactly one base letter and at most one of `b`/`t` are allowed (so `bt` is
/// invalid); `b` and `t` may appear in any position relative to the base.
/// Read/write (`+`) mode is still unsupported and rejected.
fn parse_mode(mode: &str) -> Result<OpenMode, String> {
    let mut base = None;
    let mut binary = None;
    for ch in mode.chars() {
        match ch {
            'r' | 'w' | 'a' | 'x' => {
                if base.is_some() {
                    return Err(format!("invalid mode: '{mode}'"));
                }
                base = Some(ch);
            }
            'b' | 't' => {
                if binary.is_some() {
                    return Err(format!("invalid mode: '{mode}'"));
                }
                binary = Some(ch == 'b');
            }
            '+' => return Err("read/write mode is not supported".to_string()),
            _ => return Err(format!("invalid mode: '{mode}'")),
        }
    }
    let base = base.ok_or_else(|| format!("invalid mode: '{mode}'"))?;
    Ok(OpenMode {
        base,
        binary: binary.unwrap_or(false),
    })
}

fn cpu_count() -> i64 {
    std::thread::available_parallelism()
        .map(|count| count.get() as i64)
        .unwrap_or(1)
}

#[cfg(unix)]
fn parent_pid() -> i64 {
    i64::from(std::os::unix::process::parent_id())
}

#[cfg(not(unix))]
fn parent_pid() -> i64 {
    // No portable parent-process query; report 0 rather than guessing.
    0
}

/// `os.getenv(name[, default])`: the environment value, else the default,
/// else `None`.
fn getenv(args: &[i64]) -> i64 {
    let Some(name) = value::as_str(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("str expected, not {}", value::type_name(args[0])),
        );
    };
    // Copy before querying: the lookup cannot allocate Hydro objects, but the
    // owned name keeps the intent obvious and avoids borrowing across calls.
    let name = name.to_string();
    match std::env::var(&name) {
        Ok(result) => value::str_alloc(result),
        Err(_) => {
            if args.len() == 2 {
                args[1]
            } else {
                NONE
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exc::{ATTRIBUTE_ERROR, IMPORT_ERROR, TYPE_ERROR, VALUE_ERROR};
    use crate::value::{encode_int, FALSE, TRUE};

    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn string(s: &str) -> i64 {
        value::str_from_bytes(s.as_bytes())
    }

    fn as_float(v: i64) -> f64 {
        value::as_float(v).expect("float result")
    }

    fn as_int(v: i64) -> i64 {
        value::as_integer(v).expect("int result")
    }

    fn as_big(v: i64) -> BigInt {
        value::to_bigint(v).expect("integer result")
    }

    fn message() -> String {
        value::to_string(exc::pending_value())
    }

    fn get(module: &str, name: &str) -> i64 {
        module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
    }

    fn call(module: &str, function: &str, args: &[i64]) -> i64 {
        module_call(
            module.as_ptr(),
            module.len(),
            function.as_ptr(),
            function.len(),
            args.len(),
            args.as_ptr(),
        )
    }

    fn call_value(function: i64, args: &[i64]) -> i64 {
        crate::builtins::call(function, args.len(), args.as_ptr())
    }

    /// Calls `module.function(...)` with keyword arguments, passing
    /// `[name, value, ...]` pairs through the keyword-call ABI.
    fn call_kw(module: &str, function: &str, args: &[i64], keywords: &[(&str, i64)]) -> i64 {
        let mut words = Vec::with_capacity(keywords.len() * 2);
        for (name, value) in keywords {
            words.push(string(name));
            words.push(*value);
        }
        module_call_kw(
            module.as_ptr(),
            module.len(),
            function.as_ptr(),
            function.len(),
            args.len(),
            args.as_ptr(),
            keywords.len(),
            words.as_ptr(),
        )
    }

    /// Number of native-call cache hits observed on this thread.
    fn cache_hits() -> usize {
        NATIVE_CACHE.with(|cache| cache.borrow().hits)
    }

    /// Creates a unique scratch directory for path tests.
    fn temp_root(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("hydro-{label}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch directory");
        dir
    }

    fn make_dir(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).expect("create test directory");
        path.to_path_buf()
    }

    /// Reads a string-keyed entry from a registry dict.
    fn registry_value(dict: i64, name: &str) -> Option<i64> {
        value::as_dict(dict)?
            .iter()
            .find_map(|(key, value)| (value::as_str(*key) == Some(name)).then_some(*value))
    }

    /// Reads a string-keyed entry from a namespace value.
    fn namespace_value(namespace: i64, name: &str) -> Option<i64> {
        value::as_namespace(namespace)?
            .iter()
            .find_map(|(key, item)| (key.as_str() == name).then_some(*item))
    }

    /// Calls `obj.name(args...)` through the native method dispatcher.
    fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
        crate::core::methods::call(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
    }

    /// Reads `obj.name` through the native attribute dispatcher.
    fn attribute(obj: i64, name: &str) -> i64 {
        crate::core::methods::getattr(obj, name.as_ptr(), name.len())
    }

    /// Serialises tests that depend on the process-wide site-packages setting.
    static IMPORTLIB_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// A usable installed-wheel root, configuring `HYDRO_SITE_PACKAGES` when
    /// the test binary was started without it.
    fn ensure_site_packages() -> Option<PathBuf> {
        if let Some(raw) = std::env::var_os("HYDRO_SITE_PACKAGES") {
            return std::env::split_paths(&raw).find(|path| path.is_dir());
        }
        let fallback = PathBuf::from("/tmp/pkgs");
        if !fallback.is_dir() {
            return None;
        }
        std::env::set_var("HYDRO_SITE_PACKAGES", &fallback);
        Some(fallback)
    }

    #[test]
    fn math_constants_and_functions() {
        exc::init();
        assert!((as_float(get("math", "pi")) - std::f64::consts::PI).abs() < 1e-12);
        assert_eq!(as_float(get("math", "e")), std::f64::consts::E);
        assert!(as_float(get("math", "tau")) > 6.0);
        assert!(as_float(get("math", "inf")).is_infinite());
        assert!(as_float(get("math", "nan")).is_nan());

        assert_eq!(as_float(call("math", "sqrt", &[int(9)])), 3.0);
        assert_eq!(as_float(call("math", "sqrt", &[int(16)])), 4.0);
        assert_eq!(as_float(call("math", "pow", &[int(2), int(10)])), 1024.0);
        assert!((as_float(call("math", "log", &[get("math", "e")])) - 1.0).abs() < 1e-12);
        assert_eq!(as_int(call("math", "floor", &[value::float_new(2.7)])), 2);
        assert_eq!(as_int(call("math", "ceil", &[value::float_new(2.1)])), 3);
        assert_eq!(as_int(call("math", "trunc", &[value::float_new(-2.7)])), -2);
        // Integer inputs pass through unchanged, exactly.
        assert_eq!(as_int(call("math", "floor", &[int(7)])), 7);
        assert_eq!(as_int(call("math", "ceil", &[TRUE])), 1);
        assert_eq!(as_int(call("math", "trunc", &[int(-7)])), -7);
        let huge = big("1267650600228229401496703205376");
        assert_eq!(call("math", "floor", &[huge]), huge);
        assert_eq!(call("math", "ceil", &[huge]), huge);
        assert_eq!(call("math", "trunc", &[huge]), huge);
        assert_eq!(
            as_float(call("math", "fabs", &[value::float_new(-3.5)])),
            3.5
        );
        assert_eq!(as_float(call("math", "sin", &[int(0)])), 0.0);
        assert_eq!(as_float(call("math", "cos", &[int(0)])), 1.0);
        assert_eq!(as_float(call("math", "exp", &[int(0)])), 1.0);
        // `log2` follows CPython: exact for powers of two, NaN and infinity
        // propagate, and float inputs keep full precision.
        assert_eq!(as_float(call("math", "log2", &[int(8)])), 3.0);
        assert_eq!(as_float(call("math", "log2", &[int(1)])), 0.0);
        assert_eq!(as_float(call("math", "log2", &[int(1024)])), 10.0);
        assert_eq!(
            as_float(call("math", "log2", &[value::float_new(0.5)])),
            -1.0
        );
        assert_eq!(
            as_float(call("math", "log2", &[value::float_new(10.0)])),
            10f64.log2()
        );
        assert!(as_float(call("math", "log2", &[get("math", "inf")])).is_infinite());
        assert!(as_float(call("math", "log2", &[value::float_new(f64::NAN)])).is_nan());
    }

    #[test]
    fn math_domain_and_type_errors() {
        exc::init();
        assert_eq!(call("math", "sqrt", &[int(-1)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "log", &[int(0)]), NONE);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "log2", &[int(0)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "log2", &[int(-8)]), NONE);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "log2", &[value::float_new(-0.0)]), NONE);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "log2", &[string("x")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "must be real number, not str");
        exc::init();
        assert_eq!(call("math", "asin", &[value::float_new(2.0)]), NONE);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(
            call("math", "sin", &[value::float_new(f64::INFINITY)]),
            NONE
        );
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "sqrt", &[string("x")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "must be real number, not str");
        exc::init();
        assert_eq!(call("math", "pow", &[int(-1), value::float_new(0.5)]), NONE);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "exp", &[value::float_new(1000.0)]), NONE);
        assert_eq!(exc::pending_tag(), crate::exc::OVERFLOW_ERROR as i64);
        assert_eq!(message(), "math range error");
    }

    #[test]
    fn math_arity_errors() {
        exc::init();
        assert_eq!(call("math", "sqrt", &[]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "math.sqrt() takes 1 arguments (0 given)");
        exc::init();
        assert_eq!(call("math", "pow", &[int(1)]), NONE);
        assert_eq!(message(), "math.pow() takes 2 arguments (1 given)");
    }

    /// The codegen fast path handles non-negative tagged ints inline and
    /// delegates every other value to `hydro_math_floor_sqrt_int`; both must
    /// match the generic `math.floor(math.sqrt(x))` composition exactly.
    #[test]
    fn math_floor_sqrt_fast_path_matches_the_generic_composition() {
        exc::init();
        let fast = crate::abi::hydro_math_floor_sqrt_int;
        let generic = |argument: i64| {
            let root = call("math", "sqrt", &[argument]);
            if exc::pending() {
                return root;
            }
            call("math", "floor", &[root])
        };
        // Positive tagged ints, including the largest tagged payload.
        for value in [0, 1, 2, 81, 1 << 40, (1i64 << 60) - 1] {
            assert_eq!(
                fast(int(value)),
                generic(int(value)),
                "floor(sqrt({value}))"
            );
            assert!(!exc::pending());
        }
        assert_eq!(as_int(fast(int(81))), 9);
        // `sqrt(2^60)` is exactly `2^30`; the tagged result stays exact.
        assert_eq!(as_int(fast(int((1i64 << 60) - 1))), 1 << 30);

        // Bools and small `BigInt`s keep their numeric conversion.
        assert_eq!(as_int(fast(TRUE)), 1);
        assert_eq!(as_int(fast(big("81"))), 9);

        // Negative inputs raise the `math.sqrt` domain error.
        assert_eq!(fast(int(-1)), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(fast(value::bigint_new(BigInt::from(-4))), NONE);
        assert_eq!(message(), "math domain error");
        exc::init();

        // Floats and non-numeric values fall back to the generic path.
        assert_eq!(as_int(fast(value::float_new(2.7))), 1);
        assert!(!exc::pending());
        assert_eq!(fast(string("x")), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "must be real number, not str");
        exc::init();
    }

    #[test]
    fn math_gcd_accepts_variadic_integers() {
        exc::init();
        assert_eq!(as_int(call("math", "gcd", &[])), 0);
        assert_eq!(as_int(call("math", "gcd", &[int(12)])), 12);
        assert_eq!(as_int(call("math", "gcd", &[int(12), int(8)])), 4);
        assert_eq!(as_int(call("math", "gcd", &[int(12), int(8), int(6)])), 2);
        assert_eq!(as_int(call("math", "gcd", &[int(-12), int(8)])), 4);
        assert_eq!(as_int(call("math", "gcd", &[TRUE, int(4)])), 1);
        assert!(!exc::pending());

        // Exact gcd over BigInt operands (2^100 and 3 * 2^50 => 2^50).
        let pow100 = big("1267650600228229401496703205376");
        let three_pow50 = big("3377699720527872");
        let pow50 = big("1125899906842624");
        let result = call("math", "gcd", &[pow100, three_pow50]);
        assert_eq!(result, pow50);
        assert_eq!(as_int(result), 1 << 50);
        let negative_pow100 = value::bigint_new(-as_big(pow100));
        assert_eq!(call("math", "gcd", &[negative_pow100, three_pow50]), pow50);
        assert_eq!(call("math", "gcd", &[pow100, int(3)]), int(1));
        assert!(!exc::pending());

        // A function value resolved from the module (`from math import gcd`)
        // dispatches through the same implementation.
        let gcd = get("math", "gcd");
        assert_eq!(as_int(call_value(gcd, &[int(30), int(18)])), 6);
        assert!(!exc::pending());

        // Floats are rejected like CPython's integer-argument conversion.
        assert_eq!(call("math", "gcd", &[value::float_new(1.5), int(2)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "'float' object cannot be interpreted as an integer"
        );
    }

    #[test]
    fn math_prod_multiplies_numeric_iterables() {
        exc::init();
        // Empty input keeps the start value, which defaults to the exact int 1.
        assert_eq!(as_int(call("math", "prod", &[value::list_new(vec![])])), 1);
        assert_eq!(
            as_int(call_kw(
                "math",
                "prod",
                &[value::list_new(vec![])],
                &[("start", int(5))]
            )),
            5
        );

        // Lists, tuples and an already-created iterator all work.
        let range: Vec<i64> = (2..8).map(int).collect();
        assert_eq!(
            as_int(call("math", "prod", &[value::list_new(range.clone())])),
            5040
        );
        assert_eq!(
            as_int(call("math", "prod", &[value::tuple_new(range.clone())])),
            5040
        );
        assert_eq!(
            as_int(call(
                "math",
                "prod",
                &[value::iter_new(value::list_new(range))]
            )),
            5040
        );

        // `start` multiplies too: 10 * 1 * ... * 9 = 3628800.
        let one_to_nine: Vec<i64> = (1..10).map(int).collect();
        assert_eq!(
            as_int(call_kw(
                "math",
                "prod",
                &[value::list_new(one_to_nine)],
                &[("start", int(10))]
            )),
            3628800
        );
        // `iterable` may be passed by name as well.
        assert_eq!(
            as_int(call_kw(
                "math",
                "prod",
                &[],
                &[
                    ("iterable", value::list_new(vec![int(2), int(3)])),
                    ("start", int(4))
                ]
            )),
            24
        );

        // A float operand promotes the running product, exactly like `*`.
        let floats = value::list_new(vec![
            value::float_new(1.0),
            value::float_new(2.0),
            value::float_new(3.0),
        ]);
        assert_eq!(as_float(call("math", "prod", &[floats])), 6.0);
        let mixed = value::list_new(vec![
            int(1),
            int(2),
            int(3),
            value::float_new(4.0),
            value::float_new(5.0),
        ]);
        assert_eq!(as_float(call("math", "prod", &[mixed])), 120.0);

        // Tagged ints overflow into exact BigInts instead of wrapping.
        let squared = call(
            "math",
            "prod",
            &[value::list_new(vec![int(1 << 40), int(1 << 40)])],
        );
        assert_eq!(as_big(squared), BigInt::from(1u64) << 80);
        let pow100 = big("1267650600228229401496703205376");
        let square = call("math", "prod", &[value::list_new(vec![pow100, pow100])]);
        assert_eq!(as_big(square), BigInt::from(1u64) << 200);

        // Bools are integers, and a zero factor short-circuits to 0.
        assert_eq!(
            as_int(call("math", "prod", &[value::list_new(vec![TRUE, int(2)])])),
            2
        );
        assert_eq!(
            as_int(call(
                "math",
                "prod",
                &[value::list_new(vec![int(0), int(2), int(3)])]
            )),
            0
        );

        // NaN and infinity follow float multiplication rules.
        let nan = f64::NAN;
        let nan_product = call(
            "math",
            "prod",
            &[value::list_new(vec![int(1), value::float_new(nan), int(2)])],
        );
        assert!(as_float(nan_product).is_nan());
        let infinite = call(
            "math",
            "prod",
            &[value::list_new(vec![
                int(2),
                value::float_new(f64::INFINITY),
                int(-3),
            ])],
        );
        assert_eq!(as_float(infinite), f64::NEG_INFINITY);
        let zero_times_infinity = call(
            "math",
            "prod",
            &[value::list_new(vec![
                value::float_new(0.0),
                value::float_new(f64::INFINITY),
            ])],
        );
        assert!(as_float(zero_times_infinity).is_nan());
        assert!(!exc::pending());

        // A function value resolved from the module dispatches identically.
        let prod = get("math", "prod");
        assert_eq!(
            as_int(call_value(prod, &[value::list_new(vec![int(2), int(3)])])),
            6
        );
        assert!(!exc::pending());
    }

    #[test]
    fn math_prod_follows_the_number_protocol_for_starts_and_sequences() {
        exc::init();
        // Odd but valid cases from CPython's own `test_prod`.
        let start = string("ab");
        assert_eq!(
            call("math", "prod", &[value::list_new(vec![int(2), int(3)])]),
            int(6)
        );
        let repeated = call_kw(
            "math",
            "prod",
            &[value::list_new(vec![int(2), int(3)])],
            &[("start", start)],
        );
        assert_eq!(value::to_string(repeated), "abababababab");
        let list_start = value::list_new(vec![int(1), int(2)]);
        let repeated_list = call_kw(
            "math",
            "prod",
            &[value::list_new(vec![int(3)])],
            &[("start", list_start)],
        );
        let repeated_values = value::as_list(repeated_list).expect("list result");
        assert_eq!(repeated_values.len(), 6);
        // An empty iterable returns the start object itself.
        let identity_start = value::list_new(vec![int(7)]);
        assert_eq!(
            call_kw(
                "math",
                "prod",
                &[value::list_new(vec![])],
                &[("start", identity_start)]
            ),
            identity_start
        );
        assert!(!exc::pending());
    }

    #[test]
    fn math_prod_type_errors_match_cpython() {
        exc::init();
        // Non-iterable input.
        assert_eq!(call("math", "prod", &[int(42)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "'int' object is not iterable");
        exc::init();

        // Two sequences cannot be multiplied together.
        assert_eq!(
            call(
                "math",
                "prod",
                &[value::list_new(vec![string("a"), string("b")])]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "unsupported operand type(s) for *: 'str' and 'str'"
        );
        exc::init();
        assert_eq!(
            call(
                "math",
                "prod",
                &[value::list_new(vec![
                    value::list_new(vec![int(1)]),
                    value::list_new(vec![int(2)]),
                ])]
            ),
            NONE
        );
        assert_eq!(
            message(),
            "unsupported operand type(s) for *: 'list' and 'list'"
        );
        exc::init();

        // Unknown keyword names are rejected, mirroring CPython.
        assert_eq!(
            call_kw(
                "math",
                "prod",
                &[value::list_new(vec![int(1)])],
                &[("foo", int(1))]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "prod() got an unexpected keyword argument 'foo'");
        exc::init();

        // A second positional argument is rejected: `start` is keyword-only.
        assert_eq!(
            call("math", "prod", &[value::list_new(vec![int(1)]), int(2)]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "math.prod() takes 1 arguments (2 given)");
        exc::init();
        assert_eq!(call("math", "prod", &[]), NONE);
        assert_eq!(message(), "math.prod() takes 1 arguments (0 given)");
    }

    #[test]
    fn native_call_cache_hits_for_alternating_names() {
        exc::init();
        let module = "math";
        let sqrt = "sqrt";
        let floor = "floor";
        let sin = "sin";
        let sqrt_arg = int(9);
        let floor_arg = value::float_new(2.7);
        let sin_arg = int(0);

        // Warm the cache (one miss per name) and confirm the results.
        assert_eq!(as_float(call(module, sqrt, &[sqrt_arg])), 3.0);
        assert_eq!(as_int(call(module, floor, &[floor_arg])), 2);
        assert_eq!(as_float(call(module, sin, &[sin_arg])), 0.0);
        let warmed = cache_hits();

        // Alternating call sites with the same names must all hit the cache
        // and keep returning identical results.
        for _ in 0..32 {
            assert_eq!(as_float(call(module, sqrt, &[sqrt_arg])), 3.0);
            assert_eq!(as_int(call(module, floor, &[floor_arg])), 2);
            assert_eq!(as_float(call(module, sin, &[sin_arg])), 0.0);
        }
        assert_eq!(cache_hits() - warmed, 32 * 3);
        assert!(!exc::pending());
    }

    #[test]
    fn native_call_cache_handles_fresh_name_buffers() {
        exc::init();

        // Fresh heap buffers per call: the cache must not assume that equal
        // names imply equal pointers.
        for _ in 0..8 {
            let module = String::from("math");
            let function = String::from("sqrt");
            let argument = int(16);
            let result = module_call(
                module.as_bytes().as_ptr(),
                module.len(),
                function.as_bytes().as_ptr(),
                function.len(),
                1,
                &argument,
            );
            assert_eq!(as_float(result), 4.0);
        }

        // One stack buffer whose contents change between calls: the
        // pointer/length pair repeats with different bytes, so the cache must
        // fall through to a fresh resolution instead of trusting the pointer.
        let module = *b"math";
        let argument = value::float_new(-2.7);
        let mut function = [0u8; 5];
        function.copy_from_slice(b"floor");
        assert_eq!(
            as_int(module_call(
                module.as_ptr(),
                module.len(),
                function.as_ptr(),
                function.len(),
                1,
                &argument
            )),
            -3
        );
        function.copy_from_slice(b"trunc");
        assert_eq!(
            as_int(module_call(
                module.as_ptr(),
                module.len(),
                function.as_ptr(),
                function.len(),
                1,
                &argument
            )),
            -2
        );
        function.copy_from_slice(b"fabs!");
        assert_eq!(
            module_call(
                module.as_ptr(),
                module.len(),
                function.as_ptr(),
                function.len(),
                1,
                &argument
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'math' has no attribute 'fabs!'");
    }

    #[test]
    fn native_call_cache_preserves_unknown_and_arity_errors() {
        exc::init();
        let module = "math";
        // An unknown member is never cached but still raises AttributeError.
        assert_eq!(call(module, "nope", &[]), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'math' has no attribute 'nope'");

        // Arity validation runs on the cached spec on hit paths only.
        exc::init();
        let sqrt = "sqrt";
        let argument = int(4);
        assert_eq!(as_float(call(module, sqrt, &[argument])), 2.0);
        assert_eq!(call(module, sqrt, &[]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "math.sqrt() takes 1 arguments (0 given)");
        exc::init();
        assert_eq!(call(module, sqrt, &[argument, argument]), NONE);
        assert_eq!(message(), "math.sqrt() takes 1 arguments (2 given)");
    }

    #[test]
    fn native_function_values_use_the_call_cache() {
        exc::init();
        let sqrt = get("math", "sqrt");
        let argument = int(25);
        assert_eq!(as_float(call_value(sqrt, &[argument])), 5.0);
        let warmed = cache_hits();
        for _ in 0..16 {
            assert_eq!(as_float(call_value(sqrt, &[argument])), 5.0);
        }
        // `FunctionBody::Native` names are `&'static str`, so every call
        // reaches the cache through the same pointer/length key.
        assert_eq!(cache_hits() - warmed, 16);
        assert!(!exc::pending());
    }

    #[test]
    fn producer_functions_build_function_values() {
        exc::init();
        let sqrt = get("math", "sqrt");
        assert_eq!(value::type_name(sqrt), "function");
        assert_eq!(as_float(call_value(sqrt, &[int(25)])), 5.0);

        // Native function as a map callback.
        let values = value::list_new(vec![int(1), int(4), int(9)]);
        let mapped = crate::builtins::map(sqrt, values);
        let items = value::as_list(mapped).expect("list");
        assert_eq!(items.len(), 3);
        assert_eq!(as_float(items[0]), 1.0);
        assert_eq!(as_float(items[2]), 3.0);

        // Native function as a filter predicate (`operator.truth`).
        let truth = get("operator", "truth");
        let candidates = value::list_new(vec![int(0), int(2), NONE, string("x")]);
        let filtered = crate::builtins::filter(truth, candidates);
        let items = value::as_list(filtered).expect("list");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn reduce_cases_and_errors() {
        exc::init();
        let add = get("operator", "add");
        let values = value::list_new(vec![int(1), int(2), int(3), int(4)]);
        assert_eq!(as_int(call_value(reduce_fn(), &[add, values])), 10);

        let empty = value::list_new(Vec::new());
        assert_eq!(call_value(reduce_fn(), &[add, empty]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "reduce() of empty iterable with no initial value"
        );

        exc::init();
        assert_eq!(as_int(call_value(reduce_fn(), &[add, empty, int(10)])), 10);
        exc::init();
        let values = value::list_new(vec![int(1), int(2)]);
        assert_eq!(as_int(call_value(reduce_fn(), &[add, values, int(10)])), 13);

        exc::init();
        assert_eq!(
            call_value(reduce_fn(), &[int(1), value::list_new(Vec::new())]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "reduce() arg 1 must be a function");
    }

    fn reduce_fn() -> i64 {
        get("functools", "reduce")
    }

    #[test]
    fn operator_functions_cover_all_specs() {
        exc::init();
        assert_eq!(as_int(call("operator", "add", &[int(2), int(3)])), 5);
        assert_eq!(as_int(call("operator", "sub", &[int(2), int(3)])), -1);
        assert_eq!(as_int(call("operator", "mul", &[int(4), int(5)])), 20);
        assert_eq!(
            as_float(call("operator", "truediv", &[int(1), int(2)])),
            0.5
        );
        assert_eq!(as_int(call("operator", "floordiv", &[int(7), int(2)])), 3);
        assert_eq!(as_int(call("operator", "mod", &[int(7), int(2)])), 1);
        assert_eq!(as_int(call("operator", "pow", &[int(2), int(5)])), 32);
        assert_eq!(call("operator", "eq", &[int(1), int(1)]), TRUE);
        assert_eq!(call("operator", "ne", &[int(1), int(2)]), TRUE);
        assert_eq!(call("operator", "lt", &[int(1), int(2)]), TRUE);
        assert_eq!(call("operator", "le", &[int(2), int(2)]), TRUE);
        assert_eq!(call("operator", "gt", &[int(3), int(2)]), TRUE);
        assert_eq!(call("operator", "ge", &[int(2), int(3)]), FALSE);
        // `or_` is bitwise `|` for integers and set union for sets.
        assert_eq!(as_int(call("operator", "or_", &[int(6), int(3)])), 7);
        assert_eq!(as_int(call("operator", "or_", &[int(0), int(5)])), 5);
        let left = value::set_new(vec![int(1), int(2)]);
        let right = value::set_new(vec![int(2), int(3)]);
        let union = call("operator", "or_", &[left, right]);
        let members = value::as_set(union).expect("set");
        assert_eq!(members.len(), 3);
        assert_eq!(as_int(call("operator", "neg", &[int(5)])), -5);
        assert_eq!(call("operator", "not_", &[int(0)]), TRUE);
        assert_eq!(call("operator", "truth", &[int(0)]), FALSE);
        assert_eq!(call("operator", "truth", &[string("x")]), TRUE);
        assert!(!exc::pending());
    }

    #[test]
    fn operator_index_returns_exact_integers() {
        exc::init();
        // Tagged ints and BigInts come back unchanged.
        let tagged = int(42);
        assert_eq!(call("operator", "index", &[tagged]), tagged);
        let big = value::bigint_new(BigInt::from(1u64) << 100);
        assert_eq!(call("operator", "index", &[big]), big);
        // `bool.__index__()` produces an exact int, not the bool itself.
        let one = call("operator", "index", &[TRUE]);
        assert!(value::is_int(one));
        assert_eq!(as_int(one), 1);
        let zero = call("operator", "index", &[FALSE]);
        assert!(value::is_int(zero));
        assert_eq!(as_int(zero), 0);
        assert!(!exc::pending());
        // `_operator` is the same module table.
        assert_eq!(call("_operator", "index", &[tagged]), tagged);
        assert!(!exc::pending());
    }

    #[test]
    fn operator_index_calls_the_dunder_and_validates_its_result() {
        exc::init();
        extern "C" fn index_probe(_argv: *const i64) -> i64 {
            value::encode_int(7).expect("representable")
        }
        let method = value::function_new(
            index_probe as *const () as usize,
            1,
            "__index__".to_string(),
        );
        let methods = vec![("__index__".to_string(), method)];
        let class = value::class_new("Indexable".to_string(), NONE, methods);
        let instance = value::instance_new(class);
        assert_eq!(as_int(call("operator", "index", &[instance])), 7);
        assert!(!exc::pending());

        // A non-int `__index__` result is rejected with CPython's wording.
        exc::init();
        extern "C" fn bad_index_probe(_argv: *const i64) -> i64 {
            value::str_from_bytes(b"seven")
        }
        let bad_method = value::function_new(
            bad_index_probe as *const () as usize,
            1,
            "__index__".to_string(),
        );
        let bad_class = value::class_new(
            "BadIndexable".to_string(),
            NONE,
            vec![("__index__".to_string(), bad_method)],
        );
        let bad = value::instance_new(bad_class);
        assert_eq!(call("operator", "index", &[bad]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "__index__ returned non-int (type str)");

        // Objects without `__index__` report their type, ordinary values too.
        exc::init();
        let plain = value::instance_new(value::class_new("Plain".to_string(), NONE, Vec::new()));
        assert_eq!(call("operator", "index", &[plain]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "'Plain' object cannot be interpreted as an integer"
        );

        exc::init();
        assert_eq!(call("operator", "index", &[string("x")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "'str' object cannot be interpreted as an integer"
        );

        exc::init();
        assert_eq!(call("operator", "index", &[value::float_new(1.5)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "'float' object cannot be interpreted as an integer"
        );
    }

    #[test]
    fn time_functions_and_sleep() {
        exc::init();
        let start = as_float(call("time", "perf_counter", &[]));
        assert_eq!(call("time", "sleep", &[value::float_new(0.001)]), NONE);
        assert!(!exc::pending());
        let end = as_float(call("time", "perf_counter", &[]));
        assert!(end >= start);
        assert!(as_float(call("time", "monotonic", &[])) >= 0.0);
        assert!(as_float(call("time", "time", &[])) > 0.0);
        // Nanosecond clocks are exact: past the tagged range the result is a
        // BigInt with the same decimal text.
        let wall = call("time", "time_ns", &[]);
        let wall_ns = as_big(wall);
        assert!(wall_ns > BigInt::from(value::MAX_INT));
        assert_eq!(value::to_string(wall), wall_ns.to_string());
        assert!(as_big(call("time", "perf_counter_ns", &[])) >= BigInt::from(0));
        assert!(as_big(call("time", "monotonic_ns", &[])) >= BigInt::from(0));
        exc::init();
        assert_eq!(call("time", "sleep", &[int(-1)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
    }

    #[test]
    fn sys_constants() {
        exc::init();
        // The contents depend on which process argument initialized the
        // store; the shape is always a list of strings.
        let argv = get("sys", "argv");
        let items = value::as_list(argv).expect("list");
        assert!(items.iter().all(|item| value::as_str(*item).is_some()));
        assert!(value::as_str(get("sys", "executable")).is_some());

        assert_eq!(value::as_str(get("sys", "platform")), Some(platform()));
        let version = value::as_str(get("sys", "version")).expect("version string");
        assert_eq!(
            version,
            format!(
                "{PYTHON_VERSION_STRING} (Hydro {})",
                env!("CARGO_PKG_VERSION")
            )
        );
        let info = value::as_tuple(get("sys", "version_info")).expect("tuple");
        assert_eq!(info.len(), 5);
        assert_eq!(as_int(info[0]), i64::from(PYTHON_VERSION.0));
        assert_eq!(as_int(info[1]), i64::from(PYTHON_VERSION.1));
        assert_eq!(as_int(info[2]), i64::from(PYTHON_VERSION.2));
        assert_eq!(value::as_str(info[3]), Some("final"));
        assert_eq!(as_int(info[4]), 0);
        assert_eq!(
            value::as_str(get("sys", "hydro_version")),
            Some(env!("CARGO_PKG_VERSION"))
        );
        let hydro_info = value::as_tuple(get("sys", "hydro_version_info")).expect("tuple");
        assert_eq!(hydro_info.len(), 5);
        assert_eq!(as_int(hydro_info[0]), 0);
        assert_eq!(as_int(hydro_info[1]), 1);
        assert_eq!(as_int(hydro_info[2]), 0);
        assert_eq!(value::as_str(hydro_info[3]), Some("dev"));
        assert_eq!(as_int(hydro_info[4]), 0);
        assert_eq!(as_int(get("sys", "maxsize")), value::MAX_INT);
        assert!(value::as_list(get("sys", "path")).is_some());
        assert!(value::as_dict(get("sys", "modules")).is_some());
        let builtin_names = value::as_tuple(get("sys", "builtin_module_names")).expect("tuple");
        assert!(builtin_names
            .iter()
            .any(|name| value::as_str(*name) == Some("sys")));
        // The constant is derived from the central module table, so every
        // registered spec (including the shim modules) must be listed once.
        for spec in stdlib::MODULES {
            assert!(
                builtin_names
                    .iter()
                    .any(|name| value::as_str(*name) == Some(spec.name)),
                "{} missing from builtin_module_names",
                spec.name
            );
        }
        for name in ["prefix", "base_prefix", "exec_prefix"] {
            assert!(value::as_str(get("sys", name)).is_some(), "{name}");
        }
        // No interactive frame survives an uncaught exception, so the last
        // exception attributes are the `None` singleton rather than an
        // invented exception object.
        for name in ["last_exc", "last_type", "last_value", "last_traceback"] {
            assert_eq!(get("sys", name), NONE, "{name}");
        }
        assert!(stdlib::function("sys", "exit").is_some());
        for name in [
            "stdin",
            "stdout",
            "stderr",
            "__stdin__",
            "__stdout__",
            "__stderr__",
        ] {
            let stream = get("sys", name);
            assert!(value::as_std_stream(stream).is_some(), "{name}");
            assert_eq!(value::type_name(stream), "TextIOWrapper");
            let expected = match name {
                "stdin" | "__stdin__" => 0,
                "stdout" | "__stdout__" => 1,
                _ => 2,
            };
            assert_eq!(
                value::as_std_stream(stream).map(|state| state.fd()),
                Some(expected)
            );
        }
    }

    #[test]
    fn sys_exception_functions_follow_handled_stack() {
        exc::init();
        // Outside `except` handling the triple is all `None`.
        let outside = call("sys", "exc_info", &[]);
        assert_eq!(value::as_tuple(outside), Some(&[NONE, NONE, NONE][..]));

        // The spec and a first-class read yield the same callable behaviour.
        assert!(stdlib::function("sys", "exc_info").is_some());
        assert!(stdlib::function("sys", "excepthook").is_some());
        assert_eq!(value::type_name(get("sys", "exc_info")), "function");
        assert_eq!(value::type_name(get("sys", "excepthook")), "function");
        let exc_info_fn = get("sys", "exc_info");
        assert_eq!(
            value::as_tuple(call_value(exc_info_fn, &[])),
            Some(&[NONE, NONE, NONE][..])
        );

        // Inside a handler the triple is `(type(value), value, None)`.
        let error = exc::exception_value(VALUE_ERROR, "boom", vec!["boom".to_string()]);
        exc::handler_push(error);
        let items = value::as_tuple(call("sys", "exc_info", &[])).expect("tuple");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], value::type_class(error));
        assert_eq!(items[1], error);
        assert_eq!(items[2], NONE);
        // A `finally` marker above the handler does not shadow it.
        exc::handler_push(NONE);
        let items = value::as_tuple(call_value(exc_info_fn, &[])).expect("tuple");
        assert_eq!(items[1], error);
        exc::handler_pop();
        exc::handler_pop();
        assert_eq!(
            value::as_tuple(call("sys", "exc_info", &[])),
            Some(&[NONE, NONE, NONE][..])
        );

        // The hook returns `None`; `type=None` prints nothing. First-class
        // access and the module-call path behave alike.
        let hook = get("sys", "excepthook");
        assert_eq!(call("sys", "excepthook", &[NONE, NONE, NONE]), NONE);
        assert_eq!(call_value(hook, &[NONE, NONE, NONE]), NONE);
        // A real exception is rendered to stderr, still returning `None`.
        assert_eq!(
            call(
                "sys",
                "excepthook",
                &[value::type_class(error), error, NONE]
            ),
            NONE
        );
        assert!(!exc::pending());

        // The declared arity is enforced for the module-call and the
        // first-class entry point alike.
        call("sys", "excepthook", &[NONE]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        call_value(hook, &[NONE]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::init();
    }

    #[test]
    fn sys_frame_apis_report_absence_instead_of_fabricating_frames() {
        exc::init();
        // No frame is representable, so no module name is observable; the
        // result is the same `None` CPython reports beyond the stack depth.
        assert_eq!(call("sys", "_getframemodulename", &[]), NONE);
        assert_eq!(call("sys", "_getframemodulename", &[int(0)]), NONE);
        assert_eq!(call("sys", "_getframemodulename", &[int(42)]), NONE);
        assert!(!exc::pending());

        // `_getframe` cannot return an invented frame: it fails with the
        // precise CPython error for a depth beyond the available stack.
        call("sys", "_getframe", &[]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        call("sys", "_getframe", &[int(0)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        // Non-integer depths are rejected with CPython's TypeError, including
        // an explicit `None` (only an omitted argument means depth 0).
        for depth in [string("x"), NONE] {
            call("sys", "_getframemodulename", &[depth]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();
            call("sys", "_getframe", &[depth]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();
        }
        // The first-class function values expose the same behaviour.
        let frame_module = get("sys", "_getframemodulename");
        let frame = get("sys", "_getframe");
        assert_eq!(value::type_name(frame_module), "function");
        assert_eq!(call_value(frame_module, &[int(0)]), NONE);
        assert!(!exc::pending());
        call_value(frame, &[int(0)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn sys_path_follows_documented_order() {
        let root = temp_root("sys-path");
        let site_a = make_dir(&root.join("site-a"));
        let site_b = make_dir(&root.join("site-b"));
        let missing = root.join("missing");
        let venv = root.join("venv");
        let venv_site = make_dir(&venv.join("lib").join("python3.14").join("site-packages"));
        let conda = root.join("conda");
        let conda_site = make_dir(&conda.join("Lib").join("site-packages"));
        let py_a = make_dir(&root.join("py-a"));
        let py_b = make_dir(&root.join("py-b"));
        let stdlib = make_dir(&root.join("stdlib"));

        let site_packages =
            std::env::join_paths([site_a.as_path(), site_b.as_path(), missing.as_path()])
                .expect("join site packages");
        // The duplicate `site_b` and the empty component must both disappear.
        let pythonpath = std::env::join_paths([py_a.as_path(), site_b.as_path(), py_b.as_path()])
            .expect("join PYTHONPATH");

        let path = search_path_from(
            Some(site_packages.as_os_str()),
            Some(pythonpath.as_os_str()),
            Some(venv.as_os_str()),
            Some(conda.as_os_str()),
            Some(stdlib.as_os_str()),
        );
        let expected: Vec<String> = [
            &site_a,
            &site_b,
            &venv_site,
            &conda_site,
            &py_a,
            &py_b,
            &stdlib,
        ]
        .into_iter()
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect();
        assert_eq!(path, expected);

        let empty = search_path_from(None, None, None, None, None);
        assert!(empty.is_empty(), "a bare environment has no search dirs");
        std::fs::remove_dir_all(&root).expect("remove scratch directory");
    }

    #[test]
    fn sys_path_is_a_cached_list() {
        globals::reset();
        exc::init();
        let first = get("sys", "path");
        assert!(value::as_list(first).is_some(), "sys.path must be a list");
        assert_eq!(first, get("sys", "path"), "reads must share identity");
        // Mutating the list is observable on later reads (imports ignore it).
        let sentinel = string("reset-sentinel");
        crate::container::list_append(first, sentinel);
        assert!(value::as_list(get("sys", "path"))
            .expect("list")
            .contains(&sentinel));
        assert!(!exc::pending());

        // `reset` is what a fresh program run uses; it must invalidate the
        // cache so environment changes are observed.
        globals::reset();
        let second = get("sys", "path");
        assert!(value::as_list(second).is_some());
        assert!(
            !value::as_list(second).expect("list").contains(&sentinel),
            "reset must drop the cached list"
        );
    }

    #[test]
    fn sys_modules_membership_identity_and_module_names() {
        globals::reset();
        exc::init();
        let first = get("sys", "modules");
        assert_eq!(first, get("sys", "modules"), "reads must share identity");
        let sys_module = registry_value(first, "sys").expect("sys is registered");
        assert_eq!(
            value::as_module(sys_module).map(|module| module.name()),
            Some("sys")
        );
        assert_eq!(
            registry_value(first, "sys"),
            Some(sys_module),
            "sys.modules['sys'] is stable"
        );
        assert_eq!(value::as_str(get("sys", "__name__")), Some("sys"));

        // A user module becomes visible after its first global is stored,
        // without rebuilding the cached dict.
        globals::set("helper", "answer", int(42));
        let modules = get("sys", "modules");
        assert_eq!(modules, first);
        let helper = registry_value(modules, "helper").expect("user module registered");
        assert_eq!(
            value::as_module(helper).map(|module| module.name()),
            Some("helper")
        );
        assert_eq!(get("helper", "answer"), int(42));
        assert_eq!(value::as_str(get("helper", "__name__")), Some("helper"));

        // Unknown attributes still raise AttributeError, never a fake value.
        exc::init();
        assert_eq!(get("helper", "missing"), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'helper' has no attribute 'missing'");
    }

    #[test]
    fn builtins_module_materializes_native_functions() {
        globals::reset();
        exc::init();
        assert!(stdlib::module("builtins").is_some());
        assert_eq!(value::as_str(get("builtins", "__name__")), Some("builtins"));
        for name in [
            "len",
            "print",
            "open",
            "setattr",
            "compile",
            "classmethod",
            "staticmethod",
            "property",
        ] {
            assert_eq!(
                value::type_name(get("builtins", name)),
                "function",
                "{name} must materialize from the builtins spec"
            );
        }
        // `object` is the cached universal base class, not a native function.
        let object = get("builtins", "object");
        assert_eq!(
            value::class_display_name(object),
            Some("object"),
            "builtins.object must be a class"
        );
        assert_eq!(value::type_name(object), "type");
        assert!(!exc::pending());
    }

    #[test]
    fn reset_invalidates_module_registry() {
        globals::reset();
        exc::init();
        globals::set("helper", "value", int(1));
        let before = get("sys", "modules");
        assert!(registry_value(before, "helper").is_some());

        globals::reset();
        let after = get("sys", "modules");
        assert!(
            registry_value(after, "helper").is_none(),
            "user modules disappear with their globals"
        );
        assert!(registry_value(after, "sys").is_some());
    }

    #[test]
    fn sys_streams_write_flush_and_report_a_tty() {
        exc::init();
        let stdout = get("sys", "stdout");
        let empty = string("");
        let count = crate::methods::call(stdout, b"write".as_ptr(), 5, 1, [empty].as_ptr());
        assert_eq!(as_int(count), 0);
        assert_eq!(
            crate::methods::call(stdout, b"flush".as_ptr(), 5, 0, std::ptr::null()),
            NONE
        );
        let tty = crate::methods::call(stdout, b"isatty".as_ptr(), 6, 0, std::ptr::null());
        assert!(tty == TRUE || tty == FALSE);
        assert!(!exc::pending());

        exc::init();
        assert_eq!(
            crate::methods::call(stdout, b"write".as_ptr(), 5, 1, [int(1)].as_ptr()),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "write() argument must be str, not int");

        exc::init();
        assert_eq!(
            crate::methods::call(stdout, b"close".as_ptr(), 5, 0, std::ptr::null()),
            NONE
        );
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
    }

    #[test]
    fn random_getrandbits_draws_the_requested_width() {
        exc::init();
        assert_eq!(as_int(call("random", "getrandbits", &[int(0)])), 0);
        for _ in 0..16 {
            let value = as_int(call("random", "getrandbits", &[int(24)]));
            assert!((0..(1 << 24)).contains(&value), "24-bit draw out of range");
        }
        // Wide requests promote instead of raising OverflowError.
        for bits in [60i64, 61, 64, 65, 100, 129, 512] {
            let drawn = call("random", "getrandbits", &[int(bits)]);
            assert!(!exc::pending(), "getrandbits({bits}) raised");
            let n = as_big(drawn);
            assert!(n.sign() != Sign::Minus);
            assert!(
                n.bits() <= bits as u64,
                "draw of {} bits exceeds {bits}",
                n.bits()
            );
            assert_eq!(value::to_string(drawn), n.to_string());
            let dump = call("json", "dumps", &[drawn]);
            assert!(!exc::pending(), "json.dumps raised");
            assert_eq!(
                value::as_str(dump).expect("json str result"),
                n.to_string().as_str()
            );
        }

        // The bit stream is deterministic for a given seed.
        let seed = int(12345);
        let first = call("random", "Random", &[seed]);
        let second = call("random", "Random", &[seed]);
        let draw =
            |rng| crate::methods::call(rng, b"getrandbits".as_ptr(), 11, 1, [int(200)].as_ptr());
        assert_eq!(as_big(draw(first)), as_big(draw(second)));

        exc::init();
        assert_eq!(call("random", "getrandbits", &[int(-1)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "number of bits must be non-negative");

        // Memory is capped at the runtime-wide BigInt limit.
        exc::init();
        assert_eq!(
            call("random", "getrandbits", &[int(MAX_GETRANDBITS + 1)]),
            NONE
        );
        assert_eq!(exc::pending_tag(), crate::exc::OVERFLOW_ERROR as i64);

        // The same operation is a `Random` method.
        exc::init();
        let rng = call("random", "Random", &[int(5)]);
        let drawn = crate::methods::call(rng, b"getrandbits".as_ptr(), 11, 1, [int(8)].as_ptr());
        assert!(!exc::pending(), "unexpected exception");
        assert!((0..256).contains(&as_int(drawn)));
    }

    #[test]
    fn os_environ_lists_the_process_environment() {
        exc::init();
        std::env::set_var("HYDRO_TEST_OS_ENVIRON", "value");
        let environ = get("os", "environ");
        std::env::remove_var("HYDRO_TEST_OS_ENVIRON");
        let entries = value::as_dict(environ).expect("environ dict");
        let mut found = None;
        for (key, item) in entries {
            if value::as_str(*key) == Some("HYDRO_TEST_OS_ENVIRON") {
                found = Some(*item);
                break;
            }
        }
        let stored = found.expect("os.environ must contain HYDRO_TEST_OS_ENVIRON");
        assert_eq!(value::as_str(stored), Some("value"));
        assert!(!exc::pending());
    }

    #[test]
    fn os_access_checks_permission_bits() {
        exc::init();
        assert_eq!(as_int(get("os", "F_OK")), 0);
        assert_eq!(as_int(get("os", "X_OK")), 1);
        assert_eq!(as_int(get("os", "W_OK")), 2);
        assert_eq!(as_int(get("os", "R_OK")), 4);

        let executable = std::env::current_exe().expect("current executable");
        let path = string(&executable.to_string_lossy());
        assert_eq!(call("os", "access", &[path, int(0)]), TRUE);
        assert_eq!(call("os", "access", &[path, int(4)]), TRUE);
        // Modes other than the four supported bits report false.
        assert_eq!(call("os", "access", &[path, int(8)]), FALSE);
        assert_eq!(
            call(
                "os",
                "access",
                &[string("/nonexistent/hydro-access-test"), int(0)]
            ),
            FALSE
        );
        assert!(!exc::pending());
    }

    #[test]
    fn os_pathlike_devnull_and_open_flags() {
        exc::init();
        let pathlike = get("os", "PathLike");
        assert_eq!(value::class_display_name(pathlike), Some("PathLike"));
        assert!(value::as_class(pathlike).is_some());
        assert!(stdlib::constant("os", "PathLike").is_some());

        let devnull = if cfg!(windows) { "nul" } else { "/dev/null" };
        assert_eq!(value::as_str(get("os", "devnull")), Some(devnull));

        // The flags match the libc constants used by `os.open`.
        assert_eq!(as_int(get("os", "O_RDONLY")), i64::from(libc::O_RDONLY));
        assert_eq!(as_int(get("os", "O_WRONLY")), i64::from(libc::O_WRONLY));
        assert_eq!(as_int(get("os", "O_RDWR")), i64::from(libc::O_RDWR));
        assert_eq!(as_int(get("os", "O_CREAT")), i64::from(libc::O_CREAT));
        assert_eq!(as_int(get("os", "O_TRUNC")), i64::from(libc::O_TRUNC));
        assert_eq!(as_int(get("os", "O_APPEND")), i64::from(libc::O_APPEND));
        assert_eq!(as_int(get("os", "O_CLOEXEC")), i64::from(libc::O_CLOEXEC));
        for name in [
            "O_RDONLY",
            "O_WRONLY",
            "O_RDWR",
            "O_CREAT",
            "O_TRUNC",
            "O_APPEND",
            "O_CLOEXEC",
        ] {
            assert!(stdlib::constant("os", name).is_some(), "{name}");
        }
    }

    #[test]
    fn os_open_dup2_and_terminal_size() {
        exc::init();
        let path = std::env::temp_dir().join(format!("hydro-os-open-{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_value = string(&path.to_string_lossy());
        let flags = as_int(get("os", "O_WRONLY"))
            | as_int(get("os", "O_CREAT"))
            | as_int(get("os", "O_TRUNC"));
        let fd = as_int(call("os", "open", &[path_value, int(flags), int(0o600)]));
        assert!(
            fd >= 0,
            "os.open failed: {}",
            if exc::pending() {
                message()
            } else {
                String::new()
            }
        );
        assert!(!exc::pending());

        let payload = b"hydro-os-open";
        // SAFETY: `fd` is a live descriptor and the buffer is valid.
        let written = unsafe {
            libc::write(
                fd as libc::c_int,
                payload.as_ptr() as *const libc::c_void,
                payload.len(),
            )
        };
        assert_eq!(written as usize, payload.len());

        // `dup2(fd, fd)` is the identity case and returns the descriptor.
        assert_eq!(as_int(call("os", "dup2", &[int(fd), int(fd)])), fd);
        assert!(!exc::pending());

        // A regular file is not a terminal, so the ioctl reports `OSError`.
        exc::init();
        assert_eq!(call("os", "get_terminal_size", &[int(fd)]), NONE);
        assert_eq!(exc::pending_tag(), crate::exc::OS_ERROR as i64);
        let text = message();
        assert!(text.starts_with("get_terminal_size: [Errno"), "{text}");

        exc::init();
        assert_eq!(call("os", "get_terminal_size", &[string("fd")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "get_terminal_size() argument must be an integer, not str"
        );

        // SAFETY: `fd` was opened above and is closed exactly once.
        unsafe { libc::close(fd as libc::c_int) };
        assert_eq!(
            std::fs::read(&path).expect("read back").as_slice(),
            &payload[..]
        );
        let _ = std::fs::remove_file(&path);

        // Opening a missing path and passing non-integer arguments fail.
        exc::init();
        let readonly = as_int(get("os", "O_RDONLY"));
        assert_eq!(
            call(
                "os",
                "open",
                &[string("/nonexistent/hydro-open-test"), int(readonly)]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), crate::exc::OS_ERROR as i64);
        let text = message();
        assert!(text.starts_with("open: [Errno 2]"), "{text}");

        exc::init();
        assert_eq!(call("os", "open", &[int(1), int(0)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::init();
        assert_eq!(call("os", "open", &[string("x"), string("y")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::init();
        assert_eq!(
            call("os", "open", &[string("x"), int(0), string("mode")]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);

        exc::init();
        assert_eq!(call("os", "dup2", &[string("fd"), int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);

        // An invalid descriptor is rejected by the kernel with `OSError`.
        exc::init();
        assert_eq!(call("os", "dup2", &[int(-1), int(1)]), NONE);
        assert_eq!(exc::pending_tag(), crate::exc::OS_ERROR as i64);
    }

    #[test]
    fn dataclasses_field_returns_a_marker_namespace() {
        exc::init();
        let factory = value::builtin_function_new(hydro_ir::Builtin::List);
        let marker = call("dataclasses", "field", &[NONE, factory]);
        assert!(!exc::pending());
        let pairs = value::as_namespace(marker).expect("field() marker namespace");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "default");
        assert_eq!(pairs[0].1, NONE);
        assert_eq!(pairs[1].0, "default_factory");
        assert_eq!(pairs[1].1, factory);

        // Both settings omitted default to `None`.
        let empty = call("dataclasses", "field", &[]);
        let pairs = value::as_namespace(empty).expect("field() marker namespace");
        assert!(pairs.iter().all(|(_, value)| *value == NONE));

        // `from dataclasses import field` resolves to a callable value.
        let function = get("dataclasses", "field");
        assert!(value::as_function(function).is_some());
    }

    #[test]
    fn dataclasses_replace_copies_the_instance() {
        exc::init();
        let class = value::class_new("Point".to_string(), NONE, Vec::new());
        let instance = value::instance_new(class);
        value::instance_setattr(instance, "x".to_string(), int(1));
        value::instance_setattr(instance, "y".to_string(), int(2));

        // `replace(self, y=9, z=3)`: `y` overwrites the copied slot, `z` is
        // added after the existing attributes.
        let y_name = string("y");
        let z_name = string("z");
        let keywords = [y_name, int(9), z_name, int(3)];
        let copy = module_call_kw(
            b"dataclasses".as_ptr(),
            11,
            b"replace".as_ptr(),
            7,
            1,
            &instance,
            2,
            keywords.as_ptr(),
        );
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        let state = value::as_instance(copy).expect("instance copy");
        assert_eq!(state.class, class);
        let pairs: Vec<(String, i64)> = state.lock_attrs().clone();
        assert_eq!(
            pairs
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["x", "y", "z"],
            "insertion order is preserved and the new name is appended"
        );
        assert_eq!(pairs[0].1, int(1));
        assert_eq!(pairs[1].1, int(9));
        assert_eq!(pairs[2].1, int(3));

        // The original instance is untouched.
        assert_eq!(value::instance_attr(instance, "y"), Some(int(2)));
        assert_eq!(value::instance_attr(instance, "z"), None);
        assert_eq!(value::instance_attr(instance, "x"), Some(int(1)));

        // The positional form copies without changes.
        let plain = call("dataclasses", "replace", &[instance]);
        assert!(!exc::pending());
        assert_ne!(plain, instance);
        assert_eq!(value::instance_attr(plain, "x"), Some(int(1)));
        assert_eq!(value::instance_attr(plain, "y"), Some(int(2)));

        // `get` exposes the same callable value.
        assert!(value::as_function(get("dataclasses", "replace")).is_some());

        // A non-instance is rejected.
        exc::init();
        assert_eq!(call("dataclasses", "replace", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "replace() should be called on dataclass instances"
        );
        exc::init();
    }

    #[test]
    fn threading_local_is_an_empty_base_class() {
        exc::init();
        let class = get("threading", "local");
        assert_eq!(value::class_display_name(class), Some("local"));
        assert!(value::as_class(class).is_some());
        assert!(stdlib::constant("threading", "local").is_some());
        assert!(!exc::pending());
    }

    #[test]
    fn argparse_explicit_argv_help_does_not_exit() {
        exc::init();
        // The internal test mode keeps the legacy non-exiting behaviour; the
        // default path raises `SystemExit` (covered by the CLI tests).
        let parser = call("argparse", "ArgumentParser", &[string("prog")]);
        assert!(!exc::pending());
        assert!(value::as_class(get("argparse", "RawTextHelpFormatter")).is_some());
        argparse::method(parser, "_hydro_test_mode", &[]);
        let argv = value::list_new(vec![string("--help")]);
        let parsed = argparse::method(parser, "parse_args", &[argv]);
        assert!(!exc::pending());
        let entries = value::as_namespace(parsed).expect("namespace");
        let mut help = None;
        for (name, entry) in entries {
            if name.as_str() == "_help" {
                help = Some(*entry);
                break;
            }
        }
        assert_eq!(help, Some(TRUE));
    }

    #[test]
    fn platform_reports_compat_target() {
        exc::init();
        assert_eq!(
            value::as_str(call("platform", "python_version", &[])),
            Some(PYTHON_VERSION_STRING)
        );
        assert_eq!(
            value::as_str(call("platform", "python_implementation", &[])),
            Some("Hydro")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_modules_and_members() {
        exc::init();
        assert_eq!(get("numpy", "sqrt"), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'numpy' has no attribute 'sqrt'");

        exc::init();
        assert_eq!(get("math", "nope"), NONE);
        assert_eq!(message(), "module 'math' has no attribute 'nope'");

        exc::init();
        assert_eq!(call("math", "nope", &[]), NONE);
        assert_eq!(message(), "module 'math' has no attribute 'nope'");

        exc::init();
        assert_eq!(call("numpy", "sqrt", &[int(4)]), NONE);
        assert_eq!(message(), "module 'numpy' has no attribute 'sqrt'");

        // Constants are not callable through the module-call path.
        exc::init();
        assert_eq!(call("sys", "maxsize", &[]), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
    }

    #[test]
    fn compiled_module_globals_read_as_module_attributes() {
        // A compiled package mirrors its imported globals (`from .version
        // import __version__`) and submodules into the global store; reading
        // them through the module value must find them.
        exc::init();
        globals::set("numpy", "__version__", string("2.5.0"));
        globals::set(
            "numpy",
            "version",
            value::module_value("numpy.version".to_string()),
        );
        assert_eq!(value::as_str(get("numpy", "__version__")), Some("2.5.0"));
        assert_eq!(
            value::as_module(get("numpy", "version")).map(|module| module.name()),
            Some("numpy.version")
        );
        assert!(!exc::pending());

        // A name the module never bound is still an `AttributeError`.
        exc::init();
        assert_eq!(get("numpy", "missing_attr"), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'numpy' has no attribute 'missing_attr'");

        // The registry caches and the globals store are reset per program run.
        globals::remove("numpy", "__version__");
        globals::remove("numpy", "version");
    }

    #[test]
    fn abi_module_entries() {
        exc::init();
        let sqrt = crate::abi::hydro_module_get(b"math".as_ptr(), 4, b"sqrt".as_ptr(), 4);
        let argument = int(36);
        assert_eq!(as_float(crate::abi::hydro_call(sqrt, 1, &argument)), 6.0);
        let direct =
            crate::abi::hydro_module_call(b"math".as_ptr(), 4, b"sqrt".as_ptr(), 4, 1, &argument);
        assert_eq!(as_float(direct), 6.0);

        let values = value::list_new(vec![int(1), int(9)]);
        let mapped = crate::abi::hydro_map(sqrt, values);
        let items = value::as_list(mapped).expect("list");
        assert_eq!(as_float(items[1]), 3.0);
        exc::init();
        assert_eq!(
            crate::abi::hydro_module_call(
                b"math".as_ptr(),
                4,
                b"nope".as_ptr(),
                4,
                0,
                std::ptr::null(),
            ),
            NONE
        );
        assert_eq!(message(), "module 'math' has no attribute 'nope'");
    }

    #[test]
    fn native_functions_survive_gc_in_containers() {
        exc::init();
        let sqrt = get("math", "sqrt");
        let fabs = get("math", "fabs");
        let functions = value::list_new(vec![sqrt, fabs]);
        crate::abi::hydro_gc_collect();
        let items = value::as_list(functions).expect("list");
        assert_eq!(as_float(call_value(items[0], &[int(16)])), 4.0);
        assert_eq!(
            as_float(call_value(items[1], &[value::float_new(-2.5)])),
            2.5
        );
    }

    // json

    fn dumps(v: i64) -> String {
        let out = call("json", "dumps", &[v]);
        assert!(!exc::pending(), "dumps raised: {}", message());
        value::as_str(out).expect("str result").to_string()
    }

    fn loads(source: &str) -> i64 {
        let out = call("json", "loads", &[string(source)]);
        assert!(!exc::pending(), "loads raised: {}", message());
        out
    }

    #[test]
    fn json_dumps_scalars() {
        exc::init();
        assert_eq!(dumps(NONE), "null");
        assert_eq!(dumps(TRUE), "true");
        assert_eq!(dumps(FALSE), "false");
        assert_eq!(dumps(int(-7)), "-7");
        assert_eq!(dumps(int(0)), "0");
        assert_eq!(dumps(value::float_new(2.5)), "2.5");
        assert_eq!(dumps(value::float_new(2.0)), "2.0");
        assert_eq!(dumps(value::float_new(1e20)), "1e+20");
        assert_eq!(dumps(value::float_new(1e-5)), "1e-05");
        assert_eq!(dumps(value::float_new(1e16)), "1e+16");
        assert_eq!(dumps(value::float_new(0.0)), "0.0");
        assert_eq!(dumps(value::float_new(-0.0)), "-0.0");
        assert_eq!(dumps(value::float_new(f64::INFINITY)), "Infinity");
        assert_eq!(dumps(value::float_new(f64::NEG_INFINITY)), "-Infinity");
        assert_eq!(dumps(value::float_new(f64::NAN)), "NaN");
        assert_eq!(dumps(string("")), "\"\"");
        assert_eq!(dumps(string("plain")), "\"plain\"");
    }

    /// Arbitrary-precision literal through the normalizing constructor.
    fn big(text: &str) -> i64 {
        value::bigint_new(text.parse::<num_bigint::BigInt>().expect("big literal"))
    }

    #[test]
    fn json_bigint_round_trip() {
        exc::init();
        let two_100 = big("1267650600228229401496703205376");
        assert!(value::is_bigint(two_100));
        assert_eq!(dumps(two_100), "1267650600228229401496703205376");

        let negative = big("-123456789012345678901234567890");
        assert_eq!(dumps(negative), "-123456789012345678901234567890");

        // `loads` restores the exact arbitrary-precision value.
        let parsed = loads("1267650600228229401496703205376");
        assert!(value::is_bigint(parsed));
        assert_eq!(value::to_string(parsed), "1267650600228229401496703205376");
        assert_eq!(
            value::to_string(loads("-123456789012345678901234567890")),
            "-123456789012345678901234567890"
        );
        // Small integers keep the tagged representation.
        let small = loads("42");
        assert!(value::is_int(small));
        assert_eq!(value::as_integer(small), Some(42));

        // Lists keep every element exact through the round trip.
        let list = value::list_new(vec![two_100, negative, int(1)]);
        let encoded = dumps(list);
        assert_eq!(
            encoded,
            "[1267650600228229401496703205376, -123456789012345678901234567890, 1]"
        );
        let items = value::as_list(loads(&encoded)).expect("list");
        assert_eq!(items.len(), 3);
        assert_eq!(
            value::to_string(items[0]),
            "1267650600228229401496703205376"
        );
        assert_eq!(
            value::to_string(items[1]),
            "-123456789012345678901234567890"
        );
        assert_eq!(value::as_integer(items[2]), Some(1));

        // Integer dict keys become quoted decimals, like CPython.
        let dict = value::dict_new(vec![(two_100, string("v"))]);
        let encoded = dumps(dict);
        assert_eq!(encoded, "{\"1267650600228229401496703205376\": \"v\"}");
        // `loads` restores the key as the equivalent decimal string.
        let reloaded = value::as_dict(loads(&encoded)).expect("dict");
        assert_eq!(reloaded.len(), 1);
        assert_eq!(
            value::as_str(reloaded[0].0),
            Some("1267650600228229401496703205376")
        );
        assert_eq!(value::as_str(reloaded[0].1), Some("v"));
        assert!(!exc::pending());
    }

    #[test]
    fn json_dumps_string_escaping() {
        exc::init();
        assert_eq!(dumps(string("a\"b\nc")), "\"a\\\"b\\nc\"");
        assert_eq!(dumps(string("a\\b")), "\"a\\\\b\"");
        assert_eq!(dumps(string("/")), "\"/\"");
        assert_eq!(dumps(string("\u{08}\u{0c}\r\t")), "\"\\b\\f\\r\\t\"");
        // Printable ASCII stays raw; DEL and controls use \uXXXX.
        assert_eq!(dumps(string("a\u{7f}b\u{01}c")), "\"a\\u007fb\\u0001c\"");
        // Non-ASCII is escaped (ensure_ascii=True).
        assert_eq!(dumps(string("é€")), "\"\\u00e9\\u20ac\"");
        assert_eq!(dumps(string("😀")), "\"\\ud83d\\ude00\"");
    }

    #[test]
    fn json_dumps_containers() {
        exc::init();
        let list = value::list_new(vec![int(1), value::float_new(2.5), NONE, TRUE]);
        assert_eq!(dumps(list), "[1, 2.5, null, true]");
        assert_eq!(
            dumps(value::tuple_new(vec![int(1), string("a")])),
            "[1, \"a\"]"
        );
        assert_eq!(dumps(value::list_new(Vec::new())), "[]");
        assert_eq!(dumps(value::tuple_new(Vec::new())), "[]");
        assert_eq!(dumps(value::dict_new(Vec::new())), "{}");

        // Insertion order is preserved.
        let dict = value::dict_new(vec![(string("b"), int(1)), (string("a"), int(2))]);
        assert_eq!(dumps(dict), "{\"b\": 1, \"a\": 2}");

        let nested = value::dict_new(vec![(
            string("a"),
            value::list_new(vec![
                int(1),
                value::dict_new(vec![(string("b"), string("c"))]),
            ]),
        )]);
        assert_eq!(dumps(nested), "{\"a\": [1, {\"b\": \"c\"}]}");
    }

    #[test]
    fn json_dumps_dict_keys() {
        exc::init();
        let dict = value::dict_new(vec![
            (string("k"), int(0)),
            (int(3), int(1)),
            (value::float_new(1.5), int(2)),
            (TRUE, int(3)),
            (FALSE, int(4)),
            (NONE, int(5)),
            (value::float_new(1e20), int(6)),
            (value::float_new(f64::NAN), int(7)),
        ]);
        assert_eq!(
            dumps(dict),
            "{\"k\": 0, \"3\": 1, \"1.5\": 2, \"true\": 3, \"false\": 4, \"null\": 5, \
             \"1e+20\": 6, \"NaN\": 7}"
        );

        exc::init();
        let bad_key = value::dict_new(vec![(value::tuple_new(vec![int(1)]), int(2))]);
        assert_eq!(call("json", "dumps", &[bad_key]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "keys must be str, int, float, bool or None, not tuple"
        );
    }

    #[test]
    fn json_dumps_errors_and_guards() {
        exc::init();
        assert_eq!(call("json", "dumps", &[get("math", "sqrt")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "Object of type function is not JSON serializable"
        );

        exc::init();
        let set = value::set_new(vec![int(1)]);
        assert_eq!(call("json", "dumps", &[set]), NONE);
        assert_eq!(message(), "Object of type set is not JSON serializable");

        exc::init();
        assert_eq!(call("json", "dumps", &[value::range_new(0, 3, 1)]), NONE);
        assert_eq!(message(), "Object of type range is not JSON serializable");

        exc::init();
        let list = value::list_new(vec![int(1)]);
        let iterator = value::iter_new(list);
        assert_eq!(call("json", "dumps", &[iterator]), NONE);
        assert_eq!(
            message(),
            "Object of type iterator is not JSON serializable"
        );

        // Self reference is reported, never rendered recursively.
        exc::init();
        let cycle = value::list_new(Vec::new());
        if let Some(mut items) = value::list_items_mut(cycle) {
            items.push(cycle);
        }
        assert_eq!(call("json", "dumps", &[cycle]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "Circular reference detected");

        // Depth guard.
        exc::init();
        let mut deep = value::list_new(Vec::new());
        for _ in 0..MAX_JSON_DEPTH + 2 {
            deep = value::list_new(vec![deep]);
        }
        assert_eq!(call("json", "dumps", &[deep]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "Circular reference detected");

        // A chain just below the limit still serializes.
        exc::init();
        let mut ok = value::list_new(Vec::new());
        for _ in 0..MAX_JSON_DEPTH - 1 {
            ok = value::list_new(vec![ok]);
        }
        let text = dumps(ok);
        assert_eq!(text.matches('[').count(), text.matches(']').count());
        assert!(text.starts_with("[["));
    }

    #[test]
    fn json_loads_scalars_and_containers() {
        exc::init();
        assert_eq!(loads("null"), NONE);
        assert_eq!(loads("true"), TRUE);
        assert_eq!(loads("false"), FALSE);
        assert_eq!(as_int(loads("42")), 42);
        assert_eq!(as_int(loads("-3")), -3);
        assert_eq!(as_int(loads("0")), 0);
        assert_eq!(as_float(loads("2.5")), 2.5);
        assert_eq!(as_float(loads("1e2")), 100.0);
        assert_eq!(as_float(loads("1E+2")), 100.0);
        assert_eq!(as_float(loads("-0.5")), -0.5);
        assert!(as_float(loads("NaN")).is_nan());
        assert_eq!(as_float(loads("Infinity")), f64::INFINITY);
        assert_eq!(as_float(loads("-Infinity")), f64::NEG_INFINITY);
        assert_eq!(value::as_str(loads("\"hello\"")), Some("hello"));
        assert_eq!(value::as_str(loads("  \"hi\"  ")), Some("hi"));

        assert_eq!(
            value::to_string(loads("[1, \"a\", true, null, 2.5]")),
            "[1, 'a', True, None, 2.5]"
        );
        let array = loads(" [1, 2 , 3 ] ");
        let items = value::as_list(array).expect("list");
        assert_eq!(items.len(), 3);
        assert_eq!(as_int(items[2]), 3);
        assert!(value::as_list(loads("[]")).expect("list").is_empty());
        assert!(value::as_dict(loads("{}")).expect("dict").is_empty());

        // Duplicate keys: last wins, first insertion position.
        assert_eq!(value::to_string(loads("{\"a\": 1, \"a\": 2}")), "{'a': 2}");
        assert_eq!(
            value::to_string(loads("{\"a\": [1, {\"b\": \"c\"}]}")),
            "{'a': [1, {'b': 'c'}]}"
        );
    }

    #[test]
    fn json_loads_string_escapes() {
        exc::init();
        assert_eq!(value::as_str(loads("\"a\\\\b\"")), Some("a\\b"));
        assert_eq!(value::as_str(loads("\"\\/\"")), Some("/"));
        assert_eq!(value::as_str(loads("\"\\\"\"")), Some("\""));
        assert_eq!(
            value::as_str(loads("\"\\b\\f\\n\\r\\t\"")),
            Some("\u{08}\u{0c}\n\r\t")
        );
        assert_eq!(value::as_str(loads("\"\\u00e9\\u20ac\"")), Some("é€"));
        // Raw non-ASCII passes through.
        assert_eq!(value::as_str(loads("\"é€\"")), Some("é€"));
        // Surrogate pairs combine; lone surrogates degrade to U+FFFD.
        assert_eq!(
            value::as_str(loads("\"\\ud83d\\ude00\"")),
            Some("\u{1f600}")
        );
        assert_eq!(value::as_str(loads("\"\\ud83d\"")), Some("\u{fffd}"));
        assert_eq!(
            value::as_str(loads("\"\\ud83d\\u0041\"")),
            Some("\u{fffd}A")
        );
    }

    #[test]
    fn json_loads_errors() {
        let case = |source: &str, expected: &str| {
            exc::init();
            assert_eq!(
                call("json", "loads", &[string(source)]),
                NONE,
                "input {source:?}"
            );
            assert_eq!(exc::pending_tag(), VALUE_ERROR as i64, "input {source:?}");
            assert_eq!(message(), expected, "input {source:?}");
        };
        case("", "Expecting value");
        case("   ", "Expecting value");
        case("{", "Expecting property name enclosed in double quotes");
        case("{,}", "Expecting property name enclosed in double quotes");
        case("[1,", "Expecting value");
        case("[1,]", "Expecting value");
        case("[1 2]", "Expecting ',' delimiter");
        case("{\"a\" 1}", "Expecting ':' delimiter");
        case("{\"a\": 1", "Expecting ',' delimiter");
        case("1 2", "Extra data");
        case("\"abc", "Unterminated string starting at");
        case("\"a\\q\"", "Invalid \\escape");
        case("\"a\nb\"", "Invalid control character at");
        case("tru", "Expecting value");

        exc::init();
        let deep = "[".repeat(MAX_JSON_DEPTH + 5) + &"]".repeat(MAX_JSON_DEPTH + 5);
        assert_eq!(call("json", "loads", &[string(&deep)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "Maximum nesting depth exceeded");

        exc::init();
        assert_eq!(call("json", "loads", &[int(5)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "the JSON object must be str, bytes or bytearray, not int"
        );

        exc::init();
        assert_eq!(call("json", "loads", &[NONE]), NONE);
        assert_eq!(
            message(),
            "the JSON object must be str, bytes or bytearray, not NoneType"
        );
    }

    #[test]
    fn json_roundtrip() {
        exc::init();
        let source = "{\"name\": \"Hydro\", \"n\": 3, \"ok\": true, \"items\": [1, 2.5, null], \
                      \"nested\": {\"x\": \"y\"}}";
        let parsed = loads(source);
        assert_eq!(dumps(parsed), source);
        let dumped = dumps(parsed);
        assert_eq!(dumps(loads(&dumped)), dumped);
    }

    #[test]
    fn json_nested_survives_collection() {
        exc::init();
        let source = "{\"a\": [1, {\"b\": [\"x\", \"y\", {\"c\": null}]}], \"d\": true}";
        let parsed = call("json", "loads", &[string(source)]);
        assert!(!exc::pending(), "loads raised: {}", message());
        // Force a collection while the parsed value is only rooted on the
        // native stack, then serialize the surviving graph.
        crate::abi::hydro_gc_collect();
        let dumped = call("json", "dumps", &[parsed]);
        assert!(!exc::pending(), "dumps raised: {}", message());
        assert_eq!(value::to_string(dumped), source);

        // Dumping after the collector ran again is still stable.
        crate::abi::hydro_gc_collect();
        assert_eq!(value::to_string(dumped), source);
    }

    // os

    #[test]
    fn os_functions_and_constants() {
        exc::init();
        assert!(as_int(call("os", "getpid", &[])) > 0);
        assert!(as_int(call("os", "getppid", &[])) >= 0);
        assert!(as_int(call("os", "cpu_count", &[])) >= 1);
        assert!(!value::as_str(call("os", "getcwd", &[]))
            .unwrap_or("")
            .is_empty());

        assert_eq!(
            value::as_str(get("os", "name")),
            Some(if cfg!(windows) { "nt" } else { "posix" })
        );
        assert_eq!(
            value::as_str(get("os", "sep")),
            Some(if cfg!(windows) { "\\" } else { "/" })
        );

        assert_eq!(
            call("os", "getenv", &[string("DEFINITELY_NOT_SET_HYD")]),
            NONE
        );
        assert_eq!(
            value::as_str(call(
                "os",
                "getenv",
                &[string("DEFINITELY_NOT_SET_HYD"), string("fallback")]
            )),
            Some("fallback")
        );

        std::env::set_var("HYDRO_TEST_OS_ENV", "value");
        assert_eq!(
            value::as_str(call("os", "getenv", &[string("HYDRO_TEST_OS_ENV")])),
            Some("value")
        );
        std::env::remove_var("HYDRO_TEST_OS_ENV");

        exc::init();
        assert_eq!(call("os", "getenv", &[int(5)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "str expected, not int");

        exc::init();
        assert_eq!(call("os", "getpid", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "os.getpid() takes 0 arguments (1 given)");

        exc::init();
        assert_eq!(get("os", "nope"), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'os' has no attribute 'nope'");
    }

    #[test]
    fn asyncio_eager_helpers() {
        exc::init();
        // `run(value)` returns the already-computed argument.
        assert_eq!(call("asyncio", "run", &[int(7)]), int(7));
        assert!(!exc::pending());

        // `gather(iterable)` materialises the elements in order.
        let values = value::list_new(vec![int(1), int(2), int(3)]);
        let gathered = call("asyncio", "gather", &[values]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_list(gathered).expect("list").to_vec(),
            vec![int(1), int(2), int(3)]
        );

        // `gather` over a non-iterable fails like any other iteration.
        exc::init();
        assert_eq!(call("asyncio", "gather", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);

        // `sleep` blocks the calling thread and returns None.
        exc::init();
        assert_eq!(call("asyncio", "sleep", &[value::float_new(0.001)]), NONE);
        assert!(!exc::pending());
        assert_eq!(call("asyncio", "sleep", &[int(0)]), NONE);
        assert!(!exc::pending());

        exc::init();
        assert_eq!(call("asyncio", "sleep", &[int(-1)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);

        // Task APIs validate their argument and the running loop instead of
        // pretending to be coroutines.
        exc::init();
        assert_eq!(call("asyncio", "create_task", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "create_task() requires a coroutine");

        exc::init();
        assert_eq!(call("asyncio", "current_task", &[]), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn asyncio_coroutines_submodule_exposes_identity_sentinel() {
        exc::init();
        // `import asyncio.coroutines` then `asyncio.coroutines` resolves to
        // the exact `sys.modules` entry.
        let submodule = get("asyncio", "coroutines");
        assert_eq!(
            value::as_module(submodule).map(|module| module.name()),
            Some("asyncio.coroutines")
        );
        assert_eq!(registry_lookup("asyncio.coroutines"), Some(submodule));

        // `_is_coroutine` is one stable object: shims compare it with `is`.
        let sentinel = get("asyncio.coroutines", "_is_coroutine");
        assert_eq!(sentinel, get("asyncio.coroutines", "_is_coroutine"));
        assert_eq!(
            get("asyncio.coroutines", "_is_coroutine"),
            constant_value("asyncio.coroutines", "_is_coroutine")
        );
        // It is a plain object instance, not a shared interned value.
        assert!(value::instance_class_value(sentinel).is_some());
        let other = value::instance_new(value::object_class());
        assert_ne!(sentinel, other);
        assert!(!exc::pending());
    }

    /// Extracts a `str` result, naming the surface that produced it when the
    /// call returned something else (or failed with a pending exception).
    fn text_at(surface: &str, v: i64) -> String {
        match value::as_str(v) {
            Some(text) => text.to_string(),
            None => {
                let detail = if exc::pending() {
                    value::to_string(exc::pending_value())
                } else {
                    "no pending exception".to_string()
                };
                panic!(
                    "{surface} returned {} ({detail}), not str",
                    value::type_name(v)
                )
            }
        }
    }

    #[test]
    fn datetime_module_values() {
        exc::init();
        assert_eq!(as_int(get("datetime", "MINYEAR")), 1);
        assert_eq!(as_int(get("datetime", "MAXYEAR")), 9_999);
        let singleton = get("datetime", "datetime");
        assert!(!exc::pending());
        let rendered = call(
            "datetime",
            "_datetime_strftime",
            &[singleton, string("%Y%m%d")],
        );
        let rendered = text_at("datetime._datetime_strftime", rendered);
        assert_eq!(rendered.len(), 8);
        assert!(rendered.bytes().all(|byte| byte.is_ascii_digit()));
    }

    #[cfg(unix)]
    #[test]
    fn os_sysconf_and_getloadavg() {
        exc::init();
        let page_size = as_int(call("os", "sysconf", &[string("SC_PAGE_SIZE")]));
        assert!(page_size >= 4096, "page size {page_size}");
        assert!(as_int(call("os", "sysconf", &[string("SC_PHYS_PAGES")])) > 0);
        // Integer configuration names are accepted like CPython.
        #[cfg(unix)]
        assert_eq!(
            page_size,
            as_int(call("os", "sysconf", &[int(i64::from(libc::_SC_PAGESIZE))]))
        );

        let averages = call("os", "getloadavg", &[]);
        assert!(!exc::pending());
        let averages = value::as_tuple(averages).expect("tuple");
        assert_eq!(averages.len(), 3);
        for average in averages {
            assert!(value::as_float(*average).is_some());
        }

        exc::init();
        assert_eq!(call("os", "sysconf", &[string("SC_NOT_A_NAME")]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);

        exc::init();
        assert_eq!(call("os", "sysconf", &[value::float_new(1.0)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[cfg(unix)]
    #[test]
    fn os_uname_exposes_the_platform_identity() {
        exc::init();
        let result = call("os", "uname", &[]);
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        // Attribute access like `os.uname().release`.
        let sysname = value::instance_attr(result, "sysname").expect("sysname");
        assert_eq!(value::as_str(sysname), Some(platform_system()));
        let machine = value::instance_attr(result, "machine").expect("machine");
        assert_eq!(value::as_str(machine), Some(platform_machine()));
        let release = value::instance_attr(result, "release").expect("release");
        assert!(
            !value::as_str(release).unwrap_or_default().is_empty(),
            "uname release must be the real kernel release"
        );
        let _ = value::instance_attr(result, "nodename").expect("nodename");
        let _ = value::instance_attr(result, "version").expect("version");

        // Tuple unpacking, as used by `system, _, _, _, machine = os.uname()`.
        let fields = value::namedtuple_values(result).expect("uname_result fields");
        assert_eq!(fields.len(), 5);
        for (index, name) in ["sysname", "nodename", "release", "version", "machine"]
            .iter()
            .enumerate()
        {
            let attribute = value::instance_attr(result, name).expect(name);
            assert_eq!(fields[index], attribute, "field {name} out of order");
        }

        // Tuple protocol: `len(os.uname())` and `os.uname()[0]`.
        assert_eq!(crate::core::builtins::len(result), int(5));
        assert_eq!(
            value::as_str(container::subscript(result, int(0))),
            Some(platform_system())
        );
        assert_eq!(
            value::as_str(container::subscript(result, int(-1))),
            Some(platform_machine())
        );
        exc::init();
    }

    #[test]
    fn json_keyword_options_shape_output() {
        exc::init();
        let object = value::dict_new(vec![(string("b"), int(1)), (string("a"), int(2))]);
        assert_eq!(
            text_at("json.dumps", call("json", "dumps", &[object, int(2)])),
            "{\n  \"b\": 1,\n  \"a\": 2\n}"
        );
        // Trailing `None` keyword slots keep the defaults.
        assert_eq!(
            text_at(
                "json.dumps",
                call("json", "dumps", &[object, NONE, NONE, NONE, TRUE]),
            ),
            "{\"a\": 2, \"b\": 1}"
        );
        assert_eq!(
            text_at(
                "json.dumps",
                call("json", "dumps", &[string("ä"), NONE, FALSE]),
            ),
            "\"ä\""
        );
        assert_eq!(
            text_at("json.dumps", call("json", "dumps", &[string("ä")])),
            "\"\\u00e4\""
        );
        let separators = value::tuple_new(vec![string(","), string(":")]);
        let list = value::list_new(vec![int(1), int(2)]);
        assert_eq!(
            text_at(
                "json.dumps",
                call("json", "dumps", &[list, NONE, NONE, separators, NONE]),
            ),
            "[1,2]"
        );

        exc::init();
        assert_eq!(call("json", "dumps", &[object, string("two")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn json_dump_accepts_keyword_defaults() {
        exc::init();
        let path =
            std::env::temp_dir().join(format!("hydro-json-dump-{}.json", std::process::id()));
        let file = std::fs::File::create(&path).expect("create temp file");
        let writer = value::file_new_write(path.to_string_lossy().into_owned(), 'w', file, false);
        let object = value::dict_new(vec![(string("a"), int(1))]);
        let result = call("json", "dump", &[object, writer, int(2), NONE, NONE, TRUE]);
        assert_eq!(result, NONE);
        assert!(!exc::pending());
        let written = std::fs::read_to_string(&path).expect("read temp file");
        std::fs::remove_file(&path).ok();
        assert_eq!(written, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn typing_abc_and_enum_resolve_through_the_module_surface() {
        exc::init();
        assert_eq!(get("typing", "TYPE_CHECKING"), FALSE);
        for name in ["Any", "Generic", "NamedTuple", "Protocol", "Tuple"] {
            let class = value::as_class(get("typing", name)).expect("class value");
            assert_eq!(class.name, name);
        }

        let payload = string("payload");
        let any = get("typing", "Any");
        assert_eq!(call("typing", "cast", &[any, payload]), payload);
        assert_eq!(call("typing", "TypeVar", &[string("T")]), NONE);
        assert_eq!(
            value::as_dict(call("typing", "get_type_hints", &[]))
                .expect("dict value")
                .len(),
            0
        );
        assert_eq!(call("typing", "runtime_checkable", &[payload]), payload);
        assert_eq!(call("typing", "overload", &[payload]), payload);

        assert_eq!(
            value::as_class(get("abc", "ABC"))
                .expect("class value")
                .name,
            "ABC"
        );
        assert_eq!(
            value::as_class(get("abc", "ABCMeta"))
                .expect("class value")
                .name,
            "ABCMeta"
        );
        assert_eq!(call("abc", "abstractmethod", &[payload]), payload);
        assert!(value::as_integer(call("abc", "get_cache_token", &[])).is_some());

        for name in ["Enum", "IntEnum"] {
            let class = value::as_class(get("enum", name)).expect("class value");
            assert_eq!(class.name, name);
        }
        assert_eq!(call("enum", "auto", &[]), NONE);
        assert!(!exc::pending());

        // Arity still comes from the central spec.
        assert_eq!(call("typing", "cast", &[NONE]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn new_stdlib_surfaces_resolve_through_the_module_api() {
        // Nothing may reset the heap while this test resolves classes and
        // instances: the values it compares are owned by that heap.
        let _heap_guard = crate::heap::heap_reset_test_guard();
        exc::init();
        // Module-level caches are process-global: the `globals` registry holds
        // the cached classes/instances and each module's native class
        // registry compares against the heap generation, so a concurrent
        // `globals::reset()` (and the heap reset behind it) can invalidate a
        // cached value between two calls here. This broad smoke test therefore
        // only checks that the module API yields usable values (a class value,
        // an instance, a constant); pinning exact class identity or comparing
        // pointers to independently cached values is left to each module's own
        // tests, which resolve the class and the instance under one lock.
        //
        // `functools.Placeholder` is a public value (the singleton instance of
        // its type); the repeat read shows the cache returns one value.
        let placeholder = get("functools", "Placeholder");
        assert_eq!(placeholder, get("functools", "Placeholder"));
        assert!(
            value::as_instance(placeholder).is_some(),
            "functools.Placeholder is not an instance"
        );

        // `functools.partialmethod` is a class value and a constructor.
        let pm_class = get("functools", "partialmethod");
        assert!(value::as_class(pm_class).is_some());
        let pm = call(
            "functools",
            "partialmethod",
            &[get("functools", "reduce"), int(0)],
        );
        assert!(
            value::as_instance(pm).is_some(),
            "functools.partialmethod(...) returned no instance"
        );

        // `enum.IntFlag` and `ast.arg`.
        assert_eq!(
            value::as_class(get("enum", "IntFlag")).map(|class| class.name.as_str()),
            Some("IntFlag")
        );
        // `ast.arg` may be built against a class registry rebuilt by another
        // test's reset, so only the shape of the result is checked here;
        // `ast`'s own tests assert the exact `arg` class of the node.
        let arg = call("ast", "arg", &[string("x")]);
        assert!(
            value::as_instance(arg).is_some(),
            "ast.arg(...) returned no instance"
        );

        // `types.DynamicClassAttribute` is a class value and a constructor.
        let dca_class = get("types", "DynamicClassAttribute");
        assert!(value::as_class(dca_class).is_some());
        let dca = call("types", "DynamicClassAttribute", &[]);
        assert!(
            value::as_instance(dca).is_some(),
            "types.DynamicClassAttribute(...) returned no instance"
        );

        // `sys` recursion limit is a real process-wide setting.
        let limit = as_int(call("sys", "getrecursionlimit", &[]));
        assert!(limit > 0);
        assert_eq!(call("sys", "setrecursionlimit", &[int(limit)]), NONE);
        assert_eq!(as_int(call("sys", "getrecursionlimit", &[])), limit);
        assert_eq!(call("sys", "setrecursionlimit", &[int(0)]), NONE);
        assert_eq!(exc::pending_tag(), RECURSION_ERROR as i64);
        exc::init();

        // `os.path.normcase` is the POSIX identity.
        assert_eq!(
            text_at(
                "os.path.normcase",
                call("os.path", "normcase", &[string("/tmp/Mixed/Case")]),
            ),
            "/tmp/Mixed/Case"
        );

        // `os.path.realpath` resolves a symlinked directory.
        let root = temp_root("realpath");
        let dir = make_dir(&root.join("dir"));
        std::fs::write(dir.join("file.txt"), b"x").expect("write file");
        let link = root.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&dir, &link).expect("create symlink");
        let resolved = text_at(
            "os.path.realpath",
            call(
                "os.path",
                "realpath",
                &[value::str_alloc(link.to_string_lossy().into_owned())],
            ),
        );
        assert!(resolved.ends_with("/dir"), "{resolved}");
        assert!(!resolved.contains("/link"), "{resolved}");

        // Non-strict keeps a missing path absolute; strict raises OSError.
        let missing = root.join("absent");
        let lenient = text_at(
            "os.path.realpath",
            call(
                "os.path",
                "realpath",
                &[value::str_alloc(missing.to_string_lossy().into_owned())],
            ),
        );
        assert!(Path::new(&lenient).is_absolute());
        exc::init();
        call(
            "os.path",
            "realpath",
            &[
                value::str_alloc(missing.to_string_lossy().into_owned()),
                TRUE,
            ],
        );
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        exc::init();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn future_and_types_are_reachable() {
        exc::init();
        for name in [
            "annotations",
            "generator_stop",
            "division",
            "absolute_import",
            "print_function",
            "unicode_literals",
        ] {
            assert_eq!(get("__future__", name), TRUE, "{name}");
        }
        assert_eq!(get("__future__", "barry_as_FLUFL"), FALSE);

        let class = value::as_class(get("types", "TracebackType")).expect("class value");
        assert_eq!(class.name, "TracebackType");

        let mapping = value::dict_new(vec![(string("a"), int(1))]);
        let namespace = call("types", "SimpleNamespace", &[mapping]);
        assert_eq!(
            value::as_namespace(namespace).map(|pairs| pairs.len()),
            Some(1)
        );
        assert!(!exc::pending());
    }

    #[test]
    fn itertools_and_literal_eval_dispatch() {
        exc::init();
        let first = value::list_new(vec![int(1), int(2)]);
        let second = value::list_new(vec![int(3)]);
        let chained = call("itertools", "chain", &[first, second]);
        assert_eq!(
            value::as_list(chained)
                .expect("list result")
                .iter()
                .map(|item| as_int(*item))
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            value::as_tuple(call("ast", "literal_eval", &[string("(1, 2)")]))
                .map(|items| items.len()),
            Some(2)
        );
        let literal = call("ast", "literal_eval", &[string("42")]);
        assert_eq!(as_int(literal), 42);
        assert!(!exc::pending());
    }

    #[test]
    fn operator_getters_and_functools_helpers() {
        exc::init();
        let namespace = value::namespace_new(vec![("value".to_string(), int(4))]);
        let getter = call("operator", "attrgetter", &[string("value")]);
        assert_eq!(call_value(getter, &[namespace]), int(4));

        let list = value::list_new(vec![int(5)]);
        let item = call("operator", "itemgetter", &[int(0)]);
        assert_eq!(call_value(item, &[list]), int(5));

        let target = value::builtin_function_new(hydro_ir::Builtin::Str);
        let partial = call("functools", "partial", &[target, int(9)]);
        assert_eq!(value::as_str(call_value(partial, &[])), Some("9"));

        // `cache` is the identity decorator.
        let decorated = string("function");
        assert_eq!(call("functools", "cache", &[decorated]), decorated);

        // `wraps` copies the wrapped function's metadata and records the
        // `__wrapped__` back-reference; `lru_cache` stays an identity value.
        let wrapped = value::identity_function_new("wrapped");
        let wrapper = value::identity_function_new("wrapper");
        let decorator = call("functools", "wraps", &[wrapped]);
        assert!(!exc::pending());
        assert_eq!(call_value(decorator, &[wrapper]), wrapper);
        assert_eq!(value::function_qualname(wrapper), "wrapped");
        assert_eq!(
            value::function_attribute(wrapper, "__wrapped__"),
            Some(wrapped)
        );
        let cached = call("functools", "lru_cache", &[int(16), FALSE]);
        assert_eq!(call_value(cached, &[decorated]), decorated);
        assert!(!exc::pending());
    }

    #[test]
    fn functools_cached_property_is_a_descriptor_class() {
        // The registry caches one class value for the whole process, so pin
        // the heap generation while the identity assertions run.
        let _heap_guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let class = get("functools", "cached_property");
        assert_eq!(
            value::as_class(class).map(|state| state.name.as_str()),
            Some("cached_property")
        );

        // The module-call entry and the class value build the same descriptor.
        let getter = value::identity_function_new("computed");
        let descriptor = call("functools", "cached_property", &[getter]);
        assert_eq!(value::instance_class_value(descriptor), Some(class));
        assert_eq!(value::instance_attr(descriptor, "func"), Some(getter));
        assert_eq!(
            value::instance_attr(descriptor, "attrname").and_then(value::as_str),
            None,
            "the attribute name is learned on first access"
        );
        let via_class = call_value(class, &[getter]);
        assert_eq!(value::instance_class_value(via_class), Some(class));
        let via_keyword = call_kw("functools", "cached_property", &[], &[("func", getter)]);
        assert_eq!(value::instance_attr(via_keyword, "func"), Some(getter));
        assert!(!exc::pending());

        // The descriptor caches in the instance namespace: the getter returns
        // the instance itself, so the cached value is recognisable.
        let holder = value::class_new_full(
            "Holder".to_string(),
            NONE,
            Vec::new(),
            vec![("computed".to_string(), descriptor)],
        );
        let instance = value::instance_new(holder);
        assert_eq!(attribute(holder, "computed"), descriptor);
        assert_eq!(attribute(instance, "computed"), instance);
        assert_eq!(value::instance_attr(instance, "computed"), Some(instance));
        assert_eq!(
            value::instance_attr(descriptor, "attrname").and_then(value::as_str),
            Some("computed")
        );
        assert!(!exc::pending());
    }

    /// Stand-in for a generated wrapper taking two positional parameters.
    extern "C" fn pair_probe(argv: *const i64) -> i64 {
        // SAFETY: the runtime only enters a wrapper after checking its arity.
        let (left, right) = unsafe { (*argv, *argv.add(1)) };
        value::tuple_new(vec![left, right])
    }

    #[test]
    fn functools_partial_accepts_keywords_through_the_kw_abi() {
        exc::init();
        let target = value::function_new(pair_probe as *const () as usize, 2, "pair".to_string());
        value::function_set_params(target, vec!["left".to_string(), "right".to_string()]);
        let name = string("right");
        let bound = int(9);
        let keywords = [name, bound];
        let partial = module_call_kw(
            b"functools".as_ptr(),
            9,
            b"partial".as_ptr(),
            7,
            1,
            &target,
            1,
            keywords.as_ptr(),
        );
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        let left = int(1);
        let result = crate::builtins::call(partial, 1, &left);
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(value::to_string(result), "(1, 9)");

        // `func` may be passed by name: the keyword takes the positional slot,
        // while `right` stays a forwarded keyword.
        let func_name = string("func");
        let right_name = string("right");
        let bound = int(4);
        let named = [func_name, target, right_name, bound];
        let partial = module_call_kw(
            b"functools".as_ptr(),
            9,
            b"partial".as_ptr(),
            7,
            0,
            std::ptr::null(),
            2,
            named.as_ptr(),
        );
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        let result = crate::builtins::call(partial, 1, &left);
        assert_eq!(value::to_string(result), "(1, 4)");

        // A spec without dynamic keywords rejects the call outright.
        let value_name = string("x");
        let rejected = [value_name, int(1)];
        let result = module_call_kw(
            b"math".as_ptr(),
            4,
            b"sqrt".as_ptr(),
            4,
            0,
            std::ptr::null(),
            1,
            rejected.as_ptr(),
        );
        assert_eq!(result, NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "math.sqrt() does not accept keyword arguments"
        );
        exc::init();
    }

    #[test]
    fn importlib_and_fractions_are_reachable() {
        exc::init();
        let table = call(
            "importlib",
            "import_module",
            &[string("rich._unicode_data")],
        );
        assert!(value::as_namespace(table).is_some());
        let math = call("importlib", "import_module", &[string("math")]);
        assert_eq!(
            value::as_module(math).map(|module| module.name()),
            Some("math")
        );
        assert_eq!(
            value::as_float(call("fractions", "Fraction", &[int(1), int(4)])),
            Some(0.25)
        );
        assert!(!exc::pending());
    }

    #[test]
    fn importlib_submodules_resolve_to_native_modules() {
        globals::reset();
        exc::init();
        for name in [
            "importlib.util",
            "importlib.machinery",
            "importlib.metadata",
            "importlib.resources",
        ] {
            let module = call("importlib", "import_module", &[string(name)]);
            assert_eq!(
                value::as_module(module).map(|module| module.name()),
                Some(name),
                "{name}"
            );
            // Importing the CPython package would surface its files as origin.
            let spec = get(name, "__spec__");
            assert_eq!(
                value::instance_attr(spec, "origin"),
                Some(NONE),
                "{name} must be native, not a package file"
            );
            assert_eq!(value::as_str(get(name, "__name__")), Some(name));
            assert_eq!(
                get(name, "__loader__"),
                get("importlib.machinery", "BuiltinImporter")
            );
            assert!(!exc::pending(), "{name}");
        }

        // `importlib.util` and friends are attributes of the parent module.
        for short in ["util", "machinery", "metadata", "resources"] {
            let expected = format!("importlib.{short}");
            let module = get("importlib", short);
            assert_eq!(
                value::as_module(module).map(|module| module.name()),
                Some(expected.as_str()),
                "{expected}"
            );
            assert_eq!(Some(module), crate::modules::registry_lookup(&expected));
        }
        assert_eq!(
            value::as_str(get("importlib", "__package__")),
            Some("importlib")
        );
        assert_eq!(
            value::as_str(get("importlib.util", "__package__")),
            Some("importlib")
        );
    }

    #[test]
    fn recomputed_spec_and_cache_functions_report_real_state() {
        globals::reset();
        exc::init();
        let spec = call(
            "importlib.util",
            "find_spec",
            &[string("importlib.metadata")],
        );
        assert!(
            value::instance_class_value(spec).is_some(),
            "ModuleSpec instance"
        );
        assert_eq!(
            value::instance_attr(spec, "name").and_then(value::as_str),
            Some("importlib.metadata")
        );
        assert_eq!(value::instance_attr(spec, "origin"), Some(NONE));
        let loader = value::instance_attr(spec, "loader").expect("loader");
        assert_eq!(
            value::class_display_name(loader),
            Some("BuiltinImporter"),
            "native modules use BuiltinImporter"
        );
        assert_eq!(call("importlib", "invalidate_caches", &[]), NONE);
        assert!(!exc::pending());

        // `reload` re-imports built-ins (a no-op) but refuses to fake
        // re-execution of a compiled Python module.
        let math = call("importlib", "import_module", &[string("math")]);
        assert_eq!(call("importlib", "reload", &[math]), math);
        globals::reset();
        exc::init();
        globals::set("reload_target", "__name__", string("reload_target"));
        let target = call("importlib", "import_module", &[string("reload_target")]);
        exc::clear();
        call("importlib", "reload", &[target]);
        assert_eq!(exc::pending_tag(), i64::from(IMPORT_ERROR));
        assert!(
            message().contains("no runtime execution engine"),
            "{}",
            message()
        );
    }

    #[test]
    fn metadata_versions_come_from_installed_dist_info() {
        let _guard = IMPORTLIB_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(_site) = ensure_site_packages() else {
            return;
        };
        globals::reset();
        exc::init();
        for (name, expected) in [
            ("numpy", "2.5.0"),
            ("pandas", "3.1.0"),
            ("pydantic", "2.14.1"),
        ] {
            let version = call("importlib.metadata", "version", &[string(name)]);
            assert_eq!(
                value::as_str(version),
                Some(expected),
                "importlib.metadata.version({name:?})"
            );
            assert!(!exc::pending());
        }

        let package_metadata = call("importlib.metadata", "metadata", &[string("numpy")]);
        assert_eq!(
            registry_value(package_metadata, "Name").and_then(value::as_str),
            Some("numpy")
        );
        assert_eq!(
            registry_value(package_metadata, "Version").and_then(value::as_str),
            Some("2.5.0")
        );

        let dist = call("importlib.metadata", "distribution", &[string("numpy")]);
        assert_eq!(
            value::instance_attr(dist, "version").and_then(value::as_str),
            Some("2.5.0")
        );
        assert_eq!(
            value::instance_attr(dist, "name").and_then(value::as_str),
            Some("numpy")
        );
        let entries = value::instance_attr(dist, "entry_points").expect("entry_points");
        let entries = value::as_list(entries).expect("entry list");
        assert!(!entries.is_empty(), "numpy installs console entry points");
        let console = entries
            .iter()
            .find(|entry| {
                value::instance_attr(**entry, "group").and_then(value::as_str) == Some("pkg_config")
            })
            .expect("numpy pkg_config entry point");
        assert_eq!(
            value::instance_attr(*console, "name").and_then(value::as_str),
            Some("numpy")
        );
        assert_eq!(
            value::instance_attr(*console, "module").and_then(value::as_str),
            Some("numpy._core.lib.pkgconfig")
        );

        // Metadata is read from the real file, not synthesized.
        let dist_metadata = method(dist, "read_text", &[string("METADATA")]);
        assert!(
            value::as_str(dist_metadata).is_some_and(|text| text.contains("Name: numpy")),
            "distribution.read_text('METADATA')"
        );
        // `entry_points()` parses the same `entry_points.txt`.
        let all = call("importlib.metadata", "entry_points", &[]);
        let all = value::as_list(all).expect("entry point list");
        assert!(
            all.iter().any(|entry| {
                value::instance_attr(*entry, "module").and_then(value::as_str)
                    == Some("numpy.f2py.f2py2e")
            }),
            "entry_points() must include numpy's f2py console script"
        );
        // The distribution names the .dist-info directory it was parsed from.
        let metadata_path = value::instance_attr(dist, "_path")
            .and_then(value::as_str)
            .expect("dist-info path");
        assert!(Path::new(metadata_path).is_dir(), "{metadata_path}");
        assert_eq!(
            Path::new(metadata_path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("numpy-2.5.0.dist-info")
        );

        assert_eq!(call("importlib", "invalidate_caches", &[]), NONE);
    }

    #[test]
    fn metadata_missing_package_raises_package_not_found() {
        let _guard = IMPORTLIB_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if ensure_site_packages().is_none() {
            return;
        }
        globals::reset();
        exc::init();
        call(
            "importlib.metadata",
            "version",
            &[string("hydro-metadata-missing")],
        );
        // The class is the native `PackageNotFoundError` (a
        // `ModuleNotFoundError` subclass); handler matching uses Hydro's
        // `RuntimeError` approximation for non-builtin exception classes.
        assert_eq!(exc::pending_tag(), i64::from(crate::exc::RUNTIME_ERROR));
        let raised = exc::pending_value();
        let class = value::instance_class_value(raised).expect("exception instance");
        assert_eq!(
            value::class_display_name(class),
            Some("PackageNotFoundError")
        );
        assert_eq!(
            value::to_string(raised),
            "No package metadata was found for hydro-metadata-missing"
        );
    }

    #[test]
    fn util_file_specs_build_module_metadata_and_refuse_execution() {
        let _guard = IMPORTLIB_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(site) = ensure_site_packages() else {
            return;
        };
        let six = site.join("six.py");
        if !six.is_file() {
            return;
        }
        globals::reset();
        exc::init();
        let six_text = six.to_string_lossy().into_owned();
        let spec = call(
            "importlib.util",
            "spec_from_file_location",
            &[string("six"), string(&six_text)],
        );
        assert!(!exc::pending(), "{}", message());
        assert!(value::instance_class_value(spec).is_some());
        assert_eq!(
            value::instance_attr(spec, "name").and_then(value::as_str),
            Some("six")
        );
        assert_eq!(
            value::instance_attr(spec, "origin").and_then(value::as_str),
            Some(six_text.as_str())
        );
        assert_eq!(value::instance_attr(spec, "has_location"), Some(TRUE));
        assert_eq!(
            value::instance_attr(spec, "submodule_search_locations"),
            Some(NONE),
            "six is a module, not a package"
        );
        let loader = value::instance_attr(spec, "loader").expect("loader");
        assert_eq!(
            value::instance_attr(loader, "path").and_then(value::as_str),
            Some(six_text.as_str())
        );

        let module = call("importlib.util", "module_from_spec", &[spec]);
        assert_eq!(
            namespace_value(module, "__name__").and_then(value::as_str),
            Some("six")
        );
        assert_eq!(
            namespace_value(module, "__file__").and_then(value::as_str),
            Some(six_text.as_str())
        );
        assert_eq!(
            namespace_value(module, "__loader__"),
            value::instance_attr(spec, "loader")
        );

        // The source is not compiled into this program, so executing it must
        // fail with a precise error instead of an empty module.
        exc::init();
        method(loader, "exec_module", &[module]);
        assert_eq!(exc::pending_tag(), i64::from(IMPORT_ERROR));
        assert!(
            message().contains("no runtime Python compiler"),
            "{}",
            message()
        );

        // An installed package spec carries its search directory.
        exc::init();
        let package_spec = call("importlib.util", "find_spec", &[string("pydantic")]);
        let locations = value::instance_attr(package_spec, "submodule_search_locations")
            .expect("submodule_search_locations");
        let locations = value::as_list(locations).expect("search location list");
        assert!(locations
            .iter()
            .filter_map(|dir| value::as_str(*dir))
            .map(Path::new)
            .any(|dir| dir.is_dir() && dir.ends_with("pydantic")));
        let package_module = call("importlib.util", "module_from_spec", &[package_spec]);
        let package_path = namespace_value(package_module, "__path__").expect("__path__");
        assert_eq!(value::as_list(package_path).map(<[i64]>::len), Some(1));
    }

    #[test]
    fn resources_files_reads_real_files() {
        let _guard = IMPORTLIB_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(site) = ensure_site_packages() else {
            return;
        };
        if !site.join("pydantic/version.py").is_file() {
            return;
        }
        globals::reset();
        exc::init();
        let root = call("importlib.resources", "files", &[string("pydantic")]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            value::class_display_name(value::instance_class_value(root).expect("instance")),
            Some("Traversable")
        );
        assert_eq!(value::as_str(attribute(root, "name")), Some("pydantic"));
        let root_path = method(root, "__fspath__", &[]);
        let root_path = value::as_str(root_path).expect("__fspath__");
        assert!(Path::new(root_path).is_dir(), "{root_path}");
        assert_eq!(
            Path::new(root_path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("pydantic")
        );
        assert_eq!(method(root, "is_dir", &[]), TRUE);
        assert_eq!(method(root, "is_file", &[]), FALSE);

        let file = method(root, "joinpath", &[string("version.py")]);
        assert_eq!(value::as_str(attribute(file, "name")), Some("version.py"));
        assert_eq!(method(file, "is_file", &[]), TRUE);
        assert_eq!(method(file, "is_dir", &[]), FALSE);
        let text = method(file, "read_text", &[]);
        assert!(
            value::as_str(text).is_some_and(|text| text.contains("VERSION = '2.14.1'")),
            "read_text must return the real file contents"
        );
        let bytes = method(file, "read_bytes", &[]);
        assert_eq!(
            value::as_bytes(bytes).map(|bytes| &bytes[..3]),
            Some(b"\"\"\"" as &[u8])
        );

        let children = method(root, "iterdir", &[]);
        let children = value::as_list(children).expect("list of children");
        assert!(children
            .iter()
            .any(|child| value::as_str(attribute(*child, "name")) == Some("version.py")));
        let missing = method(root, "joinpath", &[string("missing.py")]);
        assert_eq!(
            value::as_str(attribute(missing, "name")),
            Some("missing.py")
        );
    }

    #[test]
    fn os_stat_reports_real_metadata() {
        exc::init();
        let root = temp_root("stat");
        let path = root.join("data.txt");
        std::fs::write(&path, b"hello").expect("write stat fixture");
        let stat = call("os", "stat", &[string(&path.to_string_lossy())]);
        assert!(!exc::pending(), "{}", message());

        assert_eq!(
            as_int(namespace_value(stat, "st_size").expect("st_size")),
            5
        );
        let mode = as_int(namespace_value(stat, "st_mode").expect("st_mode"));
        assert_eq!(mode & 0o170000, 0o100000, "regular file mode");
        assert!(
            as_int(namespace_value(stat, "st_nlink").expect("st_nlink")) >= 1,
            "st_nlink must be at least one"
        );
        for name in ["st_ino", "st_dev", "st_uid", "st_gid"] {
            let field = namespace_value(stat, name).unwrap_or_else(|| panic!("{name}"));
            assert!(
                as_big(field) >= BigInt::from(0),
                "{name} must be a non-negative integer"
            );
        }
        let mtime = as_float(namespace_value(stat, "st_mtime").expect("st_mtime"));
        assert!(mtime > 0.0);
        assert!(as_float(namespace_value(stat, "st_atime").expect("st_atime")) > 0.0);
        assert!(as_float(namespace_value(stat, "st_ctime").expect("st_ctime")) > 0.0);
        // Nanosecond aliases are exact integers; 2026 timestamps already
        // exceed the tagged range, so they arrive as BigInts.
        let mtime_ns = as_big(namespace_value(stat, "st_mtime_ns").expect("st_mtime_ns"));
        let approx = value::bigint_to_f64(&mtime_ns).expect("ns fits f64") / 1e9;
        assert!((approx - mtime).abs() < 1e-3);
        assert!(
            as_big(namespace_value(stat, "st_atime_ns").expect("st_atime_ns")) >= BigInt::from(0)
        );

        // Directories keep their file-type bits too.
        let dir_stat = call("os", "stat", &[string(&root.to_string_lossy())]);
        assert!(!exc::pending(), "{}", message());
        let dir_mode = as_int(namespace_value(dir_stat, "st_mode").expect("st_mode"));
        assert_eq!(dir_mode & 0o170000, 0o040000, "directory mode");
    }

    #[test]
    fn os_stat_missing_path_raises_file_not_found() {
        exc::init();
        let missing = "/nonexistent/hydro-stat-missing";
        assert_eq!(call("os", "stat", &[string(missing)]), NONE);
        assert_eq!(
            exc::pending_tag(),
            i64::from(crate::exc::FILE_NOT_FOUND_ERROR)
        );
        let text = message();
        assert!(
            text.contains("[Errno 2]")
                && text.contains("No such file or directory")
                && text.contains(missing),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn os_stat_follow_symlinks_flag_selects_link_or_target() {
        exc::init();
        let root = temp_root("stat-link");
        let target = root.join("target.txt");
        std::fs::write(&target, b"1234567").expect("write link target");
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let link_value = string(&link.to_string_lossy());
        let followed = call("os", "stat", &[link_value]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            as_int(namespace_value(followed, "st_size").expect("st_size")),
            7
        );

        // Keyword calls arrive with the omitted `dir_fd` padded to `None`.
        let link_stat = call("os", "stat", &[link_value, NONE, FALSE]);
        assert!(!exc::pending(), "{}", message());
        let link_mode = as_int(namespace_value(link_stat, "st_mode").expect("st_mode"));
        assert_eq!(link_mode & 0o170000, 0o120000, "symbolic link mode");
        assert_ne!(
            as_int(namespace_value(link_stat, "st_size").expect("st_size")),
            7
        );
    }

    #[test]
    fn os_path_isabs_classifies_absolute_paths() {
        exc::init();
        assert_eq!(call("os.path", "isabs", &[string("/tmp/x")]), TRUE);
        assert_eq!(call("os.path", "isabs", &[string("/")]), TRUE);
        assert_eq!(call("os.path", "isabs", &[string("tmp/x")]), FALSE);
        assert_eq!(call("os.path", "isabs", &[string("")]), FALSE);
        assert!(!exc::pending());

        assert_eq!(call("os.path", "isabs", &[int(3)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "isabs() argument must be a path string, not int");
    }

    #[test]
    fn sys_path_importer_cache_is_a_stable_mutable_dict() {
        exc::init();
        let cache = get("sys", "path_importer_cache");
        assert_eq!(get("sys", "path_importer_cache"), cache);
        assert!(value::as_dict(cache).is_some(), "must be a real dict");

        let key = string("hydro-cache-probe");
        let item = int(1);
        let roots = crate::heap::root_values(&[key, item]);
        crate::container::dict_set(cache, key, item);
        crate::heap::release_roots(roots);
        assert_eq!(
            registry_value(get("sys", "path_importer_cache"), "hydro-cache-probe"),
            Some(int(1))
        );
    }

    #[test]
    fn importlib_decode_source_honours_bom_cookies_and_errors() {
        exc::init();
        let bytes = |data: &[u8]| value::bytes_new(data.to_vec());

        let plain = call("importlib.util", "decode_source", &[bytes(b"x = 1\n")]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(value::as_str(plain), Some("x = 1\n"));

        // A UTF-8 BOM is stripped before decoding.
        let bom = call(
            "importlib.util",
            "decode_source",
            &[bytes(b"\xef\xbb\xbfx = 1\n")],
        );
        assert_eq!(value::as_str(bom), Some("x = 1\n"));

        // Universal newlines are translated.
        let newlines = call("importlib.util", "decode_source", &[bytes(b"a\r\nb\rc\n")]);
        assert_eq!(value::as_str(newlines), Some("a\nb\nc\n"));

        // A coding cookie on the first or second line selects the codec.
        let latin1 = call(
            "importlib.util",
            "decode_source",
            &[bytes(b"# -*- coding: latin-1 -*-\nname = '\xe9'\n")],
        );
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            value::as_str(latin1),
            Some("# -*- coding: latin-1 -*-\nname = '\u{e9}'\n")
        );
        let second_line = call(
            "importlib.util",
            "decode_source",
            &[bytes(b"# comment\n# coding: iso-8859-1\nname = '\xe9'\n")],
        );
        assert!(
            value::as_str(second_line).is_some_and(|text| text.ends_with("'\u{e9}'\n")),
            "{second_line:?}"
        );

        // Unknown codecs are a SyntaxError; bad data is a UnicodeDecodeError.
        assert_eq!(
            call(
                "importlib.util",
                "decode_source",
                &[bytes(b"# coding: klingon\n")]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(crate::exc::SYNTAX_ERROR));
        assert_eq!(message(), "unknown encoding: klingon");

        // A BOM contradicting the cookie is rejected, like tokenize.
        exc::init();
        assert_eq!(
            call(
                "importlib.util",
                "decode_source",
                &[bytes(b"\xef\xbb\xbf# coding: latin-1\n")]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(crate::exc::SYNTAX_ERROR));
        assert_eq!(message(), "encoding problem: utf-8");

        // Invalid UTF-8 data raises the codec error.
        exc::init();
        assert_eq!(
            call("importlib.util", "decode_source", &[bytes(b"\xff")]),
            NONE
        );
        assert_eq!(
            exc::pending_tag(),
            i64::from(crate::exc::UNICODE_DECODE_ERROR)
        );

        // A decodable cookie line followed by invalid UTF-8 propagates the
        // codec error.
        exc::init();
        assert_eq!(
            call(
                "importlib.util",
                "decode_source",
                &[bytes(b"# coding: utf-8\n\xff\n")]
            ),
            NONE
        );
        assert_eq!(
            exc::pending_tag(),
            i64::from(crate::exc::UNICODE_DECODE_ERROR)
        );
        assert!(
            message().contains("'utf-8' codec can't decode byte 0xff"),
            "{}",
            message()
        );
    }

    #[test]
    fn token_module_exposes_cpython_tables() {
        exc::init();
        assert_eq!(as_int(get("token", "ENDMARKER")), 0);
        assert_eq!(as_int(get("token", "OP")), 55);
        assert_eq!(as_int(get("token", "ENCODING")), 68);
        assert_eq!(as_int(get("token", "N_TOKENS")), 69);
        assert_eq!(as_int(get("token", "NT_OFFSET")), 256);

        let names = get("token", "tok_name");
        let entries = value::as_dict(names).expect("tok_name is a dict");
        let op_name = entries.iter().find_map(|(key, item)| {
            (value::as_integer(*key) == Some(55))
                .then(|| value::as_str(*item))
                .flatten()
        });
        assert_eq!(op_name, Some("OP"));

        let exact = get("token", "EXACT_TOKEN_TYPES");
        let entries = value::as_dict(exact).expect("EXACT_TOKEN_TYPES is a dict");
        let at_token = entries.iter().find_map(|(key, item)| {
            (value::as_str(*key) == Some("@"))
                .then(|| value::as_integer(*item))
                .flatten()
        });
        assert_eq!(at_token, Some(49));

        assert!(!value::as_list(get("token", "__all__"))
            .expect("__all__ is a list")
            .is_empty());
        assert_eq!(call("token", "ISTERMINAL", &[int(55)]), TRUE);
        assert_eq!(call("token", "ISTERMINAL", &[int(256)]), FALSE);
        assert_eq!(call("token", "ISNONTERMINAL", &[int(256)]), TRUE);
        assert_eq!(call("token", "ISEOF", &[int(0)]), TRUE);
        assert_eq!(call("token", "ISEOF", &[int(68)]), FALSE);
    }

    #[test]
    fn codecs_lookup_and_byte_order_marks_are_real() {
        exc::init();
        let info = call("codecs", "lookup", &[string("ascii")]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(value::as_tuple(info).expect("CodecInfo tuple").len(), 4);

        assert_eq!(
            value::as_bytes(get("codecs", "BOM_UTF8")).unwrap(),
            &[0xEF, 0xBB, 0xBF]
        );
        assert_eq!(
            value::as_bytes(get("codecs", "BOM_UTF16_LE")).unwrap(),
            &[0xFF, 0xFE]
        );
        assert_eq!(
            value::as_bytes(get("codecs", "BOM_UTF16_BE")).unwrap(),
            &[0xFE, 0xFF]
        );
        assert_eq!(
            value::as_bytes(get("codecs", "BOM_UTF32_LE")).unwrap(),
            &[0xFF, 0xFE, 0x00, 0x00]
        );
        assert_eq!(
            value::as_bytes(get("codecs", "BOM_UTF32_BE")).unwrap(),
            &[0x00, 0x00, 0xFE, 0xFF]
        );
        assert_eq!(
            value::as_bytes(get("codecs", "BOM_LE")).unwrap(),
            value::as_bytes(get("codecs", "BOM_UTF16_LE")).unwrap()
        );
        assert_eq!(
            value::as_bytes(get("codecs", "BOM_BE")).unwrap(),
            value::as_bytes(get("codecs", "BOM_UTF16_BE")).unwrap()
        );
    }

    #[test]
    fn text_io_wrapper_reads_and_writes_file_objects() {
        exc::init();
        let dir = temp_root("textio");
        let path = dir.join("data.txt");
        std::fs::write(&path, b"hello\nworld\n").unwrap();
        let path_text = path.to_string_lossy().into_owned();

        let file = call("io", "open", &[string(&path_text), string("rb")]);
        assert!(!exc::pending(), "{}", message());
        let wrapper = call("io", "TextIOWrapper", &[file]);
        assert!(!exc::pending(), "{}", message());
        // The wrapper is an instance of the native `io.TextIOWrapper` class:
        // `type(wrapper)` is that class and its `__name__` is what the runtime
        // reports on the resolving paths. `value::type_name` is the static
        // fallback (it answers "instance" for every class instance by design).
        let wrapper_class = value::type_class(wrapper);
        assert_eq!(
            value::class_display_name(wrapper_class),
            Some("TextIOWrapper")
        );
        assert_eq!(
            wrapper_class,
            get("io", "TextIOWrapper"),
            "type(wrapper) must be the module's TextIOWrapper class"
        );
        assert_eq!(attribute(wrapper, "__class__"), wrapper_class);
        assert_eq!(
            value::as_str(attribute(wrapper_class, "__name__")),
            Some("TextIOWrapper")
        );
        assert_eq!(value::type_name_of(wrapper), "TextIOWrapper");
        assert_eq!(value::as_str(attribute(wrapper, "encoding")), Some("utf-8"));
        assert_eq!(
            value::as_str(attribute(wrapper, "name")),
            Some(path_text.as_str())
        );
        assert_eq!(value::as_str(attribute(wrapper, "mode")), Some("rb"));
        assert_eq!(value::as_str(attribute(wrapper, "errors")), Some("strict"));
        assert_eq!(attribute(wrapper, "closed"), FALSE);
        assert_eq!(attribute(wrapper, "line_buffering"), FALSE);
        assert_eq!(attribute(wrapper, "write_through"), FALSE);

        let line = method(wrapper, "readline", &[]);
        assert_eq!(value::as_str(line), Some("hello\n"));
        assert_eq!(value::as_str(attribute(wrapper, "newlines")), Some("\n"));
        let rest = method(wrapper, "read", &[]);
        assert_eq!(value::as_str(rest), Some("world\n"));
        let eof = method(wrapper, "readline", &[]);
        assert_eq!(value::as_str(eof), Some(""));
        method(wrapper, "close", &[]);
        assert_eq!(attribute(wrapper, "closed"), TRUE);

        // Writes go to the wrapped binary file, with the text layer encoding.
        let out = dir.join("out.txt");
        let out_text = out.to_string_lossy().into_owned();
        let file = call("io", "open", &[string(&out_text), string("wb")]);
        assert!(!exc::pending(), "{}", message());
        let wrapper = call("io", "TextIOWrapper", &[file]);
        assert!(!exc::pending(), "{}", message());
        let written = method(wrapper, "write", &[string("a\nb")]);
        assert_eq!(as_int(written), 3);
        method(wrapper, "flush", &[]);
        method(wrapper, "close", &[]);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "a\nb");
    }

    #[test]
    fn contextlib_nullcontext_enters_with_its_result_and_does_not_suppress() {
        exc::init();
        let manager = call("contextlib", "nullcontext", &[string("resource")]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            value::class_display_name(value::instance_class_value(manager).expect("instance")),
            Some("nullcontext")
        );
        assert_eq!(
            value::as_str(attribute(manager, "enter_result")),
            Some("resource")
        );
        assert_eq!(
            value::as_str(method(manager, "__enter__", &[])),
            Some("resource")
        );
        // CPython's nullcontext is reusable: `__enter__` re-reads the stored
        // result instead of consuming it.
        assert_eq!(
            value::as_str(method(manager, "__enter__", &[])),
            Some("resource")
        );
        // `False` keeps the exception raised by the block.
        assert_eq!(method(manager, "__exit__", &[NONE, NONE, NONE]), FALSE);

        // The default `enter_result` is `None`.
        let bare = call("contextlib", "nullcontext", &[]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(attribute(bare, "enter_result"), NONE);
        assert_eq!(method(bare, "__enter__", &[]), NONE);
        assert_eq!(method(bare, "__exit__", &[NONE, NONE, NONE]), FALSE);

        // `suppress` shares the same native-method dispatch, so its internal
        // methods must be registered in the module table as well.
        let suppress = call("contextlib", "suppress", &[]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(method(suppress, "__enter__", &[]), NONE);
        // `None` is falsy: the manager never suppresses the block exception.
        assert_eq!(method(suppress, "__exit__", &[NONE, NONE, NONE]), NONE);
    }

    // Generator bodies for the `contextmanager` tests. A generator function
    // value is created with `FUNCTION_FLAG_GENERATOR`, so calling it produces
    // a fiber whose body runs these entry points on the fiber stack.
    extern "C" fn cm_yield_once(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(int(1));
        NONE
    }

    extern "C" fn cm_yield_twice(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(int(1));
        crate::fiber::yield_value(int(2));
        NONE
    }

    extern "C" fn cm_yield_nothing(_argv: *const i64) -> i64 {
        NONE
    }

    extern "C" fn cm_echo(argv: *const i64) -> i64 {
        // SAFETY: the decorator test calls the generator function with exactly
        // one positional (or bound keyword) argument.
        let value = unsafe { *argv };
        crate::fiber::yield_value(value);
        NONE
    }

    extern "C" fn cm_swallow_yield(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(int(1));
        // `except Exception: pass`: the thrown exception surfaces as the
        // pending exception at the suspension point.
        if exc::pending() {
            let _ = exc::clear();
        }
        NONE
    }

    extern "C" fn cm_propagate_yield(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(int(1));
        // Leave the injected exception pending: it escapes the generator.
        NONE
    }

    extern "C" fn cm_swallow_then_yield(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(int(1));
        if exc::pending() {
            let _ = exc::clear();
        }
        crate::fiber::yield_value(int(2));
        NONE
    }

    extern "C" fn cm_swallow_then_raise(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(int(1));
        if exc::pending() {
            let _ = exc::clear();
        }
        exc::raise_with_message(VALUE_ERROR, "from generator");
        NONE
    }

    extern "C" fn cm_not_a_generator(_argv: *const i64) -> i64 {
        int(7)
    }

    /// A generator function value produced by the given fiber entry point.
    fn generator_function(
        entry: extern "C" fn(*const i64) -> i64,
        arity: usize,
        name: &str,
    ) -> i64 {
        value::function_new_with_flags(
            entry as usize,
            arity,
            name.to_string(),
            value::FUNCTION_FLAG_GENERATOR,
        )
    }

    /// Applies `contextlib.contextmanager` to `function` and calls the
    /// resulting decorator value with `args`.
    fn contextmanager_manager(function: i64, args: &[i64]) -> i64 {
        let decorator = call("contextlib", "contextmanager", &[function]);
        assert!(!exc::pending(), "{}", message());
        let manager = call_value(decorator, args);
        assert!(!exc::pending(), "{}", message());
        manager
    }

    /// A detached exception value, leaving no pending exception behind.
    fn exception(tag: u32, text: &str) -> i64 {
        exc::raise_with_message(tag, text);
        let value = exc::pending_value();
        let _ = exc::clear();
        value
    }

    #[test]
    fn contextlib_contextmanager_enter_advances_and_exit_finishes() {
        exc::init();
        let manager = contextmanager_manager(generator_function(cm_yield_once, 0, "once"), &[]);
        assert_eq!(
            value::class_display_name(
                value::instance_class_value(manager).expect("context manager instance")
            ),
            Some("_GeneratorContextManager")
        );
        assert_eq!(as_int(method(manager, "__enter__", &[])), 1);
        assert!(!exc::pending(), "{}", message());
        // The normal exit resumes the generator; it returns, so nothing is
        // suppressed.
        assert_eq!(method(manager, "__exit__", &[NONE, NONE, NONE]), FALSE);
        assert!(!exc::pending(), "{}", message());

        // Each decorated call owns a fresh generator: the same decorator can
        // be used again.
        let second = contextmanager_manager(generator_function(cm_yield_once, 0, "once"), &[]);
        assert_eq!(as_int(method(second, "__enter__", &[])), 1);
        assert_eq!(method(second, "__exit__", &[NONE, NONE, NONE]), FALSE);
    }

    #[test]
    fn contextlib_contextmanager_reports_didnt_yield_and_didnt_stop() {
        exc::init();
        let empty = contextmanager_manager(generator_function(cm_yield_nothing, 0, "nothing"), &[]);
        assert_eq!(method(empty, "__enter__", &[]), NONE);
        assert_eq!(exc::pending_tag(), crate::exc::RUNTIME_ERROR as i64);
        assert_eq!(message(), "generator didn't yield");

        exc::init();
        let twice = contextmanager_manager(generator_function(cm_yield_twice, 0, "twice"), &[]);
        assert_eq!(as_int(method(twice, "__enter__", &[])), 1);
        assert_eq!(method(twice, "__exit__", &[NONE, NONE, NONE]), NONE);
        assert_eq!(exc::pending_tag(), crate::exc::RUNTIME_ERROR as i64);
        assert_eq!(message(), "generator didn't stop");
        // The error path closed the suspended generator.
        assert!(crate::fiber::is_done(
            value::instance_attr(twice, "gen").expect("generator")
        ));
    }

    #[test]
    fn contextlib_contextmanager_exit_throws_the_block_exception() {
        exc::init();
        let swallowing =
            contextmanager_manager(generator_function(cm_swallow_yield, 0, "swallow"), &[]);
        assert_eq!(as_int(method(swallowing, "__enter__", &[])), 1);
        let boom = exception(VALUE_ERROR, "boom");
        // The generator handled the exception and returned: suppress it.
        assert_eq!(method(swallowing, "__exit__", &[NONE, boom, NONE]), TRUE);
        assert!(!exc::pending(), "{}", message());

        exc::init();
        let propagating =
            contextmanager_manager(generator_function(cm_propagate_yield, 0, "propagate"), &[]);
        assert_eq!(as_int(method(propagating, "__enter__", &[])), 1);
        let boom = exception(VALUE_ERROR, "boom");
        // The same exception escapes the generator: do not suppress it. The
        // `with` lowering re-raises the exception it is still holding.
        assert_eq!(method(propagating, "__exit__", &[NONE, boom, NONE]), FALSE);
        assert!(!exc::pending(), "{}", message());

        exc::init();
        let raising =
            contextmanager_manager(generator_function(cm_swallow_then_raise, 0, "raising"), &[]);
        assert_eq!(as_int(method(raising, "__enter__", &[])), 1);
        let boom = exception(VALUE_ERROR, "boom");
        // A different exception from the generator propagates unchanged.
        assert_eq!(method(raising, "__exit__", &[NONE, boom, NONE]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "from generator");
    }

    #[test]
    fn contextlib_contextmanager_reports_a_generator_that_keeps_going_after_throw() {
        exc::init();
        let manager = contextmanager_manager(
            generator_function(cm_swallow_then_yield, 0, "stubborn"),
            &[],
        );
        assert_eq!(as_int(method(manager, "__enter__", &[])), 1);
        let boom = exception(VALUE_ERROR, "boom");
        assert_eq!(method(manager, "__exit__", &[NONE, boom, NONE]), NONE);
        assert_eq!(exc::pending_tag(), crate::exc::RUNTIME_ERROR as i64);
        assert_eq!(message(), "generator didn't stop after throw()");
    }

    #[test]
    fn contextlib_contextmanager_forwards_positional_and_keyword_arguments() {
        exc::init();
        let manager = contextmanager_manager(generator_function(cm_echo, 1, "echo"), &[int(5)]);
        assert_eq!(as_int(method(manager, "__enter__", &[])), 5);
        assert_eq!(method(manager, "__exit__", &[NONE, NONE, NONE]), FALSE);

        exc::init();
        let function = generator_function(cm_echo, 1, "echo");
        value::function_set_params(function, vec!["value".to_string()]);
        let decorator = call("contextlib", "contextmanager", &[function]);
        assert!(!exc::pending(), "{}", message());
        let words = [string("value"), int(9)];
        let manager =
            crate::core::builtins::call_kw(decorator, 0, std::ptr::null(), 1, words.as_ptr());
        assert!(!exc::pending(), "{}", message());
        assert_eq!(as_int(method(manager, "__enter__", &[])), 9);
        assert_eq!(method(manager, "__exit__", &[NONE, NONE, NONE]), FALSE);
    }

    #[test]
    fn contextlib_contextmanager_rejects_a_non_generator_result() {
        exc::init();
        let entry: extern "C" fn(*const i64) -> i64 = cm_not_a_generator;
        let plain = value::function_new(entry as usize, 0, "plain".to_string());
        let manager = contextmanager_manager(plain, &[]);
        assert_eq!(method(manager, "__enter__", &[]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "'int' object is not a generator");
    }

    #[test]
    fn os_fspath_passes_through_and_asks_path_like_objects() {
        exc::init();
        let text_path = string("data.txt");
        assert_eq!(call("os", "fspath", &[text_path]), text_path);
        let raw = value::bytes_new(b"data.bin".to_vec());
        assert_eq!(call("os", "fspath", &[raw]), raw);

        let path = call("pathlib", "Path", &[string("/tmp/data.txt")]);
        assert_eq!(
            value::as_str(call("os", "fspath", &[path])),
            Some("/tmp/data.txt")
        );

        // A class implementing `__fspath__` is asked for its result; the
        // native function stands in for the user method and returns a str.
        let methods = vec![(
            "__fspath__".to_string(),
            value::native_function_new("builtins", "repr", 1),
        )];
        let class = value::class_new("PathLikeFixture".to_string(), NONE, methods);
        let fixture = value::instance_new(class);
        let resolved = call("os", "fspath", &[fixture]);
        assert!(!exc::pending(), "{}", message());
        assert!(value::as_str(resolved).is_some_and(|text| !text.is_empty()));
    }

    #[test]
    fn os_fspath_rejects_other_values_and_bad_fspath_results() {
        exc::init();
        assert_eq!(call("os", "fspath", &[int(7)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "expected str, bytes or os.PathLike object, not int"
        );

        exc::init();
        let methods = vec![(
            "__fspath__".to_string(),
            value::native_function_new("builtins", "id", 1),
        )];
        let class = value::class_new("BadPath".to_string(), NONE, methods);
        let bad = value::instance_new(class);
        assert_eq!(call("os", "fspath", &[bad]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "expected __fspath__() to return str or bytes, not int"
        );
    }

    #[test]
    fn pickle_dumps_accepts_protocol_and_rejects_out_of_range_versions() {
        exc::init();
        let payload = value::list_new(vec![int(1), string("two")]);
        let default = call("pickle", "dumps", &[payload]);
        let text = value::as_str(default).expect("dumps returns the HYD1 text");
        // "HYD1" is the format magic.
        assert!(text.starts_with("48594431"), "{text}");
        // The frontend binds `protocol=` into the second positional slot, so
        // every supported version yields the same version-independent payload.
        for protocol in [0, 5, -1] {
            let encoded = call("pickle", "dumps", &[payload, int(protocol)]);
            assert_eq!(value::as_str(encoded), Some(text));
        }

        exc::init();
        assert_eq!(call("pickle", "dumps", &[payload, int(6)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "pickle protocol must be <= 5");

        // A protocol that only exists as a heap integer is out of range too.
        exc::init();
        let huge = big("1180591620717411303424");
        assert_eq!(call("pickle", "dumps", &[payload, huge]), NONE);
        assert_eq!(message(), "pickle protocol must be <= 5");

        exc::init();
        assert_eq!(call("pickle", "dumps", &[payload, string("four")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "protocol must be an integer or None, not str");

        // `protocol` is a declared keyword parameter of both functions.
        let dumps = hydro_ir::stdlib::function("pickle", "dumps").expect("dumps spec");
        assert_eq!(dumps.params, &["obj", "protocol"][..]);
        assert_eq!(dumps.max_args, 2);
        let dump = hydro_ir::stdlib::function("pickle", "dump").expect("dump spec");
        assert_eq!(dump.params.len(), 5);
        assert_eq!(dump.params[0], "obj");
        assert_eq!(dump.params[1], "file");
        assert_eq!(dump.params[2], "protocol");
        assert_eq!(dump.params[3], "fix_imports");
        assert_eq!(dump.params[4], "buffer_callback");
        assert_eq!((dump.min_args, dump.max_args), (2, 5));
    }

    #[test]
    fn pickle_dump_writes_the_hyd1_text_to_binary_and_text_files() {
        exc::init();
        let dir = temp_root("pickle");
        let payload = value::list_new(vec![int(3), string("four")]);
        let text = value::as_str(call("pickle", "dumps", &[payload]))
            .expect("HYD1 text")
            .to_string();
        assert!(!exc::pending(), "{}", message());

        // Binary files receive the payload's UTF-8 bytes; the keyword-only
        // `fix_imports`/`buffer_callback` slots are accepted and ignored.
        let binary_path = dir.join("payload.bin");
        let binary_name = binary_path.to_string_lossy().into_owned();
        let file = call("io", "open", &[string(&binary_name), string("wb")]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            call("pickle", "dump", &[payload, file, NONE, TRUE, NONE]),
            NONE
        );
        assert!(!exc::pending(), "{}", message());
        method(file, "close", &[]);
        assert_eq!(
            std::fs::read(&binary_path).unwrap().as_slice(),
            text.as_bytes()
        );

        // Text files receive the `str` itself.
        let text_path = dir.join("payload.txt");
        let text_name = text_path.to_string_lossy().into_owned();
        let file = call("io", "open", &[string(&text_name), string("w")]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(call("pickle", "dump", &[payload, file]), NONE);
        assert!(!exc::pending(), "{}", message());
        method(file, "close", &[]);
        assert_eq!(std::fs::read_to_string(&text_path).unwrap(), text);

        // A non-file second argument is rejected instead of being dropped.
        exc::init();
        assert_eq!(
            call("pickle", "dump", &[payload, string("not a file")]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "file must be a writable file object, not str");

        // The protocol is validated before anything is written.
        exc::init();
        let empty_path = dir.join("empty.bin");
        let empty_name = empty_path.to_string_lossy().into_owned();
        let file = call("io", "open", &[string(&empty_name), string("wb")]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(call("pickle", "dump", &[payload, file, int(9)]), NONE);
        assert_eq!(message(), "pickle protocol must be <= 5");
        method(file, "close", &[]);
        assert!(std::fs::read(&empty_path).unwrap().is_empty());
    }

    fn tuple_ints(list: i64) -> Vec<Vec<i64>> {
        value::as_list(list)
            .unwrap_or_default()
            .iter()
            .map(|item| {
                value::as_tuple(*item)
                    .unwrap_or_default()
                    .iter()
                    .map(|value| value::as_integer(*value).unwrap_or(i64::MIN))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn itertools_permutations_product_and_takewhile_are_correct() {
        exc::init();
        let input = value::list_new(vec![int(1), int(2), int(3)]);
        let permutations = call("itertools", "permutations", &[input]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            tuple_ints(permutations),
            vec![
                vec![1, 2, 3],
                vec![1, 3, 2],
                vec![2, 1, 3],
                vec![2, 3, 1],
                vec![3, 1, 2],
                vec![3, 2, 1],
            ]
        );

        // `r` shorter than the input and `r > len(iterable)`.
        let pairs = call("itertools", "permutations", &[input, int(2)]);
        assert_eq!(
            tuple_ints(pairs),
            vec![
                vec![1, 2],
                vec![1, 3],
                vec![2, 1],
                vec![2, 3],
                vec![3, 1],
                vec![3, 2],
            ]
        );
        let none = call("itertools", "permutations", &[input, int(5)]);
        assert!(tuple_ints(none).is_empty());
        assert!(!exc::pending(), "{}", message());

        // `r = 0` yields exactly one empty tuple, and the empty pool does too.
        let zero = call("itertools", "permutations", &[input, int(0)]);
        assert!(!exc::pending(), "{}", message());
        let zero_rows = value::as_list(zero).unwrap_or_default();
        assert_eq!(zero_rows.len(), 1);
        assert_eq!(value::as_tuple(zero_rows[0]).map(|row| row.len()), Some(0));
        assert_eq!(tuple_ints(zero), vec![Vec::<i64>::new()]);
        let empty_pool = value::list_new(Vec::new());
        let empty = call("itertools", "permutations", &[empty_pool]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(tuple_ints(empty), vec![Vec::<i64>::new()]);

        // Repeated values are permuted by position: duplicates are emitted and
        // the rows still come out in non-decreasing lexicographic order.
        let duplicates = value::list_new(vec![int(1), int(1), int(2)]);
        let duplicated = call("itertools", "permutations", &[duplicates]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            tuple_ints(duplicated),
            vec![
                vec![1, 1, 2],
                vec![1, 2, 1],
                vec![1, 1, 2],
                vec![1, 2, 1],
                vec![2, 1, 1],
                vec![2, 1, 1],
            ]
        );

        // Every row is a fresh tuple (no row aliases another or the input),
        // and the input list is never reordered by the combinator.
        let rows = value::as_list(permutations).unwrap_or_default();
        assert_eq!(rows.len(), 6);
        for (offset, row) in rows.iter().enumerate() {
            assert!(
                value::as_tuple(*row).is_some(),
                "row {offset} is not a tuple"
            );
            for (other, peer) in rows.iter().enumerate().skip(offset + 1) {
                assert_ne!(*row, *peer, "rows {offset} and {other} alias");
            }
        }
        let remaining: Vec<i64> = value::as_list(input)
            .unwrap_or_default()
            .iter()
            .map(|item| value::as_integer(*item).unwrap_or(i64::MIN))
            .collect();
        assert_eq!(remaining, vec![1, 2, 3]);

        let left = value::list_new(vec![int(1), int(2)]);
        let right = value::list_new(vec![int(3), int(4)]);
        let product = call("itertools", "product", &[left, right]);
        assert_eq!(
            tuple_ints(product),
            vec![vec![1, 3], vec![1, 4], vec![2, 3], vec![2, 4],]
        );

        // `repeat` is keyword-only.
        let argv = [left];
        let keyword_name = string("repeat");
        let keyword_value = int(2);
        let keywords = [keyword_name, keyword_value];
        let repeated = module_call_kw(
            "itertools".as_ptr(),
            "itertools".len(),
            "product".as_ptr(),
            "product".len(),
            1,
            argv.as_ptr(),
            1,
            keywords.as_ptr(),
        );
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            tuple_ints(repeated),
            vec![vec![1, 1], vec![1, 2], vec![2, 1], vec![2, 2]]
        );

        // `repeat=0` is the one empty tuple; an empty pool short-circuits the
        // whole product; a negative repeat raises `ValueError`.
        let zero_keywords = [string("repeat"), int(0)];
        let collapsed = module_call_kw(
            "itertools".as_ptr(),
            "itertools".len(),
            "product".as_ptr(),
            "product".len(),
            1,
            argv.as_ptr(),
            1,
            zero_keywords.as_ptr(),
        );
        assert!(!exc::pending(), "{}", message());
        assert_eq!(tuple_ints(collapsed), vec![Vec::<i64>::new()]);

        let no_items = value::list_new(Vec::new());
        let empty_product = call("itertools", "product", &[no_items, left]);
        assert!(!exc::pending(), "{}", message());
        assert!(tuple_ints(empty_product).is_empty());

        let negative_keywords = [string("repeat"), int(-1)];
        let _ = module_call_kw(
            "itertools".as_ptr(),
            "itertools".len(),
            "product".as_ptr(),
            "product".len(),
            1,
            argv.as_ptr(),
            1,
            negative_keywords.as_ptr(),
        );
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        assert_eq!(message(), "repeat argument cannot be negative");
        exc::clear();

        let predicate = get("operator", "truth");
        let values = value::list_new(vec![int(1), int(2), int(0), int(4)]);
        let prefix = call("itertools", "takewhile", &[predicate, values]);
        assert!(!exc::pending(), "{}", message());
        let prefix: Vec<i64> = value::as_list(prefix)
            .unwrap_or_default()
            .iter()
            .map(|item| value::as_integer(*item).unwrap_or(i64::MIN))
            .collect();
        assert_eq!(prefix, vec![1, 2]);
    }

    #[test]
    fn itertools_combinations_are_lexicographic() {
        exc::init();
        let input = value::list_new(vec![int(1), int(2), int(3), int(4)]);

        let pairs = call("itertools", "combinations", &[input, int(2)]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            tuple_ints(pairs),
            vec![
                vec![1, 2],
                vec![1, 3],
                vec![1, 4],
                vec![2, 3],
                vec![2, 4],
                vec![3, 4],
            ]
        );

        // `r == 0` yields exactly one empty tuple; `r > n` yields nothing.
        let zero = call("itertools", "combinations", &[input, int(0)]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(tuple_ints(zero), vec![Vec::<i64>::new()]);
        let too_many = call("itertools", "combinations", &[input, int(5)]);
        assert!(!exc::pending(), "{}", message());
        assert!(tuple_ints(too_many).is_empty());

        // The empty pool only produces the empty selection.
        let empty_pool = value::list_new(Vec::new());
        let empty_zero = call("itertools", "combinations", &[empty_pool, int(0)]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(tuple_ints(empty_zero), vec![Vec::<i64>::new()]);
        let empty_one = call("itertools", "combinations", &[empty_pool, int(1)]);
        assert!(!exc::pending(), "{}", message());
        assert!(tuple_ints(empty_one).is_empty());

        // Positions are combined, so repeated values produce repeated rows.
        let duplicates = value::list_new(vec![int(1), int(1), int(2)]);
        let duplicated = call("itertools", "combinations", &[duplicates, int(2)]);
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            tuple_ints(duplicated),
            vec![vec![1, 1], vec![1, 2], vec![1, 2]]
        );

        // Rows are fresh tuples placed in the result list, and the pool keeps
        // its order.
        let rows = value::as_list(pairs).unwrap_or_default();
        assert_eq!(rows.len(), 6);
        for (offset, row) in rows.iter().enumerate() {
            assert!(
                value::as_tuple(*row).is_some(),
                "row {offset} is not a tuple"
            );
            for (other, peer) in rows.iter().enumerate().skip(offset + 1) {
                assert_ne!(*row, *peer, "rows {offset} and {other} alias");
            }
        }
        let remaining: Vec<i64> = value::as_list(input)
            .unwrap_or_default()
            .iter()
            .map(|item| value::as_integer(*item).unwrap_or(i64::MIN))
            .collect();
        assert_eq!(remaining, vec![1, 2, 3, 4]);
    }

    #[test]
    fn itertools_combinations_with_replacement_reuses_positions() {
        exc::init();
        let input = value::list_new(vec![int(1), int(2), int(3)]);

        let pairs = call(
            "itertools",
            "combinations_with_replacement",
            &[input, int(2)],
        );
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            tuple_ints(pairs),
            vec![
                vec![1, 1],
                vec![1, 2],
                vec![1, 3],
                vec![2, 2],
                vec![2, 3],
                vec![3, 3],
            ]
        );

        // `r` may exceed the pool; only the empty pool cannot supply it.
        // Replacement allows repeated positions, so every length-5 multiset
        // of the three values appears: C(3 + 5 - 1, 5) == C(7, 5) == 21 rows,
        // in the order CPython's `combinations_with_replacement` yields.
        let longer = call(
            "itertools",
            "combinations_with_replacement",
            &[input, int(5)],
        );
        assert!(!exc::pending(), "{}", message());
        assert_eq!(
            tuple_ints(longer),
            vec![
                vec![1, 1, 1, 1, 1],
                vec![1, 1, 1, 1, 2],
                vec![1, 1, 1, 1, 3],
                vec![1, 1, 1, 2, 2],
                vec![1, 1, 1, 2, 3],
                vec![1, 1, 1, 3, 3],
                vec![1, 1, 2, 2, 2],
                vec![1, 1, 2, 2, 3],
                vec![1, 1, 2, 3, 3],
                vec![1, 1, 3, 3, 3],
                vec![1, 2, 2, 2, 2],
                vec![1, 2, 2, 2, 3],
                vec![1, 2, 2, 3, 3],
                vec![1, 2, 3, 3, 3],
                vec![1, 3, 3, 3, 3],
                vec![2, 2, 2, 2, 2],
                vec![2, 2, 2, 2, 3],
                vec![2, 2, 2, 3, 3],
                vec![2, 2, 3, 3, 3],
                vec![2, 3, 3, 3, 3],
                vec![3, 3, 3, 3, 3],
            ]
        );

        let zero = call(
            "itertools",
            "combinations_with_replacement",
            &[input, int(0)],
        );
        assert!(!exc::pending(), "{}", message());
        assert_eq!(tuple_ints(zero), vec![Vec::<i64>::new()]);

        let empty_pool = value::list_new(Vec::new());
        let empty_zero = call(
            "itertools",
            "combinations_with_replacement",
            &[empty_pool, int(0)],
        );
        assert!(!exc::pending(), "{}", message());
        assert_eq!(tuple_ints(empty_zero), vec![Vec::<i64>::new()]);
        let empty_one = call(
            "itertools",
            "combinations_with_replacement",
            &[empty_pool, int(1)],
        );
        assert!(!exc::pending(), "{}", message());
        assert!(tuple_ints(empty_one).is_empty());
    }

    #[test]
    fn itertools_combinations_validate_r() {
        exc::init();
        let input = value::list_new(vec![int(1), int(2)]);
        call("itertools", "combinations", &[input, int(-1)]);
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        assert_eq!(message(), "r must be non-negative");
        exc::clear();
        call(
            "itertools",
            "combinations_with_replacement",
            &[input, int(-1)],
        );
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        assert_eq!(message(), "r must be non-negative");
        exc::clear();

        let not_an_integer = value::float_new(1.0);
        call("itertools", "combinations", &[input, not_an_integer]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        assert_eq!(message(), "integer argument expected, got float");
        exc::clear();
        call(
            "itertools",
            "combinations_with_replacement",
            &[input, not_an_integer],
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        assert_eq!(message(), "integer argument expected, got float");
        exc::clear();
    }
}
