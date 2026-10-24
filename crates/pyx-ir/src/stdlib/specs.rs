//! Concrete builtin module tables: functions, arities, return types and
//! constants.

use super::{c, f, kw, ConstantSpec, FunctionSpec, ModuleSpec};
use crate::Type;

pub static MATH: ModuleSpec = ModuleSpec {
    name: "math",
    functions: &[
        f("sqrt", 1, 1, Type::Float),
        f("sin", 1, 1, Type::Float),
        f("cos", 1, 1, Type::Float),
        f("tan", 1, 1, Type::Float),
        f("asin", 1, 1, Type::Float),
        f("acos", 1, 1, Type::Float),
        f("atan", 1, 1, Type::Float),
        f("exp", 1, 1, Type::Float),
        f("log", 1, 1, Type::Float),
        f("log10", 1, 1, Type::Float),
        f("pow", 2, 2, Type::Float),
        f("floor", 1, 1, Type::Int),
        f("ceil", 1, 1, Type::Int),
        f("trunc", 1, 1, Type::Int),
        f("fabs", 1, 1, Type::Float),
    ],
    constants: &[
        c("pi", Type::Float),
        c("e", Type::Float),
        c("tau", Type::Float),
        c("inf", Type::Float),
        c("nan", Type::Float),
    ],
};

/// `functools` and `_functools` share the same function table: the C
/// accelerator only exposes `reduce`.
const FUNCTOOLS_FUNCTIONS: &[FunctionSpec] = &[f("reduce", 2, 3, Type::Any)];

pub static FUNCTOOLS: ModuleSpec = ModuleSpec {
    name: "functools",
    functions: FUNCTOOLS_FUNCTIONS,
    constants: &[],
};

/// `_functools`: the accelerator alias of `functools`.
pub static _FUNCTOOLS: ModuleSpec = ModuleSpec {
    name: "_functools",
    functions: FUNCTOOLS_FUNCTIONS,
    constants: &[],
};

/// `operator` and `_operator` share the same function table.
const OPERATOR_FUNCTIONS: &[FunctionSpec] = &[
    f("add", 2, 2, Type::Any),
    f("sub", 2, 2, Type::Any),
    f("mul", 2, 2, Type::Any),
    f("truediv", 2, 2, Type::Any),
    f("floordiv", 2, 2, Type::Any),
    f("mod", 2, 2, Type::Any),
    f("pow", 2, 2, Type::Any),
    f("eq", 2, 2, Type::Bool),
    f("ne", 2, 2, Type::Bool),
    f("lt", 2, 2, Type::Bool),
    f("le", 2, 2, Type::Bool),
    f("gt", 2, 2, Type::Bool),
    f("ge", 2, 2, Type::Bool),
    f("neg", 1, 1, Type::Any),
    f("not_", 1, 1, Type::Bool),
    f("truth", 1, 1, Type::Bool),
];

pub static OPERATOR: ModuleSpec = ModuleSpec {
    name: "operator",
    functions: OPERATOR_FUNCTIONS,
    constants: &[],
};

/// `_operator`: the accelerator alias of `operator`.
pub static _OPERATOR: ModuleSpec = ModuleSpec {
    name: "_operator",
    functions: OPERATOR_FUNCTIONS,
    constants: &[],
};

pub static TIME: ModuleSpec = ModuleSpec {
    name: "time",
    functions: &[
        f("time", 0, 0, Type::Float),
        f("perf_counter", 0, 0, Type::Float),
        f("monotonic", 0, 0, Type::Float),
        f("time_ns", 0, 0, Type::Int),
        f("perf_counter_ns", 0, 0, Type::Int),
        f("monotonic_ns", 0, 0, Type::Int),
        f("sleep", 1, 1, Type::None),
    ],
    constants: &[],
};

pub static THREAD: ModuleSpec = ModuleSpec {
    name: "_thread",
    functions: &[
        f("start_new_thread", 2, 2, Type::Int),
        f("join", 1, 1, Type::None),
        f("get_ident", 0, 0, Type::Int),
        f("allocate_lock", 0, 0, Type::Any),
    ],
    constants: &[],
};

pub static RANDOM: ModuleSpec = ModuleSpec {
    name: "random",
    functions: &[
        f("seed", 0, 1, Type::None),
        f("random", 0, 0, Type::Float),
        f("randint", 2, 2, Type::Int),
        f("randrange", 1, 3, Type::Int),
        f("choice", 1, 1, Type::Any),
        f("uniform", 2, 2, Type::Float),
        f("shuffle", 1, 1, Type::None),
        kw("Random", 0, 1, Type::Any, &["seed"]),
    ],
    constants: &[],
};

pub static PATHLIB: ModuleSpec = ModuleSpec {
    name: "pathlib",
    functions: &[
        kw("Path", 0, 1, Type::Any, &["path"]),
        f("cwd", 0, 0, Type::Any),
        f("home", 0, 0, Type::Any),
    ],
    constants: &[],
};

pub static PLATFORM: ModuleSpec = ModuleSpec {
    name: "platform",
    functions: &[
        f("system", 0, 0, Type::Str),
        f("machine", 0, 0, Type::Str),
        f("release", 0, 0, Type::Str),
        f("version", 0, 0, Type::Str),
        f("node", 0, 0, Type::Str),
        f("platform", 0, 0, Type::Str),
        f("python_version", 0, 0, Type::Str),
        f("python_implementation", 0, 0, Type::Str),
    ],
    constants: &[],
};

pub static STATISTICS: ModuleSpec = ModuleSpec {
    name: "statistics",
    functions: &[
        f("mean", 1, 1, Type::Float),
        f("fmean", 1, 1, Type::Float),
        f("median", 1, 1, Type::Float),
        f("pstdev", 1, 1, Type::Float),
        f("stdev", 1, 1, Type::Float),
    ],
    constants: &[],
};

pub static JSON: ModuleSpec = ModuleSpec {
    name: "json",
    functions: &[
        f("dumps", 1, 1, Type::Str),
        f("loads", 1, 1, Type::Any),
        kw("dump", 2, 2, Type::None, &["obj", "file"]),
        kw("load", 1, 1, Type::Any, &["file"]),
    ],
    constants: &[],
};

pub static CSV: ModuleSpec = ModuleSpec {
    name: "csv",
    functions: &[
        kw("reader", 1, 1, Type::Any, &["file"]),
        kw("writer", 1, 1, Type::Any, &["file"]),
        kw("DictReader", 1, 1, Type::Any, &["file"]),
        kw("DictWriter", 2, 2, Type::Any, &["file", "fieldnames"]),
    ],
    constants: &[],
};

pub static OS_PATH: ModuleSpec = ModuleSpec {
    name: "os.path",
    functions: &[
        f("join", 0, usize::MAX, Type::Str),
        f("basename", 1, 1, Type::Str),
        f("dirname", 1, 1, Type::Str),
        f("exists", 1, 1, Type::Bool),
        f("isfile", 1, 1, Type::Bool),
        f("isdir", 1, 1, Type::Bool),
        f("abspath", 1, 1, Type::Str),
        f("splitext", 1, 1, Type::Tuple),
        f("split", 1, 1, Type::Tuple),
    ],
    constants: &[],
};

pub static OS: ModuleSpec = ModuleSpec {
    name: "os",
    functions: &[
        f("getpid", 0, 0, Type::Int),
        f("getppid", 0, 0, Type::Int),
        f("cpu_count", 0, 0, Type::Int),
        f("getcwd", 0, 0, Type::Str),
        f("getenv", 1, 2, Type::Any),
        f("fork", 0, 0, Type::Int),
        f("waitpid", 2, 2, Type::Tuple),
        f("_exit", 1, 1, Type::None),
        f("remove", 1, 1, Type::None),
        f("rename", 2, 2, Type::None),
    ],
    constants: &[c("name", Type::Str), c("sep", Type::Str)],
};

pub static THREADING: ModuleSpec = ModuleSpec {
    name: "threading",
    functions: &[
        kw("Thread", 1, 3, Type::Any, &["target", "args", "name"]),
        f("Lock", 0, 0, Type::Any),
        f("RLock", 0, 0, Type::Any),
        f("Event", 0, 0, Type::Any),
        f("get_ident", 0, 0, Type::Int),
        f("active_count", 0, 0, Type::Int),
    ],
    constants: &[],
};

pub static QUEUE: ModuleSpec = ModuleSpec {
    name: "queue",
    functions: &[kw("Queue", 0, 1, Type::Any, &["maxsize"])],
    constants: &[],
};

pub static MULTIPROCESSING: ModuleSpec = ModuleSpec {
    name: "multiprocessing",
    functions: &[
        kw("Process", 1, 3, Type::Any, &["target", "args", "name"]),
        kw("Queue", 0, 1, Type::Any, &["maxsize"]),
        kw("Pool", 0, 1, Type::Any, &["processes"]),
        f("cpu_count", 0, 0, Type::Int),
        f("get_start_method", 0, 0, Type::Str),
        f("set_start_method", 1, 1, Type::None),
    ],
    constants: &[],
};

pub static PICKLE: ModuleSpec = ModuleSpec {
    name: "pickle",
    functions: &[f("dumps", 1, 1, Type::Str), f("loads", 1, 1, Type::Any)],
    constants: &[],
};

pub static SYS: ModuleSpec = ModuleSpec {
    name: "sys",
    functions: &[],
    constants: &[
        c("argv", Type::List),
        c("platform", Type::Str),
        c("version", Type::Str),
        c("version_info", Type::Tuple),
        c("pyx_version", Type::Str),
        c("pyx_version_info", Type::Tuple),
        c("maxsize", Type::Int),
    ],
};

/// `io` and `_io` share the same function table.
const IO_FUNCTIONS: &[FunctionSpec] = &[kw(
    "open",
    1,
    5,
    Type::Any,
    &["file", "mode", "encoding", "newline", "errors"],
)];

pub static IO: ModuleSpec = ModuleSpec {
    name: "io",
    functions: IO_FUNCTIONS,
    constants: &[],
};

/// `_io`: the accelerator alias of `io`.
pub static _IO: ModuleSpec = ModuleSpec {
    name: "_io",
    functions: IO_FUNCTIONS,
    constants: &[],
};

pub static ARGPARSE: ModuleSpec = ModuleSpec {
    name: "argparse",
    functions: &[kw(
        "ArgumentParser",
        0,
        2,
        Type::Any,
        &["prog", "description"],
    )],
    constants: &[],
};

pub static COLLECTIONS: ModuleSpec = ModuleSpec {
    name: "collections",
    functions: &[kw("defaultdict", 1, 1, Type::Any, &["default_factory"])],
    constants: &[],
};

pub static SOCKET: ModuleSpec = ModuleSpec {
    name: "socket",
    functions: &[
        kw("socket", 0, 3, Type::Any, &["family", "type", "proto"]),
        kw(
            "create_connection",
            1,
            2,
            Type::Any,
            &["address", "timeout"],
        ),
        f("gethostname", 0, 0, Type::Str),
    ],
    constants: &[
        c("AF_INET", Type::Int),
        c("SOCK_STREAM", Type::Int),
        c("SOL_SOCKET", Type::Int),
        c("SO_REUSEADDR", Type::Int),
        c("SHUT_RD", Type::Int),
        c("SHUT_WR", Type::Int),
        c("SHUT_RDWR", Type::Int),
    ],
};

pub static RE: ModuleSpec = ModuleSpec {
    name: "re",
    functions: &[
        f("search", 2, 2, Type::Any),
        f("match", 2, 2, Type::Any),
        f("fullmatch", 2, 2, Type::Any),
        f("findall", 2, 2, Type::List),
        f("sub", 3, 4, Type::Str),
        f("split", 2, 3, Type::List),
        f("escape", 1, 1, Type::Str),
        f("compile", 1, 1, Type::Any),
    ],
    constants: &[],
};

/// Cooperative asyncio layer over stackful coroutine fibers.
///
/// `run(coro)` drives the root coroutine, `sleep(seconds)` suspends only the
/// calling fiber, and `gather(*awaitables)` starts every coroutine as a
/// concurrent task and returns the results in input order. Unsupported APIs
/// surface as `AttributeError` from the runtime.
pub static ASYNCIO: ModuleSpec = ModuleSpec {
    name: "asyncio",
    functions: &[
        f("run", 1, 1, Type::Any),
        f("sleep", 1, 1, Type::None),
        f("gather", 0, usize::MAX, Type::List),
    ],
    constants: &[],
};

/// `errno`: POSIX error numbers and the `errorcode` reverse mapping.
///
/// The runtime resolves every other attribute through
/// `modules::errno::lookup`, which raises `AttributeError` for unknown names.
pub static ERRNO: ModuleSpec = ModuleSpec {
    name: "errno",
    functions: &[],
    constants: &[
        c("EDOM", Type::Int),
        c("ERANGE", Type::Int),
        c("errorcode", Type::Dict),
        c("__dict__", Type::Dict),
    ],
};

/// `unittest`: the minimal test runner used by the compatibility corpus.
///
/// `TestCase` is a native class whose methods are dispatched by the runtime;
/// `main()` runs every registered test case. The `_`-prefixed functions are
/// internal runtime hooks and are not part of the Python-visible API.
pub static UNITTEST: ModuleSpec = ModuleSpec {
    name: "unittest",
    functions: &[
        f("main", 0, 0, Type::None),
        f("_case_marker", 1, 1, Type::None),
        f("_assert_has_attr", 3, 4, Type::None),
        f("_assert_in", 3, 4, Type::None),
        f("_assert_equal", 3, 4, Type::None),
        f("_assert_true", 2, 2, Type::None),
        f("_assert_false", 2, 2, Type::None),
        f("_assert_is", 3, 3, Type::None),
        f("_fail", 1, 2, Type::None),
    ],
    constants: &[c("TestCase", Type::Any)],
};

// -------------------------------------------------------------------------
// Standard library batch A
// -------------------------------------------------------------------------

/// `bisect` and `_bisect` share the same function table. `lo`, `hi` and
/// `key` are optional and may be passed by keyword.
const BISECT_FUNCTIONS: &[FunctionSpec] = &[
    kw(
        "bisect_left",
        2,
        5,
        Type::Int,
        &["a", "x", "lo", "hi", "key"],
    ),
    kw(
        "bisect_right",
        2,
        5,
        Type::Int,
        &["a", "x", "lo", "hi", "key"],
    ),
    kw("bisect", 2, 5, Type::Int, &["a", "x", "lo", "hi", "key"]),
    kw(
        "insort_left",
        2,
        5,
        Type::None,
        &["a", "x", "lo", "hi", "key"],
    ),
    kw(
        "insort_right",
        2,
        5,
        Type::None,
        &["a", "x", "lo", "hi", "key"],
    ),
    kw("insort", 2, 5, Type::None, &["a", "x", "lo", "hi", "key"]),
];

pub static BISECT: ModuleSpec = ModuleSpec {
    name: "bisect",
    functions: BISECT_FUNCTIONS,
    constants: &[],
};

/// `_bisect`: the accelerator alias of `bisect`.
pub static _BISECT: ModuleSpec = ModuleSpec {
    name: "_bisect",
    functions: BISECT_FUNCTIONS,
    constants: &[],
};

/// `heapq` and `_heapq` share the same function table.
const HEAPQ_FUNCTIONS: &[FunctionSpec] = &[
    f("heappush", 2, 2, Type::None),
    f("heappop", 1, 1, Type::Any),
    f("heapify", 1, 1, Type::None),
    f("heappushpop", 2, 2, Type::Any),
    f("heapreplace", 2, 2, Type::Any),
    kw("nlargest", 2, 3, Type::List, &["n", "iterable", "key"]),
    kw("nsmallest", 2, 3, Type::List, &["n", "iterable", "key"]),
];

pub static HEAPQ: ModuleSpec = ModuleSpec {
    name: "heapq",
    functions: HEAPQ_FUNCTIONS,
    constants: &[],
};

/// `_heapq`: the accelerator alias of `heapq`.
pub static _HEAPQ: ModuleSpec = ModuleSpec {
    name: "_heapq",
    functions: HEAPQ_FUNCTIONS,
    constants: &[],
};

/// `colorsys`: conversions between RGB and the YIQ/HLS/HSV colour spaces.
/// Every entry takes three components and returns a three-tuple.
pub static COLORSYS: ModuleSpec = ModuleSpec {
    name: "colorsys",
    functions: &[
        f("rgb_to_yiq", 3, 3, Type::Tuple),
        f("yiq_to_rgb", 3, 3, Type::Tuple),
        f("rgb_to_hls", 3, 3, Type::Tuple),
        f("hls_to_rgb", 3, 3, Type::Tuple),
        f("rgb_to_hsv", 3, 3, Type::Tuple),
        f("hsv_to_rgb", 3, 3, Type::Tuple),
    ],
    constants: &[],
};

/// `keyword`: the interpreter's keyword tables.
pub static KEYWORD: ModuleSpec = ModuleSpec {
    name: "keyword",
    functions: &[
        f("iskeyword", 1, 1, Type::Bool),
        f("issoftkeyword", 1, 1, Type::Bool),
    ],
    constants: &[c("kwlist", Type::List), c("softkwlist", Type::List)],
};

/// `stat` and `_stat` share the same function and constant tables.
const STAT_FUNCTIONS: &[FunctionSpec] = &[
    f("S_IMODE", 1, 1, Type::Int),
    f("S_IFMT", 1, 1, Type::Int),
    f("S_ISDIR", 1, 1, Type::Bool),
    f("S_ISCHR", 1, 1, Type::Bool),
    f("S_ISBLK", 1, 1, Type::Bool),
    f("S_ISREG", 1, 1, Type::Bool),
    f("S_ISFIFO", 1, 1, Type::Bool),
    f("S_ISLNK", 1, 1, Type::Bool),
    f("S_ISSOCK", 1, 1, Type::Bool),
    f("S_ISDOOR", 1, 1, Type::Bool),
    f("S_ISPORT", 1, 1, Type::Bool),
    f("S_ISWHT", 1, 1, Type::Bool),
    f("filemode", 1, 1, Type::Str),
];

const STAT_CONSTANTS: &[ConstantSpec] = &[
    c("FILE_ATTRIBUTE_ARCHIVE", Type::Int),
    c("FILE_ATTRIBUTE_COMPRESSED", Type::Int),
    c("FILE_ATTRIBUTE_DEVICE", Type::Int),
    c("FILE_ATTRIBUTE_DIRECTORY", Type::Int),
    c("FILE_ATTRIBUTE_ENCRYPTED", Type::Int),
    c("FILE_ATTRIBUTE_HIDDEN", Type::Int),
    c("FILE_ATTRIBUTE_INTEGRITY_STREAM", Type::Int),
    c("FILE_ATTRIBUTE_NORMAL", Type::Int),
    c("FILE_ATTRIBUTE_NOT_CONTENT_INDEXED", Type::Int),
    c("FILE_ATTRIBUTE_NO_SCRUB_DATA", Type::Int),
    c("FILE_ATTRIBUTE_OFFLINE", Type::Int),
    c("FILE_ATTRIBUTE_READONLY", Type::Int),
    c("FILE_ATTRIBUTE_REPARSE_POINT", Type::Int),
    c("FILE_ATTRIBUTE_SPARSE_FILE", Type::Int),
    c("FILE_ATTRIBUTE_SYSTEM", Type::Int),
    c("FILE_ATTRIBUTE_TEMPORARY", Type::Int),
    c("FILE_ATTRIBUTE_VIRTUAL", Type::Int),
    c("SF_APPEND", Type::Int),
    c("SF_ARCHIVED", Type::Int),
    c("SF_DATALESS", Type::Int),
    c("SF_FIRMLINK", Type::Int),
    c("SF_IMMUTABLE", Type::Int),
    c("SF_NOUNLINK", Type::Int),
    c("SF_RESTRICTED", Type::Int),
    c("SF_SETTABLE", Type::Int),
    c("SF_SNAPSHOT", Type::Int),
    c("ST_ATIME", Type::Int),
    c("ST_CTIME", Type::Int),
    c("ST_DEV", Type::Int),
    c("ST_GID", Type::Int),
    c("ST_INO", Type::Int),
    c("ST_MODE", Type::Int),
    c("ST_MTIME", Type::Int),
    c("ST_NLINK", Type::Int),
    c("ST_SIZE", Type::Int),
    c("ST_UID", Type::Int),
    c("S_ENFMT", Type::Int),
    c("S_IEXEC", Type::Int),
    c("S_IFBLK", Type::Int),
    c("S_IFCHR", Type::Int),
    c("S_IFDIR", Type::Int),
    c("S_IFDOOR", Type::Int),
    c("S_IFIFO", Type::Int),
    c("S_IFLNK", Type::Int),
    c("S_IFPORT", Type::Int),
    c("S_IFREG", Type::Int),
    c("S_IFSOCK", Type::Int),
    c("S_IFWHT", Type::Int),
    c("S_IREAD", Type::Int),
    c("S_IRGRP", Type::Int),
    c("S_IROTH", Type::Int),
    c("S_IRUSR", Type::Int),
    c("S_IRWXG", Type::Int),
    c("S_IRWXO", Type::Int),
    c("S_IRWXU", Type::Int),
    c("S_ISGID", Type::Int),
    c("S_ISUID", Type::Int),
    c("S_ISVTX", Type::Int),
    c("S_IWGRP", Type::Int),
    c("S_IWOTH", Type::Int),
    c("S_IWRITE", Type::Int),
    c("S_IWUSR", Type::Int),
    c("S_IXGRP", Type::Int),
    c("S_IXOTH", Type::Int),
    c("S_IXUSR", Type::Int),
    c("UF_APPEND", Type::Int),
    c("UF_COMPRESSED", Type::Int),
    c("UF_DATAVAULT", Type::Int),
    c("UF_HIDDEN", Type::Int),
    c("UF_IMMUTABLE", Type::Int),
    c("UF_NODUMP", Type::Int),
    c("UF_NOUNLINK", Type::Int),
    c("UF_OPAQUE", Type::Int),
    c("UF_SETTABLE", Type::Int),
    c("UF_TRACKED", Type::Int),
];

pub static STAT: ModuleSpec = ModuleSpec {
    name: "stat",
    functions: STAT_FUNCTIONS,
    constants: STAT_CONSTANTS,
};

/// `_stat`: the accelerator alias of `stat`.
pub static _STAT: ModuleSpec = ModuleSpec {
    name: "_stat",
    functions: STAT_FUNCTIONS,
    constants: STAT_CONSTANTS,
};
