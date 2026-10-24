//! Concrete builtin module tables: functions, arities, return types and
//! constants.

use super::{c, f, kw, ModuleSpec};
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

pub static FUNCTOOLS: ModuleSpec = ModuleSpec {
    name: "functools",
    functions: &[f("reduce", 2, 3, Type::Any)],
    constants: &[],
};

pub static OPERATOR: ModuleSpec = ModuleSpec {
    name: "operator",
    functions: &[
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
    ],
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
    ],
    constants: &[c("python_implementation", Type::Str)],
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
        c("maxsize", Type::Int),
    ],
};

pub static IO: ModuleSpec = ModuleSpec {
    name: "io",
    functions: &[kw(
        "open",
        1,
        5,
        Type::Any,
        &["file", "mode", "encoding", "newline", "errors"],
    )],
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
