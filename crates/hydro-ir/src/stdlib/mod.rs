//! Static module/function/constant metadata shared by the frontend (type
//! checking) and the runtime (arity/type validation and native function
//! objects). Keeping one table avoids signature drift.

use crate::Type;

pub struct FunctionSpec {
    pub name: &'static str,
    pub min_args: usize,
    pub max_args: usize,
    pub ret: Type,
    /// Parameter names accepted as keyword arguments. Empty means that a
    /// call with keywords is rejected by the frontend. Constructors use
    /// this to support the common Python spelling (`Thread(target=...)`).
    pub params: &'static [&'static str],
    /// The call also accepts keyword names that are not listed in `params`:
    /// `functools.partial(func, *args, **kwargs)` keeps the extra names for
    /// the wrapped callee. Without this flag an unknown keyword name is a
    /// compile-time error.
    pub dynamic_keywords: bool,
}

pub struct ConstantSpec {
    pub name: &'static str,
    pub ty: Type,
}

pub struct ModuleSpec {
    pub name: &'static str,
    pub functions: &'static [FunctionSpec],
    pub constants: &'static [ConstantSpec],
}

pub(crate) const fn f(name: &'static str, min: usize, max: usize, ret: Type) -> FunctionSpec {
    FunctionSpec {
        name,
        min_args: min,
        max_args: max,
        ret,
        params: &[],
        dynamic_keywords: false,
    }
}

/// A function whose parameters may also be passed by keyword.
pub(crate) const fn kw(
    name: &'static str,
    min: usize,
    max: usize,
    ret: Type,
    params: &'static [&'static str],
) -> FunctionSpec {
    FunctionSpec {
        name,
        min_args: min,
        max_args: max,
        ret,
        params,
        dynamic_keywords: false,
    }
}

/// A function that accepts the listed parameters plus arbitrary keyword
/// names it forwards to the callee (`functools.partial`'s `**kwargs`).
pub(crate) const fn kw_dynamic(
    name: &'static str,
    min: usize,
    max: usize,
    ret: Type,
    params: &'static [&'static str],
) -> FunctionSpec {
    FunctionSpec {
        name,
        min_args: min,
        max_args: max,
        ret,
        params,
        dynamic_keywords: true,
    }
}

pub(crate) const fn c(name: &'static str, ty: Type) -> ConstantSpec {
    ConstantSpec { name, ty }
}

mod specs;
pub use specs::*;

pub static MODULES: &[&ModuleSpec] = &[
    &MATH,
    &IO,
    &_IO,
    &FUNCTOOLS,
    &_FUNCTOOLS,
    &OPERATOR,
    &_OPERATOR,
    &BISECT,
    &_BISECT,
    &HEAPQ,
    &_HEAPQ,
    &COLORSYS,
    &KEYWORD,
    &STAT,
    &_STAT,
    &TIME,
    &SYS,
    &BUILTINS,
    &RANDOM,
    &PLATFORM,
    &PATHLIB,
    &STATISTICS,
    &JSON,
    &CSV,
    &OS,
    &OS_PATH,
    &PWD,
    &GRP,
    &RESOURCE,
    &FCNTL,
    &SYSLOG,
    &_SIGNAL,
    &TERMIOS,
    &ARRAY,
    &CMATH,
    &RE_CONSTANTS_MODULE,
    &RE_COMPILER,
    &RE_PARSER,
    &THREAD,
    &THREADING,
    &QUEUE,
    &HTML,
    &RICH_STATUS,
    &MULTIPROCESSING,
    &PICKLE,
    &ARGPARSE,
    &COLLECTIONS,
    &_COLLECTIONS,
    &_CODECS,
    &_IMP,
    &_OPCODE,
    &_STRING,
    &_WEAKREF,
    &RE,
    &SOCKET,
    &_SOCKET,
    &SELECT,
    &SELECTORS,
    &SSL,
    &ASYNCIO,
    &ASYNCIO_COROUTINES,
    &ERRNO,
    &UNITTEST,
    &PYTEST,
    &BINASCII,
    &BASE64,
    &QUOPRI,
    &FNMATCH,
    &GETOPT,
    &MIMETYPES,
    &SUBPROCESS,
    &TMPFILE,
    &SHUTIL,
    &TEXTWRAP,
    &TIMEIT,
    &DATACLASSES,
    &DATETIME,
    &CONTEXTLIB,
    &TYPING,
    &ABC,
    &ANNOTATIONLIB,
    &FUTURE,
    &TYPES,
    &INSPECT,
    &ITERTOOLS,
    &AST,
    &FRACTIONS,
    &IMPORTLIB,
    &IMPORTLIB_UTIL,
    &IMPORTLIB_MACHINERY,
    &IMPORTLIB_METADATA,
    &IMPORTLIB_RESOURCES,
    &WARNINGS,
    &_WARNINGS,
    &MARSHAL,
    &_TOKENIZE,
    &TOKEN,
    &CODECS,
    &_COLLECTIONS_ABC,
    &COLLECTIONS_ABC,
    &GC,
    &ATEXIT,
    &SITE,
    &_CONTEXTVARS,
    &CONTEXTVARS,
    &CTYPES,
    &_CTYPES,
    &LOCALE,
    &SYSCONFIG,
    &ZONEINFO,
    &DECIMAL,
    &_DECIMAL,
    &_STRUCT,
    &STRUCT,
    &UNICODEDATA,
    &ZLIB,
    &MMAP,
    &HTTP,
    &POSTGRES,
    &MEMCACHED,
    // Accelerator / alternative spellings of modules above. Each shares the
    // target's tables (see the alias block in `specs.rs`).
    &_ABC,
    &_DATETIME,
    &_LOCALE,
    &_TYPES,
    &_TYPING,
    &_SRE,
    &POSIX,
    &POSIXPATH,
    &NTPATH,
    &_FROZEN_IMPORTLIB,
    &_FROZEN_IMPORTLIB_EXTERNAL,
    &IMPORTLIB_BOOTSTRAP_EXTERNAL,
    &ZIPIMPORT,
    &_SYSCONFIG,
    &FAULTHANDLER,
    &_SUGGESTIONS,
    &RUNPY,
    &_JSON,
    &_CSV,
    &_PICKLE,
    &_RANDOM,
    &_STATISTICS,
    &_QUEUE,
    &_ASYNCIO,
    &_MULTIPROCESSING,
    &_ZONEINFO,
    &_AST,
    &HASHLIB,
    &_HASHLIB,
    &_SHA1,
    &_SHA2,
    &_MD5,
    &_BLAKE2,
    &_HMAC,
    &_UUID,
    &HYDRO_SIDECAR,
];

pub fn module(name: &str) -> Option<&'static ModuleSpec> {
    MODULES.iter().copied().find(|module| module.name == name)
}

pub fn function(module: &str, name: &str) -> Option<&'static FunctionSpec> {
    self::module(module)?
        .functions
        .iter()
        .find(|function| function.name == name)
}

pub fn constant(module: &str, name: &str) -> Option<&'static ConstantSpec> {
    self::module(module)?
        .constants
        .iter()
        .find(|constant| constant.name == name)
}
