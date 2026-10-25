//! Concrete builtin module tables: functions, arities, return types and
//! constants.

use super::{c, f, kw, kw_dynamic, ConstantSpec, FunctionSpec, ModuleSpec};
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
        // `gcd(*integers)`: CPython accepts any number of integers (zero
        // arguments return 0) and rejects floats.
        f("gcd", 0, usize::MAX, Type::Int),
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
/// accelerator exposes `reduce` and `partial`, while the no-op decorator
/// helpers live in the pure-Python module.
const FUNCTOOLS_FUNCTIONS: &[FunctionSpec] = &[
    f("reduce", 2, 3, Type::Any),
    f("cache", 1, 1, Type::Any),
    kw("lru_cache", 0, 2, Type::Any, &["maxsize", "typed"]),
    f("wraps", 1, 1, Type::Any),
    // `partial(func, *args, **kwargs)`: the extra keyword names are forwarded
    // to the wrapped callee and therefore kept by name.
    kw_dynamic("partial", 1, usize::MAX, Type::Any, &["func"]),
];

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
    f("attrgetter", 1, usize::MAX, Type::Function),
    f("itemgetter", 1, usize::MAX, Type::Function),
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
        // `getrandbits(k)`: a non-negative random int with `k` bits drawn from
        // the global SplitMix64 generator.
        f("getrandbits", 1, 1, Type::Int),
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
        f("processor", 0, 0, Type::Str),
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
        kw(
            "dumps",
            1,
            10,
            Type::Str,
            &[
                "obj",
                "indent",
                "ensure_ascii",
                "separators",
                "sort_keys",
                "skipkeys",
                "check_circular",
                "allow_nan",
                "default",
                "cls",
            ],
        ),
        f("loads", 1, 1, Type::Any),
        kw(
            "dump",
            2,
            6,
            Type::None,
            &[
                "obj",
                "file",
                "indent",
                "ensure_ascii",
                "separators",
                "sort_keys",
            ],
        ),
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
        f("access", 2, 2, Type::Bool),
        f("sysconf", 1, 1, Type::Int),
        f("getloadavg", 0, 0, Type::Tuple),
        // `os.get_terminal_size(fd=1)`: a `(columns, lines)`-style namespace.
        kw("get_terminal_size", 0, 1, Type::Any, &["fd"]),
        // `os.open(path, flags, mode=0o777)` returns the new descriptor.
        f("open", 2, 3, Type::Int),
        f("dup2", 2, 2, Type::Int),
    ],
    constants: &[
        c("name", Type::Str),
        c("sep", Type::Str),
        c("environ", Type::Dict),
        c("PathLike", Type::Any),
        c("devnull", Type::Str),
        c("F_OK", Type::Int),
        c("X_OK", Type::Int),
        c("W_OK", Type::Int),
        c("R_OK", Type::Int),
        // `os.open` flags.
        c("O_RDONLY", Type::Int),
        c("O_WRONLY", Type::Int),
        c("O_RDWR", Type::Int),
        c("O_CREAT", Type::Int),
        c("O_TRUNC", Type::Int),
        c("O_APPEND", Type::Int),
        c("O_CLOEXEC", Type::Int),
    ],
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
        f("_threadbase_init", 1, 4, Type::None),
        f("_threadbase_start", 1, 1, Type::None),
        f("_threadbase_join", 1, 2, Type::None),
        f("_threadbase_is_alive", 1, 1, Type::Bool),
        f("_threadbase_run", 1, 1, Type::None),
    ],
    constants: &[
        // Empty base class; PyX has no per-thread attribute storage.
        c("local", Type::Any),
    ],
};

/// Minimal native `rich.status` shim used by tools that only need the
/// context-manager and update API while the rest of `rich` is loaded from
/// site-packages. Rendering is delegated to the caller's console; the status
/// itself is display-neutral.
/// The two `html` helpers used by HTML exporters. `quote=False` is accepted
/// and disables quote escaping; the four entity escapes match CPython.
pub static HTML: ModuleSpec = ModuleSpec {
    name: "html",
    functions: &[
        kw("escape", 1, 2, Type::Str, &["s", "quote"]),
        f("unescape", 1, 1, Type::Str),
    ],
    constants: &[],
};

pub static RICH_STATUS: ModuleSpec = ModuleSpec {
    name: "rich.status",
    functions: &[
        kw(
            "Status",
            1,
            6,
            Type::Any,
            &[
                "status",
                "console",
                "spinner",
                "spinner_style",
                "speed",
                "refresh_per_second",
            ],
        ),
        f("_status_enter", 1, 1, Type::Any),
        f("_status_exit", 1, 4, Type::None),
        f("_status_start", 1, 1, Type::None),
        f("_status_stop", 1, 1, Type::None),
        f("_status_update", 1, 3, Type::None),
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
    functions: &[f("exit", 0, 1, Type::None)],
    constants: &[
        c("argv", Type::List),
        c("executable", Type::Str),
        c("platform", Type::Str),
        c("version", Type::Str),
        c("version_info", Type::Tuple),
        c("pyx_version", Type::Str),
        c("pyx_version_info", Type::Tuple),
        c("maxsize", Type::Int),
        // Native text streams over the standard descriptors (`read`,
        // `readline`, `write`, `flush`, `isatty`, `fileno`). The static type
        // is dynamic because PyX has no stream class hierarchy.
        // `__stdin__`/`__stdout__`/`__stderr__` are the originals; this
        // runtime never rebinds them, so both spellings resolve alike.
        c("stdin", Type::Any),
        c("stdout", Type::Any),
        c("stderr", Type::Any),
        c("__stdin__", Type::Any),
        c("__stdout__", Type::Any),
        c("__stderr__", Type::Any),
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
    constants: &[
        c("IOBase", Type::Any),
        c("TextIOBase", Type::Any),
        c("BufferedIOBase", Type::Any),
    ],
};

/// `_io`: the accelerator alias of `io`.
pub static _IO: ModuleSpec = ModuleSpec {
    name: "_io",
    functions: IO_FUNCTIONS,
    constants: &[],
};

pub static ARGPARSE: ModuleSpec = ModuleSpec {
    name: "argparse",
    functions: &[
        kw(
            "ArgumentParser",
            0,
            13,
            Type::Any,
            &[
                "prog",
                "usage",
                "description",
                "epilog",
                "parents",
                "formatter_class",
                "prefix_chars",
                "fromfile_prefix_chars",
                "argument_default",
                "conflict_handler",
                "add_help",
                "allow_abbrev",
                "exit_on_error",
            ],
        ),
        // `Namespace` is a class in CPython; exposing it as a native
        // constructor keeps `Namespace(**kwargs)` and `from argparse import
        // Namespace` working without a second object kind.
        kw_dynamic("Namespace", 0, 0, Type::Any, &[]),
        kw(
            "FileType",
            0,
            4,
            Type::Any,
            &["mode", "bufsize", "encoding", "errors"],
        ),
    ],
    constants: &[
        c("SUPPRESS", Type::Any),
        c("HelpFormatter", Type::Any),
        c("RawTextHelpFormatter", Type::Any),
        c("RawDescriptionHelpFormatter", Type::Any),
        c("ArgumentDefaultsHelpFormatter", Type::Any),
        c("MetavarTypeHelpFormatter", Type::Any),
        c("Action", Type::Any),
        c("BooleanOptionalAction", Type::Any),
    ],
};

pub static COLLECTIONS: ModuleSpec = ModuleSpec {
    name: "collections",
    functions: &[kw("defaultdict", 1, 1, Type::Any, &["default_factory"])],
    constants: &[],
};

pub static SOCKET: ModuleSpec = ModuleSpec {
    name: "socket",
    functions: &[
        // The runtime binds these keyword names itself, so the spec keeps the
        // parameter list empty (the frontend still validates the arity).
        kw_dynamic("socket", 0, 3, Type::Any, &[]),
        kw_dynamic("create_connection", 0, 2, Type::Any, &[]),
        f("gethostname", 0, 0, Type::Str),
        f("socketpair", 0, 3, Type::Any),
        f("getaddrinfo", 2, 6, Type::List),
        f("gethostbyname", 1, 1, Type::Str),
        f("inet_pton", 2, 2, Type::Bytes),
        f("inet_ntop", 2, 2, Type::Str),
    ],
    constants: &[
        c("AF_UNSPEC", Type::Int),
        c("AF_UNIX", Type::Int),
        c("AF_INET", Type::Int),
        c("AF_INET6", Type::Int),
        c("SOCK_STREAM", Type::Int),
        c("SOCK_DGRAM", Type::Int),
        c("SOCK_NONBLOCK", Type::Int),
        c("SOCK_CLOEXEC", Type::Int),
        c("SOL_SOCKET", Type::Int),
        c("SO_REUSEADDR", Type::Int),
        c("SO_REUSEPORT", Type::Int),
        c("SO_ERROR", Type::Int),
        c("IPPROTO_IP", Type::Int),
        c("IPPROTO_TCP", Type::Int),
        c("IPPROTO_UDP", Type::Int),
        c("SHUT_RD", Type::Int),
        c("SHUT_WR", Type::Int),
        c("SHUT_RDWR", Type::Int),
        c("MSG_OOB", Type::Int),
        c("MSG_PEEK", Type::Int),
        c("MSG_DONTWAIT", Type::Int),
        c("MSG_WAITALL", Type::Int),
        c("MSG_NOSIGNAL", Type::Int),
    ],
};

/// `select`: readiness on descriptors without external dependencies.
pub static SELECT: ModuleSpec = ModuleSpec {
    name: "select",
    functions: &[
        kw_dynamic("select", 3, 4, Type::Any, &[]),
        f("poll", 0, 0, Type::Any),
        // Native methods of the `poll` object. The receiver is the first
        // argument, exactly as for builtin methods.
        f("_poll_register", 2, 3, Type::Any),
        f("_poll_unregister", 2, 2, Type::Any),
        f("_poll_modify", 3, 3, Type::Any),
        f("_poll_poll", 1, 2, Type::List),
        f("_poll_close", 1, 1, Type::Any),
        f("_poll_enter", 1, 1, Type::Any),
        f("_poll_exit", 1, 4, Type::Any),
    ],
    constants: &[
        c("POLLIN", Type::Int),
        c("POLLPRI", Type::Int),
        c("POLLOUT", Type::Int),
        c("POLLERR", Type::Int),
        c("POLLHUP", Type::Int),
        c("POLLNVAL", Type::Int),
        c("error", Type::Any),
    ],
};

/// `selectors`: the native `DefaultSelector` object and its event bits.
pub static SELECTORS: ModuleSpec = ModuleSpec {
    name: "selectors",
    functions: &[
        f("DefaultSelector", 0, 0, Type::Any),
        f("SelectSelector", 0, 0, Type::Any),
        f("PollSelector", 0, 0, Type::Any),
        f("_selector_register", 2, 4, Type::Any),
        f("_selector_unregister", 2, 2, Type::Any),
        f("_selector_modify", 2, 4, Type::Any),
        f("_selector_select", 1, 2, Type::List),
        f("_selector_get_key", 2, 2, Type::Any),
        f("_selector_get_map", 1, 1, Type::Dict),
        f("_selector_close", 1, 1, Type::Any),
        f("_selector_enter", 1, 1, Type::Any),
        f("_selector_exit", 1, 4, Type::Any),
    ],
    constants: &[c("EVENT_READ", Type::Int), c("EVENT_WRITE", Type::Int)],
};

/// `ssl`: client-side TLS backed by `rustls` and the Mozilla root store.
///
/// Server-side TLS is intentionally not exposed: `SSLContext(
/// PROTOCOL_TLS_SERVER)` can be constructed but `wrap_socket` reports that the
/// mode is unsupported instead of pretending to serve.
pub static SSL: ModuleSpec = ModuleSpec {
    name: "ssl",
    functions: &[
        kw_dynamic("SSLContext", 0, 1, Type::Any, &[]),
        kw_dynamic("create_default_context", 0, 4, Type::Any, &[]),
        kw_dynamic("wrap_socket", 0, 2, Type::Any, &[]),
        f("_ssl_context_wrap_socket", 2, 3, Type::Any),
        f("_ssl_context_load_verify_locations", 1, 4, Type::Any),
        f("_ssl_context_load_cert_chain", 2, 4, Type::Any),
    ],
    constants: &[
        c("CERT_NONE", Type::Int),
        c("CERT_OPTIONAL", Type::Int),
        c("CERT_REQUIRED", Type::Int),
        c("PROTOCOL_TLS", Type::Int),
        c("PROTOCOL_TLS_CLIENT", Type::Int),
        c("PROTOCOL_TLS_SERVER", Type::Int),
        c("PROTOCOL_TLSv1_2", Type::Int),
        c("OP_ALL", Type::Int),
        c("HAS_SNI", Type::Bool),
        c("HAS_TLSv1_3", Type::Bool),
        c("SSLError", Type::Any),
        c("OPENSSL_VERSION", Type::Str),
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
        // `compile(pattern, flags=0)`; the runtime forwards the flags to the
        // regex builder.
        f("compile", 1, 2, Type::Any),
        // `finditer(pattern, string, flags=0)`: every non-overlapping match
        // as a `Match` object in a list. `flags` may be passed by keyword,
        // which rich does (`flags=re.MULTILINE`).
        kw(
            "finditer",
            2,
            3,
            Type::List,
            &["pattern", "string", "flags"],
        ),
    ],
    // The `RegexFlag` members, with CPython's numeric values. They are plain
    // tagged ints: the runtime reads the bits it understands.
    constants: &[
        c("NOFLAG", Type::Int),
        c("TEMPLATE", Type::Int),
        c("T", Type::Int),
        c("IGNORECASE", Type::Int),
        c("I", Type::Int),
        c("LOCALE", Type::Int),
        c("L", Type::Int),
        c("MULTILINE", Type::Int),
        c("M", Type::Int),
        c("DOTALL", Type::Int),
        c("S", Type::Int),
        c("UNICODE", Type::Int),
        c("U", Type::Int),
        c("VERBOSE", Type::Int),
        c("X", Type::Int),
        c("ASCII", Type::Int),
        c("A", Type::Int),
        c("DEBUG", Type::Int),
    ],
};

/// Cooperative asyncio layer over stackful coroutine fibers.
///
/// `run(coro)` drives the root coroutine on a persistent loop,
/// `sleep(seconds)` suspends only the calling fiber, `create_task(coro)`
/// schedules a concurrent task, `current_task()` reports the running task and
/// `gather(*awaitables)` returns the results in input order.
/// `TaskGroup` is a native class whose instances wait for and cancel their
/// tasks at `async with` exit. Unsupported APIs surface as `AttributeError`
/// from the runtime.
pub static ASYNCIO: ModuleSpec = ModuleSpec {
    name: "asyncio",
    functions: &[
        f("run", 1, 1, Type::Any),
        f("sleep", 1, 1, Type::None),
        f("gather", 0, usize::MAX, Type::List),
        f("create_task", 1, 1, Type::Any),
        f("current_task", 0, 0, Type::Any),
        // `TaskGroup` is a native class; the frontend types module members
        // uniformly as functions, so expose the constructor as one. The
        // runtime dispatch arm returns a TaskGroup instance, keeping
        // `from asyncio import TaskGroup; TaskGroup()` callable.
        f("TaskGroup", 0, 0, Type::Any),
    ],
    constants: &[c("CancelledError", Type::Any)],
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

/// `pytest`: the PyX test runner's native support module.
///
/// Decorators (`fixture`, `mark.*`) are recognised and ignored by the
/// frontend, so only the runtime helpers have real implementations. The
/// `_`-prefixed entries are internal hooks used by generated wrappers.
pub static PYTEST: ModuleSpec = ModuleSpec {
    name: "pytest",
    functions: &[
        f("main", 0, 0, Type::None),
        kw("raises", 1, 2, Type::Any, &["expected", "match"]),
        kw("approx", 1, 3, Type::Any, &["expected", "rel", "abs"]),
        kw("fail", 0, 1, Type::None, &["msg"]),
        kw("skip", 0, 1, Type::None, &["reason"]),
        kw("xfail", 0, 1, Type::None, &["reason"]),
        f("fixture", 0, 1, Type::Any),
        f("_fixture_resolve", 1, 1, Type::Any),
        f("_fixture_module", 2, 2, Type::Any),
        f("_xfail", 0, 1, Type::None),
        f("_identity", 0, usize::MAX, Type::Any),
        f("_raises_enter", 1, 1, Type::Any),
        f("_raises_exit", 4, 4, Type::Bool),
        f("_approx_eq", 2, 2, Type::Bool),
    ],
    constants: &[c("mark", Type::Any)],
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

/// `binascii`: conversions between binary data and ASCII encodings.
pub static BINASCII: ModuleSpec = ModuleSpec {
    name: "binascii",
    functions: &[
        kw(
            "hexlify",
            1,
            3,
            Type::Bytes,
            &["data", "sep", "bytes_per_sep"],
        ),
        kw(
            "b2a_hex",
            1,
            3,
            Type::Bytes,
            &["data", "sep", "bytes_per_sep"],
        ),
        f("unhexlify", 1, 1, Type::Bytes),
        f("a2b_hex", 1, 1, Type::Bytes),
        kw("b2a_base64", 1, 2, Type::Bytes, &["data", "newline"]),
        kw("a2b_base64", 1, 2, Type::Bytes, &["data", "strict_mode"]),
        kw("crc32", 1, 2, Type::Int, &["data", "value"]),
        kw("adler32", 1, 2, Type::Int, &["data", "value"]),
    ],
    constants: &[],
};

/// `base64`: RFC 4648 encodings plus the legacy uuencode-style helpers.
pub static BASE64: ModuleSpec = ModuleSpec {
    name: "base64",
    functions: &[
        kw("b64encode", 1, 2, Type::Bytes, &["s", "altchars"]),
        kw(
            "b64decode",
            1,
            3,
            Type::Bytes,
            &["s", "altchars", "validate"],
        ),
        f("standard_b64encode", 1, 1, Type::Bytes),
        f("standard_b64decode", 1, 1, Type::Bytes),
        f("urlsafe_b64encode", 1, 1, Type::Bytes),
        f("urlsafe_b64decode", 1, 1, Type::Bytes),
        f("encodebytes", 1, 1, Type::Bytes),
        f("decodebytes", 1, 1, Type::Bytes),
        f("encodestring", 1, 1, Type::Bytes),
        f("decodestring", 1, 1, Type::Bytes),
        f("b16encode", 1, 1, Type::Bytes),
        kw("b16decode", 1, 2, Type::Bytes, &["s", "casefold"]),
        f("b32encode", 1, 1, Type::Bytes),
        kw("b32decode", 1, 3, Type::Bytes, &["s", "casefold", "map01"]),
    ],
    constants: &[],
};

/// `quopri`: quoted-printable encoding and decoding.
pub static QUOPRI: ModuleSpec = ModuleSpec {
    name: "quopri",
    functions: &[
        kw(
            "encodestring",
            1,
            3,
            Type::Bytes,
            &["data", "quotetabs", "header"],
        ),
        kw("decodestring", 1, 2, Type::Bytes, &["data", "header"]),
        kw(
            "encode",
            3,
            4,
            Type::None,
            &["input", "output", "quotetabs", "header"],
        ),
        kw("decode", 2, 3, Type::None, &["input", "output", "header"]),
    ],
    constants: &[],
};

/// `fnmatch`: Unix shell-style wildcard matching (case-sensitive on Linux).
pub static FNMATCH: ModuleSpec = ModuleSpec {
    name: "fnmatch",
    functions: &[
        f("fnmatch", 2, 2, Type::Bool),
        f("fnmatchcase", 2, 2, Type::Bool),
        f("filter", 2, 2, Type::List),
        f("filterfalse", 2, 2, Type::List),
        f("translate", 1, 1, Type::Str),
    ],
    constants: &[],
};

/// `getopt`: C-style command-line option parsing.
pub static GETOPT: ModuleSpec = ModuleSpec {
    name: "getopt",
    functions: &[
        kw(
            "getopt",
            2,
            3,
            Type::Tuple,
            &["args", "shortopts", "longopts"],
        ),
        kw(
            "gnu_getopt",
            2,
            3,
            Type::Tuple,
            &["args", "shortopts", "longopts"],
        ),
    ],
    constants: &[],
};

/// `mimetypes`: mapping between filenames and MIME types.
pub static MIMETYPES: ModuleSpec = ModuleSpec {
    name: "mimetypes",
    functions: &[
        kw("guess_type", 1, 2, Type::Tuple, &["url", "strict"]),
        kw(
            "guess_all_extensions",
            1,
            2,
            Type::List,
            &["type", "strict"],
        ),
        kw("guess_extension", 1, 2, Type::Str, &["type", "strict"]),
        kw("add_type", 2, 3, Type::None, &["type", "ext", "strict"]),
        kw("init", 0, 1, Type::None, &["files"]),
    ],
    constants: &[
        c("types_map", Type::Dict),
        c("common_types", Type::Dict),
        c("encodings_map", Type::Dict),
        c("suffix_map", Type::Dict),
    ],
};

// -------------------------------------------------------------------------
// Standard library batch B: process/filesystem/text helpers
// -------------------------------------------------------------------------

/// `subprocess`: external command execution.
///
/// `run` returns a `CompletedProcess` namespace and `Popen` a native process
/// object; the `_popen_*` entries are the internal methods of that object and
/// are not part of the Python-visible module API.
pub static SUBPROCESS: ModuleSpec = ModuleSpec {
    name: "subprocess",
    functions: &[
        kw(
            "run",
            1,
            11,
            Type::Any,
            &[
                "args",
                "stdin",
                "input",
                "stdout",
                "stderr",
                "capture_output",
                "text",
                "cwd",
                "timeout",
                "check",
                "env",
            ],
        ),
        // `text=True` switches the captured result from `bytes` to `str`.
        kw(
            "check_output",
            1,
            8,
            Type::Any,
            &[
                "args", "stdin", "input", "stderr", "cwd", "timeout", "text", "env",
            ],
        ),
        kw(
            "Popen",
            1,
            6,
            Type::Any,
            &["args", "cwd", "stdout", "stderr", "text", "env"],
        ),
        f("_popen_wait", 1, 2, Type::Int),
        f("_popen_poll", 1, 1, Type::Int),
        f("_popen_terminate", 1, 1, Type::None),
        f("_popen_kill", 1, 1, Type::None),
    ],
    constants: &[
        c("PIPE", Type::Int),
        c("STDOUT", Type::Int),
        c("DEVNULL", Type::Int),
    ],
};

/// `tempfile`: temporary files and directories.
///
/// `TemporaryDirectory` returns a native object exposing `name`; the
/// `_tempdir_*` entries are its internal context-manager methods.
pub static TMPFILE: ModuleSpec = ModuleSpec {
    name: "tempfile",
    functions: &[
        f("gettempdir", 0, 0, Type::Str),
        kw("mkdtemp", 0, 3, Type::Str, &["suffix", "prefix", "dir"]),
        kw(
            "TemporaryDirectory",
            0,
            3,
            Type::Any,
            &["suffix", "prefix", "dir"],
        ),
        f("_tempdir_enter", 1, 1, Type::Str),
        f("_tempdir_exit", 4, 4, Type::None),
        f("_tempdir_cleanup", 1, 1, Type::None),
    ],
    constants: &[],
};

/// `shutil`: high-level file operations.
pub static SHUTIL: ModuleSpec = ModuleSpec {
    name: "shutil",
    functions: &[
        // Returns the resolved executable path, or `None` when not found.
        kw("which", 1, 3, Type::Any, &["cmd", "mode", "path"]),
        kw("get_terminal_size", 0, 1, Type::Any, &["fallback"]),
        kw("copyfile", 2, 2, Type::Str, &["src", "dst"]),
        kw("copy", 2, 2, Type::Str, &["src", "dst"]),
        kw("copy2", 2, 2, Type::Str, &["src", "dst"]),
        kw("rmtree", 1, 2, Type::None, &["path", "ignore_errors"]),
        kw("move", 2, 2, Type::Str, &["src", "dst"]),
    ],
    constants: &[],
};

/// `textwrap`: text wrapping and indentation helpers.
///
/// The `TextWrapper` class is intentionally not provided; only the
/// module-level convenience functions are available.
pub static TEXTWRAP: ModuleSpec = ModuleSpec {
    name: "textwrap",
    functions: &[
        f("dedent", 1, 1, Type::Str),
        kw("indent", 2, 3, Type::Str, &["text", "prefix", "predicate"]),
        kw("wrap", 1, 2, Type::List, &["text", "width"]),
        kw("fill", 1, 2, Type::Str, &["text", "width"]),
        kw(
            "shorten",
            2,
            3,
            Type::Str,
            &["text", "width", "placeholder"],
        ),
    ],
    constants: &[],
};

/// `timeit`: micro-benchmark timing.
///
/// Statements must be callables in PyX; source strings are rejected by the
/// runtime. `Timer` is a native object whose internal methods are
/// `_timer_timeit`, `_timer_repeat` and `_timer_autorange`.
pub static TIMEIT: ModuleSpec = ModuleSpec {
    name: "timeit",
    functions: &[
        f("default_timer", 0, 0, Type::Float),
        kw(
            "timeit",
            1,
            5,
            Type::Float,
            &["stmt", "setup", "timer", "number", "globals"],
        ),
        kw(
            "repeat",
            1,
            6,
            Type::List,
            &["stmt", "setup", "timer", "repeat", "number", "globals"],
        ),
        kw(
            "Timer",
            1,
            4,
            Type::Any,
            &["stmt", "setup", "timer", "globals"],
        ),
        f("_timer_timeit", 1, 2, Type::Float),
        f("_timer_repeat", 1, 3, Type::List),
        f("_timer_autorange", 1, 2, Type::Tuple),
    ],
    constants: &[],
};

/// `dataclasses`: the decorator is expanded by the frontend, which reads a
/// field's `field(...)` call from the class-body AST. Both names still need a
/// runtime value: `dataclass` is inert, while `field` returns a marker
/// namespace so executing the class body succeeds.
///
/// `replace(obj, **changes)` takes its single positional argument plus
/// arbitrary keyword names, so it forwards every keyword to the runtime
/// ([`kw_dynamic`]); the runtime shallow-copies the instance attributes.
pub static DATACLASSES: ModuleSpec = ModuleSpec {
    name: "dataclasses",
    functions: &[
        kw("field", 0, 2, Type::Any, &["default", "default_factory"]),
        kw_dynamic("replace", 1, 1, Type::Any, &["obj"]),
    ],
    constants: &[c("dataclass", Type::Any)],
};

/// `datetime`: the `datetime` singleton plus the `now`/`strftime` methods the
/// runtime object exposes as native functions.
pub static DATETIME: ModuleSpec = ModuleSpec {
    name: "datetime",
    functions: &[
        f("_datetime_now", 1, 1, Type::Any),
        f("_datetime_strftime", 2, 2, Type::Str),
    ],
    constants: &[
        c("datetime", Type::Any),
        c("MINYEAR", Type::Int),
        c("MAXYEAR", Type::Int),
    ],
};

/// `typing`: annotation placeholders. The runtime values are inert (`None`,
/// `False`, or empty classes); the frontend consumes annotations itself. Class
/// bases (`Generic`, `NamedTuple`, `Protocol`) are exposed as constants so a
/// class definition can subclass them.
pub static TYPING: ModuleSpec = ModuleSpec {
    name: "typing",
    functions: &[
        kw("cast", 2, 2, Type::Any, &["typ", "val"]),
        f("runtime_checkable", 1, 1, Type::Any),
        f("overload", 1, 1, Type::Any),
        f("TypeVar", 1, 3, Type::Any),
        f("NewType", 2, 2, Type::Any),
        f("get_type_hints", 0, 1, Type::Dict),
        f("get_args", 1, 1, Type::Tuple),
    ],
    constants: &[
        c("TYPE_CHECKING", Type::Bool),
        c("Any", Type::Any),
        c("AnyStr", Type::Any),
        c("BinaryIO", Type::Any),
        c("Callable", Type::Any),
        c("ClassVar", Type::Any),
        c("Collection", Type::Any),
        c("ContextManager", Type::Any),
        c("DefaultDict", Type::Any),
        c("Deque", Type::Any),
        c("Dict", Type::Any),
        c("Final", Type::Any),
        c("ForwardRef", Type::Any),
        c("Generator", Type::Any),
        c("Generic", Type::Any),
        c("IO", Type::Any),
        c("Iterable", Type::Any),
        c("Iterator", Type::Any),
        c("List", Type::Any),
        c("Literal", Type::Any),
        c("Mapping", Type::Any),
        c("Match", Type::Any),
        c("NamedTuple", Type::Any),
        c("NoReturn", Type::Any),
        c("Optional", Type::Any),
        c("Pattern", Type::Any),
        c("Protocol", Type::Any),
        c("Sequence", Type::Any),
        c("Set", Type::Any),
        c("TextIO", Type::Any),
        c("Tuple", Type::Any),
        c("Type", Type::Any),
        c("Union", Type::Any),
    ],
};

/// `abc`: the `ABC` base and the `abstractmethod` decorator.
pub static ABC: ModuleSpec = ModuleSpec {
    name: "abc",
    functions: &[f("abstractmethod", 1, 1, Type::Any)],
    constants: &[c("ABC", Type::Any)],
};

/// `enum`: placeholder enum bases and `auto()`.
pub static ENUM: ModuleSpec = ModuleSpec {
    name: "enum",
    functions: &[f("auto", 0, 0, Type::Any)],
    constants: &[c("Enum", Type::Any), c("IntEnum", Type::Any)],
};

/// `__future__`: the compile-time feature flags as booleans. The parser handles
/// the actual feature semantics; the values only serve direct lookups.
pub static FUTURE: ModuleSpec = ModuleSpec {
    name: "__future__",
    functions: &[],
    constants: &[
        c("annotations", Type::Bool),
        c("barry_as_FLUFL", Type::Bool),
        c("division", Type::Bool),
        c("absolute_import", Type::Bool),
        c("print_function", Type::Bool),
        c("generator_stop", Type::Bool),
        c("unicode_literals", Type::Bool),
    ],
};

/// `types`: annotation/`isinstance` placeholders and `SimpleNamespace`.
///
/// `SimpleNamespace` takes one optional mapping; native module calls have no
/// `**kwargs` lowering yet, so the mapping is the runtime spelling of
/// CPython's keyword constructor.
pub static TYPES: ModuleSpec = ModuleSpec {
    name: "types",
    functions: &[f("SimpleNamespace", 0, 1, Type::Any)],
    constants: &[
        c("CodeType", Type::Any),
        c("FrameType", Type::Any),
        c("FunctionType", Type::Any),
        c("GenericAlias", Type::Any),
        c("MappingProxyType", Type::Any),
        c("ModuleType", Type::Any),
        c("NoneType", Type::Any),
        c("TracebackType", Type::Any),
    ],
};

/// `itertools`: combinators materialised as lists (see the runtime module for
/// the documented limitations).
pub static ITERTOOLS: ModuleSpec = ModuleSpec {
    name: "itertools",
    functions: &[
        f("islice", 2, 4, Type::List),
        f("filterfalse", 2, 2, Type::List),
        f("chain", 0, usize::MAX, Type::List),
        f("zip_longest", 0, usize::MAX, Type::List),
        f("count", 0, 2, Type::Any),
        f("repeat", 1, 2, Type::List),
        f("cycle", 1, 1, Type::List),
        f("accumulate", 1, 3, Type::List),
        f("tee", 1, 2, Type::Any),
    ],
    constants: &[],
};

/// `ast`: literal parsing for markup parameters.
pub static AST: ModuleSpec = ModuleSpec {
    name: "ast",
    functions: &[f("literal_eval", 1, 1, Type::Any)],
    constants: &[],
};

/// `fractions`: `Fraction` evaluates to a float quotient in this runtime.
pub static FRACTIONS: ModuleSpec = ModuleSpec {
    name: "fractions",
    functions: &[f("Fraction", 1, 2, Type::Float)],
    constants: &[],
};

/// `importlib`: dynamic imports of native modules and the Unicode-table shim.
pub static IMPORTLIB: ModuleSpec = ModuleSpec {
    name: "importlib",
    functions: &[kw("import_module", 1, 2, Type::Any, &["name", "package"])],
    constants: &[],
};

/// `contextlib`: the `suppress` context manager used by rich/ansi and other
/// libraries. Its `__exit__` never suppresses exceptions in this subset; the
/// common no-error path is supported.
pub static CONTEXTLIB: ModuleSpec = ModuleSpec {
    name: "contextlib",
    functions: &[f("suppress", 0, usize::MAX, Type::Any)],
    constants: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stdlib::{constant, function};

    #[test]
    fn json_keyword_defaults_are_declared() {
        let dumps = function("json", "dumps").expect("json.dumps");
        assert_eq!(dumps.max_args, 10);
        assert_eq!(
            &dumps.params[..5],
            &["obj", "indent", "ensure_ascii", "separators", "sort_keys"][..]
        );
        assert_eq!(dumps.params.last().copied(), Some("cls"));
        let dump = function("json", "dump").expect("json.dump");
        assert_eq!(dump.max_args, 6);
        assert_eq!(dump.params.last().copied(), Some("sort_keys"));
    }

    #[test]
    fn datetime_and_dataclasses_constants_are_registered() {
        assert_eq!(constant("datetime", "MINYEAR").unwrap().ty, Type::Int);
        assert_eq!(constant("datetime", "MAXYEAR").unwrap().ty, Type::Int);
        assert_eq!(constant("datetime", "datetime").unwrap().ty, Type::Any);
        assert_eq!(function("datetime", "_datetime_now").unwrap().max_args, 1);
        assert_eq!(
            function("datetime", "_datetime_strftime").unwrap().min_args,
            2
        );
        assert_eq!(constant("dataclasses", "dataclass").unwrap().ty, Type::Any);
        let field = function("dataclasses", "field").expect("dataclasses.field");
        assert_eq!((field.min_args, field.max_args), (0, 2));
        assert_eq!(
            field.params,
            &["default", "default_factory"][..],
            "field() accepts both marker attributes by name"
        );
        let replace = function("dataclasses", "replace").expect("dataclasses.replace");
        assert_eq!((replace.min_args, replace.max_args), (1, 1));
        assert_eq!(replace.params, &["obj"][..]);
        assert!(
            replace.dynamic_keywords,
            "replace() forwards arbitrary keyword names to the runtime"
        );
    }

    #[test]
    fn os_and_platform_new_entries() {
        assert_eq!(function("os", "sysconf").unwrap().ret, Type::Int);
        assert_eq!(function("os", "getloadavg").unwrap().ret, Type::Tuple);
        assert_eq!(function("platform", "processor").unwrap().ret, Type::Str);
    }

    #[test]
    fn os_console_surface_is_registered() {
        let open = function("os", "open").expect("os.open");
        assert_eq!((open.min_args, open.max_args, open.ret), (2, 3, Type::Int));
        let dup2 = function("os", "dup2").expect("os.dup2");
        assert_eq!((dup2.min_args, dup2.max_args, dup2.ret), (2, 2, Type::Int));
        let size = function("os", "get_terminal_size").expect("os.get_terminal_size");
        assert_eq!((size.min_args, size.max_args), (0, 1));
        assert_eq!(size.params, &["fd"][..]);
        assert_eq!(size.ret, Type::Any);

        assert_eq!(constant("os", "PathLike").unwrap().ty, Type::Any);
        assert_eq!(constant("os", "devnull").unwrap().ty, Type::Str);
        for name in [
            "O_RDONLY",
            "O_WRONLY",
            "O_RDWR",
            "O_CREAT",
            "O_TRUNC",
            "O_APPEND",
            "O_CLOEXEC",
        ] {
            assert_eq!(constant("os", name).unwrap().ty, Type::Int, "{name}");
        }

        assert_eq!(constant("threading", "local").unwrap().ty, Type::Any);
        for name in [
            "stdin",
            "stdout",
            "stderr",
            "__stdin__",
            "__stdout__",
            "__stderr__",
        ] {
            assert_eq!(constant("sys", name).unwrap().ty, Type::Any, "{name}");
        }
    }

    #[test]
    fn typing_surface_is_registered() {
        assert_eq!(
            function("typing", "cast").unwrap().params,
            &["typ", "val"][..]
        );
        assert_eq!(function("typing", "cast").unwrap().min_args, 2);
        assert_eq!(function("typing", "runtime_checkable").unwrap().max_args, 1);
        assert_eq!(function("typing", "overload").unwrap().min_args, 1);
        let typevar = function("typing", "TypeVar").unwrap();
        assert_eq!((typevar.min_args, typevar.max_args), (1, 3));
        assert_eq!(function("typing", "NewType").unwrap().min_args, 2);
        assert_eq!(
            function("typing", "get_type_hints").unwrap().ret,
            Type::Dict
        );
        assert_eq!(constant("typing", "TYPE_CHECKING").unwrap().ty, Type::Bool);
        for name in [
            "Any",
            "Callable",
            "Optional",
            "Union",
            "Dict",
            "Iterable",
            "List",
            "Literal",
            "Mapping",
            "TextIO",
            "Tuple",
            "Type",
            "IO",
            "Sequence",
            "ClassVar",
            "Final",
            "NoReturn",
            "Generic",
            "NamedTuple",
            "Protocol",
        ] {
            assert_eq!(constant("typing", name).unwrap().ty, Type::Any, "{name}");
        }
    }

    #[test]
    fn abc_and_enum_surfaces_are_registered() {
        assert_eq!(constant("abc", "ABC").unwrap().ty, Type::Any);
        assert_eq!(function("abc", "abstractmethod").unwrap().max_args, 1);
        assert_eq!(constant("enum", "Enum").unwrap().ty, Type::Any);
        assert_eq!(constant("enum", "IntEnum").unwrap().ty, Type::Any);
        let auto = function("enum", "auto").unwrap();
        assert_eq!((auto.min_args, auto.max_args), (0, 0));
    }

    #[test]
    fn future_and_types_surfaces_are_registered() {
        for name in [
            "annotations",
            "generator_stop",
            "barry_as_FLUFL",
            "division",
            "absolute_import",
            "print_function",
            "unicode_literals",
        ] {
            assert_eq!(
                constant("__future__", name).unwrap().ty,
                Type::Bool,
                "{name}"
            );
        }
        for name in [
            "FrameType",
            "ModuleType",
            "TracebackType",
            "FunctionType",
            "CodeType",
            "GenericAlias",
            "MappingProxyType",
            "NoneType",
        ] {
            assert_eq!(constant("types", name).unwrap().ty, Type::Any, "{name}");
        }
        let namespace = function("types", "SimpleNamespace").unwrap();
        assert_eq!((namespace.min_args, namespace.max_args), (0, 1));
    }

    #[test]
    fn itertools_surface_is_registered() {
        let islice = function("itertools", "islice").unwrap();
        assert_eq!((islice.min_args, islice.max_args), (2, 4));
        assert_eq!(function("itertools", "filterfalse").unwrap().min_args, 2);
        assert_eq!(function("itertools", "chain").unwrap().max_args, usize::MAX);
        assert_eq!(
            function("itertools", "zip_longest").unwrap().max_args,
            usize::MAX
        );
        assert_eq!(function("itertools", "count").unwrap().max_args, 2);
        assert_eq!(function("itertools", "repeat").unwrap().max_args, 2);
        assert_eq!(function("itertools", "cycle").unwrap().max_args, 1);
        assert_eq!(function("itertools", "accumulate").unwrap().max_args, 3);
        assert_eq!(function("itertools", "tee").unwrap().max_args, 2);
    }

    #[test]
    fn ast_fractions_and_importlib_are_registered() {
        assert_eq!(function("ast", "literal_eval").unwrap().max_args, 1);
        let fraction = function("fractions", "Fraction").unwrap();
        assert_eq!((fraction.min_args, fraction.max_args), (1, 2));
        assert_eq!(fraction.ret, Type::Float);
        let import_module = function("importlib", "import_module").unwrap();
        assert_eq!(import_module.params, &["name", "package"][..]);
    }

    #[test]
    fn functools_and_operator_helpers_are_registered() {
        assert_eq!(function("functools", "cache").unwrap().min_args, 1);
        let lru_cache = function("functools", "lru_cache").unwrap();
        assert_eq!(lru_cache.params, &["maxsize", "typed"][..]);
        assert_eq!(function("functools", "wraps").unwrap().max_args, 1);
        let partial = function("functools", "partial").unwrap();
        assert_eq!(partial.max_args, usize::MAX);
        assert_eq!(partial.params, &["func"][..]);
        assert!(partial.dynamic_keywords);
        assert!(!lru_cache.dynamic_keywords);
        assert_eq!(
            function("operator", "attrgetter").unwrap().max_args,
            usize::MAX
        );
        assert_eq!(
            function("operator", "itemgetter").unwrap().max_args,
            usize::MAX
        );
        assert_eq!(
            function("operator", "attrgetter").unwrap().ret,
            Type::Function
        );
    }

    #[test]
    fn re_flags_are_registered_as_ints() {
        for name in [
            "NOFLAG",
            "TEMPLATE",
            "T",
            "IGNORECASE",
            "I",
            "LOCALE",
            "L",
            "MULTILINE",
            "M",
            "DOTALL",
            "S",
            "UNICODE",
            "U",
            "VERBOSE",
            "X",
            "ASCII",
            "A",
            "DEBUG",
        ] {
            assert_eq!(constant("re", name).expect(name).ty, Type::Int, "{name}");
        }
        // `compile(pattern, flags=0)` takes the optional flag argument.
        let compile = function("re", "compile").expect("re.compile");
        assert_eq!((compile.min_args, compile.max_args), (1, 2));
        let finditer = function("re", "finditer").expect("re.finditer");
        assert_eq!((finditer.min_args, finditer.max_args), (2, 3));
        assert_eq!(finditer.params, &["pattern", "string", "flags"][..]);
        assert_eq!(finditer.ret, Type::List);
    }

    #[test]
    fn math_gcd_is_registered_as_a_variadic_int_function() {
        let gcd = function("math", "gcd").expect("math.gcd");
        assert_eq!((gcd.min_args, gcd.max_args), (0, usize::MAX));
        assert_eq!(gcd.ret, Type::Int);
    }
}
