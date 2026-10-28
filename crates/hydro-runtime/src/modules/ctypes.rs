//! `ctypes` / `_ctypes`: a bounded, memory-safe subset of `ctypes`.
//!
//! The module implements the parts of `ctypes` that an interpreter can provide
//! without exposing arbitrary process memory:
//!
//! - the C scalar types with their real sizes/alignments (`sizeof`,
//!   `alignment`, `offsetof`), including the `c_*` aliases;
//! - `Structure`/`Union`/`Array` classes defined by subclassing plus
//!   `_fields_`, `_pack_`, `_type_`/`_length_`, with natural or packed layout;
//! - `POINTER`, `pointer`, `byref`, `cast`, `addressof`, `contents`;
//! - `create_string_buffer`, `create_unicode_buffer`, `string_at`,
//!   `wstring_at`, `memmove`, `memset`, `resize`;
//! - `CDLL`/`LibraryLoader` with real `dlopen`/`dlsym`;
//! - `get_errno`/`set_errno` over the calling thread's `errno`, and the
//!   `CField` field descriptor;
//! - `ctypes.wintypes`: the portable alias subset of the Windows type module
//!   (the Windows-only calls themselves raise `OSError` off Windows);
//! - `CFUNCTYPE`-style callbacks for up to eight integer/pointer arguments.
//!
//! # Memory model
//!
//! Every ctypes object owns a slot in a process-global arena instead of a raw
//! `malloc` block. Pointers are *checked*: `addr`/`string_at`/`memmove`/
//! `memset`/`contents` only accept addresses that fall inside an arena slot,
//! and every access is bounded by the slot and by the object's declared size.
//! Reading outside the arena raises `ValueError` instead of dereferencing
//! memory, so no Python code can use this module for arbitrary reads. Slots
//! live for the process lifetime; `ctypes` in CPython likewise leaves
//! memory management to the caller.
//!
//! # Calling C
//!
//! `CDLL` loads real shared objects and exposes every dynamic function symbol
//! as a callable object. Calls use the platform C ABI only for
//! integer/pointer-class arguments and results: `float`, `double`, structure
//! and array arguments/results raise `NotImplementedError` instead of passing
//! garbage. Each call validates the argument count and converts Python values
//! exactly as `ctypes` does for the declared types.
//!
//! Callback trampolines call back into Hydro and therefore only work while a
//! Hydro thread is running (i.e. during a `CDLL` call). They support up to
//! eight integer/pointer arguments; float/struct signatures raise
//! `NotImplementedError` when the `CFUNCTYPE` object is created.
//!
//! # Unsupported surfaces
//!
//! `(T * n)` (multiplying a type by an integer) is not available because class
//! values have no runtime `__mul__`; use `Array` subclasses or
//! `create_string_buffer`. Bitfield *values* (not layout) raise
//! `NotImplementedError`. `PQ`/COM helpers are absent.

use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, INDEX_ERROR, NOT_IMPLEMENTED_ERROR, OS_ERROR, OVERFLOW_ERROR,
    TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::value::{self, ClassEntry, NONE};

/// Runtime module that owns the native function values.
const MODULE: &str = "ctypes";

/// `_ctypes.__version__` in CPython 3.14.
const VERSION: &str = "1.1.0";

/// Upper bound for one arena slot (defensive: sizes come from Python ints).
const MAX_SLOT: usize = 1 << 30;

/// Upper bound for one `memmove`/`memset`/`string_at` request.
const MAX_TRANSFER: usize = 1 << 24;

/// Upper bound for enumerated library symbols.
const MAX_SYMBOLS: usize = 20_000;

const RTLD_LOCAL: i64 = 0;
const RTLD_GLOBAL: i64 = 0x100;
const RTLD_NOW: i64 = 0x2;

/// `ctypes.CDLL`/`LoadLibrary` default mode on POSIX.
const DEFAULT_MODE: i64 = RTLD_LOCAL;

/// Function-pointer flags kept for import compatibility.
const FUNCFLAG_CDECL: i64 = 0;
const FUNCFLAG_PYTHONAPI: i64 = 4;
const FUNCFLAG_USE_ERRNO: i64 = 1;
const FUNCFLAG_USE_LASTERROR: i64 = 2;

/// Native function names used by the class methods below.
const INIT: &str = "_cdata_init";
const GET_VALUE: &str = "_cdata_get_value";
const SET_VALUE: &str = "_cdata_set_value";
const GET_CONTENTS: &str = "_cdata_get_contents";
const GET_RAW: &str = "_cdata_get_raw";
const SET_RAW: &str = "_cdata_set_raw";
const LEN: &str = "_cdata_len";
const GETITEM: &str = "_cdata_getitem";
const SETITEM: &str = "_cdata_setitem";
const STR: &str = "_cdata_str";
const REPR: &str = "_cdata_repr";
const CALL: &str = "_cdata_call";
const ADD: &str = "_cdata_add";
const RADD: &str = "_cdata_radd";
const CDLL_INIT: &str = "_cdll_init";
const CDLL_GETITEM: &str = "_cdll_getitem";
const LOADER_INIT: &str = "_loader_init";
const LOADER_LOAD: &str = "_loader_load";
const LOADER_GETITEM: &str = "_loader_getitem";

type TypeId = u64;

/// Raises `msg` with `tag` and returns the `None` dummy.
fn raise(tag: u32, msg: &str) -> i64 {
    exc::raise_with_message(tag, msg)
}

fn type_error(msg: &str) -> i64 {
    raise(TYPE_ERROR, msg)
}

/// Encodes an integer that is known to fit the tagged range.
fn encode(n: i64) -> i64 {
    value::encode_int(n).unwrap_or_else(|| raise(OVERFLOW_ERROR, "integer overflow"))
}

/// Encodes a length/offset that must be non-negative.
fn encode_len(n: usize) -> i64 {
    match i64::try_from(n) {
        Ok(n) => encode(n),
        Err(_) => raise(OVERFLOW_ERROR, "ctypes size is out of range"),
    }
}

/// Encodes an unsigned machine word, promoting values above `i64::MAX`.
fn unsigned_result(n: u64) -> i64 {
    match i64::try_from(n) {
        Ok(n) => encode(n),
        Err(_) => value::bigint_new(num_bigint::BigInt::from(n)),
    }
}

/// Reads a Python integer argument.
fn int_arg(v: i64, what: &str) -> Result<i64, i64> {
    value::as_integer(v).ok_or_else(|| {
        raise(
            TYPE_ERROR,
            &format!("{what} must be an integer, not {}", value::type_name(v)),
        )
    })
}

/// Reads a Python string argument.
fn str_arg(v: i64, what: &str) -> Result<String, i64> {
    value::as_str(v).map(str::to_string).ok_or_else(|| {
        raise(
            TYPE_ERROR,
            &format!("{what} must be str, not {}", value::type_name(v)),
        )
    })
}

fn as_items(v: i64) -> Option<Vec<i64>> {
    if let Some(items) = value::as_list(v) {
        return Some(items.to_vec());
    }
    value::as_tuple(v).map(<[i64]>::to_vec)
}

// -------------------------------------------------------------------------
// Simple C types
// -------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Simple {
    Bool,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    LongDouble,
    CChar,
    WChar,
    VoidP,
    CharP,
    WCharP,
}

impl Simple {
    /// Every scalar the module exposes, in type-id allocation order.
    const ALL: [Simple; 17] = [
        Simple::Bool,
        Simple::I8,
        Simple::U8,
        Simple::I16,
        Simple::U16,
        Simple::I32,
        Simple::U32,
        Simple::I64,
        Simple::U64,
        Simple::F32,
        Simple::F64,
        Simple::LongDouble,
        Simple::CChar,
        Simple::WChar,
        Simple::VoidP,
        Simple::CharP,
        Simple::WCharP,
    ];

    fn name(self) -> &'static str {
        match self {
            Simple::Bool => "c_bool",
            Simple::I8 => "c_byte",
            Simple::U8 => "c_ubyte",
            Simple::I16 => "c_short",
            Simple::U16 => "c_ushort",
            Simple::I32 => "c_int",
            Simple::U32 => "c_uint",
            Simple::I64 => "c_long",
            Simple::U64 => "c_ulong",
            Simple::F32 => "c_float",
            Simple::F64 => "c_double",
            Simple::LongDouble => "c_longdouble",
            Simple::CChar => "c_char",
            Simple::WChar => "c_wchar",
            Simple::VoidP => "c_void_p",
            Simple::CharP => "c_char_p",
            Simple::WCharP => "c_wchar_p",
        }
    }

    fn size(self) -> usize {
        match self {
            Simple::Bool | Simple::I8 | Simple::U8 | Simple::CChar => 1,
            Simple::I16 | Simple::U16 => 2,
            Simple::I32 | Simple::U32 | Simple::F32 | Simple::WChar => 4,
            Simple::I64 | Simple::U64 | Simple::F64 => 8,
            // x86_64 SysV: 80-bit extended stored in 16 bytes.
            Simple::LongDouble => 16,
            Simple::VoidP | Simple::CharP | Simple::WCharP => 8,
        }
    }

    fn align(self) -> usize {
        match self {
            Simple::LongDouble => 16,
            other => other.size(),
        }
    }

    fn is_integer(self) -> bool {
        matches!(
            self,
            Simple::I8
                | Simple::U8
                | Simple::I16
                | Simple::U16
                | Simple::I32
                | Simple::U32
                | Simple::I64
                | Simple::U64
                | Simple::CChar
                | Simple::WChar
        )
    }

    fn is_signed(self) -> bool {
        matches!(
            self,
            Simple::I8 | Simple::I16 | Simple::I32 | Simple::I64 | Simple::CChar
        )
    }

    fn is_pointer(self) -> bool {
        matches!(self, Simple::VoidP | Simple::CharP | Simple::WCharP)
    }

    fn is_float(self) -> bool {
        matches!(self, Simple::F32 | Simple::F64 | Simple::LongDouble)
    }

    /// Python type exposed as `_type_` (used by `numpy._core._dtype_ctypes`).
    fn python_type(self) -> i64 {
        match self {
            Simple::CChar | Simple::CharP => value::type_class(value::bytes_new(Vec::new())),
            Simple::WChar | Simple::WCharP => value::type_class(value::str_alloc(String::new())),
            Simple::F32 | Simple::F64 | Simple::LongDouble => {
                value::type_class(value::float_new(0.0))
            }
            Simple::Bool => value::type_class(value::TRUE),
            _ => value::type_class(value::encode_int(0).unwrap_or(NONE)),
        }
    }
}

/// Parsed C type.
#[derive(Clone, Debug)]
enum CType {
    Simple(Simple),
    Pointer(TypeId),
    Array {
        elem: TypeId,
        len: usize,
    },
    Struct {
        union: bool,
        fields: Vec<FieldLayout>,
        size: usize,
        align: usize,
    },
    Func {
        restype: TypeId,
        argtypes: Vec<TypeId>,
    },
    /// `CFUNCTYPE(None, ...)`: no return value.
    Void,
}

/// One declared `Structure`/`Union` field.
#[derive(Clone, Debug)]
struct FieldLayout {
    name: String,
    ty: TypeId,
    offset: usize,
    /// Bit width for a bitfield; the value access is unsupported.
    bits: Option<u32>,
    bit_offset: u32,
}

/// Owned byte storage for one ctypes object.
struct Slot {
    bytes: Vec<u8>,
}

/// Process-global ctypes bookkeeping.
struct State {
    types: HashMap<TypeId, CType>,
    next_type: TypeId,
    simple_ids: HashMap<Simple, TypeId>,
    pointer_types: HashMap<TypeId, TypeId>,
    func_types: HashMap<(TypeId, Vec<TypeId>), TypeId>,
    void: Option<TypeId>,
    slots: HashMap<u64, Slot>,
    next_slot: u64,
}

impl State {
    fn new() -> Self {
        let mut state = State {
            types: HashMap::new(),
            next_type: 1,
            simple_ids: HashMap::new(),
            pointer_types: HashMap::new(),
            func_types: HashMap::new(),
            void: None,
            slots: HashMap::new(),
            next_slot: 1,
        };
        for simple in Simple::ALL {
            let id = state.next_type;
            state.next_type += 1;
            state.types.insert(id, CType::Simple(simple));
            state.simple_ids.insert(simple, id);
        }
        state
    }

    fn register(&mut self, ty: CType) -> TypeId {
        let id = self.next_type;
        self.next_type += 1;
        self.types.insert(id, ty);
        id
    }

    fn simple_id(&self, simple: Simple) -> TypeId {
        self.simple_ids.get(&simple).copied().unwrap_or(1)
    }

    fn pointer_to(&mut self, pointee: TypeId) -> TypeId {
        if let Some(&id) = self.pointer_types.get(&pointee) {
            return id;
        }
        let id = self.register(CType::Pointer(pointee));
        self.pointer_types.insert(pointee, id);
        id
    }

    fn func_type(&mut self, restype: TypeId, argtypes: Vec<TypeId>) -> TypeId {
        let key = (restype, argtypes.clone());
        if let Some(&id) = self.func_types.get(&key) {
            return id;
        }
        let id = self.register(CType::Func { restype, argtypes });
        self.func_types.insert(key, id);
        id
    }

    fn void_type(&mut self) -> TypeId {
        if let Some(id) = self.void {
            return id;
        }
        let id = self.register(CType::Void);
        self.void = Some(id);
        id
    }

    fn alloc(&mut self, size: usize) -> u64 {
        let size = size.min(MAX_SLOT);
        let id = self.next_slot;
        self.next_slot += 1;
        self.slots.insert(
            id,
            Slot {
                bytes: vec![0u8; size.max(1)],
            },
        );
        id
    }

    fn slot(&self, id: u64) -> Option<&Slot> {
        self.slots.get(&id)
    }

    /// Base address of a slot (stable for the process lifetime).
    fn base(&self, id: u64) -> Option<usize> {
        self.slots.get(&id).map(|slot| slot.bytes.as_ptr() as usize)
    }

    /// Resolves `addr..addr+len` to `(slot, offset)`, refusing foreign memory.
    fn locate(&self, addr: usize, len: usize) -> Option<(u64, usize)> {
        if len > MAX_TRANSFER {
            return None;
        }
        for (&id, slot) in &self.slots {
            let base = slot.bytes.as_ptr() as usize;
            let end = base.checked_add(slot.bytes.len())?;
            let limit = addr.checked_add(len)?;
            if addr >= base && limit <= end {
                return Some((id, addr - base));
            }
        }
        None
    }

    /// Reads `len` bytes starting at a slot offset.
    fn read_bytes(&self, id: u64, offset: usize, len: usize) -> Option<&[u8]> {
        let slot = self.slots.get(&id)?;
        slot.bytes.get(offset..offset.checked_add(len)?)
    }

    fn write_bytes(&mut self, id: u64, offset: usize, data: &[u8]) -> bool {
        let Some(slot) = self.slots.get_mut(&id) else {
            return false;
        };
        if offset
            .checked_add(data.len())
            .is_none_or(|end| end > slot.bytes.len())
        {
            return false;
        }
        slot.bytes[offset..offset + data.len()].copy_from_slice(data);
        true
    }

    fn type_of(&self, id: TypeId) -> Option<CType> {
        self.types.get(&id).cloned()
    }

    fn size_of(&self, id: TypeId) -> usize {
        match self.types.get(&id) {
            Some(CType::Simple(simple)) => simple.size(),
            Some(CType::Pointer(_)) | Some(CType::Func { .. }) => 8,
            Some(CType::Array { elem, len }) => self.size_of(*elem).saturating_mul(*len),
            Some(CType::Struct { size, .. }) => *size,
            Some(CType::Void) | None => 0,
        }
    }

    fn align_of(&self, id: TypeId) -> usize {
        match self.types.get(&id) {
            Some(CType::Simple(simple)) => simple.align(),
            Some(CType::Pointer(_)) | Some(CType::Func { .. }) => 8,
            Some(CType::Array { elem, .. }) => self.align_of(*elem),
            Some(CType::Struct { align, .. }) => *align,
            Some(CType::Void) | None => 1,
        }
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::new()));

fn state() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// -------------------------------------------------------------------------
// Class construction and caches
// -------------------------------------------------------------------------

fn class_key(prefix: &str, value: impl std::fmt::Display) -> String {
    format!("ctypes.class.{prefix}.{value}")
}

fn cached(key: &str) -> Option<i64> {
    globals::registry_get(key)
}

fn native(name: &'static str, arity: usize) -> i64 {
    value::native_function_new(MODULE, name, arity)
}

/// `_CData`: the base class of every ctypes value.
fn cdata_class() -> i64 {
    if let Some(class) = cached("ctypes.class._CData") {
        return class;
    }
    let methods = vec![
        (
            "__init__".to_string(),
            native(INIT, usize::MAX),
            MethodKind::Normal,
        ),
        ("__len__".to_string(), native(LEN, 1), MethodKind::Normal),
        (
            "__getitem__".to_string(),
            native(GETITEM, 2),
            MethodKind::Normal,
        ),
        (
            "__setitem__".to_string(),
            native(SETITEM, 3),
            MethodKind::Normal,
        ),
        ("__str__".to_string(), native(STR, 1), MethodKind::Normal),
        ("__repr__".to_string(), native(REPR, 1), MethodKind::Normal),
        (
            "__call__".to_string(),
            native(CALL, usize::MAX),
            MethodKind::Normal,
        ),
        ("__add__".to_string(), native(ADD, 2), MethodKind::Normal),
        ("__radd__".to_string(), native(RADD, 2), MethodKind::Normal),
    ];
    let attrs = vec![
        (
            "value".to_string(),
            value::property_new(native(GET_VALUE, 1), native(SET_VALUE, 2), NONE),
        ),
        (
            "contents".to_string(),
            value::property_new(native(GET_CONTENTS, 1), NONE, NONE),
        ),
        (
            "raw".to_string(),
            value::property_new(native(GET_RAW, 1), native(SET_RAW, 2), NONE),
        ),
        (
            "_hydro_kind".to_string(),
            value::str_alloc("cdata".to_string()),
        ),
    ];
    let class = value::class_new_full("_CData".to_string(), NONE, methods, attrs);
    globals::registry_set("ctypes.class._CData", class);
    class
}

/// `_SimpleCData`: base class of the scalar ctypes.
fn simple_cdata_class() -> i64 {
    if let Some(class) = cached("ctypes.class._SimpleCData") {
        return class;
    }
    let tid = {
        let state = state();
        state.simple_id(Simple::I32)
    };
    let attrs = vec![
        (
            "_hydro_kind".to_string(),
            value::str_alloc("simple".to_string()),
        ),
        ("_hydro_type".to_string(), encode(tid as i64)),
    ];
    let class = value::class_new_full("_SimpleCData".to_string(), cdata_class(), Vec::new(), attrs);
    globals::registry_set("ctypes.class._SimpleCData", class);
    class
}

fn simple_class(simple: Simple) -> i64 {
    let key = class_key("simple", simple.name());
    if let Some(class) = cached(&key) {
        return class;
    }
    let tid = {
        let state = state();
        state.simple_id(simple)
    };
    let attrs = vec![
        (
            "_hydro_kind".to_string(),
            value::str_alloc("simple".to_string()),
        ),
        ("_hydro_type".to_string(), encode(tid as i64)),
        ("_type_".to_string(), simple.python_type()),
    ];
    let class = value::class_new_full(
        simple.name().to_string(),
        simple_cdata_class(),
        Vec::new(),
        attrs,
    );
    globals::registry_set(&key, class);
    class
}

/// A shared class skeleton for the structural bases.
fn base_class(cache: &str, name: &str, base: i64, kind: &str, attrs: Vec<(String, i64)>) -> i64 {
    if let Some(class) = cached(cache) {
        return class;
    }
    let mut all = vec![(
        "_hydro_kind".to_string(),
        value::str_alloc(kind.to_string()),
    )];
    all.extend(attrs);
    let class = value::class_new_full(name.to_string(), base, Vec::new(), all);
    globals::registry_set(cache, class);
    class
}

fn func_base_type() -> TypeId {
    let mut state = state();
    let restype = state.simple_id(Simple::I32);
    state.func_type(restype, Vec::new())
}

/// `Structure`.
fn structure_class() -> i64 {
    base_class(
        "ctypes.class.Structure",
        "Structure",
        cdata_class(),
        "struct",
        vec![
            ("_fields_".to_string(), value::list_new(Vec::new())),
            ("_pack_".to_string(), encode(0)),
            ("_anonymous_".to_string(), value::list_new(Vec::new())),
        ],
    )
}

/// `Union`.
fn union_class() -> i64 {
    base_class(
        "ctypes.class.Union",
        "Union",
        cdata_class(),
        "union",
        vec![
            ("_fields_".to_string(), value::list_new(Vec::new())),
            ("_pack_".to_string(), encode(0)),
        ],
    )
}

/// `Array`.
fn array_class() -> i64 {
    let elem = {
        let state = state();
        state.simple_id(Simple::U8)
    };
    base_class(
        "ctypes.class.Array",
        "Array",
        cdata_class(),
        "array",
        vec![
            ("_type_".to_string(), class_for_type(elem)),
            ("_length_".to_string(), encode(0)),
        ],
    )
}

/// `_Pointer`.
fn pointer_base_class() -> i64 {
    if let Some(class) = cached("ctypes.class._Pointer") {
        return class;
    }
    let (tid, pointee) = {
        let mut state = state();
        let pointee = state.simple_id(Simple::U8);
        (state.pointer_to(pointee), pointee)
    };
    base_class(
        "ctypes.class._Pointer",
        "_Pointer",
        cdata_class(),
        "pointer",
        vec![
            ("_hydro_type".to_string(), encode(tid as i64)),
            ("_hydro_pointee".to_string(), encode(pointee as i64)),
            ("_type_".to_string(), class_for_type(pointee)),
        ],
    )
}

/// `CFuncPtr` / `_CFuncPtr`.
fn cfunc_class() -> i64 {
    if let Some(class) = cached("ctypes.class._CFuncPtr") {
        return class;
    }
    let tid = func_base_type();
    base_class(
        "ctypes.class._CFuncPtr",
        "_CFuncPtr",
        cdata_class(),
        "func",
        vec![("_hydro_type".to_string(), encode(tid as i64))],
    )
}

/// Resolves (creating on demand) the class that represents `tid`.
fn class_for_type(tid: TypeId) -> i64 {
    let key = class_key("type", tid);
    if let Some(class) = cached(&key) {
        return class;
    }
    let owner_key = format!("ctypes.owner.{tid}");
    if let Some(class) = cached(&owner_key) {
        return class;
    }
    let info = {
        let state = state();
        state.type_of(tid)
    };
    let Some(info) = info else {
        return type_error("unknown ctypes type");
    };
    let class = match &info {
        CType::Simple(simple) => simple_class(*simple),
        CType::Pointer(pointee) => {
            let pointee_class = class_for_type(*pointee);
            let name = format!(
                "LP_{}",
                value::class_display_name(pointee_class).unwrap_or("c_void")
            );
            let attrs = vec![
                (
                    "_hydro_kind".to_string(),
                    value::str_alloc("pointer".to_string()),
                ),
                ("_hydro_type".to_string(), encode(tid as i64)),
                ("_hydro_pointee".to_string(), encode(*pointee as i64)),
                ("_type_".to_string(), pointee_class),
            ];
            value::class_new_full(name, pointer_base_class(), Vec::new(), attrs)
        }
        CType::Array { elem, len } => {
            let elem_class = class_for_type(*elem);
            let name = format!(
                "{}_Array_{}",
                value::class_display_name(elem_class).unwrap_or("c_ubyte"),
                len
            );
            let attrs = vec![
                (
                    "_hydro_kind".to_string(),
                    value::str_alloc("array".to_string()),
                ),
                ("_hydro_type".to_string(), encode(tid as i64)),
                ("_type_".to_string(), elem_class),
                ("_length_".to_string(), encode_len(*len)),
            ];
            value::class_new_full(name, array_class(), Vec::new(), attrs)
        }
        CType::Struct { .. } => structure_class(),
        CType::Void => simple_class(Simple::U8),
        CType::Func { restype, argtypes } => {
            let default_func = {
                let state = state();
                argtypes.is_empty() && *restype == state.simple_id(Simple::I32)
            };
            if default_func {
                cfunc_class()
            } else {
                let restype_is_void = {
                    let state = state();
                    matches!(state.type_of(*restype), Some(CType::Void))
                };
                let restype_class = if restype_is_void {
                    NONE
                } else {
                    class_for_type(*restype)
                };
                let args: Vec<i64> = argtypes.iter().map(|&id| class_for_type(id)).collect();
                let attrs = vec![
                    (
                        "_hydro_kind".to_string(),
                        value::str_alloc("func".to_string()),
                    ),
                    ("_hydro_type".to_string(), encode(tid as i64)),
                    ("restype".to_string(), restype_class),
                    ("argtypes".to_string(), value::tuple_new(args)),
                ];
                value::class_new_full(
                    "CFunctionType".to_string(),
                    cfunc_class(),
                    Vec::new(),
                    attrs,
                )
            }
        }
    };
    globals::registry_set(&key, class);
    class
}

/// Reads one class attribute.
fn class_attr(class: i64, name: &str) -> Option<i64> {
    match value::class_lookup(class, name) {
        Some(ClassEntry::Attr(v)) => Some(v),
        _ => None,
    }
}

fn class_int_attr(class: i64, name: &str) -> Option<i64> {
    value::as_integer(class_attr(class, name)?)
}

fn class_kind(class: i64) -> Option<&'static str> {
    value::as_str(class_attr(class, "_hydro_kind")?)
}

/// Type id of a class value, computing a user struct/array layout if needed.
fn type_id_of_class(class: i64) -> Result<TypeId, i64> {
    if let Some(tid) = class_int_attr(class, "_hydro_type") {
        if tid > 0 {
            return Ok(tid as TypeId);
        }
    }
    let layout_key = format!("ctypes.layout.{class}");
    if let Some(tid) = cached(&layout_key).and_then(value::as_integer) {
        return Ok(tid as TypeId);
    }
    let tid = layout_for_class(class)?;
    globals::registry_set(&layout_key, encode(tid as i64));
    Ok(tid)
}

/// Type id of either a ctype class or a ctypes instance.
fn type_id_of(value_arg: i64) -> Result<TypeId, i64> {
    if value::as_instance(value_arg).is_some() {
        if let Some(tid) = value::instance_attr(value_arg, "_hydro_type") {
            if let Some(tid) = value::as_integer(tid) {
                if tid > 0 {
                    return Ok(tid as TypeId);
                }
            }
        }
        let class = value::instance_class_value(value_arg)
            .ok_or_else(|| type_error("not a ctypes object"))?;
        return type_id_of_class(class);
    }
    if value::as_class(value_arg).is_some() {
        return type_id_of_class(value_arg);
    }
    Err(type_error(&format!(
        "expected a ctypes type or instance, not {}",
        value::type_name(value_arg)
    )))
}

/// Builds and registers the layout of a user `Structure`/`Union`/`Array`.
fn layout_for_class(class: i64) -> Result<TypeId, i64> {
    let name = value::class_display_name(class)
        .unwrap_or("ctypes type")
        .to_string();
    match class_kind(class) {
        Some("struct") => layout_struct(class, false),
        Some("union") => layout_struct(class, true),
        Some("array") => layout_array(class),
        Some("simple") | Some("pointer") | Some("func") => {
            Err(type_error(&format!("{name} has no declared size")))
        }
        _ => Err(type_error(&format!("'{name}' is not a ctypes type"))),
    }
}

fn layout_array(class: i64) -> Result<TypeId, i64> {
    let name = value::class_display_name(class)
        .unwrap_or("Array")
        .to_string();
    let elem = class_attr(class, "_type_")
        .ok_or_else(|| type_error(&format!("{name} does not define _type_")))?;
    let elem_tid = type_id_of(elem)?;
    let len = class_int_attr(class, "_length_")
        .filter(|len| *len >= 0)
        .ok_or_else(|| type_error(&format!("{name} does not define _length_")))?
        as usize;
    let tid = {
        let mut state = state();
        state.register(CType::Array {
            elem: elem_tid,
            len,
        })
    };
    globals::registry_set(&format!("ctypes.owner.{tid}"), class);
    Ok(tid)
}

/// Parses `_fields_` into `(name, type, bits)` triples.
fn declared_fields(class: i64) -> Result<Vec<(String, TypeId, Option<u32>)>, i64> {
    let name = value::class_display_name(class)
        .unwrap_or("Structure")
        .to_string();
    let declared = class_attr(class, "_fields_")
        .ok_or_else(|| type_error(&format!("{name} does not define _fields_")))?;
    let entries = if let Some(items) = value::as_list(declared) {
        items.to_vec()
    } else if let Some(items) = value::as_tuple(declared) {
        items.to_vec()
    } else {
        return Err(type_error(&format!("{name}._fields_ must be a sequence")));
    };
    let mut fields = Vec::with_capacity(entries.len());
    for entry in entries {
        let parts = as_items(entry)
            .ok_or_else(|| type_error(&format!("{name}._fields_ entries must be tuples")))?;
        let field_name = parts
            .first()
            .copied()
            .and_then(value::as_str)
            .ok_or_else(|| type_error(&format!("{name}._fields_ names must be str")))?
            .to_string();
        let ty = parts
            .get(1)
            .copied()
            .ok_or_else(|| type_error(&format!("{name}._fields_ entries need a type")))?;
        let tid = type_id_of(ty)?;
        let bits = match parts.get(2).copied() {
            None | Some(NONE) => None,
            Some(raw) => {
                let width = int_arg(raw, "bit width")?;
                if width <= 0 || width > 64 {
                    return Err(raise(
                        NOT_IMPLEMENTED_ERROR,
                        "ctypes bitfields wider than 64 bits are not supported",
                    ));
                }
                Some(width as u32)
            }
        };
        fields.push((field_name, tid, bits));
    }
    Ok(fields)
}

fn layout_struct(class: i64, union: bool) -> Result<TypeId, i64> {
    let fields = declared_fields(class)?;
    let pack = class_int_attr(class, "_pack_").unwrap_or(0);
    let pack = if pack > 0 { Some(pack as usize) } else { None };

    let mut layouts: Vec<FieldLayout> = Vec::with_capacity(fields.len());
    let mut offset = 0usize;
    let mut struct_align = 1usize;
    let mut union_size = 0usize;
    // (unit start offset, bits used, unit size)
    let mut bit_unit: Option<(usize, u32, usize)> = None;

    let close_bit_unit = |offset: &mut usize, bit_unit: &mut Option<(usize, u32, usize)>| {
        if let Some((start, _, size)) = bit_unit.take() {
            *offset = (*offset).max(start + size);
        }
    };

    for (field_name, tid, bits) in fields {
        let (field_size, field_align) = {
            let state = state();
            (state.size_of(tid), state.align_of(tid))
        };
        let effective_align = match pack {
            Some(pack) => field_align.min(pack).max(1),
            None => field_align.max(1),
        };
        struct_align = struct_align.max(effective_align);
        if union {
            if bits.is_some() {
                return Err(raise(
                    NOT_IMPLEMENTED_ERROR,
                    "ctypes bitfields inside a Union are not supported",
                ));
            }
            layouts.push(FieldLayout {
                name: field_name,
                ty: tid,
                offset: 0,
                bits: None,
                bit_offset: 0,
            });
            union_size = union_size.max(field_size);
            continue;
        }
        match bits {
            Some(width) => {
                let unit_bits = (field_size * 8) as u32;
                let fits = matches!(bit_unit, Some((_, used, _)) if used + width <= unit_bits);
                if !fits {
                    let mut next = offset;
                    close_bit_unit(&mut next, &mut bit_unit);
                    offset = align_up(next, effective_align);
                    bit_unit = Some((offset, 0, field_size));
                }
                let Some((start, used, _)) = bit_unit else {
                    return Err(type_error("bitfield layout failed"));
                };
                layouts.push(FieldLayout {
                    name: field_name,
                    ty: tid,
                    offset: start,
                    bits: Some(width),
                    bit_offset: used,
                });
                bit_unit = Some((start, used + width, field_size));
            }
            None => {
                let mut next = offset;
                close_bit_unit(&mut next, &mut bit_unit);
                offset = align_up(next, effective_align);
                layouts.push(FieldLayout {
                    name: field_name,
                    ty: tid,
                    offset,
                    bits: None,
                    bit_offset: 0,
                });
                offset += field_size;
            }
        }
    }
    let mut end = offset;
    close_bit_unit(&mut end, &mut bit_unit);
    let size = if union {
        align_up(union_size, struct_align)
    } else {
        align_up(end, struct_align)
    };
    if size > MAX_SLOT {
        return Err(raise(OVERFLOW_ERROR, "ctypes structure is too large"));
    }
    let tid = {
        let mut state = state();
        state.register(CType::Struct {
            union,
            fields: layouts,
            size,
            align: struct_align,
        })
    };
    globals::registry_set(&format!("ctypes.owner.{tid}"), class);
    Ok(tid)
}

fn align_up(value: usize, align: usize) -> usize {
    if align <= 1 {
        return value;
    }
    value.div_ceil(align).saturating_mul(align)
}

// -------------------------------------------------------------------------
// Object helpers
// -------------------------------------------------------------------------

fn set_attr(instance: i64, name: &str, value_arg: i64) {
    value::instance_setattr(instance, name.to_string(), value_arg);
}

fn attr_int(instance: i64, name: &str) -> Option<i64> {
    value::as_integer(value::instance_attr(instance, name)?)
}

/// Points an existing instance at arena storage. `__init__` must mutate the
/// instance the class protocol allocated; its return value is discarded.
fn attach_storage(instance: i64, tid: TypeId, slot: u64, offset: usize) {
    set_attr(instance, "_hydro_type", encode(tid as i64));
    set_attr(instance, "_hydro_slot", encode_len(slot as usize));
    set_attr(instance, "_hydro_offset", encode_len(offset));
}

fn new_instance(class: i64, tid: TypeId, slot: u64, offset: usize) -> i64 {
    let instance = value::instance_new(class);
    attach_storage(instance, tid, slot, offset);
    instance
}

/// Builds a view of `tid` at `slot`/`offset`, filling struct fields.
fn make_view(tid: TypeId, slot: u64, offset: usize) -> i64 {
    make_view_at(tid, slot, offset, 0)
}

fn make_view_at(tid: TypeId, slot: u64, offset: usize, depth: usize) -> i64 {
    let info = {
        let state = state();
        state.type_of(tid)
    };
    let class = class_for_type(tid);
    let instance = new_instance(class, tid, slot, offset);
    if let Some(CType::Struct { union, fields, .. }) = info {
        if depth < 32 {
            populate_fields(instance, slot, offset, union, &fields, depth);
        }
    }
    instance
}

fn populate_fields(
    instance: i64,
    slot: u64,
    base_offset: usize,
    _union: bool,
    fields: &[FieldLayout],
    depth: usize,
) {
    for field in fields {
        let view = make_view_at(
            field.ty,
            slot,
            base_offset.saturating_add(field.offset),
            depth + 1,
        );
        if let Some(width) = field.bits {
            set_attr(view, "_hydro_bits", encode(i64::from(width)));
            set_attr(
                view,
                "_hydro_bit_offset",
                encode(i64::from(field.bit_offset)),
            );
        }
        set_attr(instance, &field.name, view);
    }
}

/// Whether a view carries bitfield metadata.
fn is_bitfield(instance: i64) -> bool {
    value::instance_attr(instance, "_hydro_bits").is_some()
}

fn instance_slot(instance: i64) -> Option<(u64, usize)> {
    let slot = attr_int(instance, "_hydro_slot")?;
    if slot < 0 {
        return None;
    }
    let offset = attr_int(instance, "_hydro_offset").unwrap_or(0);
    Some((slot as u64, usize::try_from(offset).ok()?))
}

// -------------------------------------------------------------------------
// Scalar/value conversion
// -------------------------------------------------------------------------

/// Encodes a Python value into the raw bytes of `simple`.
fn encode_scalar(simple: Simple, raw: i64) -> Result<Vec<u8>, i64> {
    match simple {
        Simple::Bool => {
            if value::as_integer(raw).is_none() && raw != value::TRUE && raw != value::FALSE {
                return Err(type_error("boolean value expected"));
            }
            Ok(vec![u8::from(value::truthy(raw))])
        }
        Simple::CChar => {
            if let Some(bytes) = value::as_bytes(raw) {
                if bytes.len() != 1 {
                    return Err(raise(
                        VALUE_ERROR,
                        "one character bytes, bytearray or integer expected",
                    ));
                }
                return Ok(vec![bytes[0]]);
            }
            let n = int_arg(raw, "char")?;
            Ok(vec![(n as u8)])
        }
        Simple::WChar => {
            let text = str_arg(raw, "wchar")?;
            let mut chars = text.chars();
            let Some(ch) = chars.next() else {
                return Err(raise(VALUE_ERROR, "one character unicode string expected"));
            };
            if chars.next().is_some() {
                return Err(raise(VALUE_ERROR, "one character unicode string expected"));
            }
            Ok((ch as u32).to_le_bytes().to_vec())
        }
        Simple::F32 => {
            let n = float_arg(raw, "float")?;
            Ok((n as f32).to_le_bytes().to_vec())
        }
        Simple::F64 => {
            let n = float_arg(raw, "float")?;
            Ok(n.to_le_bytes().to_vec())
        }
        Simple::LongDouble => {
            let n = float_arg(raw, "float")?;
            let mut bytes = vec![0u8; 16];
            bytes[..8].copy_from_slice(&n.to_le_bytes());
            Ok(bytes)
        }
        Simple::I8 | Simple::I16 | Simple::I32 | Simple::I64 => {
            let n = int_arg(raw, "integer")?;
            let size = simple.size();
            let mask = if size == 8 {
                u64::MAX
            } else {
                (1u64 << (size * 8)) - 1
            };
            Ok((n as u64 & mask).to_le_bytes()[..size].to_vec())
        }
        Simple::U8 | Simple::U16 | Simple::U32 | Simple::U64 => {
            let n = int_arg(raw, "integer")?;
            let size = simple.size();
            let mask = if size == 8 {
                u64::MAX
            } else {
                (1u64 << (size * 8)) - 1
            };
            Ok((n as u64 & mask).to_le_bytes()[..size].to_vec())
        }
        Simple::VoidP | Simple::CharP | Simple::WCharP => encode_pointer_word(simple, raw),
    }
}

fn float_arg(v: i64, what: &str) -> Result<f64, i64> {
    if let Some(n) = value::as_float(v) {
        return Ok(n);
    }
    if let Some(n) = value::as_integer(v) {
        return Ok(n as f64);
    }
    Err(raise(
        TYPE_ERROR,
        &format!("{what} must be a number, not {}", value::type_name(v)),
    ))
}

/// Encodes a pointer-sized value (None/int/ctypes pointer/bytes buffer).
fn encode_pointer_word(simple: Simple, raw: i64) -> Result<Vec<u8>, i64> {
    if raw == NONE {
        return Ok(vec![0u8; 8]);
    }
    let addr = match simple {
        Simple::CharP => {
            if let Some(bytes) = value::as_bytes(raw) {
                let mut data = bytes.to_vec();
                data.push(0);
                allocate_bytes(&data)
            } else if let Some(n) = value::as_integer(raw) {
                n as usize
            } else if value::as_instance(raw).is_some() {
                pointer_target_or_storage(raw)?
            } else {
                return Err(type_error("cannot be converted to c_char_p"));
            }
        }
        Simple::WCharP => {
            if let Some(text) = value::as_str(raw) {
                allocate_wide(text)
            } else if let Some(n) = value::as_integer(raw) {
                n as usize
            } else if value::as_instance(raw).is_some() {
                pointer_target_or_storage(raw)?
            } else {
                return Err(type_error("cannot be converted to c_wchar_p"));
            }
        }
        _ => {
            if let Some(n) = value::as_integer(raw) {
                n as usize
            } else if value::as_instance(raw).is_some() {
                pointer_target_or_storage(raw)?
            } else if let Some(bytes) = value::as_bytes(raw) {
                let mut data = bytes.to_vec();
                data.push(0);
                allocate_bytes(&data)
            } else {
                return Err(type_error("cannot be converted to a pointer"));
            }
        }
    };
    Ok((addr as u64).to_le_bytes().to_vec())
}

/// Allocates a NUL-terminated byte copy and returns its address.
fn allocate_bytes(data: &[u8]) -> usize {
    let mut state = state();
    let id = state.alloc(data.len());
    // The slot is zero-filled with at least one byte; write the payload.
    let _ = state.write_bytes(id, 0, data);
    state.base(id).unwrap_or(0)
}

/// Allocates a NUL-terminated wide string and returns its address.
fn allocate_wide(text: &str) -> usize {
    let mut units = Vec::with_capacity(text.chars().count() + 1);
    for ch in text.chars() {
        units.push(ch as u32);
    }
    units.push(0);
    let mut bytes = Vec::with_capacity(units.len() * 4);
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    allocate_bytes(&bytes)
}

/// Address a pointer-valued instance designates, falling back to its own
/// storage when it has no known target.
fn pointer_target_or_storage(instance: i64) -> Result<usize, i64> {
    // A scalar pointer (c_void_p/c_char_p/c_wchar_p) stores the address it
    // designates as its own value.
    if let Some(word) = stored_pointer_word(instance) {
        return Ok(word as usize);
    }
    if let Some(addr) = attr_int(instance, "_hydro_target_addr").filter(|addr| *addr != 0) {
        return Ok(addr as usize);
    }
    instance_address(instance)
}

/// The raw 8-byte word of a scalar pointer instance, if that is what it is.
fn stored_pointer_word(instance: i64) -> Option<u64> {
    let tid = type_id_of(instance).ok()?;
    let is_pointer = {
        let state = state();
        matches!(
            state.type_of(tid),
            Some(CType::Simple(simple)) if simple.is_pointer()
        ) || matches!(state.type_of(tid), Some(CType::Pointer(_)))
    };
    if !is_pointer {
        return None;
    }
    let (slot, offset) = instance_slot(instance)?;
    let bytes = {
        let state = state();
        state.read_bytes(slot, offset, 8).map(<[u8]>::to_vec)?
    };
    Some(u64::from_le_bytes(bytes[..8].try_into().ok()?))
}

/// Address of an instance's own storage.
fn instance_address(instance: i64) -> Result<usize, i64> {
    let (slot, offset) =
        instance_slot(instance).ok_or_else(|| type_error("ctypes object has no storage"))?;
    let state = state();
    state
        .base(slot)
        .and_then(|base| base.checked_add(offset))
        .ok_or_else(|| type_error("ctypes object has no storage"))
}

/// Decodes raw bytes into the Python value of `simple`.
fn decode_scalar(simple: Simple, raw: &[u8]) -> Result<i64, i64> {
    let need = simple.size();
    if raw.len() < need {
        return Err(type_error("truncated ctypes storage"));
    }
    match simple {
        Simple::Bool => Ok(value::bool_value(raw[0] != 0)),
        Simple::CChar => Ok(value::bytes_new(vec![raw[0]])),
        Simple::WChar => {
            let code = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            match char::from_u32(code) {
                Some(ch) => Ok(value::str_alloc(ch.to_string())),
                None => Err(raise(VALUE_ERROR, "invalid c_wchar value")),
            }
        }
        Simple::F32 => Ok(value::float_new(
            f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f64,
        )),
        Simple::F64 => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&raw[..8]);
            Ok(value::float_new(f64::from_le_bytes(bytes)))
        }
        Simple::LongDouble => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&raw[..8]);
            Ok(value::float_new(f64::from_le_bytes(bytes)))
        }
        Simple::I8 => Ok(encode(i64::from(raw[0] as i8))),
        Simple::U8 => Ok(encode(i64::from(raw[0]))),
        Simple::I16 | Simple::I32 | Simple::I64 => {
            let size = simple.size();
            let mut bytes = [0u8; 8];
            bytes[..size].copy_from_slice(&raw[..size]);
            let unsigned = u64::from_le_bytes(bytes);
            let shift = 64 - size * 8;
            let signed = ((unsigned << shift) as i64) >> shift;
            Ok(encode(signed))
        }
        Simple::U16 | Simple::U32 | Simple::U64 => {
            let size = simple.size();
            let mut bytes = [0u8; 8];
            bytes[..size].copy_from_slice(&raw[..size]);
            let value = u64::from_le_bytes(bytes);
            Ok(unsigned_result(value))
        }
        Simple::VoidP => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&raw[..8]);
            let addr = u64::from_le_bytes(bytes) as usize;
            Ok(if addr == 0 { NONE } else { encode(addr as i64) })
        }
        Simple::CharP => {
            let addr = u64::from_le_bytes(raw[..8].try_into().unwrap_or([0; 8])) as usize;
            if addr == 0 {
                return Ok(NONE);
            }
            let bytes = read_c_string(addr)?;
            Ok(value::bytes_new(bytes))
        }
        Simple::WCharP => {
            let addr = u64::from_le_bytes(raw[..8].try_into().unwrap_or([0; 8])) as usize;
            if addr == 0 {
                return Ok(NONE);
            }
            let text = read_wide_string(addr)?;
            Ok(value::str_alloc(text))
        }
    }
}

/// Reads a NUL-terminated byte string from an arena address.
fn read_c_string(addr: usize) -> Result<Vec<u8>, i64> {
    let state = state();
    if addr == 0 {
        return Err(raise(VALUE_ERROR, "NULL pointer access"));
    }
    let Some((slot, offset)) = state.locate(addr, 0) else {
        return Err(raise(
            VALUE_ERROR,
            "ctypes pointer does not refer to a Hydro-owned buffer",
        ));
    };
    let bytes = state
        .slot(slot)
        .map(|slot| &slot.bytes[offset..])
        .unwrap_or(&[]);
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    Ok(bytes[..end].to_vec())
}

/// Reads a NUL-terminated wide string from an arena address.
fn read_wide_string(addr: usize) -> Result<String, i64> {
    let state = state();
    if addr == 0 {
        return Err(raise(VALUE_ERROR, "NULL pointer access"));
    }
    let Some((slot, offset)) = state.locate(addr, 0) else {
        return Err(raise(
            VALUE_ERROR,
            "ctypes pointer does not refer to a Hydro-owned buffer",
        ));
    };
    let Some(slot) = state.slot(slot) else {
        return Err(raise(VALUE_ERROR, "invalid ctypes buffer"));
    };
    let bytes = &slot.bytes[offset..];
    let mut text = String::new();
    for chunk in bytes.as_chunks::<4>().0 {
        let code = u32::from_le_bytes(*chunk);
        if code == 0 {
            return Ok(text);
        }
        match char::from_u32(code) {
            Some(ch) => text.push(ch),
            None => return Err(raise(VALUE_ERROR, "invalid c_wchar_p contents")),
        }
    }
    Ok(text)
}

/// Writes a Python value into `tod`'s storage at `slot`/`offset`.
fn write_value(slot: u64, offset: usize, tid: TypeId, raw: i64) -> Result<(), i64> {
    let info = {
        let state = state();
        state.type_of(tid)
    };
    let Some(info) = info else {
        return Err(type_error("unknown ctypes type"));
    };
    match info {
        CType::Simple(simple) => {
            let bytes = encode_scalar(simple, raw)?;
            let mut state = state();
            if state.write_bytes(slot, offset, &bytes) {
                Ok(())
            } else {
                Err(type_error("ctypes storage is too small"))
            }
        }
        CType::Pointer(_) | CType::Func { .. } => {
            let bytes = encode_pointer_word(Simple::VoidP, raw)?;
            let mut state = state();
            if state.write_bytes(slot, offset, &bytes) {
                Ok(())
            } else {
                Err(type_error("ctypes storage is too small"))
            }
        }
        CType::Array { elem, len } => {
            let size = {
                let state = state();
                state.size_of(elem)
            };
            let items = if let Some(bytes) = value::as_bytes(raw) {
                bytes.iter().map(|byte| encode(i64::from(*byte))).collect()
            } else if let Some(text) = value::as_str(raw) {
                text.chars()
                    .map(|ch| value::str_alloc(ch.to_string()))
                    .collect()
            } else {
                as_items(raw)
                    .ok_or_else(|| type_error("ctypes array initializer must be a sequence"))?
            };
            for (index, item) in items.into_iter().take(len).enumerate() {
                write_value(slot, offset + index * size, elem, item)?;
            }
            Ok(())
        }
        CType::Void => Err(type_error("cannot assign a value to void")),
        CType::Struct { fields, size, .. } => {
            if value::as_instance(raw).is_some() {
                if let Some((other_slot, other_offset)) = instance_slot(raw) {
                    let bytes = {
                        let state = state();
                        state
                            .read_bytes(other_slot, other_offset, size)
                            .map(<[u8]>::to_vec)
                    };
                    if let Some(bytes) = bytes {
                        let mut state = state();
                        if state.write_bytes(slot, offset, &bytes) {
                            return Ok(());
                        }
                    }
                }
                return Err(type_error("cannot copy this ctypes object"));
            }
            let items = as_items(raw)
                .ok_or_else(|| type_error("ctypes structure initializer must be a sequence"))?;
            if items.len() > fields.len() {
                return Err(type_error("too many initializers for ctypes structure"));
            }
            for (field, item) in fields.iter().zip(items) {
                write_value(slot, offset + field.offset, field.ty, item)?;
            }
            Ok(())
        }
    }
}

/// Reads the Python value of `tid` from `slot`/`offset`.
fn read_value(slot: u64, offset: usize, tid: TypeId) -> Result<i64, i64> {
    let info = {
        let state = state();
        state.type_of(tid)
    };
    let Some(info) = info else {
        return Err(type_error("unknown ctypes type"));
    };
    match info {
        CType::Simple(simple) => {
            let bytes = {
                let state = state();
                state
                    .read_bytes(slot, offset, simple.size())
                    .map(<[u8]>::to_vec)
            };
            match bytes {
                Some(bytes) => decode_scalar(simple, &bytes),
                None => Err(type_error("ctypes storage is too small")),
            }
        }
        CType::Pointer(_) => {
            let bytes = {
                let state = state();
                state.read_bytes(slot, offset, 8).map(<[u8]>::to_vec)
            };
            match bytes {
                Some(bytes) => decode_scalar(Simple::VoidP, &bytes),
                None => Err(type_error("ctypes storage is too small")),
            }
        }
        CType::Func { .. } => {
            let bytes = {
                let state = state();
                state.read_bytes(slot, offset, 8).map(<[u8]>::to_vec)
            };
            match bytes {
                Some(bytes) => {
                    let addr = u64::from_le_bytes(bytes[..8].try_into().unwrap_or([0; 8]));
                    Ok(encode(addr as i64))
                }
                None => Err(type_error("ctypes storage is too small")),
            }
        }
        CType::Void => Ok(NONE),
        CType::Array { .. } | CType::Struct { .. } => Err(raise(
            ATTRIBUTE_ERROR,
            "this ctypes object has no .value attribute",
        )),
    }
}

// -------------------------------------------------------------------------
// `_cdata_*` methods
// -------------------------------------------------------------------------

fn cdata_init(args: &[i64]) -> i64 {
    let instance = args[0];
    let Some(class) = value::instance_class_value(instance) else {
        return type_error("ctypes constructor received a non-instance");
    };
    let Some(kind) = class_kind(class) else {
        return type_error("ctype class has no layout");
    };
    let tid = match type_id_of_class(class) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let info = {
        let state = state();
        state.type_of(tid)
    };
    let Some(info) = info else {
        return type_error("unknown ctypes type");
    };
    let rest = &args[1..];
    match (kind, info) {
        ("simple", CType::Simple(simple)) => {
            let slot = state().alloc(simple.size());
            attach_storage(instance, tid, slot, 0);
            if rest.len() > 1 {
                return type_error(&format!("{}() takes at most 1 argument", simple.name()));
            }
            if let Some(&raw) = rest.first() {
                if let Err(error) = write_value(slot, 0, tid, raw) {
                    return error;
                }
            }
            instance
        }
        ("pointer", CType::Pointer(pointee)) => {
            let slot = state().alloc(8);
            attach_storage(instance, tid, slot, 0);
            set_attr(instance, "_hydro_target_slot", encode(-1));
            set_attr(instance, "_hydro_target_offset", encode(0));
            set_attr(instance, "_hydro_target_addr", encode(0));
            if rest.len() > 1 {
                return type_error("pointer type takes at most 1 argument");
            }
            if let Some(&raw) = rest.first() {
                if value::as_instance(raw).is_some() {
                    if let Some((target_slot, target_offset)) = instance_slot(raw) {
                        set_attr(
                            instance,
                            "_hydro_target_slot",
                            encode_len(target_slot as usize),
                        );
                        set_attr(instance, "_hydro_target_offset", encode_len(target_offset));
                        let addr = instance_address(raw).unwrap_or(0);
                        set_attr(instance, "_hydro_target_addr", encode(addr as i64));
                    }
                } else if let Some(addr) = value::as_integer(raw) {
                    set_attr(instance, "_hydro_target_addr", encode(addr));
                } else if raw != NONE {
                    return type_error("pointer initializer must be an int or ctypes object");
                }
                let _ = pointee;
                if let Err(error) = write_value(slot, 0, tid, raw) {
                    return error;
                }
            }
            instance
        }
        ("array", CType::Array { elem, len }) => {
            let elem_size = {
                let state = state();
                state.size_of(elem)
            };
            let slot = state().alloc(elem_size.saturating_mul(len));
            attach_storage(instance, tid, slot, 0);
            if rest.len() > 1 {
                return type_error("array type takes at most 1 argument");
            }
            if let Some(&raw) = rest.first() {
                if let Err(error) = write_value(slot, 0, tid, raw) {
                    return error;
                }
            }
            instance
        }
        ("struct", CType::Struct { fields, size, .. })
        | ("union", CType::Struct { fields, size, .. }) => {
            let slot = state().alloc(size);
            if rest.len() > fields.len() {
                return type_error("too many initializers for ctypes structure");
            }
            for (field, raw) in fields.iter().zip(rest.iter().copied()) {
                if field.bits.is_some() {
                    if raw != NONE {
                        return raise(
                            NOT_IMPLEMENTED_ERROR,
                            "ctypes bitfield values are not supported",
                        );
                    }
                    continue;
                }
                if let Err(error) = write_value(slot, field.offset, field.ty, raw) {
                    return error;
                }
            }
            attach_storage(instance, tid, slot, 0);
            populate_fields(instance, slot, 0, kind == "union", &fields, 0);
            instance
        }
        ("func", CType::Func { restype, argtypes }) => {
            let slot = state().alloc(8);
            attach_storage(instance, tid, slot, 0);
            set_attr(instance, "_hydro_target_addr", encode(0));
            if rest.len() > 1 {
                return type_error("function pointer type takes at most 1 argument");
            }
            if let Some(&raw) = rest.first() {
                match install_function(instance, raw, restype, &argtypes) {
                    Ok(()) => {}
                    Err(error) => return error,
                }
            }
            instance
        }
        _ => type_error("ctype class has an unsupported layout"),
    }
}

fn cdata_get_value(args: &[i64]) -> i64 {
    let instance = args[0];
    if is_bitfield(instance) {
        return raise(
            NOT_IMPLEMENTED_ERROR,
            "ctypes bitfield values are not supported",
        );
    }
    let Some((slot, offset)) = instance_slot(instance) else {
        return type_error("ctypes object has no storage");
    };
    let tid = match type_id_of(instance) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let info = {
        let state = state();
        state.type_of(tid)
    };
    match info {
        Some(CType::Array { elem, len }) => {
            let simple = {
                let state = state();
                match state.type_of(elem) {
                    Some(CType::Simple(simple)) => simple,
                    _ => {
                        return raise(
                            ATTRIBUTE_ERROR,
                            "the .value attribute is only available for character arrays",
                        )
                    }
                }
            };
            match simple {
                Simple::CChar => {
                    let bytes = {
                        let state = state();
                        state.read_bytes(slot, offset, len).map(<[u8]>::to_vec)
                    };
                    match bytes {
                        Some(bytes) => {
                            let end = bytes
                                .iter()
                                .position(|byte| *byte == 0)
                                .unwrap_or(bytes.len());
                            value::bytes_new(bytes[..end].to_vec())
                        }
                        None => type_error("ctypes storage is too small"),
                    }
                }
                Simple::WChar => {
                    let bytes = {
                        let state = state();
                        state
                            .read_bytes(slot, offset, len.saturating_mul(4))
                            .map(<[u8]>::to_vec)
                    };
                    match bytes {
                        Some(bytes) => {
                            let mut text = String::new();
                            for chunk in bytes.as_chunks::<4>().0 {
                                let code = u32::from_le_bytes(*chunk);
                                if code == 0 {
                                    break;
                                }
                                if let Some(ch) = char::from_u32(code) {
                                    text.push(ch);
                                }
                            }
                            value::str_alloc(text)
                        }
                        None => type_error("ctypes storage is too small"),
                    }
                }
                _ => raise(
                    ATTRIBUTE_ERROR,
                    "the .value attribute is only available for character arrays",
                ),
            }
        }
        Some(_) => match read_value(slot, offset, tid) {
            Ok(value_arg) => value_arg,
            Err(error) => error,
        },
        None => type_error("unknown ctypes type"),
    }
}

fn cdata_set_value(args: &[i64]) -> i64 {
    let instance = args[0];
    if is_bitfield(instance) {
        return raise(
            NOT_IMPLEMENTED_ERROR,
            "ctypes bitfield values are not supported",
        );
    }
    let Some((slot, offset)) = instance_slot(instance) else {
        return type_error("ctypes object has no storage");
    };
    let tid = match type_id_of(instance) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let info = {
        let state = state();
        state.type_of(tid)
    };
    match info {
        Some(CType::Array { elem, len }) => {
            let simple = {
                let state = state();
                match state.type_of(elem) {
                    Some(CType::Simple(simple)) => simple,
                    _ => {
                        return raise(
                            ATTRIBUTE_ERROR,
                            "the .value attribute is only available for character arrays",
                        )
                    }
                }
            };
            match simple {
                Simple::CChar => {
                    let bytes = match char_buffer_bytes(args[1]) {
                        Ok(bytes) => bytes,
                        Err(error) => return error,
                    };
                    if bytes.len() > len {
                        return raise(VALUE_ERROR, "string too long");
                    }
                    let mut state = state();
                    let _ = state.write_bytes(slot, offset, &bytes);
                    NONE
                }
                Simple::WChar => {
                    let text = match str_arg(args[1], "c_wchar array value") {
                        Ok(text) => text,
                        Err(error) => return error,
                    };
                    let mut units = Vec::new();
                    for ch in text.chars() {
                        units.push(ch as u32);
                    }
                    if units.len() > len {
                        return raise(VALUE_ERROR, "string too long");
                    }
                    let mut bytes = Vec::new();
                    for unit in units {
                        bytes.extend_from_slice(&unit.to_le_bytes());
                    }
                    let mut state = state();
                    let _ = state.write_bytes(slot, offset, &bytes);
                    NONE
                }
                _ => raise(
                    ATTRIBUTE_ERROR,
                    "the .value attribute is only available for character arrays",
                ),
            }
        }
        Some(_) => match write_value(slot, offset, tid, args[1]) {
            Ok(()) => NONE,
            Err(error) => error,
        },
        None => type_error("unknown ctypes type"),
    }
}

fn char_buffer_bytes(v: i64) -> Result<Vec<u8>, i64> {
    if let Some(bytes) = value::as_bytes(v) {
        return Ok(bytes.to_vec());
    }
    if let Some(text) = value::as_str(v) {
        return Ok(text.as_bytes().to_vec());
    }
    Err(type_error("bytes-like value expected"))
}

fn cdata_get_contents(args: &[i64]) -> i64 {
    let instance = args[0];
    let tid = match type_id_of(instance) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let pointee = {
        let state = state();
        match state.type_of(tid) {
            Some(CType::Pointer(pointee)) => pointee,
            _ => {
                return raise(
                    ATTRIBUTE_ERROR,
                    "contents is only available for pointer objects",
                )
            }
        }
    };
    let target_slot = attr_int(instance, "_hydro_target_slot");
    let target_offset = attr_int(instance, "_hydro_target_offset").unwrap_or(0);
    let slot = if let Some(slot) = target_slot.filter(|slot| *slot >= 0) {
        slot as u64
    } else {
        let addr = attr_int(instance, "_hydro_target_addr").unwrap_or(0);
        match resolve_address(addr, 0) {
            Ok(slot) => slot,
            Err(error) => return error,
        }
    };
    let offset = usize::try_from(target_offset).unwrap_or(0);
    make_view(pointee, slot, offset)
}

fn cdata_get_raw(args: &[i64]) -> i64 {
    let instance = args[0];
    let Some((slot, offset)) = instance_slot(instance) else {
        return type_error("ctypes object has no storage");
    };
    let tid = match type_id_of(instance) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let size = {
        let state = state();
        state.size_of(tid)
    };
    let bytes = {
        let state = state();
        state.read_bytes(slot, offset, size).map(<[u8]>::to_vec)
    };
    match bytes {
        Some(bytes) => value::bytes_new(bytes),
        None => type_error("ctypes storage is too small"),
    }
}

fn cdata_set_raw(args: &[i64]) -> i64 {
    let instance = args[0];
    let Some((slot, offset)) = instance_slot(instance) else {
        return type_error("ctypes object has no storage");
    };
    let bytes = match char_buffer_bytes(args[1]) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let tid = match type_id_of(instance) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let size = {
        let state = state();
        state.size_of(tid)
    };
    if bytes.len() > size {
        return raise(VALUE_ERROR, "raw data is too long");
    }
    let mut state = state();
    if state.write_bytes(slot, offset, &bytes) {
        NONE
    } else {
        type_error("ctypes storage is too small")
    }
}

fn cdata_len(args: &[i64]) -> i64 {
    let tid = match type_id_of(args[0]) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let len = {
        let state = state();
        match state.type_of(tid) {
            Some(CType::Array { len, .. }) => Some(len),
            _ => None,
        }
    };
    match len {
        Some(len) => encode_len(len),
        None => type_error("len() of unsized ctypes object"),
    }
}

fn array_index(instance: i64, index: i64) -> Result<(u64, usize, usize, TypeId), i64> {
    let index = int_arg(index, "ctypes array index")?;
    let Some((slot, offset)) = instance_slot(instance) else {
        return Err(type_error("ctypes object has no storage"));
    };
    let tid = type_id_of(instance)?;
    let (elem, len) = {
        let state = state();
        match state.type_of(tid) {
            Some(CType::Array { elem, len }) => (elem, len),
            _ => return Err(type_error("ctypes object is not an array")),
        }
    };
    let index = if index < 0 {
        let shifted = index + len as i64;
        if shifted < 0 {
            return Err(raise(INDEX_ERROR, "ctypes array index out of range"));
        }
        shifted
    } else {
        index
    };
    if index as usize >= len {
        return Err(raise(INDEX_ERROR, "ctypes array index out of range"));
    }
    let elem_size = {
        let state = state();
        state.size_of(elem)
    };
    Ok((slot, offset + index as usize * elem_size, elem_size, elem))
}

fn cdata_getitem(args: &[i64]) -> i64 {
    let instance = args[0];
    let (slot, offset, _elem_size, elem) = match array_index(instance, args[1]) {
        Ok(parts) => parts,
        Err(error) => return error,
    };
    let simple = {
        let state = state();
        match state.type_of(elem) {
            Some(CType::Simple(simple)) => Some(simple),
            _ => None,
        }
    };
    match simple {
        Some(Simple::CChar) => {
            let bytes = {
                let state = state();
                state.read_bytes(slot, offset, 1).map(<[u8]>::to_vec)
            };
            match bytes {
                Some(bytes) => value::bytes_new(bytes),
                None => type_error("ctypes storage is too small"),
            }
        }
        Some(Simple::WChar) => match read_value(slot, offset, elem) {
            Ok(value_arg) => value_arg,
            Err(error) => error,
        },
        _ => make_view(elem, slot, offset),
    }
}

fn cdata_setitem(args: &[i64]) -> i64 {
    let instance = args[0];
    let (slot, offset, _elem_size, elem) = match array_index(instance, args[1]) {
        Ok(parts) => parts,
        Err(error) => return error,
    };
    match write_value(slot, offset, elem, args[2]) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

fn cdata_str(args: &[i64]) -> i64 {
    let instance = args[0];
    let class = value::instance_class_value(instance);
    let name = class
        .and_then(value::class_display_name)
        .unwrap_or("cdata")
        .to_string();
    let tid = match type_id_of(instance) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let info = {
        let state = state();
        state.type_of(tid)
    };
    let rendered = match info {
        Some(CType::Simple(_))
        | Some(CType::Pointer(_))
        | Some(CType::Func { .. })
        | Some(CType::Void) => match cdata_get_value(args) {
            _ if exc::pending() => {
                exc::clear();
                name
            }
            raw => format!("{name}({})", value::repr(raw)),
        },
        Some(CType::Array { .. }) | Some(CType::Struct { .. }) => {
            format!("{name}(<{} bytes>)", cdata_raw_len(instance))
        }
        None => name,
    };
    value::str_alloc(rendered)
}

fn cdata_raw_len(instance: i64) -> usize {
    let Some(tid) = type_id_of(instance).ok() else {
        return 0;
    };
    let state = state();
    state.size_of(tid)
}

fn cdata_repr(args: &[i64]) -> i64 {
    cdata_str(args)
}

fn cdata_call(args: &[i64]) -> i64 {
    let instance = args[0];
    let tid = match type_id_of(instance) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let (restype, argtypes) = {
        let state = state();
        match state.type_of(tid) {
            Some(CType::Func { restype, argtypes }) => (restype, argtypes),
            _ => return type_error("ctypes object is not callable"),
        }
    };
    let restype = instance_restype(instance).unwrap_or(restype);
    let argtypes = instance_argtypes(instance).unwrap_or(argtypes);
    invoke_pointer(instance, restype, &argtypes, &args[1..])
}

fn cdata_add(args: &[i64]) -> i64 {
    pointer_offset(args[0], args[1])
}

fn cdata_radd(args: &[i64]) -> i64 {
    pointer_offset(args[0], args[1])
}

fn pointer_offset(instance: i64, other: i64) -> i64 {
    let tid = match type_id_of(instance) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let pointee = {
        let state = state();
        match state.type_of(tid) {
            Some(CType::Pointer(pointee)) => pointee,
            _ => return type_error("pointer arithmetic requires a ctypes pointer"),
        }
    };
    let offset = match int_arg(other, "pointer offset") {
        Ok(offset) => offset,
        Err(error) => return error,
    };
    let size = {
        let state = state();
        state.size_of(pointee)
    };
    let addr = attr_int(instance, "_hydro_target_addr")
        .unwrap_or_else(|| instance_address(instance).unwrap_or(0) as i64);
    let addr = addr.saturating_add(offset.saturating_mul(size as i64));
    let slot = state().alloc(8);
    let class = class_for_type(tid);
    let result = new_instance(class, tid, slot, 0);
    set_attr(result, "_hydro_target_slot", encode(-1));
    set_attr(result, "_hydro_target_offset", encode(0));
    set_attr(result, "_hydro_target_addr", encode(addr));
    if let Err(error) = write_value(slot, 0, tid, encode(addr)) {
        return error;
    }
    result
}

/// Resolves an absolute address into `(slot, offset)`, refusing foreign memory.
fn resolve_address(addr: i64, len: usize) -> Result<u64, i64> {
    if addr == 0 {
        return Err(raise(VALUE_ERROR, "NULL pointer access"));
    }
    let state = state();
    state
        .locate(addr as usize, len)
        .map(|(slot, _)| slot)
        .ok_or_else(|| {
            raise(
                VALUE_ERROR,
                "ctypes pointer does not refer to a Hydro-owned buffer",
            )
        })
}

// -------------------------------------------------------------------------
// Module functions
// -------------------------------------------------------------------------

fn sizeof(args: &[i64]) -> i64 {
    match type_id_of(args[0]) {
        Ok(tid) => {
            let size = state().size_of(tid);
            encode_len(size)
        }
        Err(error) => error,
    }
}

fn alignment(args: &[i64]) -> i64 {
    match type_id_of(args[0]) {
        Ok(tid) => {
            let align = state().align_of(tid);
            encode_len(align)
        }
        Err(error) => error,
    }
}

fn offsetof(args: &[i64]) -> i64 {
    let class = args[0];
    if value::as_class(class).is_none() {
        return type_error("offsetof() requires a ctypes structure type");
    }
    let tid = match type_id_of_class(class) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let field_name = match str_arg(args[1], "field name") {
        Ok(name) => name,
        Err(error) => return error,
    };
    let field = {
        let state = state();
        match state.type_of(tid) {
            Some(CType::Struct { fields, .. }) => fields
                .iter()
                .find(|field| field.name == field_name)
                .cloned(),
            _ => None,
        }
    };
    match field {
        Some(field) if field.bits.is_some() => raise(
            NOT_IMPLEMENTED_ERROR,
            "offsetof() of a bitfield is not supported",
        ),
        Some(field) => encode_len(field.offset),
        None => type_error(&format!("no field '{field_name}'")),
    }
}

fn make_pointer_type(args: &[i64]) -> i64 {
    let pointee = match type_id_of(args[0]) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let tid = {
        let mut state = state();
        state.pointer_to(pointee)
    };
    class_for_type(tid)
}

fn pointer_new(args: &[i64]) -> i64 {
    let instance = args[0];
    let Some(class) = value::instance_class_value(instance) else {
        return type_error("pointer() requires a ctypes object");
    };
    let tid = match type_id_of(instance) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    let pointer_tid = {
        let mut state = state();
        state.pointer_to(tid)
    };
    let pointer_class = class_for_type(pointer_tid);
    let slot = state().alloc(8);
    let result = new_instance(pointer_class, pointer_tid, slot, 0);
    let (target_slot, target_offset) = instance_slot(instance).unwrap_or((0, 0));
    let addr = instance_address(instance).unwrap_or(0);
    set_attr(
        result,
        "_hydro_target_slot",
        encode_len(target_slot as usize),
    );
    set_attr(result, "_hydro_target_offset", encode_len(target_offset));
    set_attr(result, "_hydro_target_addr", encode(addr as i64));
    set_attr(result, "_hydro_pointee_class", class);
    if let Err(error) = write_value(slot, 0, pointer_tid, encode(addr as i64)) {
        return error;
    }
    result
}

fn cast(args: &[i64]) -> i64 {
    let target_class = args[1];
    if value::as_class(target_class).is_none() {
        return type_error("cast() target must be a ctypes type");
    }
    let target_tid = match type_id_of_class(target_class) {
        Ok(tid) => tid,
        Err(error) => return error,
    };
    // A pointer source casts its target address; any other cdata object casts
    // the address of its own storage.
    let pointer_word = if value::as_instance(args[0]).is_some() {
        stored_pointer_word(args[0])
    } else {
        None
    };
    let (source_slot, source_offset) = if pointer_word.is_some() {
        (None, 0)
    } else if value::as_instance(args[0]).is_some() {
        match instance_slot(args[0]) {
            Some(parts) => (Some(parts.0), parts.1),
            None => (None, 0),
        }
    } else {
        (None, 0)
    };
    let addr = if let Some(word) = pointer_word {
        word as i64
    } else if value::as_instance(args[0]).is_some() {
        if let Some(target) = attr_int(args[0], "_hydro_target_addr").filter(|a| *a != 0) {
            target
        } else {
            match instance_address(args[0]) {
                Ok(addr) => addr as i64,
                Err(error) => return error,
            }
        }
    } else {
        match int_arg(args[0], "cast() address") {
            Ok(addr) => addr,
            Err(error) => return error,
        }
    };
    let slot = state().alloc(8);
    let result = new_instance(target_class, target_tid, slot, 0);
    if let Some(slot_id) = source_slot {
        set_attr(result, "_hydro_target_slot", encode_len(slot_id as usize));
        set_attr(result, "_hydro_target_offset", encode_len(source_offset));
    } else {
        set_attr(result, "_hydro_target_slot", encode(-1));
        set_attr(result, "_hydro_target_offset", encode(0));
    }
    set_attr(result, "_hydro_target_addr", encode(addr));
    if let Err(error) = write_value(slot, 0, target_tid, encode(addr)) {
        return error;
    }
    result
}

fn addressof(args: &[i64]) -> i64 {
    if value::as_instance(args[0]).is_none() {
        return type_error("addressof() requires a ctypes object");
    }
    match instance_address(args[0]) {
        Ok(addr) => encode(addr as i64),
        Err(error) => error,
    }
}

fn string_at(args: &[i64]) -> i64 {
    let addr = match int_arg(args[0], "string_at() address") {
        Ok(addr) => addr,
        Err(error) => return error,
    };
    let size = match args.get(1).copied() {
        None | Some(NONE) => None,
        Some(raw) => match int_arg(raw, "string_at() size") {
            Ok(value) if value >= 0 => Some(value as usize),
            Ok(_) => return raise(VALUE_ERROR, "size must not be negative"),
            Err(error) => return error,
        },
    };
    if size.is_none() {
        return match read_c_string(addr as usize) {
            Ok(bytes) => value::bytes_new(bytes),
            Err(error) => error,
        };
    }
    let size = size.unwrap_or(0);
    if size > MAX_TRANSFER {
        return raise(VALUE_ERROR, "size is too large");
    }
    let state = state();
    let Some((slot, offset)) = state.locate(addr as usize, size) else {
        return raise(
            VALUE_ERROR,
            "ctypes pointer does not refer to a Hydro-owned buffer",
        );
    };
    let bytes = state
        .read_bytes(slot, offset, size)
        .map(<[u8]>::to_vec)
        .unwrap_or_default();
    value::bytes_new(bytes)
}

fn wstring_at(args: &[i64]) -> i64 {
    let addr = match int_arg(args[0], "wstring_at() address") {
        Ok(addr) => addr,
        Err(error) => return error,
    };
    match read_wide_string(addr as usize) {
        Ok(text) => value::str_alloc(text),
        Err(error) => error,
    }
}

/// Resolves a source/destination operand into `(slot, offset, available)`.
fn memory_operand(v: i64) -> Result<(u64, usize, usize), i64> {
    if value::as_instance(v).is_some() {
        let Some((slot, offset)) = instance_slot(v) else {
            return Err(type_error("ctypes object has no storage"));
        };
        let tid = type_id_of(v)?;
        let (size, slot_len) = {
            let state = state();
            let size = state.size_of(tid);
            let slot_len = state.slot(slot).map(|slot| slot.bytes.len()).unwrap_or(0);
            (size, slot_len)
        };
        let available = size.min(slot_len.saturating_sub(offset));
        return Ok((slot, offset, available));
    }
    let addr = int_arg(v, "memory address")?;
    let state = state();
    match state.locate(addr as usize, 0) {
        Some((slot, offset)) => {
            let available = state
                .slot(slot)
                .map(|slot| slot.bytes.len() - offset)
                .unwrap_or(0);
            Ok((slot, offset, available))
        }
        None => Err(raise(
            VALUE_ERROR,
            "ctypes pointer does not refer to a Hydro-owned buffer",
        )),
    }
}

fn memmove(args: &[i64]) -> i64 {
    let count = match int_arg(args[2], "memmove() count") {
        Ok(count) if count >= 0 => count as usize,
        Ok(_) => return raise(VALUE_ERROR, "count must not be negative"),
        Err(error) => return error,
    };
    if count > MAX_TRANSFER {
        return raise(VALUE_ERROR, "count is too large");
    }
    let (dest_slot, dest_offset, dest_available) = match memory_operand(args[0]) {
        Ok(parts) => parts,
        Err(error) => return error,
    };
    if count > dest_available {
        return raise(VALUE_ERROR, "destination buffer is too small");
    }
    let data = if let Some(bytes) = value::as_bytes(args[1]) {
        bytes.to_vec()
    } else {
        let (src_slot, src_offset, src_available) = match memory_operand(args[1]) {
            Ok(parts) => parts,
            Err(error) => return error,
        };
        if count > src_available {
            return raise(VALUE_ERROR, "source buffer is too small");
        }
        let state = state();
        state
            .read_bytes(src_slot, src_offset, count)
            .map(<[u8]>::to_vec)
            .unwrap_or_default()
    };
    if data.len() < count {
        return raise(VALUE_ERROR, "source buffer is too small");
    }
    let mut state = state();
    if state.write_bytes(dest_slot, dest_offset, &data[..count]) {
        NONE
    } else {
        raise(VALUE_ERROR, "destination buffer is too small")
    }
}

fn memset(args: &[i64]) -> i64 {
    let byte = match int_arg(args[1], "memset() byte") {
        Ok(byte) => byte as u8,
        Err(error) => return error,
    };
    let count = match int_arg(args[2], "memset() count") {
        Ok(count) if count >= 0 => count as usize,
        Ok(_) => return raise(VALUE_ERROR, "count must not be negative"),
        Err(error) => return error,
    };
    if count > MAX_TRANSFER {
        return raise(VALUE_ERROR, "count is too large");
    }
    let (slot, offset, available) = match memory_operand(args[0]) {
        Ok(parts) => parts,
        Err(error) => return error,
    };
    if count > available {
        return raise(VALUE_ERROR, "destination buffer is too small");
    }
    let data = vec![byte; count];
    let mut state = state();
    if state.write_bytes(slot, offset, &data) {
        NONE
    } else {
        raise(VALUE_ERROR, "destination buffer is too small")
    }
}

fn create_string_buffer(args: &[i64]) -> i64 {
    create_char_buffer(args, Simple::CChar)
}

fn create_unicode_buffer(args: &[i64]) -> i64 {
    create_char_buffer(args, Simple::WChar)
}

fn create_char_buffer(args: &[i64], simple: Simple) -> i64 {
    let first = args[0];
    let mut data: Vec<u8> = Vec::new();
    let mut text: Option<String> = None;
    let mut explicit_size: Option<usize> = None;
    if let Some(size) = value::as_integer(first) {
        if size < 0 {
            return raise(VALUE_ERROR, "negative size");
        }
        explicit_size = Some(size as usize);
    } else if let Some(bytes) = value::as_bytes(first) {
        data.extend_from_slice(bytes);
    } else if let Some(value_text) = value::as_str(first) {
        text = Some(value_text.to_string());
    } else {
        return type_error("create_string_buffer() argument must be an int, bytes or str");
    }
    if let Some(raw) = args.get(1).copied() {
        match int_arg(raw, "size") {
            Ok(size) if size >= 0 => {
                if explicit_size.is_some() {
                    return type_error("size given twice");
                }
                explicit_size = Some(size as usize);
            }
            Ok(_) => return raise(VALUE_ERROR, "negative size"),
            Err(error) => return error,
        }
    }
    let len = match (explicit_size, text.as_ref()) {
        (Some(size), _) => size,
        (None, Some(text)) => text.chars().count() + 1,
        (None, None) => data.len() + 1,
    };
    if len > MAX_SLOT {
        return raise(OVERFLOW_ERROR, "buffer is too large");
    }
    let elem = {
        let state = state();
        state.simple_id(simple)
    };
    let tid = {
        let mut state = state();
        state.register(CType::Array { elem, len })
    };
    let class = class_for_type(tid);
    let elem_size = simple.size();
    let slot = state().alloc(elem_size.saturating_mul(len));
    let instance = new_instance(class, tid, slot, 0);
    let payload = match (simple, text) {
        (Simple::WChar, Some(text)) => {
            let mut units = Vec::new();
            for ch in text.chars() {
                units.extend_from_slice(&(ch as u32).to_le_bytes());
            }
            units
        }
        (Simple::WChar, None) => {
            let mut units = Vec::new();
            for ch in String::from_utf8_lossy(&data).chars() {
                units.extend_from_slice(&(ch as u32).to_le_bytes());
            }
            units
        }
        (_, Some(text)) => text.into_bytes(),
        (_, None) => data,
    };
    let capacity = elem_size.saturating_mul(len);
    if payload.len() > capacity {
        return raise(VALUE_ERROR, "byte string too long");
    }
    {
        let mut state = state();
        let _ = state.write_bytes(slot, 0, &payload);
    }
    instance
}

fn resize(args: &[i64]) -> i64 {
    let instance = args[0];
    let Some(class) = value::instance_class_value(instance) else {
        return type_error("resize() requires a ctypes object");
    };
    let size = match int_arg(args[1], "resize() size") {
        Ok(size) if size >= 0 => size as usize,
        Ok(_) => return raise(VALUE_ERROR, "negative size"),
        Err(error) => return error,
    };
    if size > MAX_SLOT {
        return raise(OVERFLOW_ERROR, "buffer is too large");
    }
    let Some((old_slot, old_offset)) = instance_slot(instance) else {
        return type_error("ctypes object has no storage");
    };
    let old_size = {
        let state = state();
        state.size_of(type_id_of(instance).unwrap_or(0))
    };
    let copy = {
        let state = state();
        state
            .read_bytes(old_slot, old_offset, old_size.min(size))
            .map(<[u8]>::to_vec)
            .unwrap_or_default()
    };
    let tid = type_id_of(instance).unwrap_or(0);
    let info = {
        let state = state();
        state.type_of(tid)
    };
    let new_tid = match info {
        Some(CType::Array { elem, .. }) => {
            let mut state = state();
            state.register(CType::Array { elem, len: size })
        }
        _ => return type_error("resize() requires a ctypes array"),
    };
    let new_slot = state().alloc(size);
    {
        let mut state = state();
        let _ = state.write_bytes(new_slot, 0, &copy);
    }
    set_attr(instance, "_hydro_type", encode(new_tid as i64));
    set_attr(instance, "_hydro_slot", encode_len(new_slot as usize));
    set_attr(instance, "_hydro_offset", encode(0));
    let _ = class;
    NONE
}

fn make_cfunctype(args: &[i64]) -> i64 {
    let restype = match cfunctype_restype(args[0]) {
        Ok(restype) => restype,
        Err(error) => return error,
    };
    let mut argtypes = Vec::with_capacity(args.len().saturating_sub(1));
    for &arg in &args[1..] {
        let tid = match type_id_of(arg) {
            Ok(tid) => tid,
            Err(error) => return error,
        };
        let supported = {
            let state = state();
            match state.type_of(tid) {
                Some(CType::Simple(simple)) => !simple.is_float(),
                Some(CType::Pointer(_)) | Some(CType::Func { .. }) => true,
                _ => false,
            }
        };
        if !supported {
            return raise(
                NOT_IMPLEMENTED_ERROR,
                "CFUNCTYPE supports only integer and pointer arguments",
            );
        }
        argtypes.push(tid);
    }
    if argtypes.len() > 8 {
        return raise(
            NOT_IMPLEMENTED_ERROR,
            "CFUNCTYPE supports at most 8 arguments",
        );
    }
    let tid = {
        let mut state = state();
        state.func_type(restype, argtypes)
    };
    class_for_type(tid)
}

fn cfunctype_restype(v: i64) -> Result<TypeId, i64> {
    if v == NONE {
        return Ok({
            let mut state = state();
            state.void_type()
        });
    }
    let tid = type_id_of(v)?;
    let supported = {
        let state = state();
        match state.type_of(tid) {
            Some(CType::Simple(simple)) => !simple.is_float(),
            Some(CType::Pointer(_)) | Some(CType::Func { .. }) => true,
            _ => false,
        }
    };
    if !supported {
        return Err(raise(
            NOT_IMPLEMENTED_ERROR,
            "CFUNCTYPE supports only integer and pointer return values",
        ));
    }
    Ok(tid)
}

/// Installs `raw` as the value of a function-pointer instance.
fn install_function(
    instance: i64,
    raw: i64,
    restype: TypeId,
    argtypes: &[TypeId],
) -> Result<(), i64> {
    if let Some(addr) = value::as_integer(raw) {
        set_attr(instance, "_hydro_target_addr", encode(addr));
        set_attr(instance, "_hydro_callback", encode(-1));
        return Ok(());
    }
    if raw == NONE {
        set_attr(instance, "_hydro_target_addr", encode(0));
        set_attr(instance, "_hydro_callback", encode(-1));
        return Ok(());
    }
    if value::as_function(raw).is_some() || value::as_instance(raw).is_some() {
        let slot = register_callback(raw, restype, argtypes)?;
        let addr = callback_address(slot);
        set_attr(instance, "_hydro_callback", encode_len(slot));
        set_attr(instance, "_hydro_target_addr", encode(addr as i64));
        return Ok(());
    }
    Err(type_error(
        "function pointer initializer must be an int or callable",
    ))
}

fn get_last_error(_args: &[i64]) -> i64 {
    encode(LAST_ERROR.with(|cell| cell.get()))
}

fn set_last_error(args: &[i64]) -> i64 {
    match int_arg(args[0], "last error") {
        Ok(error) => {
            LAST_ERROR.with(|cell| cell.set(error));
            NONE
        }
        Err(error) => error,
    }
}

thread_local! {
    static LAST_ERROR: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// ctypes' private per-thread `errno` copy.
    ///
    /// CPython keeps this separate from the C library's `errno`:
    /// `get_errno`/`set_errno` read and write the copy, and a foreign call
    /// through a `use_errno=True` function refreshes it from the system value
    /// after the call. Calls without the flag leave it untouched.
    static CTYPES_ERRNO: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

/// The calling thread's `errno` slot, when the C library exposes one.
#[cfg(target_os = "linux")]
fn errno_slot() -> Option<*mut libc::c_int> {
    // SAFETY: `__errno_location` returns a valid pointer to the thread's
    // errno slot on glibc and musl.
    Some(unsafe { libc::__errno_location() })
}

#[cfg(not(target_os = "linux"))]
fn errno_slot() -> Option<*mut libc::c_int> {
    None
}

/// The current value of the system `errno` (not the ctypes copy).
fn current_errno() -> i64 {
    match errno_slot() {
        // SAFETY: `slot` points at this thread's errno cell.
        Some(slot) => i64::from(unsafe { *slot }),
        None => 0,
    }
}

/// Records the system `errno` in ctypes' private copy.
fn save_errno() {
    CTYPES_ERRNO.with(|cell| cell.set(current_errno()));
}

/// `_ctypes.get_errno()`: the thread-local copy of `errno`.
fn get_errno(_args: &[i64]) -> i64 {
    encode(CTYPES_ERRNO.with(std::cell::Cell::get))
}

/// `_ctypes.set_errno(value)`: assigns the thread-local copy of `errno`.
fn set_errno(args: &[i64]) -> i64 {
    let value = match int_arg(args[0], "errno value") {
        Ok(value) => value,
        Err(error) => return error,
    };
    if i32::try_from(value).is_err() {
        return raise(OVERFLOW_ERROR, "errno value is out of range");
    }
    CTYPES_ERRNO.with(|cell| cell.set(value));
    NONE
}

/// `_ctypes.dlopen(name, mode=DEFAULT_MODE)`: opens a shared object and
/// returns its handle as an integer.
///
/// The handle stays open for the process lifetime, exactly like a `CDLL`
/// handle: `FuncPtr` objects built from it keep working.
fn dlopen(args: &[i64]) -> i64 {
    let mode = args
        .get(1)
        .copied()
        .and_then(value::as_integer)
        .unwrap_or(DEFAULT_MODE);
    match load_library_object(args[0], mode, false) {
        Ok(library) => encode_len(library.handle),
        Err(error) => error,
    }
}

/// Raises the explicit `OSError` reported by Windows-only `ctypes` entries on
/// every other platform.
fn windows_only_error(name: &str) -> i64 {
    raise(
        OS_ERROR,
        &format!("ctypes.{name} is only available on Windows"),
    )
}

/// `ctypes.FreeLibrary(handle)`: releases a handle returned by `LoadLibrary`.
///
/// The Hydro runtime does not fake Windows handles: on non-Windows platforms
/// the call raises `OSError` instead of pretending to free one.
fn free_library(args: &[i64]) -> i64 {
    #[cfg(windows)]
    {
        let handle = match int_arg(args[0], "library handle") {
            Ok(handle) => handle,
            Err(error) => return error,
        };
        // SAFETY: the caller passes a handle obtained from `LoadLibrary`;
        // `FreeLibrary` ignores an invalid one.
        unsafe { win::FreeLibrary(handle as *mut c_void) };
        NONE
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        windows_only_error("FreeLibrary")
    }
}

/// `ctypes.GetLastError()`: the Win32 last-error value.
fn get_last_error_win32(_args: &[i64]) -> i64 {
    #[cfg(windows)]
    {
        // SAFETY: `GetLastError` has no preconditions.
        encode(i64::from(unsafe { win::GetLastError() }))
    }
    #[cfg(not(windows))]
    {
        windows_only_error("GetLastError")
    }
}

/// `ctypes.WinError(code=None, descr=None)`: builds an `OSError` from a Win32
/// error code.
///
/// On non-Windows platforms there is no Win32 error space, so the call raises
/// `OSError` instead of producing a fabricated message.
fn win_error(args: &[i64]) -> i64 {
    #[cfg(windows)]
    {
        let code = match args.first().copied().filter(|raw| *raw != NONE) {
            Some(raw) => match int_arg(raw, "error code") {
                Ok(code) => code,
                Err(error) => return error,
            },
            // SAFETY: `GetLastError` has no preconditions.
            None => i64::from(unsafe { win::GetLastError() }),
        };
        let description = match args.get(1).copied().filter(|raw| *raw != NONE) {
            Some(raw) => match str_arg(raw, "error description") {
                Ok(description) => description,
                Err(error) => return error,
            },
            None => std::io::Error::from_raw_os_error(code as i32).to_string(),
        };
        let message = format!("[WinError {code}] {description}");
        exc::exception_value(OS_ERROR, &message, vec![message.clone()])
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        windows_only_error("WinError")
    }
}

/// Win32 entry points used by the Windows-only `ctypes` surface.
#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        pub(super) fn FreeLibrary(module: *mut c_void) -> i32;
        pub(super) fn GetLastError() -> u32;
    }
}

// -------------------------------------------------------------------------
// Calling convention
// -------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, Ordering};

fn instance_restype(instance: i64) -> Option<TypeId> {
    value::instance_attr(instance, "restype").and_then(|raw| {
        if raw == NONE {
            Some({
                let mut state = state();
                state.void_type()
            })
        } else {
            type_id_of(raw).ok()
        }
    })
}

fn instance_argtypes(instance: i64) -> Option<Vec<TypeId>> {
    let raw = value::instance_attr(instance, "argtypes")?;
    let items = as_items(raw)?;
    let mut types = Vec::with_capacity(items.len());
    for item in items {
        types.push(type_id_of(item).ok()?);
    }
    Some(types)
}

fn instance_use_errno(instance: i64) -> bool {
    value::instance_attr(instance, "use_errno").is_some_and(value::truthy)
}

fn invoke_pointer(instance: i64, restype: TypeId, argtypes: &[TypeId], args: &[i64]) -> i64 {
    let callback = attr_int(instance, "_hydro_callback").filter(|slot| *slot >= 0);
    if let Some(slot) = callback {
        return call_callback_from_python(slot as usize, restype, argtypes, args);
    }
    if !argtypes.is_empty() && argtypes.len() != args.len() {
        return type_error(&format!(
            "this function takes {} arguments ({} given)",
            argtypes.len(),
            args.len()
        ));
    }
    // Float/struct results cannot be captured from a fixed integer ABI.
    if !abi_supported(restype) {
        return raise(
            NOT_IMPLEMENTED_ERROR,
            "CFUNCTYPE supports only integer and pointer return values",
        );
    }
    if args.len() > 8 {
        return raise(NOT_IMPLEMENTED_ERROR, "C calls support at most 8 arguments");
    }
    let mut words = [0u64; 8];
    for (index, &arg) in args.iter().enumerate() {
        let want = argtypes.get(index).copied();
        match arg_word(arg, want) {
            Ok(word) => words[index] = word,
            Err(error) => return error,
        }
    }
    let addr = attr_int(instance, "_hydro_target_addr").unwrap_or(0);
    if addr == 0 {
        return raise(VALUE_ERROR, "NULL function pointer");
    }
    let use_errno = instance_use_errno(instance);
    let raw = unsafe { call_raw(addr as usize, &words) };
    if use_errno {
        save_errno();
    }
    match restype_word(restype, raw) {
        Ok(value_arg) => value_arg,
        Err(error) => error,
    }
}

fn abi_supported(tid: TypeId) -> bool {
    let state = state();
    match state.type_of(tid) {
        Some(CType::Simple(simple)) => !simple.is_float(),
        Some(CType::Pointer(_)) | Some(CType::Func { .. }) | Some(CType::Void) => true,
        _ => false,
    }
}

type RawFn8 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64;

unsafe fn call_raw(addr: usize, words: &[u64; 8]) -> u64 {
    // SAFETY: the caller validated the symbol/address came from `dlsym` or a
    // callback trampoline, validated the argument count and converted every
    // argument to a machine word. The signature covers the SysV integer
    // argument registers and stack slots used by the supported signatures.
    let function: RawFn8 = unsafe { std::mem::transmute(addr) };
    unsafe {
        function(
            words[0], words[1], words[2], words[3], words[4], words[5], words[6], words[7],
        )
    }
}

/// Reads the current Python value of an instance's own storage.
fn read_value_at(instance: i64) -> Result<i64, i64> {
    let Some((slot, offset)) = instance_slot(instance) else {
        return Err(type_error("ctypes object has no storage"));
    };
    let tid = type_id_of(instance)?;
    read_value(slot, offset, tid)
}

/// Converts one Python argument into a machine word.
fn arg_word(arg: i64, want: Option<TypeId>) -> Result<u64, i64> {
    if let Some(tid) = want {
        let info = {
            let state = state();
            state.type_of(tid)
        };
        return match info {
            Some(CType::Simple(simple)) => simple_arg_word(simple, arg),
            Some(CType::Pointer(_)) | Some(CType::Func { .. }) => pointer_arg_word(arg),
            Some(CType::Void) => Err(type_error("cannot pass a void value as an argument")),
            Some(CType::Struct { .. }) | Some(CType::Array { .. }) => {
                if value::as_instance(arg).is_some() {
                    instance_address(arg).map(|addr| addr as u64)
                } else {
                    Err(raise(
                        NOT_IMPLEMENTED_ERROR,
                        "structure and array arguments must be ctypes instances",
                    ))
                }
            }
            None => Err(type_error("unknown ctypes argument type")),
        };
    }
    // No declared argtypes: pass integers, pointers and buffers.
    if arg == NONE {
        return Ok(0);
    }
    if let Some(n) = value::as_integer(arg) {
        return Ok(n as u64);
    }
    if let Some(bytes) = value::as_bytes(arg) {
        let mut data = bytes.to_vec();
        data.push(0);
        return Ok(allocate_bytes(&data) as u64);
    }
    if let Some(text) = value::as_str(arg) {
        return Ok(allocate_wide(text) as u64);
    }
    if value::as_instance(arg).is_some() {
        // Default conversion: scalars pass by value, everything else by
        // pointer (CPython's ctypes convention).
        if let Ok(tid) = type_id_of(arg) {
            let info = {
                let state = state();
                state.type_of(tid)
            };
            match info {
                Some(CType::Simple(simple)) if simple.is_integer() || simple == Simple::Bool => {
                    if let Ok(raw) = read_value_at(arg) {
                        if let Some(n) = value::as_integer(raw) {
                            return Ok(n as u64);
                        }
                    }
                }
                Some(CType::Simple(Simple::VoidP))
                | Some(CType::Simple(Simple::CharP))
                | Some(CType::Simple(Simple::WCharP)) => {
                    return pointer_target_or_storage(arg).map(|addr| addr as u64);
                }
                _ => {}
            }
        }
        return instance_address(arg).map(|addr| addr as u64);
    }
    Err(type_error(&format!(
        "cannot convert {} to a C argument without argtypes",
        value::type_name(arg)
    )))
}

fn simple_arg_word(simple: Simple, arg: i64) -> Result<u64, i64> {
    match simple {
        Simple::Bool => Ok(u64::from(value::truthy(arg))),
        Simple::CharP => match arg {
            NONE => Ok(0),
            _ => {
                if let Some(bytes) = value::as_bytes(arg) {
                    let mut data = bytes.to_vec();
                    data.push(0);
                    return Ok(allocate_bytes(&data) as u64);
                }
                pointer_arg_word(arg)
            }
        },
        Simple::WCharP => match arg {
            NONE => Ok(0),
            _ => {
                if let Some(text) = value::as_str(arg) {
                    return Ok(allocate_wide(text) as u64);
                }
                pointer_arg_word(arg)
            }
        },
        Simple::VoidP => match arg {
            NONE => Ok(0),
            _ => pointer_arg_word(arg),
        },
        simple if simple.is_float() => Err(raise(
            NOT_IMPLEMENTED_ERROR,
            "float and double arguments are not supported",
        )),
        Simple::CChar => {
            if let Some(bytes) = value::as_bytes(arg) {
                if bytes.len() != 1 {
                    return Err(raise(VALUE_ERROR, "one character bytes expected"));
                }
                return Ok(u64::from(bytes[0]));
            }
            int_arg(arg, "c_char argument").map(|n| n as u64)
        }
        Simple::WChar => {
            if let Some(text) = value::as_str(arg) {
                let mut chars = text.chars();
                let Some(ch) = chars.next() else {
                    return Err(raise(VALUE_ERROR, "one character string expected"));
                };
                if chars.next().is_some() {
                    return Err(raise(VALUE_ERROR, "one character string expected"));
                }
                return Ok(u64::from(ch as u32));
            }
            int_arg(arg, "c_wchar argument").map(|n| n as u64)
        }
        _ => int_arg(arg, "integer argument").map(|n| n as u64),
    }
}

fn pointer_arg_word(arg: i64) -> Result<u64, i64> {
    if arg == NONE {
        return Ok(0);
    }
    if let Some(n) = value::as_integer(arg) {
        return Ok(n as u64);
    }
    if let Some(bytes) = value::as_bytes(arg) {
        let mut data = bytes.to_vec();
        data.push(0);
        return Ok(allocate_bytes(&data) as u64);
    }
    if let Some(text) = value::as_str(arg) {
        return Ok(allocate_wide(text) as u64);
    }
    if value::as_instance(arg).is_some() {
        return pointer_target_or_storage(arg).map(|addr| addr as u64);
    }
    Err(type_error(&format!(
        "cannot convert {} to a pointer",
        value::type_name(arg)
    )))
}

/// Converts the raw return word into a Python value.
fn restype_word(tid: TypeId, raw: u64) -> Result<i64, i64> {
    let info = {
        let state = state();
        state.type_of(tid)
    };
    match info {
        Some(CType::Void) => Ok(NONE),
        Some(CType::Simple(Simple::VoidP)) | Some(CType::Pointer(_)) => {
            Ok(if raw == 0 { NONE } else { unsigned_result(raw) })
        }
        Some(CType::Simple(Simple::CharP)) => {
            if raw == 0 {
                return Ok(NONE);
            }
            match read_c_string(raw as usize) {
                Ok(bytes) => Ok(value::bytes_new(bytes)),
                Err(error) => Err(error),
            }
        }
        Some(CType::Simple(Simple::WCharP)) => {
            if raw == 0 {
                return Ok(NONE);
            }
            match read_wide_string(raw as usize) {
                Ok(text) => Ok(value::str_alloc(text)),
                Err(error) => Err(error),
            }
        }
        Some(CType::Simple(Simple::Bool)) => Ok(value::bool_value(raw != 0)),
        Some(CType::Simple(Simple::CChar)) => Ok(value::bytes_new(vec![raw as u8])),
        Some(CType::Simple(Simple::WChar)) => match char::from_u32(raw as u32) {
            Some(ch) => Ok(value::str_alloc(ch.to_string())),
            None => Err(raise(VALUE_ERROR, "invalid c_wchar return value")),
        },
        Some(CType::Simple(simple)) if simple.is_integer() => {
            if simple.is_signed() {
                let size = simple.size();
                let shift = 64 - size * 8;
                let signed = ((raw << shift) as i64) >> shift;
                Ok(encode(signed))
            } else {
                let size = simple.size();
                let mask = if size == 8 {
                    u64::MAX
                } else {
                    (1u64 << (size * 8)) - 1
                };
                Ok(unsigned_result(raw & mask))
            }
        }
        Some(CType::Simple(simple)) if simple.is_float() => Err(raise(
            NOT_IMPLEMENTED_ERROR,
            "float and double results are not supported",
        )),
        Some(CType::Func { .. }) => Ok(encode(raw as i64)),
        Some(CType::Struct { .. }) | Some(CType::Array { .. }) => Err(raise(
            NOT_IMPLEMENTED_ERROR,
            "structure and array results are not supported",
        )),
        Some(_) => Err(type_error("unsupported ctypes return type")),
        None => Err(type_error("unknown ctypes return type")),
    }
}

// -------------------------------------------------------------------------
// Callbacks
// -------------------------------------------------------------------------

struct CallbackState {
    function: i64,
    restype: TypeId,
    argtypes: Vec<TypeId>,
    busy: AtomicBool,
}

static CALLBACKS: Mutex<Option<HashMap<usize, CallbackState>>> = Mutex::new(None);

fn callbacks() -> MutexGuard<'static, Option<HashMap<usize, CallbackState>>> {
    CALLBACKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn register_callback(function: i64, restype: TypeId, argtypes: &[TypeId]) -> Result<usize, i64> {
    let root = heap::add_persistent_root(function);
    let mut guard = callbacks();
    let map = guard.get_or_insert_with(HashMap::new);
    for slot in 0..TRAMPOLINES.len() {
        if let std::collections::hash_map::Entry::Vacant(entry) = map.entry(slot) {
            entry.insert(CallbackState {
                function,
                restype,
                argtypes: argtypes.to_vec(),
                busy: AtomicBool::new(false),
            });
            return Ok(slot);
        }
    }
    heap::remove_persistent_root(root);
    Err(raise(
        NOT_IMPLEMENTED_ERROR,
        "too many live ctypes callbacks (limit 8)",
    ))
}

fn callback_address(slot: usize) -> usize {
    match TRAMPOLINES.get(slot) {
        Some(function) => *function as usize,
        None => 0,
    }
}

/// Calls a registered Python callback directly (used by `__call__`).
fn call_callback_from_python(
    slot: usize,
    restype: TypeId,
    argtypes: &[TypeId],
    args: &[i64],
) -> i64 {
    let compatible = {
        let guard = callbacks();
        guard
            .as_ref()
            .and_then(|map| map.get(&slot))
            .map(|state| state.argtypes.len() == argtypes.len())
            .unwrap_or(false)
    };
    if !compatible {
        return type_error("callback signature mismatch");
    }
    let mut words = [0u64; 8];
    for (index, &arg) in args.iter().enumerate().take(8) {
        let want = argtypes.get(index).copied();
        match arg_word(arg, want) {
            Ok(word) => words[index] = word,
            Err(error) => return error,
        }
    }
    match invoke_callback(slot, words) {
        Ok(raw) => match restype_word(restype, raw) {
            Ok(value_arg) => value_arg,
            Err(error) => error,
        },
        Err(error) => error,
    }
}

/// Runs one callback with machine-word arguments.
fn invoke_callback(slot: usize, words: [u64; 8]) -> Result<u64, i64> {
    let (function, restype, argtypes) = {
        let guard = callbacks();
        let Some(state) = guard.as_ref().and_then(|map| map.get(&slot)) else {
            return Ok(0);
        };
        if state
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // Re-entered from a nested C call; refuse instead of corrupting.
            return Ok(0);
        }
        (state.function, state.restype, state.argtypes.clone())
    };
    let result = run_callback_function(function, restype, &argtypes, &words);
    if let Some(state) = callbacks().as_ref().and_then(|map| map.get(&slot)) {
        state.busy.store(false, Ordering::SeqCst);
    }
    result
}

fn run_callback_function(
    function: i64,
    restype: TypeId,
    argtypes: &[TypeId],
    words: &[u64; 8],
) -> Result<u64, i64> {
    let mut args = Vec::with_capacity(argtypes.len());
    for (index, &tid) in argtypes.iter().enumerate() {
        let raw = words.get(index).copied().unwrap_or(0);
        args.push(match restype_word(tid, raw) {
            Ok(value_arg) => value_arg,
            Err(_) => {
                exc::clear();
                NONE
            }
        });
    }
    let roots = heap::root_values(&args);
    let result = crate::builtins::call(function, args.len(), args.as_ptr());
    heap::release_roots(roots);
    if exc::pending() {
        // A C caller has no exception channel: drop the exception and return 0.
        exc::clear();
        return Ok(0);
    }
    // Convert the Python result back to a machine word.
    if let Some(n) = value::as_integer(result) {
        return Ok(n as u64);
    }
    if value::as_instance(result).is_some() {
        if let Ok(addr) = instance_address(result) {
            return Ok(addr as u64);
        }
    }
    if result == NONE {
        return Ok(0);
    }
    let _ = restype;
    Ok(0)
}

macro_rules! trampoline {
    ($name:ident, $slot:expr) => {
        unsafe extern "C" fn $name(
            a0: u64,
            a1: u64,
            a2: u64,
            a3: u64,
            a4: u64,
            a5: u64,
            a6: u64,
            a7: u64,
        ) -> u64 {
            // Registered trampolines are only reachable through a
            // `CFUNCTYPE` value that mapped this slot.
            invoke_callback($slot, [a0, a1, a2, a3, a4, a5, a6, a7]).unwrap_or(0)
        }
    };
}

trampoline!(callback_0, 0);
trampoline!(callback_1, 1);
trampoline!(callback_2, 2);
trampoline!(callback_3, 3);
trampoline!(callback_4, 4);
trampoline!(callback_5, 5);
trampoline!(callback_6, 6);
trampoline!(callback_7, 7);

static TRAMPOLINES: [unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64; 8] = [
    callback_0, callback_1, callback_2, callback_3, callback_4, callback_5, callback_6, callback_7,
];

// -------------------------------------------------------------------------
// CDLL
// -------------------------------------------------------------------------

fn cdll_class() -> i64 {
    if let Some(class) = cached("ctypes.class.CDLL") {
        return class;
    }
    let methods = vec![
        (
            "__init__".to_string(),
            native(CDLL_INIT, 6),
            MethodKind::Normal,
        ),
        (
            "__getitem__".to_string(),
            native(CDLL_GETITEM, 2),
            MethodKind::Normal,
        ),
    ];
    let class = value::class_new_full("CDLL".to_string(), NONE, methods, Vec::new());
    globals::registry_set("ctypes.class.CDLL", class);
    class
}

fn library_loader_class() -> i64 {
    if let Some(class) = cached("ctypes.class.LibraryLoader") {
        return class;
    }
    let methods = vec![
        (
            "__init__".to_string(),
            native(LOADER_INIT, 2),
            MethodKind::Normal,
        ),
        (
            "LoadLibrary".to_string(),
            native(LOADER_LOAD, 2),
            MethodKind::Normal,
        ),
        (
            "__getitem__".to_string(),
            native(LOADER_GETITEM, 2),
            MethodKind::Normal,
        ),
    ];
    let class = value::class_new_full("LibraryLoader".to_string(), NONE, methods, Vec::new());
    globals::registry_set("ctypes.class.LibraryLoader", class);
    class
}

fn cdll_instance() -> i64 {
    if let Some(instance) = cached("ctypes.cdll") {
        return instance;
    }
    let class = library_loader_class();
    let instance = value::instance_new(class);
    set_attr(instance, "_loader_class", cdll_class());
    globals::registry_set("ctypes.cdll", instance);
    instance
}

fn loader_init(args: &[i64]) -> i64 {
    let instance = args[0];
    let class = match args.get(1).copied() {
        Some(class) if value::as_class(class).is_some() => class,
        _ => cdll_class(),
    };
    set_attr(instance, "_loader_class", class);
    NONE
}

fn loader_load(args: &[i64]) -> i64 {
    let instance = args[0];
    let class = match value::instance_attr(instance, "_loader_class") {
        Some(class) if value::as_class(class).is_some() => class,
        _ => cdll_class(),
    };
    load_library(class, args[1], DEFAULT_MODE, false)
}

fn loader_getitem(args: &[i64]) -> i64 {
    loader_load(args)
}

fn cdll_init(args: &[i64]) -> i64 {
    let instance = args[0];
    let name = args[1];
    let mode = args
        .get(2)
        .copied()
        .and_then(value::as_integer)
        .unwrap_or(DEFAULT_MODE);
    let use_errno = args.get(4).copied().is_some_and(value::truthy);
    let library = match load_library_object(name, mode, use_errno) {
        Ok(library) => library,
        Err(error) => return error,
    };
    set_attr(instance, "_handle", encode_len(library.handle));
    set_attr(
        instance,
        "_name",
        match value::as_str(name) {
            Some(text) => value::str_alloc(text.to_string()),
            None => NONE,
        },
    );
    set_attr(instance, "use_errno", value::bool_value(use_errno));
    populate_symbols(instance, &library);
    NONE
}

fn cdll_getitem(args: &[i64]) -> i64 {
    let instance = args[0];
    let name = match str_arg(args[1], "symbol name") {
        Ok(name) => name,
        Err(error) => return error,
    };
    match value::instance_attr(instance, &name) {
        Some(symbol) => symbol,
        None => raise(ATTRIBUTE_ERROR, &format!("function '{name}' not found")),
    }
}

struct Library {
    handle: usize,
    path: Option<PathBuf>,
}

fn load_library(class: i64, name: i64, mode: i64, use_errno: bool) -> i64 {
    let library = match load_library_object(name, mode, use_errno) {
        Ok(library) => library,
        Err(error) => return error,
    };
    let instance = value::instance_new(class);
    set_attr(instance, "_handle", encode_len(library.handle));
    set_attr(
        instance,
        "_name",
        match value::as_str(name) {
            Some(text) => value::str_alloc(text.to_string()),
            None => NONE,
        },
    );
    set_attr(instance, "use_errno", value::bool_value(use_errno));
    populate_symbols(instance, &library);
    instance
}

fn load_library_object(name: i64, mode: i64, _use_errno: bool) -> Result<Library, i64> {
    let text = match value::as_str(name) {
        Some(text) => text.to_string(),
        None if name == NONE => String::new(),
        None => return Err(type_error("library name must be str or None")),
    };
    let c_name = if text.is_empty() {
        None
    } else {
        match CString::new(text.clone()) {
            Ok(name) => Some(name),
            Err(_) => return Err(raise(VALUE_ERROR, "library name contains a NUL byte")),
        }
    };
    let flags = (mode as i32) | RTLD_NOW as i32;
    // SAFETY: `c_name` is a valid NUL-terminated string (or NULL for the main
    // program); `dlopen` returns an owned handle or NULL.
    let handle = unsafe {
        libc::dlopen(
            c_name
                .as_ref()
                .map(|name| name.as_ptr())
                .unwrap_or(std::ptr::null()),
            flags,
        )
    };
    if handle.is_null() {
        // SAFETY: `dlerror` returns a NUL-terminated message or NULL.
        let message = unsafe {
            let error = libc::dlerror();
            if error.is_null() {
                "cannot load library".to_string()
            } else {
                CStr::from_ptr(error).to_string_lossy().into_owned()
            }
        };
        return Err(exc::raise_with_message(OS_ERROR, &message));
    }
    let path = library_path(handle).or_else(|| {
        if text.contains('/') {
            Some(PathBuf::from(&text))
        } else {
            None
        }
    });
    Ok(Library {
        handle: handle as usize,
        path,
    })
}

/// `struct link_map` prefix used by `dlinfo(RTLD_DI_LINKMAP)`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(dead_code)]
#[repr(C)]
struct LinkMap {
    l_addr: usize,
    l_name: *const libc::c_char,
    l_ld: *mut c_void,
    l_next: *mut LinkMap,
    l_prev: *mut LinkMap,
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn library_path(handle: *mut c_void) -> Option<PathBuf> {
    let mut map: *mut LinkMap = std::ptr::null_mut();
    // SAFETY: `handle` is an open `dlopen` handle and `map` points at a
    // writable pointer; `dlinfo` fills it with the link_map address.
    let result = unsafe {
        libc::dlinfo(
            handle,
            libc::RTLD_DI_LINKMAP,
            &mut map as *mut *mut LinkMap as *mut c_void,
        )
    };
    if result != 0 || map.is_null() {
        return None;
    }
    // SAFETY: `dlinfo` returned a valid link_map for the handle.
    let name = unsafe { (*map).l_name };
    if name.is_null() {
        return None;
    }
    // SAFETY: `l_name` is a NUL-terminated path owned by the loader.
    let path = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned();
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn library_path(_handle: *mut c_void) -> Option<PathBuf> {
    None
}

fn populate_symbols(instance: i64, library: &Library) {
    let Some(path) = library.path.as_deref() else {
        return;
    };
    let names = elf_function_symbols(path);
    let handle = library.handle as *mut c_void;
    let class = cfunc_class();
    let use_errno = value::instance_attr(instance, "use_errno").is_some_and(value::truthy);
    let restype = {
        let state = state();
        state.simple_id(Simple::I32)
    };
    let function_tid = {
        let mut state = state();
        state.func_type(restype, Vec::new())
    };
    for name in names.into_iter().take(MAX_SYMBOLS) {
        if matches!(
            name.as_str(),
            "_handle" | "_name" | "use_errno" | "_hydro_library"
        ) || name.starts_with("_hydro_")
        {
            continue;
        }
        let Ok(c_name) = CString::new(name.as_str()) else {
            continue;
        };
        // SAFETY: the handle is open for the process lifetime and the name is
        // NUL-terminated.
        let symbol = unsafe { libc::dlsym(handle, c_name.as_ptr()) };
        if symbol.is_null() {
            continue;
        }
        let slot = state().alloc(8);
        let function = new_instance(class, function_tid, slot, 0);
        set_attr(function, "_hydro_target_addr", encode_len(symbol as usize));
        set_attr(function, "_hydro_callback", encode(-1));
        set_attr(function, "use_errno", value::bool_value(use_errno));
        let _ = write_value(slot, 0, function_tid, encode_len(symbol as usize));
        let roots = heap::root_values(&[function]);
        value::instance_setattr(instance, name, function);
        heap::release_roots(roots);
    }
}

/// Reads the dynamic function symbols from an ELF shared object.
fn elf_function_symbols(path: &Path) -> Vec<String> {
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    let parsed = parse_elf_symbols(&data).unwrap_or_default();
    let mut seen = HashSet::new();
    let mut names: Vec<String> = parsed
        .into_iter()
        .filter(|name| !name.is_empty() && seen.insert(name.clone()))
        .collect();
    names.sort();
    names.truncate(MAX_SYMBOLS);
    names
}

fn parse_elf_symbols(data: &[u8]) -> Option<Vec<String>> {
    if data.len() < 64 || &data[..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return None;
    }
    let shoff = read_u64(data, 0x28)? as usize;
    let shentsize = read_u16(data, 0x3a)? as usize;
    let shnum = read_u16(data, 0x3c)? as usize;
    if shentsize < 64 || shnum == 0 {
        return None;
    }
    let mut dynsym: Option<(usize, usize, usize, usize)> = None;
    for index in 0..shnum {
        let Some(offset) = shoff.checked_add(index.checked_mul(shentsize)?) else {
            break;
        };
        let Some(kind) = read_u32(data, offset.checked_add(4)?) else {
            break;
        };
        if kind == 11 {
            let file_offset = read_u64(data, offset.checked_add(0x18)?)? as usize;
            let size = read_u64(data, offset.checked_add(0x20)?)? as usize;
            let entsize = read_u64(data, offset.checked_add(0x38)?)? as usize;
            let link = read_u32(data, offset.checked_add(0x28)?)? as usize;
            dynsym = Some((file_offset, size, entsize, link));
            break;
        }
    }
    let (sym_offset, sym_size, entsize, link) = dynsym?;
    if entsize < 24 {
        return None;
    }
    let str_header = shoff.checked_add(link.checked_mul(shentsize)?)?;
    let str_offset = read_u64(data, str_header.checked_add(0x18)?)? as usize;
    let str_size = read_u64(data, str_header.checked_add(0x20)?)? as usize;
    let strings = data.get(str_offset..str_offset.checked_add(str_size)?)?;
    let count = sym_size / entsize;
    let mut names = Vec::new();
    for index in 0..count {
        let Some(entry) = sym_offset.checked_add(index.checked_mul(entsize)?) else {
            break;
        };
        let Some(name_offset) = read_u32(data, entry).map(|offset| offset as usize) else {
            break;
        };
        let Some(info) = data.get(entry.checked_add(4)?).copied() else {
            break;
        };
        let Some(section) = read_u16(data, entry.checked_add(6)?) else {
            break;
        };
        let bind = info >> 4;
        let kind = info & 0x0f;
        if section == 0 || !(kind == 1 || kind == 2) || !(bind == 1 || bind == 2) {
            continue;
        }
        let Some(tail) = strings.get(name_offset..) else {
            continue;
        };
        let Some(end) = tail.iter().position(|byte| *byte == 0) else {
            continue;
        };
        let Ok(name) = std::str::from_utf8(&tail[..end]) else {
            continue;
        };
        names.push(name.to_string());
    }
    Some(names)
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    let bytes = data.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

// -------------------------------------------------------------------------
// Dispatch
// -------------------------------------------------------------------------

/// Dispatches a native `ctypes`/`_ctypes` function. Arity is enforced by the
/// central spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "sizeof" => sizeof(args),
        "alignment" => alignment(args),
        "offsetof" => offsetof(args),
        "POINTER" => make_pointer_type(args),
        "pointer" => pointer_new(args),
        "byref" => pointer_new(args),
        "cast" => cast(args),
        "addressof" => addressof(args),
        "string_at" => string_at(args),
        "wstring_at" => wstring_at(args),
        "memmove" => memmove(args),
        "memset" => memset(args),
        "resize" => resize(args),
        "create_string_buffer" => create_string_buffer(args),
        "create_unicode_buffer" => create_unicode_buffer(args),
        "CFUNCTYPE" | "WINFUNCTYPE" | "PYFUNCTYPE" => make_cfunctype(args),
        "get_last_error" => get_last_error(args),
        "set_last_error" => set_last_error(args),
        "get_errno" => get_errno(args),
        "set_errno" => set_errno(args),
        "dlopen" => dlopen(args),
        "FreeLibrary" => free_library(args),
        "GetLastError" => get_last_error_win32(args),
        "WinError" => win_error(args),
        CFIELD_INIT => cfield_init(args),
        CFIELD_GET => cfield_get(args),
        CFIELD_SET => cfield_set(args),
        CFIELD_REPR => cfield_repr(args),
        INIT => cdata_init(args),
        GET_VALUE => cdata_get_value(args),
        SET_VALUE => cdata_set_value(args),
        GET_CONTENTS => cdata_get_contents(args),
        GET_RAW => cdata_get_raw(args),
        SET_RAW => cdata_set_raw(args),
        LEN => cdata_len(args),
        GETITEM => cdata_getitem(args),
        SETITEM => cdata_setitem(args),
        STR => cdata_str(args),
        REPR => cdata_repr(args),
        CALL => cdata_call(args),
        ADD => cdata_add(args),
        RADD => cdata_radd(args),
        CDLL_INIT => cdll_init(args),
        CDLL_GETITEM => cdll_getitem(args),
        LOADER_INIT => loader_init(args),
        LOADER_LOAD => loader_load(args),
        LOADER_GETITEM => loader_getitem(args),
        _ => raise(
            ATTRIBUTE_ERROR,
            &format!("module 'ctypes' has no attribute '{function}'"),
        ),
    }
}

/// Materialises a `ctypes` module constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "_CData" => cdata_class(),
        "_SimpleCData" => simple_cdata_class(),
        "_Pointer" => pointer_base_class(),
        "Array" => array_class(),
        "Structure" => structure_class(),
        "Union" => union_class(),
        "CFuncPtr" | "_CFuncPtr" => cfunc_class(),
        "CDLL" | "PyDLL" => cdll_class(),
        "LibraryLoader" => library_loader_class(),
        "ArgumentError" => argument_error_class(),
        "CField" => cfield_class(),
        "wintypes" => wintypes_module(),

        "c_bool" => simple_class(Simple::Bool),
        "c_byte" | "c_int8" => simple_class(Simple::I8),
        "c_ubyte" | "c_uint8" => simple_class(Simple::U8),
        "c_char" => simple_class(Simple::CChar),
        "c_wchar" => simple_class(Simple::WChar),
        "c_short" | "c_int16" => simple_class(Simple::I16),
        "c_ushort" | "c_uint16" => simple_class(Simple::U16),
        "c_int" | "c_int32" => simple_class(Simple::I32),
        "c_uint" | "c_uint32" => simple_class(Simple::U32),
        "c_long" | "c_int64" | "c_ssize_t" | "c_ptrdiff_t" => simple_class(Simple::I64),
        "c_ulong" | "c_uint64" | "c_size_t" => simple_class(Simple::U64),
        "c_longlong" => simple_class(Simple::I64),
        "c_ulonglong" => simple_class(Simple::U64),
        "c_intp" => simple_class(Simple::I64),
        "c_float" => simple_class(Simple::F32),
        "c_double" => simple_class(Simple::F64),
        "c_longdouble" => simple_class(Simple::LongDouble),
        "c_void_p" => simple_class(Simple::VoidP),
        "c_char_p" => simple_class(Simple::CharP),
        "c_wchar_p" => simple_class(Simple::WCharP),

        "cdll" => cdll_instance(),
        "RTLD_LOCAL" => encode(RTLD_LOCAL),
        "RTLD_GLOBAL" => encode(RTLD_GLOBAL),
        "DEFAULT_MODE" => encode(DEFAULT_MODE),
        "FUNCFLAG_CDECL" => encode(FUNCFLAG_CDECL),
        "FUNCFLAG_PYTHONAPI" => encode(FUNCFLAG_PYTHONAPI),
        "FUNCFLAG_USE_ERRNO" => encode(FUNCFLAG_USE_ERRNO),
        "FUNCFLAG_USE_LASTERROR" => encode(FUNCFLAG_USE_LASTERROR),
        "SIZEOF_TIME_T" => encode(8),
        "__version__" => value::str_alloc(VERSION.to_string()),
        _ => raise(
            ATTRIBUTE_ERROR,
            &format!("module 'ctypes' has no attribute '{name}'"),
        ),
    }
}

fn argument_error_class() -> i64 {
    if let Some(class) = cached("ctypes.class.ArgumentError") {
        return class;
    }
    let base = exc::exception_class(exc::EXCEPTION);
    let class = value::class_new("ArgumentError".to_string(), base, Vec::new());
    globals::registry_set("ctypes.class.ArgumentError", class);
    class
}

// -------------------------------------------------------------------------
// CField
// -------------------------------------------------------------------------

const CFIELD_INIT: &str = "_cfield_init";
const CFIELD_GET: &str = "_cfield_get";
const CFIELD_SET: &str = "_cfield_set";
const CFIELD_REPR: &str = "_cfield_repr";

/// `_ctypes.CField`: the descriptor type that describes one structure field.
///
/// CPython 3.14 constructs it internally as
/// `CField(*, name, type, byte_size, byte_offset, bit_size=None,
/// bit_offset=None, index=0)`; the older `CField(type, name)` form is
/// accepted as well. The object exposes the field metadata (`name`, `type`,
/// `byte_size`, `byte_offset`, `bit_size`, `bit_offset`, `index` and their
/// `size`/`offset` aliases) and implements the descriptor protocol over a
/// ctypes instance.
fn cfield_class() -> i64 {
    if let Some(class) = cached("ctypes.class.CField") {
        return class;
    }
    let methods = vec![
        (
            "__init__".to_string(),
            native(CFIELD_INIT, usize::MAX),
            MethodKind::Normal,
        ),
        (
            "__get__".to_string(),
            native(CFIELD_GET, usize::MAX),
            MethodKind::Normal,
        ),
        (
            "__set__".to_string(),
            native(CFIELD_SET, usize::MAX),
            MethodKind::Normal,
        ),
        (
            "__repr__".to_string(),
            native(CFIELD_REPR, 1),
            MethodKind::Normal,
        ),
    ];
    let class = value::class_new_full("CField".to_string(), NONE, methods, Vec::new());
    globals::registry_set("ctypes.class.CField", class);
    class
}

fn cfield_init(args: &[i64]) -> i64 {
    let instance = args[0];
    let rest = &args[1..];
    if rest.len() < 2 {
        return type_error("CField() requires a field name and a type");
    }
    // `CField(type, name)` (the pre-3.14 form) puts the type first; the
    // documented internal form starts with the name.
    let (name_raw, type_raw, extras) = if value::as_class(rest[0]).is_some() {
        (rest[1], rest[0], &rest[2..])
    } else {
        (rest[0], rest[1], &rest[2..])
    };
    match fill_cfield(instance, name_raw, type_raw, extras) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

/// Validates and stores the metadata of one `CField`.
fn fill_cfield(instance: i64, name_raw: i64, type_raw: i64, extras: &[i64]) -> Result<(), i64> {
    let name = str_arg(name_raw, "field name")?;
    let tid = type_id_of(type_raw)?;
    let type_size = {
        let state = state();
        state.size_of(tid)
    };
    let byte_size = match extras.first().copied().filter(|raw| *raw != NONE) {
        Some(raw) => {
            let declared = int_arg(raw, "byte_size")?;
            if declared != type_size as i64 {
                return Err(raise(
                    VALUE_ERROR,
                    &format!(
                        "CField byte_size ({declared}) does not match type size ({type_size})"
                    ),
                ));
            }
            declared
        }
        None => type_size as i64,
    };
    let byte_offset = match extras.get(1).copied().filter(|raw| *raw != NONE) {
        Some(raw) => int_arg(raw, "byte_offset")?,
        None => 0,
    };
    let bit_size = match extras.get(2).copied().filter(|raw| *raw != NONE) {
        Some(raw) => Some(int_arg(raw, "bit_size")?),
        None => None,
    };
    let bit_offset = match extras.get(3).copied().filter(|raw| *raw != NONE) {
        Some(raw) => int_arg(raw, "bit_offset")?,
        None => 0,
    };
    let index = match extras.get(4).copied().filter(|raw| *raw != NONE) {
        Some(raw) => int_arg(raw, "index")?,
        None => 0,
    };
    let total_bits = byte_size.saturating_mul(8);
    if let Some(width) = bit_size {
        if width <= 0 || bit_offset < 0 || bit_offset.saturating_add(width) > total_bits {
            return Err(raise(
                VALUE_ERROR,
                &format!(
                    "bit field '{name}' overflows its type ({bit_offset} + {width} > {total_bits})"
                ),
            ));
        }
    }
    set_attr(instance, "name", value::str_alloc(name));
    set_attr(instance, "type", type_raw);
    set_attr(instance, "_hydro_field_tid", encode(tid as i64));
    set_attr(instance, "byte_size", encode(byte_size));
    set_attr(instance, "byte_offset", encode(byte_offset));
    set_attr(instance, "size", encode(byte_size));
    set_attr(instance, "offset", encode(byte_offset));
    set_attr(instance, "bit_size", encode(bit_size.unwrap_or(total_bits)));
    set_attr(instance, "bit_offset", encode(bit_offset));
    set_attr(instance, "index", encode(index));
    set_attr(
        instance,
        "is_bitfield",
        value::bool_value(bit_size.is_some()),
    );
    Ok(())
}

fn cfield_name(field: i64) -> Result<String, i64> {
    value::instance_attr(field, "name")
        .and_then(value::as_str)
        .map(str::to_string)
        .ok_or_else(|| type_error("CField has no field name"))
}

/// Checks that `instance` is a ctypes value (a structure instance in the
/// normal case) and returns its class.
fn cfield_instance(instance: i64) -> Result<i64, i64> {
    let Some(class) = value::instance_class_value(instance) else {
        return Err(type_error("descriptor 'CField' requires a ctypes instance"));
    };
    if class_kind(class).is_none() {
        return Err(type_error("descriptor 'CField' requires a ctypes instance"));
    }
    Ok(class)
}

fn cfield_get(args: &[i64]) -> i64 {
    let field = args[0];
    let instance = args.get(1).copied().unwrap_or(NONE);
    // Class access (`MyStruct.field`) returns the descriptor itself.
    if instance == NONE {
        return field;
    }
    if let Err(error) = cfield_instance(instance) {
        return error;
    }
    let name = match cfield_name(field) {
        Ok(name) => name,
        Err(error) => return error,
    };
    match value::instance_attr(instance, &name) {
        Some(view) => view,
        None => raise(
            ATTRIBUTE_ERROR,
            &format!(
                "'{name}' is not a field of '{}'",
                value::type_name(instance)
            ),
        ),
    }
}

fn cfield_set(args: &[i64]) -> i64 {
    let field = args[0];
    let instance = args[1];
    let new_value = args.get(2).copied().unwrap_or(NONE);
    if let Err(error) = cfield_instance(instance) {
        return error;
    }
    let name = match cfield_name(field) {
        Ok(name) => name,
        Err(error) => return error,
    };
    if value::instance_attr(field, "is_bitfield").is_some_and(value::truthy) {
        return raise(
            NOT_IMPLEMENTED_ERROR,
            "ctypes bitfield values are not supported",
        );
    }
    // A materialised field view owns the conversion rules (arrays and
    // character buffers included); write through it when present.
    if let Some(view) = value::instance_attr(instance, &name) {
        if value::as_instance(view).is_some() {
            return cdata_set_value(&[view, new_value]);
        }
    }
    let tid = match attr_int(field, "_hydro_field_tid") {
        Some(tid) if tid > 0 => tid as TypeId,
        _ => return type_error("CField has no type information"),
    };
    let Some((slot, base_offset)) = instance_slot(instance) else {
        return type_error("ctypes object has no storage");
    };
    let byte_offset = attr_int(field, "byte_offset").unwrap_or(0).max(0) as usize;
    match write_value(
        slot,
        base_offset.saturating_add(byte_offset),
        tid,
        new_value,
    ) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

fn cfield_repr(args: &[i64]) -> i64 {
    let field = args[0];
    let name = cfield_name(field).unwrap_or_else(|_| "?".to_string());
    let type_name = value::instance_attr(field, "type")
        .and_then(value::class_display_name)
        .unwrap_or("ctypes type");
    let byte_offset = attr_int(field, "byte_offset").unwrap_or(0);
    let byte_size = attr_int(field, "byte_size").unwrap_or(0);
    let rendered = if value::instance_attr(field, "is_bitfield").is_some_and(value::truthy) {
        format!(
            "CField(name={name:?}, type={type_name}, byte_offset={byte_offset}, byte_size={byte_size}, bit_size={}, bit_offset={})",
            attr_int(field, "bit_size").unwrap_or(0),
            attr_int(field, "bit_offset").unwrap_or(0),
        )
    } else {
        format!(
            "CField(name={name:?}, type={type_name}, byte_offset={byte_offset}, byte_size={byte_size})"
        )
    };
    value::str_alloc(rendered)
}

// -------------------------------------------------------------------------
// ctypes.wintypes
// -------------------------------------------------------------------------

/// `ctypes.wintypes`: the portable subset of CPython's Windows type aliases.
///
/// The module mirrors `ctypes/wintypes.py`: the scalar, handle, pointer and
/// structure aliases are the same `ctypes` classes CPython builds on every
/// platform. No handle value is created or faked; only type aliases are
/// exposed. `RGB` is the only member that needs a callable and is provided by
/// the pure-Python module when it is imported from the standard library.
fn wintypes_module() -> i64 {
    if let Some(module) = cached("ctypes.module.wintypes") {
        return module;
    }
    let module = value::module_value("ctypes.wintypes".to_string());
    globals::registry_set("ctypes.module.wintypes", module);
    for (name, item) in wintypes_entries() {
        globals::set_attribute_override("ctypes.wintypes", name, item);
    }
    module
}

fn wintypes_struct(name: &str, fields: &[(&str, i64)]) -> i64 {
    let declared: Vec<i64> = fields
        .iter()
        .map(|(field, ty)| value::tuple_new(vec![value::str_alloc((*field).to_string()), *ty]))
        .collect();
    value::class_new_full(
        name.to_string(),
        structure_class(),
        Vec::new(),
        vec![
            (
                "_hydro_kind".to_string(),
                value::str_alloc("struct".to_string()),
            ),
            ("_fields_".to_string(), value::list_new(declared)),
        ],
    )
}

fn wintypes_array(name: &str, elem: i64, len: usize) -> i64 {
    value::class_new_full(
        name.to_string(),
        array_class(),
        Vec::new(),
        vec![
            (
                "_hydro_kind".to_string(),
                value::str_alloc("array".to_string()),
            ),
            ("_type_".to_string(), elem),
            ("_length_".to_string(), encode_len(len)),
        ],
    )
}

/// `ctypes.POINTER(ty)` for a class with a known layout; `None` (with the
/// pending exception dropped) when the pointee cannot be laid out.
fn wintypes_pointer(ty: i64) -> Option<i64> {
    let tid = match type_id_of(ty) {
        Ok(tid) => tid,
        Err(_) => {
            let _ = exc::clear();
            return None;
        }
    };
    let pointer = {
        let mut state = state();
        state.pointer_to(tid)
    };
    Some(class_for_type(pointer))
}

fn wintypes_entries() -> Vec<(&'static str, i64)> {
    let byte = simple_class(Simple::U8);
    let word = simple_class(Simple::U16);
    let dword = simple_class(Simple::U64);
    let char_type = simple_class(Simple::CChar);
    let wchar_type = simple_class(Simple::WChar);
    let uint = simple_class(Simple::U32);
    let int = simple_class(Simple::I32);
    let double = simple_class(Simple::F64);
    let float = simple_class(Simple::F32);
    let long = simple_class(Simple::I64);
    let ulong = simple_class(Simple::U64);
    let ushort = simple_class(Simple::U16);
    let short = simple_class(Simple::I16);
    let longlong = simple_class(Simple::I64);
    let ulonglong = simple_class(Simple::U64);
    let char_p = simple_class(Simple::CharP);
    let wchar_p = simple_class(Simple::WCharP);
    let void_p = simple_class(Simple::VoidP);

    let (wparam, lparam) = {
        let state = state();
        let long_size = state.size_of(state.simple_id(Simple::I64));
        let pointer_size = state.size_of(state.simple_id(Simple::VoidP));
        if long_size == pointer_size {
            (ulong, long)
        } else {
            (ulonglong, longlong)
        }
    };

    let mut entries: Vec<(&'static str, i64)> = vec![
        ("BYTE", byte),
        ("WORD", word),
        ("DWORD", dword),
        ("CHAR", char_type),
        ("WCHAR", wchar_type),
        ("UINT", uint),
        ("INT", int),
        ("DOUBLE", double),
        ("FLOAT", float),
        ("BOOLEAN", byte),
        ("BOOL", long),
        ("ULONG", ulong),
        ("LONG", long),
        ("USHORT", ushort),
        ("SHORT", short),
        ("LARGE_INTEGER", longlong),
        ("ULARGE_INTEGER", ulonglong),
        ("LPCOLESTR", wchar_p),
        ("LPOLESTR", wchar_p),
        ("OLESTR", wchar_p),
        ("LPCWSTR", wchar_p),
        ("LPWSTR", wchar_p),
        ("LPCSTR", char_p),
        ("LPSTR", char_p),
        ("LPCVOID", void_p),
        ("LPVOID", void_p),
        ("WPARAM", wparam),
        ("LPARAM", lparam),
        ("ATOM", word),
        ("LANGID", word),
        ("COLORREF", dword),
        ("LGRPID", dword),
        ("LCTYPE", dword),
        ("LCID", dword),
        ("HANDLE", void_p),
        ("HFILE", int),
        ("HRESULT", long),
    ];
    // Handle aliases: every one is `void *`.
    for name in [
        "HACCEL",
        "HBITMAP",
        "HBRUSH",
        "HCOLORSPACE",
        "HCONV",
        "HCONVLIST",
        "HCURSOR",
        "HDC",
        "HDDEDATA",
        "HDESK",
        "HDROP",
        "HDWP",
        "HENHMETAFILE",
        "HFONT",
        "HGDIOBJ",
        "HGLOBAL",
        "HHOOK",
        "HICON",
        "HINSTANCE",
        "HKEY",
        "HKL",
        "HLOCAL",
        "HMENU",
        "HMETAFILE",
        "HMODULE",
        "HMONITOR",
        "HPALETTE",
        "HPEN",
        "HRGN",
        "HRSRC",
        "HSTR",
        "HSZ",
        "HTASK",
        "HWINSTA",
        "HWND",
        "SC_HANDLE",
        "SERVICE_STATUS_HANDLE",
    ] {
        entries.push((name, void_p));
    }

    let rect = wintypes_struct(
        "RECT",
        &[
            ("left", long),
            ("top", long),
            ("right", long),
            ("bottom", long),
        ],
    );
    let small_rect = wintypes_struct(
        "_SMALL_RECT",
        &[
            ("Left", short),
            ("Top", short),
            ("Right", short),
            ("Bottom", short),
        ],
    );
    let coord = wintypes_struct("_COORD", &[("X", short), ("Y", short)]);
    let point = wintypes_struct("POINT", &[("x", long), ("y", long)]);
    let size = wintypes_struct("SIZE", &[("cx", long), ("cy", long)]);
    let filetime = wintypes_struct(
        "FILETIME",
        &[("dwLowDateTime", dword), ("dwHighDateTime", dword)],
    );
    let msg = wintypes_struct(
        "MSG",
        &[
            ("hWnd", void_p),
            ("message", uint),
            ("wParam", wparam),
            ("lParam", lparam),
            ("time", dword),
            ("pt", point),
        ],
    );
    let find_data_a = wintypes_struct(
        "WIN32_FIND_DATAA",
        &[
            ("dwFileAttributes", dword),
            ("ftCreationTime", filetime),
            ("ftLastAccessTime", filetime),
            ("ftLastWriteTime", filetime),
            ("nFileSizeHigh", dword),
            ("nFileSizeLow", dword),
            ("dwReserved0", dword),
            ("dwReserved1", dword),
            (
                "cFileName",
                wintypes_array("c_char_Array_260", char_type, 260),
            ),
            (
                "cAlternateFileName",
                wintypes_array("c_char_Array_14", char_type, 14),
            ),
        ],
    );
    let find_data_w = wintypes_struct(
        "WIN32_FIND_DATAW",
        &[
            ("dwFileAttributes", dword),
            ("ftCreationTime", filetime),
            ("ftLastAccessTime", filetime),
            ("ftLastWriteTime", filetime),
            ("nFileSizeHigh", dword),
            ("nFileSizeLow", dword),
            ("dwReserved0", dword),
            ("dwReserved1", dword),
            (
                "cFileName",
                wintypes_array("c_wchar_Array_260", wchar_type, 260),
            ),
            (
                "cAlternateFileName",
                wintypes_array("c_wchar_Array_14", wchar_type, 14),
            ),
        ],
    );
    entries.extend([
        ("RECT", rect),
        ("tagRECT", rect),
        ("_RECTL", rect),
        ("RECTL", rect),
        ("_SMALL_RECT", small_rect),
        ("SMALL_RECT", small_rect),
        ("_COORD", coord),
        ("POINT", point),
        ("tagPOINT", point),
        ("_POINTL", point),
        ("POINTL", point),
        ("SIZE", size),
        ("tagSIZE", size),
        ("SIZEL", size),
        ("FILETIME", filetime),
        ("_FILETIME", filetime),
        ("MSG", msg),
        ("tagMSG", msg),
        ("MAX_PATH", encode(260)),
        ("WIN32_FIND_DATAA", find_data_a),
        ("WIN32_FIND_DATAW", find_data_w),
    ]);

    let pointer_groups: &[(&[&str], i64)] = &[
        (&["LPBOOL", "PBOOL"], long),
        (&["PBOOLEAN"], byte),
        (&["LPBYTE", "PBYTE"], byte),
        (&["PCHAR"], char_type),
        (&["LPCOLORREF"], dword),
        (&["LPDWORD", "PDWORD"], dword),
        (&["PFLOAT"], float),
        (&["LPHANDLE", "PHANDLE"], void_p),
        (&["PHKEY"], void_p),
        (&["LPHKL"], void_p),
        (&["LPINT", "PINT"], int),
        (&["PLARGE_INTEGER"], longlong),
        (&["PLCID"], dword),
        (&["LPLONG", "PLONG"], long),
        (&["PSHORT"], short),
        (&["LPUINT", "PUINT"], uint),
        (&["PULARGE_INTEGER"], ulonglong),
        (&["PULONG"], ulong),
        (&["PUSHORT"], ushort),
        (&["PWCHAR"], wchar_type),
        (&["LPWORD", "PWORD"], word),
        (&["LPFILETIME", "PFILETIME"], filetime),
        (&["LPMSG", "PMSG"], msg),
        (&["LPPOINT", "PPOINT"], point),
        (&["PPOINTL"], point),
        (&["LPRECT", "PRECT"], rect),
        (&["LPRECTL", "PRECTL"], rect),
        (&["LPSIZE", "PSIZE"], size),
        (&["LPSIZEL", "PSIZEL"], size),
        (&["PSMALL_RECT"], small_rect),
        (&["LPWIN32_FIND_DATAA", "PWIN32_FIND_DATAA"], find_data_a),
        (&["LPWIN32_FIND_DATAW", "PWIN32_FIND_DATAW"], find_data_w),
        (&["LPSC_HANDLE"], void_p),
    ];
    for (names, ty) in pointer_groups {
        if let Some(pointer) = wintypes_pointer(*ty) {
            for &name in *names {
                entries.push((name, pointer));
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable")
    }

    fn string(text: &str) -> i64 {
        value::str_alloc(text.to_string())
    }

    fn new_cfield(type_class: i64, name: &str) -> i64 {
        let field = value::instance_new(cfield_class());
        let result = call(CFIELD_INIT, &[field, type_class, string(name)]);
        assert_eq!(result, NONE, "CField construction failed");
        assert!(!exc::pending(), "CField construction raised");
        field
    }

    #[test]
    fn errno_round_trips_through_the_thread_slot() {
        exc::init();
        let previous = call("get_errno", &[]);
        assert_eq!(call("set_errno", &[int(32)]), NONE);
        assert_eq!(call("get_errno", &[]), int(32));
        // The copy is ctypes-private: it does not alias the system `errno`.
        #[cfg(target_os = "linux")]
        {
            // SAFETY: writing this thread's errno slot.
            unsafe { *libc::__errno_location() = 7 };
            assert_eq!(call("get_errno", &[]), int(32));
            // A `use_errno=True` call refreshes the copy from the system
            // value (`save_errno` is the post-call hook).
            // SAFETY: writing this thread's errno slot.
            unsafe { *libc::__errno_location() = 42 };
            save_errno();
            assert_eq!(call("get_errno", &[]), int(42));
        }
        assert_eq!(call("set_errno", &[previous]), NONE);
        assert_eq!(call("get_errno", &[]), previous);
        assert!(!exc::pending());
    }

    #[test]
    fn set_errno_rejects_out_of_range_values() {
        exc::init();
        let huge = value::bigint_new(num_bigint::BigInt::from(i64::MAX));
        assert_eq!(call("set_errno", &[huge]), NONE);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), i64::from(OVERFLOW_ERROR));
        exc::clear();
    }

    #[test]
    fn cfield_records_the_declared_layout() {
        exc::init();
        let c_int = constant("c_int");
        let field = new_cfield(c_int, "value");
        assert_eq!(
            value::instance_attr(field, "name").and_then(value::as_str),
            Some("value")
        );
        assert_eq!(value::instance_attr(field, "type"), Some(c_int));
        assert_eq!(attr_int(field, "byte_size"), Some(4));
        assert_eq!(attr_int(field, "byte_offset"), Some(0));
        assert_eq!(attr_int(field, "size"), Some(4));
        assert_eq!(attr_int(field, "offset"), Some(0));
        assert_eq!(attr_int(field, "bit_size"), Some(32));
        assert_eq!(attr_int(field, "bit_offset"), Some(0));
        assert_eq!(attr_int(field, "index"), Some(0));
        assert!(!value::instance_attr(field, "is_bitfield").is_some_and(value::truthy));
        // The class is cached, so `ctypes.CField` is a stable identity.
        assert_eq!(constant("CField"), cfield_class());
        assert!(!exc::pending());
    }

    #[test]
    fn cfield_accepts_the_legacy_type_first_form() {
        exc::init();
        let c_byte = constant("c_byte");
        let field = value::instance_new(cfield_class());
        assert_eq!(call(CFIELD_INIT, &[field, c_byte, string("x")]), NONE);
        assert_eq!(attr_int(field, "byte_size"), Some(1));
        assert_eq!(cfield_name(field).as_deref(), Ok("x"));
        assert!(!exc::pending());
    }

    #[test]
    fn cfield_rejects_a_mismatched_byte_size() {
        exc::init();
        let field = value::instance_new(cfield_class());
        let c_byte = constant("c_byte");
        assert_eq!(
            call(
                CFIELD_INIT,
                &[
                    field,
                    string("a"),
                    c_byte,
                    int(2),
                    int(2),
                    NONE,
                    NONE,
                    int(1)
                ]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        let message = exc::render_exception(exc::pending_value()).expect("message");
        assert!(message.contains("does not match type size"), "{message}");
        exc::clear();
    }

    #[test]
    fn cfield_rejects_an_overflowing_bitfield() {
        exc::init();
        let field = value::instance_new(cfield_class());
        let c_byte = constant("c_byte");
        assert_eq!(
            call(
                CFIELD_INIT,
                &[field, string("x"), c_byte, int(1), int(0), int(7), int(2)]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        let message = exc::render_exception(exc::pending_value()).expect("message");
        assert!(message.contains("overflows its type"), "{message}");
        exc::clear();
    }

    #[test]
    fn cfield_descriptor_reads_and_writes_field_views() {
        exc::init();
        let c_int = constant("c_int");
        let field = new_cfield(c_int, "value");
        let structure = value::instance_new(structure_class());
        let view = call("_cdata_init", &[value::instance_new(c_int)]);
        assert_eq!(call("_cdata_set_value", &[view, int(1)]), NONE);
        value::instance_setattr(structure, "value".to_string(), view);

        // Class access returns the descriptor itself.
        assert_eq!(call(CFIELD_GET, &[field, NONE]), field);
        assert_eq!(call(CFIELD_GET, &[field, NONE, NONE]), field);
        // Instance access returns the materialised field view.
        assert_eq!(call(CFIELD_GET, &[field, structure]), view);
        // Assignment goes through the view's conversion rules.
        assert_eq!(call(CFIELD_SET, &[field, structure, int(7)]), NONE);
        assert_eq!(call("_cdata_get_value", &[view]), int(7));
        assert!(!exc::pending());
    }

    #[test]
    fn cfield_descriptor_rejects_non_ctypes_instances() {
        exc::init();
        let field = new_cfield(constant("c_int"), "value");
        assert_eq!(call(CFIELD_GET, &[field, string("wrong type self")]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::clear();
        assert_eq!(
            call(CFIELD_SET, &[field, string("wrong type self"), int(1)]),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::clear();
    }

    #[test]
    fn cfield_repr_names_the_field_and_its_type() {
        exc::init();
        let field = new_cfield(constant("c_int"), "value");
        let rendered = value::as_str(call(CFIELD_REPR, &[field])).expect("repr");
        assert!(rendered.contains("name=\"value\""), "{rendered}");
        assert!(rendered.contains("c_int"), "{rendered}");
        assert!(!exc::pending());
    }

    #[test]
    fn dlopen_returns_a_real_handle() {
        exc::init();
        // `None` opens the main program, so the handle is a real `dlopen`
        // result rather than a fabricated one.
        let handle = call("dlopen", &[NONE]);
        assert!(handle > 0, "dlopen(None) returned {handle}");
        assert!(!exc::pending());
    }

    #[test]
    fn dlopen_reports_missing_libraries() {
        exc::init();
        assert_eq!(
            call("dlopen", &[string("/nonexistent/hydro-missing.so")]),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(OS_ERROR));
        exc::clear();
    }

    #[cfg(not(windows))]
    #[test]
    fn windows_only_entries_raise_platform_errors() {
        exc::init();
        for (name, args) in [
            ("FreeLibrary", vec![int(1)]),
            ("GetLastError", Vec::new()),
            ("WinError", Vec::new()),
        ] {
            assert_eq!(call(name, &args), NONE, "{name} must raise on non-Windows");
            assert_eq!(exc::pending_tag(), i64::from(OS_ERROR), "{name}");
            let message = exc::render_exception(exc::pending_value()).expect("message");
            assert!(message.contains("only available on Windows"), "{message}");
            exc::clear();
        }
    }

    #[test]
    fn wintypes_exposes_the_portable_aliases() {
        exc::init();
        let module = constant("wintypes");
        let state = value::as_module(module).expect("module value");
        assert_eq!(state.name(), "ctypes.wintypes");
        let dword = crate::modules::module_attr(state, "DWORD");
        assert_eq!(dword, simple_class(Simple::U64));
        let handle = crate::modules::module_attr(state, "HANDLE");
        assert_eq!(handle, simple_class(Simple::VoidP));
        assert_eq!(crate::modules::module_attr(state, "MAX_PATH"), int(260));
        // Pointer aliases are real `POINTER` classes.
        let lpdword = crate::modules::module_attr(state, "LPDWORD");
        assert_eq!(class_kind(lpdword), Some("pointer"));
        // The structure aliases carry their declared fields.
        let rect = crate::modules::module_attr(state, "RECT");
        assert_eq!(class_kind(rect), Some("struct"));
        let long_size =
            value::as_integer(call("sizeof", &[simple_class(Simple::I64)])).expect("c_long size");
        assert_eq!(
            value::as_integer(call("sizeof", &[rect])),
            Some(long_size * 4)
        );
        // Repeated access returns the same module value.
        assert_eq!(constant("wintypes"), module);
        assert!(!exc::pending());
    }
}
