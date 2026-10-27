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
        f("log2", 1, 1, Type::Float),
        f("pow", 2, 2, Type::Float),
        f("floor", 1, 1, Type::Int),
        f("ceil", 1, 1, Type::Int),
        f("trunc", 1, 1, Type::Int),
        f("fabs", 1, 1, Type::Float),
        // Classification predicates accept any real number and return `bool`.
        f("isfinite", 1, 1, Type::Bool),
        f("isnan", 1, 1, Type::Bool),
        // `isinf(x, /)`: like CPython, exactly one positional-only argument.
        f("isinf", 1, 1, Type::Bool),
        // `isclose(a, b, *, rel_tol=1e-09, abs_tol=0.0)`: both tolerances are
        // keyword-only, so they are not positional slots; the runtime binds
        // them and rejects unknown names.
        kw_dynamic("isclose", 2, 2, Type::Bool, &["a", "b"]),
        // `gcd(*integers)`: CPython accepts any number of integers (zero
        // arguments return 0) and rejects floats.
        f("gcd", 0, usize::MAX, Type::Int),
        // `prod(iterable, *, start=1)`: the result type follows the operands
        // (`int`, promoted `BigInt`, `float`, or a sequence repeated by an
        // integer start), so it stays dynamic. `start` is keyword-only, hence
        // `dynamic_keywords` rather than a positional slot.
        kw_dynamic("prod", 1, 1, Type::Any, &["iterable"]),
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
/// accelerator exposes `reduce` and `partial`, while `wraps`/`update_wrapper`
/// and the pure-Python internals live in the `functools` module.
const FUNCTOOLS_FUNCTIONS: &[FunctionSpec] = &[
    f("reduce", 2, 3, Type::Any),
    f("cache", 1, 1, Type::Any),
    kw("lru_cache", 0, 2, Type::Any, &["maxsize", "typed"]),
    // `wraps(wrapped, assigned=..., updated=...)`: the decorator factory
    // binds `wrapped`/`assigned`/`updated` on a `functools.partial` of the
    // internal `_wraps_apply` helper, whose call form is therefore
    // `_wraps_apply(wrapped, assigned, updated, wrapper)`.
    kw(
        "wraps",
        1,
        3,
        Type::Any,
        &["wrapped", "assigned", "updated"],
    ),
    // `update_wrapper(wrapper, wrapped, assigned=..., updated=...)`: copies
    // the wrapped callable's metadata onto the wrapper and records
    // `wrapper.__wrapped__`. `wraps` applies it through the internal
    // `_wraps_apply` helper, which receives the wrapper last.
    kw(
        "update_wrapper",
        2,
        4,
        Type::Any,
        &["wrapper", "wrapped", "assigned", "updated"],
    ),
    f("_wraps_apply", 4, 4, Type::Any),
    // `partial(func, *args, **kwargs)`: the extra keyword names are forwarded
    // to the wrapped callee and therefore kept by name.
    kw_dynamic("partial", 1, usize::MAX, Type::Any, &["func"]),
    // Pure-Python `functools` internals used by `inspect`: unwrap a
    // `functools.partial` chain / a `partialmethod` descriptor, and resolve
    // the callable behind a `singledispatchmethod` descriptor. The last name
    // is also a real class value at runtime (`isinstance` in `inspect`), while
    // its call form takes `(unbound, obj, cls)`.
    f("_unwrap_partial", 1, 1, Type::Any),
    f("_unwrap_partialmethod", 1, 1, Type::Any),
    f("_singledispatchmethod_get", 1, 3, Type::Any),
    // `cached_property(func)`: a real descriptor class at runtime (see the
    // runtime `functools` module); the module entry builds the same instance
    // for module-call lowering. `func` may also be passed by keyword, exactly
    // like CPython's `__init__(self, func)`, which is why the entry is
    // keyword-dynamic: the module dispatcher rejects every keyword call whose
    // spec is not dynamic. The runtime still validates the name and rejects an
    // unknown one with `__init__`'s `TypeError`.
    kw_dynamic("cached_property", 1, 1, Type::Any, &["func"]),
    // `partialmethod(func, /, *args, **keywords)`: a real descriptor at
    // runtime (`module_get` returns the cached class so `isinstance` works);
    // the module entry builds the same instance for module-call lowering.
    kw_dynamic("partialmethod", 1, usize::MAX, Type::Any, &["func"]),
];

pub static FUNCTOOLS: ModuleSpec = ModuleSpec {
    name: "functools",
    functions: FUNCTOOLS_FUNCTIONS,
    constants: &[
        // `functools.Placeholder` is the singleton `_PlaceholderType`
        // instance that marks an argument slot in `partial`/`partialmethod`.
        c("Placeholder", Type::Any),
        c("_PlaceholderType", Type::Any),
        // Default assignment lists used by `update_wrapper`/`wraps`.
        c("WRAPPER_ASSIGNMENTS", Type::Any),
        c("WRAPPER_UPDATES", Type::Any),
    ],
};

/// `_functools`: the accelerator alias of `functools`.
pub static _FUNCTOOLS: ModuleSpec = ModuleSpec {
    name: "_functools",
    functions: FUNCTOOLS_FUNCTIONS,
    constants: &[
        c("Placeholder", Type::Any),
        c("_PlaceholderType", Type::Any),
    ],
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
    // `operator.or_(a, b)`: bitwise/logical OR (`a | b`).
    f("or_", 2, 2, Type::Any),
    f("neg", 1, 1, Type::Any),
    f("not_", 1, 1, Type::Bool),
    f("truth", 1, 1, Type::Bool),
    // `operator.index(a)`: `a.__index__()`, or `a` itself for exact integers.
    f("index", 1, 1, Type::Int),
    // `operator.getitem(a, b)`: the subscript `a[b]`.
    f("getitem", 2, 2, Type::Any),
    // `operator.setitem(a, b, v)`: the assignment `a[b] = v`, returning `None`.
    f("setitem", 3, 3, Type::None),
    // `operator.delitem(a, b)`: the deletion `del a[b]`, returning `None`.
    f("delitem", 2, 2, Type::None),
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
        // `platform.uname()`: CPython's `uname_result` namedtuple with
        // `system`/`node`/`release`/`version`/`machine`/`processor`.
        f("uname", 0, 0, Type::Any),
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
        f("isabs", 1, 1, Type::Bool),
        f("abspath", 1, 1, Type::Str),
        f("splitext", 1, 1, Type::Tuple),
        f("split", 1, 1, Type::Tuple),
        // `normcase` is the identity on POSIX; `realpath(filename, *,
        // strict=False)` resolves symlinks component by component (the
        // keyword is parsed by the runtime).
        f("normcase", 1, 1, Type::Str),
        kw_dynamic("realpath", 1, 2, Type::Str, &["filename", "strict"]),
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
        // `os.fspath(path)`: str/bytes pass through, other objects are asked
        // for `__fspath__`. The result is str or bytes, which the runtime
        // reports as the dynamic type.
        kw("fspath", 1, 1, Type::Any, &["path"]),
        f("fork", 0, 0, Type::Int),
        f("waitpid", 2, 2, Type::Tuple),
        f("_exit", 1, 1, Type::None),
        f("remove", 1, 1, Type::None),
        f("rename", 2, 2, Type::None),
        f("access", 2, 2, Type::Bool),
        // `os.stat(path, *, dir_fd=None, follow_symlinks=True)`: an
        // `os.stat_result`-shaped namespace with the documented attributes.
        kw(
            "stat",
            1,
            3,
            Type::Any,
            &["path", "dir_fd", "follow_symlinks"],
        ),
        f("sysconf", 1, 1, Type::Int),
        f("getloadavg", 0, 0, Type::Tuple),
        // `os.uname()`: the POSIX identity as a `uname_result`-shaped value
        // with `sysname`/`nodename`/`release`/`version`/`machine`.
        f("uname", 0, 0, Type::Any),
        // `os.get_terminal_size(fd=1)`: a `(columns, lines)`-style namespace.
        kw("get_terminal_size", 0, 1, Type::Any, &["fd"]),
        // `os.open(path, flags, mode=0o777)` returns the new descriptor.
        f("open", 2, 3, Type::Int),
        f("dup2", 2, 2, Type::Int),
        // `os.putenv(name, value, /)` and `os.unsetenv(name, /)`: both are
        // C-level functions in CPython, so their arguments are positional-only
        // and may be str or bytes.
        f("putenv", 2, 2, Type::None),
        f("unsetenv", 1, 1, Type::None),
        // `os.listdir(path=None)`: `path` may be passed by name (CPython's
        // `os.listdir` is a Python wrapper) and `None` means the current
        // directory. Keyword-dynamic so the runtime can bind `path=` instead
        // of rejecting the call when it is not first.
        kw_dynamic("listdir", 0, 1, Type::List, &["path"]),
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
        // Empty base class; Hydro has no per-thread attribute storage.
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

/// `pickle`: the HYD1 textual format. `dumps`/`dump` accept CPython's
/// `protocol` keyword (the format itself is version-independent, so the value
/// is validated but never changes the output), `dump` writes the payload to a
/// file object, and `load` reads it back from one.
pub static PICKLE: ModuleSpec = ModuleSpec {
    name: "pickle",
    functions: &[
        kw("dumps", 1, 2, Type::Str, &["obj", "protocol"]),
        // `dump(obj, file, protocol=None, *, fix_imports=True,
        // buffer_callback=None)`: the two keyword-only parameters are accepted
        // and ignored because HYD1 has no import table or out-of-band buffers.
        kw(
            "dump",
            2,
            5,
            Type::None,
            &["obj", "file", "protocol", "fix_imports", "buffer_callback"],
        ),
        f("loads", 1, 1, Type::Any),
        // `load(file, *, fix_imports=True, encoding="ASCII",
        // errors="strict", buffers=None)`: reads the HYD1 payload back from a
        // file object. The keyword-only options are accepted for signature
        // compatibility; HYD1 carries its own text encoding and has no
        // out-of-band buffers.
        kw(
            "load",
            1,
            5,
            Type::Any,
            &["file", "fix_imports", "encoding", "errors", "buffers"],
        ),
    ],
    constants: &[],
};

pub static SYS: ModuleSpec = ModuleSpec {
    name: "sys",
    functions: &[
        f("exit", 0, 1, Type::None),
        // `sys.exc_info()` returns the `(type, value, traceback)` triple of
        // the exception being handled; `sys.excepthook` is the default hook
        // `sys.excepthook(type, value, traceback)`.
        f("exc_info", 0, 0, Type::Tuple),
        f("excepthook", 3, 3, Type::None),
        // Process-wide recursion limit. `setrecursionlimit` validates the new
        // limit like CPython (integer, above the current recursion depth) and
        // stores it for the whole process.
        f("getrecursionlimit", 0, 0, Type::Int),
        f("setrecursionlimit", 1, 1, Type::None),
        // `sys._getframemodulename(depth=0)` reports the module name of a
        // caller frame, and `sys._getframe(depth=0)` returns that frame.
        // Hydro has no reified frame objects, so the first reports `None`
        // (module metadata is unavailable) and the second raises instead of
        // fabricating a frame.
        kw("_getframemodulename", 0, 1, Type::Any, &["depth"]),
        // `sys._getframe()` rejects keywords in CPython (it is C-level), so
        // this stays positional-only.
        f("_getframe", 0, 1, Type::Any),
    ],
    constants: &[
        c("argv", Type::List),
        // Process-wide importer cache; native modules and path entries may
        // stay absent, but the dict identity is stable.
        c("path_importer_cache", Type::Dict),
        c("executable", Type::Str),
        // "little" or "big": the byte order of the target the program runs on.
        c("byteorder", Type::Str),
        c("platform", Type::Str),
        c("version", Type::Str),
        c("version_info", Type::Tuple),
        // `sys.hexversion`: the packed `0xMMmmppF0` version number.
        c("hexversion", Type::Int),
        // `sys.implementation`: a namespace with `name`, `cache_tag`,
        // `version` and `hexversion`; wheel discovery reads `cache_tag`.
        c("implementation", Type::Any),
        c("hydro_version", Type::Str),
        c("hydro_version_info", Type::Tuple),
        c("maxsize", Type::Int),
        // Native text streams over the standard descriptors (`read`,
        // `readline`, `write`, `flush`, `isatty`, `fileno`). The static type
        // is dynamic because Hydro has no stream class hierarchy.
        // `__stdin__`/`__stdout__`/`__stderr__` are the originals; this
        // runtime never rebinds them, so both spellings resolve alike.
        c("stdin", Type::Any),
        c("stdout", Type::Any),
        c("stderr", Type::Any),
        c("__stdin__", Type::Any),
        c("__stdout__", Type::Any),
        c("__stderr__", Type::Any),
        // Runtime module registry. `path` is the search path reported to user
        // code (observation only: there is no runtime importer that would
        // consult mutations yet), `modules` maps every known module name to
        // its module value, and `builtin_module_names` lists the native
        // modules from `stdlib::MODULES`.
        c("path", Type::List),
        c("modules", Type::Dict),
        c("builtin_module_names", Type::Tuple),
        // Installation prefixes: `base_prefix`/`exec_prefix` describe the
        // Hydro installation, `prefix` the active virtual environment.
        c("prefix", Type::Str),
        c("base_prefix", Type::Str),
        c("exec_prefix", Type::Str),
        // Last uncaught exception. Hydro is a batch compiler with no
        // interactive frame that survives an uncaught exception, so these
        // resolve to `None`. They exist (rather than raising
        // `AttributeError`) because libraries such as `dis` and `code` read
        // them behind `hasattr`.
        c("last_exc", Type::Any),
        c("last_type", Type::Any),
        c("last_value", Type::Any),
        c("last_traceback", Type::Any),
    ],
};

/// `builtins`: the module namespace mirroring interpreter builtins.
///
/// The table covers every name the compiler resolves through
/// `hydro_ir::Builtin::from_name` plus the attribute-only entries
/// (`setattr`, `object`, ...). Runtime dispatch for these functions and
/// constants belongs to the builtins module implementation; this spec makes
/// the names importable and gives native function values their arity.
pub static BUILTINS: ModuleSpec = ModuleSpec {
    name: "builtins",
    functions: &[
        // Names the compiler also makes available unqualified.
        f("print", 0, usize::MAX, Type::None),
        f("len", 1, 1, Type::Int),
        f("str", 0, 3, Type::Str),
        f("repr", 1, 1, Type::Str),
        f("ascii", 1, 1, Type::Str),
        f("hash", 1, 1, Type::Int),
        f("int", 0, 2, Type::Int),
        f("float", 0, 1, Type::Float),
        f("abs", 1, 1, Type::Any),
        f("bool", 0, 1, Type::Bool),
        f("type", 1, 3, Type::Any),
        f("super", 0, 2, Type::Any),
        f("reversed", 1, 1, Type::Any),
        // CPython 3.14 names every parameter of `__import__`, so callers may
        // pass `globals`, `locals`, `fromlist` and `level` by keyword.
        kw(
            "__import__",
            1,
            5,
            Type::Any,
            &["name", "globals", "locals", "fromlist", "level"],
        ),
        f("set", 0, 1, Type::Set),
        f("range", 1, 3, Type::Range),
        f("list", 0, 1, Type::List),
        f("tuple", 0, 1, Type::Tuple),
        f("dict", 0, usize::MAX, Type::Dict),
        f("sum", 1, 2, Type::Any),
        f("enumerate", 1, 2, Type::Any),
        f("zip", 0, usize::MAX, Type::Any),
        f("map", 2, usize::MAX, Type::Any),
        f("filter", 2, 2, Type::Any),
        f("bytes", 0, 3, Type::Bytes),
        f("iter", 1, 2, Type::Any),
        f("next", 1, 2, Type::Any),
        f("frozenset", 0, 1, Type::Set),
        f("getattr", 2, 3, Type::Any),
        f("hasattr", 2, 2, Type::Bool),
        f("max", 1, usize::MAX, Type::Any),
        f("min", 1, usize::MAX, Type::Any),
        f("sorted", 1, 1, Type::List),
        f("round", 1, 2, Type::Any),
        f("format", 2, 2, Type::Str),
        f("isinstance", 2, 2, Type::Bool),
        f("all", 1, 1, Type::Bool),
        f("any", 1, 1, Type::Bool),
        f("ord", 1, 1, Type::Int),
        f("chr", 1, 1, Type::Str),
        f("divmod", 2, 2, Type::Tuple),
        // Integer-base spellings: `hex`, `bin` and `oct` take exactly one
        // integer (including arbitrary-precision values) and return the
        // prefixed literal text.
        f("hex", 1, 1, Type::Str),
        f("bin", 1, 1, Type::Str),
        f("oct", 1, 1, Type::Str),
        f("callable", 1, 1, Type::Bool),
        // Descriptor factories (unqualified compiler builtins). `property`
        // keeps CPython's parameter names; the runtime accepts and does not
        // store the `doc` argument.
        f("classmethod", 1, 1, Type::Any),
        f("staticmethod", 1, 1, Type::Any),
        kw(
            "property",
            1,
            4,
            Type::Any,
            &["fget", "fset", "fdel", "doc"],
        ),
        f("input", 0, 1, Type::Str),
        f("get_ipython", 0, 0, Type::Any),
        // `id` is both a compiler builtin (`Builtin::Id`) and an attribute of
        // the `builtins` module; the runtime dispatches both spellings to the
        // same identity encoding.
        f("id", 1, 1, Type::Int),
        // Attribute-only names (not unqualified compiler builtins).
        f("setattr", 3, 3, Type::None),
        f("delattr", 2, 2, Type::None),
        f("dir", 0, 1, Type::List),
        f("vars", 0, 1, Type::Dict),
        f("globals", 0, 0, Type::Dict),
        f("locals", 0, 0, Type::Dict),
        f("compile", 3, 6, Type::Any),
        f("issubclass", 2, 2, Type::Bool),
        f("open", 1, 5, Type::Any),
    ],
    constants: &[
        // Every exception class the runtime taxonomy knows (plus the IOError
        // alias of OSError). The names resolve to the cached native classes.
        c("BaseException", Type::Any),
        c("Exception", Type::Any),
        c("ArithmeticError", Type::Any),
        c("AssertionError", Type::Any),
        c("AttributeError", Type::Any),
        c("BaseExceptionGroup", Type::Any),
        c("BlockingIOError", Type::Any),
        c("BrokenPipeError", Type::Any),
        c("BufferError", Type::Any),
        c("BytesWarning", Type::Any),
        c("CancelledError", Type::Any),
        c("ChildProcessError", Type::Any),
        c("ConnectionAbortedError", Type::Any),
        c("ConnectionError", Type::Any),
        c("ConnectionRefusedError", Type::Any),
        c("ConnectionResetError", Type::Any),
        c("DeprecationWarning", Type::Any),
        c("EOFError", Type::Any),
        c("ExceptionGroup", Type::Any),
        c("FileExistsError", Type::Any),
        c("FileNotFoundError", Type::Any),
        c("FloatingPointError", Type::Any),
        c("FutureWarning", Type::Any),
        c("GeneratorExit", Type::Any),
        c("ImportError", Type::Any),
        c("ImportWarning", Type::Any),
        c("IndentationError", Type::Any),
        c("IndexError", Type::Any),
        c("InterruptedError", Type::Any),
        c("IOError", Type::Any),
        c("IsADirectoryError", Type::Any),
        c("KeyError", Type::Any),
        c("KeyboardInterrupt", Type::Any),
        c("LookupError", Type::Any),
        c("MemoryError", Type::Any),
        c("ModuleNotFoundError", Type::Any),
        c("NameError", Type::Any),
        c("NotADirectoryError", Type::Any),
        c("NotImplementedError", Type::Any),
        c("OSError", Type::Any),
        c("OverflowError", Type::Any),
        c("PendingDeprecationWarning", Type::Any),
        c("PermissionError", Type::Any),
        c("ProcessLookupError", Type::Any),
        c("RecursionError", Type::Any),
        c("ReferenceError", Type::Any),
        c("ResourceWarning", Type::Any),
        c("RuntimeError", Type::Any),
        c("RuntimeWarning", Type::Any),
        c("Skipped", Type::Any),
        c("StopAsyncIteration", Type::Any),
        c("StopIteration", Type::Any),
        c("SyntaxError", Type::Any),
        c("SyntaxWarning", Type::Any),
        c("SystemError", Type::Any),
        c("SystemExit", Type::Any),
        c("TabError", Type::Any),
        c("TimeoutError", Type::Any),
        c("TypeError", Type::Any),
        c("UnicodeDecodeError", Type::Any),
        c("UnicodeEncodeError", Type::Any),
        c("UnicodeError", Type::Any),
        c("UnicodeTranslateError", Type::Any),
        c("UnicodeWarning", Type::Any),
        c("UserWarning", Type::Any),
        c("ValueError", Type::Any),
        c("Warning", Type::Any),
        c("XFailed", Type::Any),
        c("ZeroDivisionError", Type::Any),
        // Singleton values.
        c("None", Type::None),
        c("True", Type::Bool),
        c("False", Type::Bool),
        c("Ellipsis", Type::Any),
        c("NotImplemented", Type::Any),
        c("__debug__", Type::Bool),
        // Attribute-only builtin types used as values (`isinstance(x, slice)`)
        // and as constructors. The runtime serves the class values.
        c("complex", Type::Any),
        c("slice", Type::Any),
        // `object` is CPython's universal base class. It is a class value, not
        // a native function: `class C(object)`, `isinstance(x, object)` and
        // `object()` all need the runtime class. The bare name resolves like
        // every other `builtins` constant.
        c("object", Type::Any),
    ],
};

/// `io` and `_io` share the same function table.
const IO_FUNCTIONS: &[FunctionSpec] = &[
    kw(
        "open",
        1,
        5,
        Type::Any,
        &["file", "mode", "encoding", "newline", "errors"],
    ),
    // In-memory text and binary streams. Declared as functions so
    // `io.StringIO(...)` lowers as a module call; the runtime value is the
    // cached class (identity, `isinstance` and method binding need a class).
    kw("StringIO", 0, 2, Type::Any, &["initial_value", "newline"]),
    kw("BytesIO", 0, 1, Type::Any, &["initial_value"]),
    // Binary stream classes. `FileIO(file, mode='r', closefd=True,
    // opener=None)` opens a raw binary file; the buffered wrappers take an
    // existing binary stream and expose it with the matching class identity.
    kw(
        "FileIO",
        1,
        4,
        Type::Any,
        &["file", "mode", "closefd", "opener"],
    ),
    kw("BufferedReader", 1, 2, Type::Any, &["raw", "buffer_size"]),
    kw("BufferedWriter", 1, 2, Type::Any, &["raw", "buffer_size"]),
    // `TextIOWrapper(buffer, encoding=None, errors=None, newline=None,
    // line_buffering=False, write_through=False)`: declared as a function so
    // calls lower as module calls; the runtime value is the cached class.
    kw(
        "TextIOWrapper",
        1,
        6,
        Type::Any,
        &[
            "buffer",
            "encoding",
            "errors",
            "newline",
            "line_buffering",
            "write_through",
        ],
    ),
];

pub static IO: ModuleSpec = ModuleSpec {
    name: "io",
    functions: IO_FUNCTIONS,
    constants: &[
        c("IOBase", Type::Any),
        // CPython's raw-stream base; `FileIO` derives from it.
        c("RawIOBase", Type::Any),
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
            14,
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
                "color",
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
    functions: &[
        kw("defaultdict", 1, 1, Type::Any, &["default_factory"]),
        // `OrderedDict(iterable_or_mapping=(), /, **kwargs)`: the parameter is
        // positional-only in CPython, so the keywords are data keys and the
        // dynamic-keyword flag preserves them.
        kw_dynamic("OrderedDict", 0, 1, Type::Any, &[]),
        // `Counter(iterable_or_mapping=None, /, **kwds)`: like `OrderedDict`,
        // `iterable` is positional-only and every keyword becomes a counted
        // element.
        kw_dynamic("Counter", 0, 1, Type::Any, &[]),
        // `namedtuple(typename, field_names, *, rename=False, defaults=None,
        // module=None)` returns a tuple-like class. The declared keywords let
        // the frontend fold them into the constructor's positional slots.
        kw(
            "namedtuple",
            2,
            5,
            Type::Any,
            &["typename", "field_names", "rename", "defaults", "module"],
        ),
    ],
    // `collections.abc` is a nested module; the constant lets the frontend
    // resolve `collections.abc.Mapping` through the runtime module value
    // (the general nested-module chain is still a frontend follow-up).
    constants: &[c("abc", Type::Any)],
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
    constants: &[
        c("CancelledError", Type::Any),
        // `from asyncio import coroutines` resolves the nested module; the
        // attribute value is the native submodule itself.
        c("coroutines", Type::Any),
    ],
};

/// `asyncio.coroutines`: the coroutine-function marker submodule.
///
/// The only member the compatibility corpus reaches is `_is_coroutine`, the
/// identity sentinel assigned to a callable's `_is_coroutine` attribute so
/// `asyncio.iscoroutinefunction` recognises decorated coroutine functions.
/// The runtime materialises one stable object for it.
pub static ASYNCIO_COROUTINES: ModuleSpec = ModuleSpec {
    name: "asyncio.coroutines",
    functions: &[],
    constants: &[c("_is_coroutine", Type::Any)],
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

/// `pytest`: the Hydro test runner's native support module.
///
/// Decorators (`fixture`, `mark.*`) are recognised and ignored by the
/// frontend, so only the runtime helpers have real implementations. The
/// `_`-prefixed entries are internal hooks used by generated wrappers.
pub static PYTEST: ModuleSpec = ModuleSpec {
    name: "pytest",
    functions: &[
        // `pytest.main(*args, **kwargs)` accepts any session argument list.
        // Hydro has no in-process session, so the runtime accepts the call
        // unchanged and rejects it at run time with `NotImplementedError`.
        kw_dynamic("main", 0, usize::MAX, Type::None, &[]),
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
/// Statements must be callables in Hydro; source strings are rejected by the
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
/// `False`, or empty classes) except for the real `TypeVar`/`ParamSpec`/
/// `TypeVarTuple`/`TypeAliasType`/`ForwardRef` classes; the frontend consumes
/// annotations itself. Class bases (`Generic`, `NamedTuple`, `Protocol`) are
/// exposed as constants so a class definition can subclass them.
///
/// `_type_alias_new` is Hydro's lowering target for a PEP 695
/// `type X = expr` statement: the frontend evaluates the value and the
/// runtime builds a real `typing.TypeAliasType` instance from
/// `(name, value, type_params, module)`.
pub static TYPING: ModuleSpec = ModuleSpec {
    name: "typing",
    functions: &[
        kw("cast", 2, 2, Type::Any, &["typ", "val"]),
        // `assert_never(arg, /)`: marks a statically unreachable branch and
        // always raises `AssertionError`, so the call never produces a value.
        f("assert_never", 1, 1, Type::Never),
        f("runtime_checkable", 1, 1, Type::Any),
        f("overload", 1, 1, Type::Any),
        // `TypeVar(name, *constraints, *, bound=None, covariant=False,
        // contravariant=False, infer_variance=False, default=NoDefault,
        // module=None)`: only the name is a named positional parameter; the
        // constraint run and the keyword flags are parsed by the runtime.
        kw_dynamic("TypeVar", 1, usize::MAX, Type::Any, &["name"]),
        f("NewType", 2, 2, Type::Any),
        // `ForwardRef(arg, *, module=None, owner=None, is_argument=True,
        // is_class=False)`: the runtime returns the native class for the
        // attribute value and constructs instances through this call.
        kw_dynamic("ForwardRef", 0, 6, Type::Any, &[]),
        // `get_type_hints(obj, globalns=None, localns=None,
        // include_extras=False, *, format=None)`: all keywords are parsed by
        // the runtime so omitted leading scopes do not shift later ones.
        kw_dynamic("get_type_hints", 0, 5, Type::Dict, &[]),
        kw("_get_defaults", 1, 1, Type::Dict, &["func"]),
        f("get_args", 1, 1, Type::Tuple),
        // Import-time dependencies of `typing_extensions` on Python 3.14:
        // `get_origin` and `final` are assigned unconditionally, and
        // `dataclass_transform` is taken from `typing` since 3.12.
        f("get_origin", 1, 1, Type::Any),
        f("final", 1, 1, Type::Any),
        kw_dynamic("dataclass_transform", 0, 0, Type::Any, &[]),
        // `annotationlib` helpers: keyword names are parsed by the runtime
        // module so `globals=...`/`locals=...` do not have to fill leading
        // positional slots.
        kw_dynamic(
            "_deprecation_warning_for_no_type_params_passed",
            0,
            2,
            Type::None,
            &[],
        ),
        kw_dynamic("evaluate_forward_ref", 0, 7, Type::Any, &[]),
        // PEP 612/646 type-parameter classes. They are real classes at runtime
        // (the module lookup returns the cached class), so construction goes
        // through the class-call path or this module entry with keywords.
        kw_dynamic("ParamSpec", 1, 2, Type::Any, &["name"]),
        kw_dynamic("TypeVarTuple", 1, 2, Type::Any, &["name"]),
        // CPython `typing` internals that `typing_extensions` reaches at
        // import time. `_tp_cache` decorates special-form methods,
        // `_overload_dummy` is the `@overload` result, and `_type_check` is
        // introspected with `inspect.signature` to detect the `module`
        // parameter (which Hydro exposes).
        kw_dynamic("_tp_cache", 0, 1, Type::Any, &["func", "typed"]),
        kw_dynamic("_type_check", 2, 8, Type::Any, &[]),
        kw_dynamic("_type_convert", 1, 3, Type::Any, &[]),
        f("_type_repr", 1, 1, Type::Str),
        kw_dynamic("_eval_type", 1, 9, Type::Any, &[]),
        f("_is_param_expr", 1, 1, Type::Bool),
        kw_dynamic("_deduplicate", 1, 1, Type::Any, &[]),
        f("_generic_class_getitem", 2, 2, Type::Any),
        // PEP 695 lowering target: `type X = expr` becomes
        // `_type_alias_new(name, value, type_params, module)`.
        f("_type_alias_new", 4, 4, Type::Any),
        f("_collect_type_vars", 1, 1, Type::Tuple),
        f("_collect_parameters", 1, 1, Type::Tuple),
        f("_check_generic", 2, 3, Type::None),
        f("_overload_dummy", 0, usize::MAX, Type::Any),
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
        // CPython 3.14 typing surface exercised by `typing_extensions` and
        // `pydantic`. Simple names are inert classes (subclassable and
        // subscriptable, like the placeholders above); `_`-prefixed internals
        // are cached classes so `isinstance`/subclass checks see stable
        // identity.
        c("AsyncContextManager", Type::Any),
        c("AsyncGenerator", Type::Any),
        c("Awaitable", Type::Any),
        c("Coroutine", Type::Any),
        c("AsyncIterable", Type::Any),
        c("AsyncIterator", Type::Any),
        c("OrderedDict", Type::Any),
        c("Counter", Type::Any),
        c("ChainMap", Type::Any),
        c("Text", Type::Any),
        c("Annotated", Type::Any),
        c("Concatenate", Type::Any),
        c("FrozenSet", Type::Any),
        c("Self", Type::Any),
        c("Required", Type::Any),
        c("ReadOnly", Type::Any),
        c("NotRequired", Type::Any),
        c("Never", Type::Any),
        c("LiteralString", Type::Any),
        c("Unpack", Type::Any),
        c("TypeIs", Type::Any),
        c("TypeGuard", Type::Any),
        c("TypeForm", Type::Any),
        c("TypeAlias", Type::Any),
        c("TypeAliasType", Type::Any),
        c("TypedDict", Type::Any),
        c("Doc", Type::Any),
        c("SupportsAbs", Type::Any),
        c("SupportsBytes", Type::Any),
        c("SupportsComplex", Type::Any),
        c("SupportsFloat", Type::Any),
        c("SupportsIndex", Type::Any),
        c("SupportsInt", Type::Any),
        c("SupportsRound", Type::Any),
        c("ParamSpec", Type::Any),
        c("ParamSpecArgs", Type::Any),
        c("ParamSpecKwargs", Type::Any),
        c("TypeVarTuple", Type::Any),
        c("_Final", Type::Any),
        c("_NotIterable", Type::Any),
        c("_SpecialForm", Type::Any),
        c("_SpecialFrom", Type::Any),
        c("_BaseGenericAlias", Type::Any),
        c("_GenericAlias", Type::Any),
        c("_SpecialGenericAlias", Type::Any),
        c("_UnionGenericAlias", Type::Any),
        c("_UnionGenericAliasMeta", Type::Any),
        c("_AnnotatedAlias", Type::Any),
        c("_ConcatenateGenericAlias", Type::Any),
        c("_ProtocolMeta", Type::Any),
        c("_TypedDictMeta", Type::Any),
        c("_TypeVarLikeMeta", Type::Any),
        // `typing.EXCLUDED_ATTRIBUTES` (frozenset) and `_prohibited`
        // (frozenset) are read by `typing_extensions`' protocol/NamedTuple
        // helpers; `_ASSERT_NEVER_REPR_MAX_LENGTH` is an int.
        c("EXCLUDED_ATTRIBUTES", Type::Any),
        c("_prohibited", Type::Any),
        c("_ASSERT_NEVER_REPR_MAX_LENGTH", Type::Int),
    ],
};

/// `abc`: abstract base classes (PEP 3119).
///
/// `ABCMeta` is a real metaclass class (a subclass of the native `type` class)
/// with `register`, `__instancecheck__`, `__subclasscheck__` and
/// `__abstractmethods__`; `ABC` is the mix-in helper whose metaclass is
/// `ABCMeta`. Virtual-subclass registrations are consulted by the runtime
/// `isinstance`/`issubclass` paths. The deprecated decorator classes are
/// importable but raise a tagged `NotImplementedError` when called (use
/// `@classmethod`/`@staticmethod`/`@property` with `@abstractmethod`).
pub static ABC: ModuleSpec = ModuleSpec {
    name: "abc",
    functions: &[
        f("abstractmethod", 1, 1, Type::Any),
        f("get_cache_token", 0, 0, Type::Int),
        f("update_abstractmethods", 1, 1, Type::Any),
        f("abstractclassmethod", 1, 1, Type::Any),
        f("abstractstaticmethod", 1, 1, Type::Any),
        f("abstractproperty", 1, 1, Type::Any),
    ],
    constants: &[c("ABC", Type::Any), c("ABCMeta", Type::Any)],
};

/// `enum`: placeholder enum bases and `auto()`.
pub static ENUM: ModuleSpec = ModuleSpec {
    name: "enum",
    functions: &[f("auto", 0, 0, Type::Any)],
    constants: &[
        c("Enum", Type::Any),
        c("IntEnum", Type::Any),
        c("IntFlag", Type::Any),
    ],
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
    functions: &[
        f("SimpleNamespace", 0, 1, Type::Any),
        // `FunctionType(code, globals, name=None, argdefs=None, closure=None,
        // kwdefaults=None)` builds a callable from code metadata. Declared as
        // a function so calls lower as a module call; the runtime still
        // exposes the class value through `constant` for identity checks.
        kw(
            "FunctionType",
            1,
            6,
            Type::Any,
            &[
                "code",
                "globals",
                "name",
                "argdefs",
                "closure",
                "kwdefaults",
            ],
        ),
        // `DynamicClassAttribute(fget=None, fset=None, fdel=None, doc=None)`:
        // a real data descriptor at runtime (`module_get` returns the cached
        // class, so `isinstance(v, types.DynamicClassAttribute)` holds).
        kw_dynamic(
            "DynamicClassAttribute",
            0,
            4,
            Type::Any,
            &["fget", "fset", "fdel", "doc"],
        ),
    ],
    constants: &[
        c("CodeType", Type::Any),
        c("FrameType", Type::Any),
        c("UnionType", Type::Any),
        c("BuiltinFunctionType", Type::Any),
        c("BuiltinMethodType", Type::Any),
        c("LambdaType", Type::Any),
        c("MethodType", Type::Any),
        c("GeneratorType", Type::Any),
        c("CoroutineType", Type::Any),
        c("AsyncGeneratorType", Type::Any),
        c("EllipsisType", Type::Any),
        c("MethodWrapperType", Type::Any),
        c("WrapperDescriptorType", Type::Any),
        c("MethodDescriptorType", Type::Any),
        c("ClassMethodDescriptorType", Type::Any),
        c("GetSetDescriptorType", Type::Any),
        c("MemberDescriptorType", Type::Any),
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
        f("permutations", 1, 2, Type::List),
        f("combinations", 2, 2, Type::List),
        f("combinations_with_replacement", 2, 2, Type::List),
        // `product(*iterables, repeat=1)`: every positional argument is an
        // iterable and `repeat` is keyword-only, so the runtime binds the
        // keyword itself (`dynamic_keywords`).
        kw_dynamic("product", 0, usize::MAX, Type::List, &[]),
        f("takewhile", 2, 2, Type::List),
    ],
    constants: &[],
};

/// `ast`: literal parsing, the runtime AST node surface used by
/// `annotationlib`, and `parse`/`unparse`.
///
/// Node constructors are declared as functions (so `ast.Name(...)` lowers as a
/// module call) even though `module_get` returns the corresponding native
/// class value: class identity is what `isinstance`/subclassing need.
pub static AST: ModuleSpec = ModuleSpec {
    name: "ast",
    functions: &[
        f("literal_eval", 1, 1, Type::Any),
        // `parse(source, filename='<unknown>', mode='exec', *,
        // type_comments=False, feature_version=None)`; the runtime module
        // parses the keyword names because they are not a fixed leading run.
        kw_dynamic("parse", 1, 5, Type::Any, &[]),
        f("unparse", 1, 1, Type::Str),
        f("fix_missing_locations", 1, 1, Type::Any),
        f("_node_visit", 2, 2, Type::Any),
        f("_node_generic_visit", 2, 2, Type::Any),
        kw_dynamic("Module", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Expression", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Expr", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Name", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Constant", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Attribute", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Subscript", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Call", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("keyword", 0, usize::MAX, Type::Any, &[]),
        // `ast.arg(arg, annotation, type_comment)`: `inspect.parse_name`
        // constructs this node for names it cannot resolve to a parameter.
        kw_dynamic("arg", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Tuple", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("List", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Set", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Dict", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Slice", 0, usize::MAX, Type::Any, &[]),
        // PEP 750 template nodes: `Interpolation(value, str, conversion,
        // format_spec)` and the `TemplateStr` literal node.
        kw_dynamic("Interpolation", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("TemplateStr", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Starred", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("BinOp", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("UnaryOp", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("BoolOp", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Compare", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("IfExp", 0, usize::MAX, Type::Any, &[]),
        // Operator/context node classes: declared as functions so `ast.Add()`
        // compiles; the module lookup returns the class value itself.
        f("Add", 0, 0, Type::Any),
        f("Sub", 0, 0, Type::Any),
        f("Mult", 0, 0, Type::Any),
        f("MatMult", 0, 0, Type::Any),
        f("Div", 0, 0, Type::Any),
        f("Mod", 0, 0, Type::Any),
        f("Pow", 0, 0, Type::Any),
        f("LShift", 0, 0, Type::Any),
        f("RShift", 0, 0, Type::Any),
        f("BitOr", 0, 0, Type::Any),
        f("BitXor", 0, 0, Type::Any),
        f("BitAnd", 0, 0, Type::Any),
        f("FloorDiv", 0, 0, Type::Any),
        f("UAdd", 0, 0, Type::Any),
        f("USub", 0, 0, Type::Any),
        f("Invert", 0, 0, Type::Any),
        f("Not", 0, 0, Type::Any),
        f("And", 0, 0, Type::Any),
        f("Or", 0, 0, Type::Any),
        f("Eq", 0, 0, Type::Any),
        f("NotEq", 0, 0, Type::Any),
        f("Lt", 0, 0, Type::Any),
        f("LtE", 0, 0, Type::Any),
        f("Gt", 0, 0, Type::Any),
        f("GtE", 0, 0, Type::Any),
        f("Is", 0, 0, Type::Any),
        f("IsNot", 0, 0, Type::Any),
        f("In", 0, 0, Type::Any),
        f("NotIn", 0, 0, Type::Any),
        f("Load", 0, 0, Type::Any),
        f("Store", 0, 0, Type::Any),
        f("Del", 0, 0, Type::Any),
    ],
    constants: &[
        c("AST", Type::Any),
        c("NodeVisitor", Type::Any),
        c("NodeTransformer", Type::Any),
        c("mod", Type::Any),
        c("stmt", Type::Any),
        c("expr", Type::Any),
        c("expr_context", Type::Any),
        c("boolop", Type::Any),
        c("operator", Type::Any),
        c("unaryop", Type::Any),
        c("cmpop", Type::Any),
        c("comprehension", Type::Any),
        c("excepthandler", Type::Any),
        c("arguments", Type::Any),
        c("alias", Type::Any),
        c("Interpolation", Type::Any),
        c("TemplateStr", Type::Any),
        c("Add", Type::Any),
        c("Sub", Type::Any),
        c("Mult", Type::Any),
        c("MatMult", Type::Any),
        c("Div", Type::Any),
        c("Mod", Type::Any),
        c("Pow", Type::Any),
        c("LShift", Type::Any),
        c("RShift", Type::Any),
        c("BitOr", Type::Any),
        c("BitXor", Type::Any),
        c("BitAnd", Type::Any),
        c("FloorDiv", Type::Any),
        c("UAdd", Type::Any),
        c("USub", Type::Any),
        c("Invert", Type::Any),
        c("Not", Type::Any),
        c("And", Type::Any),
        c("Or", Type::Any),
        c("Eq", Type::Any),
        c("NotEq", Type::Any),
        c("Lt", Type::Any),
        c("LtE", Type::Any),
        c("Gt", Type::Any),
        c("GtE", Type::Any),
        c("Is", Type::Any),
        c("IsNot", Type::Any),
        c("In", Type::Any),
        c("NotIn", Type::Any),
        c("Load", Type::Any),
        c("Store", Type::Any),
        c("Del", Type::Any),
    ],
};

/// `fractions`: `Fraction` evaluates to a float quotient in this runtime.
pub static FRACTIONS: ModuleSpec = ModuleSpec {
    name: "fractions",
    functions: &[f("Fraction", 1, 2, Type::Float)],
    constants: &[],
};

/// `importlib`: dynamic imports of native modules and the Unicode-table shim.
///
/// The dotted submodules are native Hydro modules, so `importlib.util` and
/// friends never fall through to CPython's package files. They are also
/// exposed as module attributes (`importlib.util`) for code that imports the
/// top-level package and then reaches a submodule.
pub static IMPORTLIB: ModuleSpec = ModuleSpec {
    name: "importlib",
    functions: &[
        kw("import_module", 1, 2, Type::Any, &["name", "package"]),
        kw("reload", 1, 1, Type::Any, &["module"]),
        f("invalidate_caches", 0, 0, Type::None),
    ],
    constants: &[
        c("util", Type::Any),
        c("machinery", Type::Any),
        c("metadata", Type::Any),
        c("resources", Type::Any),
    ],
};

/// `importlib.util`: spec discovery plus the spec/file helpers.
///
/// `spec_from_file_location` follows CPython's parameter order
/// (`name`, `location`, `loader`, `submodule_search_locations`). The optional
/// slots are declared here so keyword calls bind at compile time; the
/// runtime distinguishes an omitted `submodule_search_locations` (a `None`
/// placeholder) from an explicitly supplied list.
pub static IMPORTLIB_UTIL: ModuleSpec = ModuleSpec {
    name: "importlib.util",
    functions: &[
        kw("find_spec", 1, 2, Type::Any, &["name", "package"]),
        kw(
            "spec_from_file_location",
            1,
            4,
            Type::Any,
            &["name", "location", "loader", "submodule_search_locations"],
        ),
        kw("module_from_spec", 1, 1, Type::Any, &["spec"]),
        kw("resolve_name", 2, 2, Type::Str, &["name", "package"]),
        // `decode_source(source_bytes)`: coding-cookie aware source decoding.
        f("decode_source", 1, 1, Type::Str),
        // `LazyLoader` methods. The loader is a native class instance; every
        // method takes the receiver explicitly, matching the class table ABI.
        f("_lazy_init", 2, 2, Type::None),
        f("_lazy_create_module", 2, 2, Type::Any),
        f("_lazy_exec_module", 2, 2, Type::None),
    ],
    constants: &[c("LazyLoader", Type::Any)],
};

/// `importlib.machinery`: loaders and `ModuleSpec`.
///
/// The classes are real native classes so `isinstance` and method binding
/// work; the underscore entries implement their methods (the receiver is the
/// first argument).
pub static IMPORTLIB_MACHINERY: ModuleSpec = ModuleSpec {
    name: "importlib.machinery",
    functions: &[
        f("_spec_init", 3, 6, Type::None),
        f("_source_init", 3, 3, Type::None),
        f("_source_get_filename", 2, 2, Type::Str),
        f("_source_create_module", 2, 2, Type::Any),
        f("_source_exec_module", 2, 2, Type::None),
        f("_extension_init", 3, 3, Type::None),
        f("_extension_get_filename", 2, 2, Type::Str),
        f("_extension_create_module", 2, 2, Type::Any),
        f("_extension_exec_module", 2, 2, Type::None),
        f("_builtin_find_spec", 2, 4, Type::Any),
        f("_builtin_create_module", 2, 2, Type::Any),
        f("_builtin_exec_module", 2, 2, Type::None),
        f("_builtin_invalidate_caches", 1, 1, Type::None),
        f("_frozen_find_spec", 2, 4, Type::Any),
        f("_frozen_create_module", 2, 2, Type::Any),
        f("_frozen_exec_module", 2, 2, Type::None),
        f("_frozen_invalidate_caches", 1, 1, Type::None),
        // `all_suffixes()`: the concatenation of the three suffix lists.
        f("all_suffixes", 0, 0, Type::List),
    ],
    constants: &[
        c("ModuleSpec", Type::Any),
        c("Loader", Type::Any),
        c("SourceFileLoader", Type::Any),
        c("ExtensionFileLoader", Type::Any),
        c("BuiltinImporter", Type::Any),
        c("FrozenImporter", Type::Any),
        // Module-name suffix lists, cached lists at runtime so mutation
        // (`SOURCE_SUFFIXES.append(...)`) and identity behave like CPython.
        c("SOURCE_SUFFIXES", Type::List),
        c("BYTECODE_SUFFIXES", Type::List),
        c("EXTENSION_SUFFIXES", Type::List),
    ],
};

/// `importlib.metadata`: installed-distribution metadata from `.dist-info`
/// directories on `sys.path`.
///
/// `distributions` and `entry_points` accept the CPython keyword filters
/// (`name`, `path`, `group`, ...), so their dispatch takes the raw keyword
/// names.
pub static IMPORTLIB_METADATA: ModuleSpec = ModuleSpec {
    name: "importlib.metadata",
    functions: &[
        kw("version", 1, 1, Type::Str, &["distribution_name"]),
        kw("metadata", 1, 1, Type::Any, &["distribution_name"]),
        kw("requires", 1, 1, Type::Any, &["distribution_name"]),
        kw("distribution", 1, 1, Type::Any, &["distribution_name"]),
        kw_dynamic("distributions", 0, 0, Type::Any, &[]),
        kw_dynamic("entry_points", 0, 0, Type::Any, &[]),
        f("_distribution_read_text", 2, 2, Type::Any),
        f("_entry_point_load", 1, 1, Type::Any),
        f("_package_not_found_str", 1, 1, Type::Str),
    ],
    constants: &[
        c("PackageNotFoundError", Type::Any),
        c("Distribution", Type::Any),
        c("EntryPoint", Type::Any),
    ],
};

/// `importlib.resources`: filesystem-backed `Traversable` access.
///
/// The `_traversable_*` entries implement the `Traversable` methods of the
/// object returned by `files()`; the receiver is the first argument.
pub static IMPORTLIB_RESOURCES: ModuleSpec = ModuleSpec {
    name: "importlib.resources",
    functions: &[
        kw("files", 1, 1, Type::Any, &["package"]),
        f("_traversable_init", 2, 2, Type::None),
        f("_traversable_joinpath", 1, usize::MAX, Type::Any),
        f("_traversable_read_text", 1, 3, Type::Str),
        f("_traversable_read_bytes", 1, 1, Type::Bytes),
        f("_traversable_iterdir", 1, 1, Type::List),
        f("_traversable_is_file", 1, 1, Type::Bool),
        f("_traversable_is_dir", 1, 1, Type::Bool),
        f("_traversable_name", 1, 1, Type::Str),
        f("_traversable_fspath", 1, 1, Type::Str),
        f("_traversable_open", 1, 5, Type::Any),
    ],
    constants: &[c("Traversable", Type::Any)],
};

/// `contextlib`: the `suppress`/`nullcontext` context managers and the
/// `contextmanager` decorator used by rich/ansi, numpy and other libraries.
/// `suppress`'s `__exit__` never suppresses exceptions in this subset; the
/// common no-error path is supported. `nullcontext` is CPython's no-op
/// manager: `__enter__` returns its `enter_result` and `__exit__` returns
/// `False`, so exceptions propagate.
///
/// `contextmanager(func)` returns a callable that invokes `func` and wraps the
/// generator it returns in a real `_GeneratorContextManager`. The wrapper
/// forwards every keyword argument to `func`, so `_contextmanager_call` is
/// declared with dynamic keywords.
pub static CONTEXTLIB: ModuleSpec = ModuleSpec {
    name: "contextlib",
    functions: &[
        f("suppress", 0, usize::MAX, Type::Any),
        kw("nullcontext", 0, 1, Type::Any, &["enter_result"]),
        f("contextmanager", 1, 1, Type::Any),
        // The native methods of the three context-manager classes; the
        // runtime values carry these names, so the central table must know
        // their arities. `_contextmanager_call` receives the decorated
        // generator function as its first (bound) argument plus the call's
        // positional and keyword arguments.
        f("_suppress_enter", 1, 1, Type::Any),
        f("_suppress_exit", 4, 4, Type::None),
        f("_nullcontext_enter", 1, 1, Type::Any),
        f("_nullcontext_exit", 4, 4, Type::Bool),
        kw_dynamic("_contextmanager_call", 1, usize::MAX, Type::Any, &[]),
        f("_contextmanager_enter", 1, 1, Type::Any),
        f("_contextmanager_exit", 4, 4, Type::Bool),
    ],
    constants: &[],
};

/// `_collections`: accelerator alias table for `collections`. `defaultdict`,
/// `OrderedDict` and `Counter` are implemented; `deque` is deliberately absent
/// instead of being faked.
pub static _COLLECTIONS: ModuleSpec = ModuleSpec {
    name: "_collections",
    functions: &[
        kw("defaultdict", 1, 1, Type::Any, &["default_factory"]),
        kw_dynamic("OrderedDict", 0, 1, Type::Any, &[]),
        kw_dynamic("Counter", 0, 1, Type::Any, &[]),
    ],
    constants: &[],
};

/// `_codecs`: the built-in codecs plus the codec and error-handler registries.
pub static _CODECS: ModuleSpec = ModuleSpec {
    name: "_codecs",
    functions: &[
        f("encode", 1, 3, Type::Bytes),
        f("decode", 1, 3, Type::Str),
        f("utf_8_encode", 1, 1, Type::Tuple),
        f("utf_8_decode", 1, 3, Type::Tuple),
        f("ascii_encode", 1, 2, Type::Tuple),
        f("ascii_decode", 1, 2, Type::Tuple),
        f("latin_1_encode", 1, 2, Type::Tuple),
        f("latin_1_decode", 1, 2, Type::Tuple),
        f("escape_encode", 1, 2, Type::Tuple),
        f("escape_decode", 1, 2, Type::Tuple),
        f("lookup", 1, 1, Type::Any),
        f("register", 1, 1, Type::None),
        f("unregister", 1, 1, Type::None),
        f("register_error", 2, 2, Type::None),
        f("lookup_error", 1, 1, Type::Any),
        // The handler functions returned by `lookup_error` for the built-in
        // names; exposed so native function values stay dispatchable.
        f("strict_errors", 1, 1, Type::Any),
        f("ignore_errors", 1, 1, Type::Any),
        f("replace_errors", 1, 1, Type::Any),
    ],
    constants: &[],
};

/// `codecs`: the pure-Python wrapper surface over the native `_codecs`
/// registry. `lookup` is the same native implementation the accelerator
/// exports; the byte-order marks carry their real CPython values.
pub static CODECS: ModuleSpec = ModuleSpec {
    name: "codecs",
    functions: &[f("lookup", 1, 1, Type::Any)],
    constants: &[
        c("BOM_UTF8", Type::Bytes),
        c("BOM_UTF16_LE", Type::Bytes),
        c("BOM_UTF16_BE", Type::Bytes),
        c("BOM_UTF32_LE", Type::Bytes),
        c("BOM_UTF32_BE", Type::Bytes),
        c("BOM_LE", Type::Bytes),
        c("BOM_BE", Type::Bytes),
        c("BOM", Type::Bytes),
    ],
};

/// `_imp`: loader queries. All names are declared as functions; unsupported
/// operations raise tagged exceptions at runtime.
pub static _IMP: ModuleSpec = ModuleSpec {
    name: "_imp",
    functions: &[
        f("acquire_lock", 0, 0, Type::None),
        f("release_lock", 0, 0, Type::None),
        f("is_builtin", 1, 1, Type::Bool),
        f("is_frozen", 1, 1, Type::Bool),
        f("is_frozen_package", 1, 1, Type::Bool),
        f("extension_suffixes", 0, 0, Type::List),
        f("create_builtin", 1, 1, Type::Any),
        f("exec_builtin", 1, 1, Type::Any),
        f("find_frozen", 1, 1, Type::Any),
        f("get_frozen_object", 1, 2, Type::Any),
        f("create_dynamic", 1, 1, Type::Any),
        f("exec_dynamic", 1, 1, Type::Any),
        f("_fix_co_filename", 2, 3, Type::None),
        f("source_hash", 2, 2, Type::Int),
    ],
    constants: &[],
};

/// `_opcode`: truthful introspection for a bytecode-less runtime.
///
/// The predicates answer `False` for every opcode (this runtime has none) and
/// the operations that would need CPython bytecode (`stack_effect`,
/// `get_executor`, `get_specialization_stats`) raise `NotImplementedError`.
/// The intrinsic/specialization metadata helpers return empty collections.
/// `opmap`/`opname` are not part of `_opcode` (CPython 3.14 keeps them in
/// `_opcode_metadata`).
pub static _OPCODE: ModuleSpec = ModuleSpec {
    name: "_opcode",
    functions: &[
        kw(
            "stack_effect",
            1,
            3,
            Type::Int,
            &["opcode", "oparg", "jump"],
        ),
        f("has_arg", 1, 1, Type::Bool),
        f("has_const", 1, 1, Type::Bool),
        f("has_name", 1, 1, Type::Bool),
        f("has_jump", 1, 1, Type::Bool),
        f("has_free", 1, 1, Type::Bool),
        f("has_local", 1, 1, Type::Bool),
        f("has_exc", 1, 1, Type::Bool),
        f("is_valid", 1, 1, Type::Bool),
        f("get_executor", 2, 2, Type::Any),
        f("get_specialization_stats", 0, 0, Type::Dict),
        f("get_intrinsic1_descs", 0, 0, Type::List),
        f("get_intrinsic2_descs", 0, 0, Type::List),
        f("get_special_method_names", 0, 0, Type::Dict),
        f("get_nb_ops", 0, 0, Type::List),
    ],
    constants: &[
        c("ENABLE_SPECIALIZATION", Type::Int),
        c("ENABLE_SPECIALIZATION_FT", Type::Int),
    ],
};

/// `_string`: the format-string parser used by `string.Formatter`.
pub static _STRING: ModuleSpec = ModuleSpec {
    name: "_string",
    functions: &[
        f("formatter_parser", 1, 1, Type::Any),
        f("formatter_field_name_split", 1, 1, Type::Tuple),
    ],
    constants: &[],
};

/// `_weakref`: non-owning references. The pure-Python `weakref` module is not
/// registered natively; it imports these names.
pub static _WEAKREF: ModuleSpec = ModuleSpec {
    name: "_weakref",
    functions: &[
        f("proxy", 1, 2, Type::Any),
        f("getweakrefcount", 1, 1, Type::Int),
        f("getweakrefs", 1, 1, Type::List),
        f("_remove_dead_weakref", 2, 2, Type::None),
        // Native method bodies installed on ReferenceType.
        f("_weakref_ref_init", 2, 3, Type::None),
        f("_weakref_ref_call", 1, 1, Type::Any),
        f("_weakref_ref_eq", 2, 2, Type::Bool),
        f("_weakref_ref_ne", 2, 2, Type::Bool),
        f("_weakref_ref_hash", 1, 1, Type::Int),
        f("_weakref_ref_repr", 1, 1, Type::Str),
        // Native method bodies installed on ProxyType/CallableProxyType.
        f("_weakref_proxy_init", 2, 3, Type::None),
        f("_weakref_proxy_eq", 2, 2, Type::Bool),
        f("_weakref_proxy_ne", 2, 2, Type::Bool),
        f("_weakref_proxy_repr", 1, 1, Type::Str),
        f("_weakref_proxy_call", 1, usize::MAX, Type::Any),
    ],
    constants: &[
        // `ref` is the ReferenceType class value, not a function.
        c("ref", Type::Any),
        c("ReferenceType", Type::Any),
        c("ProxyType", Type::Any),
        c("CallableProxyType", Type::Any),
        c("ProxyTypes", Type::Tuple),
    ],
};

/// `warnings`: the warning registry, filters and display helpers. Every
/// implemented function is declared here; a name that is absent from this
/// table raises `AttributeError` instead of returning invented data.
const WARNING_FUNCTIONS: &[FunctionSpec] = &[
    // `warn(message, category=None, stacklevel=1, *, skip_file_prefixes=())`:
    // the three positional-or-keyword parameters keep their slots, while the
    // keyword-only `skip_file_prefixes` is bound by the runtime (`dynamic`
    // keywords), which validates the tuple and rejects unknown names.
    kw_dynamic(
        "warn",
        1,
        3,
        Type::None,
        &["message", "category", "stacklevel"],
    ),
    kw(
        "warn_explicit",
        4,
        7,
        Type::None,
        &[
            "message",
            "category",
            "filename",
            "lineno",
            "module",
            "registry",
            "module_globals",
        ],
    ),
    kw(
        "filterwarnings",
        1,
        6,
        Type::None,
        &[
            "action", "message", "category", "module", "lineno", "append",
        ],
    ),
    kw(
        "simplefilter",
        1,
        4,
        Type::None,
        &["action", "category", "lineno", "append"],
    ),
    f("resetwarnings", 0, 0, Type::None),
    // Natives behind the `catch_warnings` instance.
    f("_catch_warnings_enter", 1, 1, Type::Any),
    f("_catch_warnings_exit", 4, 4, Type::None),
    // `catch_warnings(*, record=False, module=None, action=None, append=False)`
    // and the private `_deprecated` helper used by `annotationlib`. The runtime
    // module parses the keyword names so skipped leading slots stay correct.
    kw_dynamic("catch_warnings", 0, 4, Type::Any, &[]),
    kw_dynamic("_deprecated", 0, 8, Type::None, &[]),
    kw(
        "showwarning",
        4,
        6,
        Type::None,
        &["message", "category", "filename", "lineno", "file", "line"],
    ),
    kw(
        "formatwarning",
        4,
        5,
        Type::Str,
        &["message", "category", "filename", "lineno", "line"],
    ),
];

/// `warnings`: the stable module spellings. `filters` is a snapshot list of
/// the currently installed filter tuples (see the runtime module docs).
pub static WARNINGS: ModuleSpec = ModuleSpec {
    name: "warnings",
    functions: WARNING_FUNCTIONS,
    constants: &[c("filters", Type::List)],
};

/// `_warnings`: the accelerator alias. The same registry and filters are
/// shared with [`WARNINGS`].
pub static _WARNINGS: ModuleSpec = ModuleSpec {
    name: "_warnings",
    functions: WARNING_FUNCTIONS,
    constants: &[c("filters", Type::List)],
};

/// `marshal`: importable, but Hydro has no CPython marshal byte stream, so
/// every conversion function raises `NotImplementedError` instead of
/// fabricating a payload. `version` reports the CPython 3.14 format version.
pub static MARSHAL: ModuleSpec = ModuleSpec {
    name: "marshal",
    functions: &[
        kw("dump", 2, 3, Type::None, &["value", "file", "version"]),
        kw("dumps", 1, 2, Type::Bytes, &["value", "version"]),
        f("load", 1, 1, Type::Any),
        f("loads", 1, 1, Type::Any),
    ],
    constants: &[c("version", Type::Int)],
};

/// `_tokenize`: only the C tokenizer iterator name is exposed, and calling it
/// raises `NotImplementedError`; Hydro never fabricates tokens.
pub static _TOKENIZE: ModuleSpec = ModuleSpec {
    name: "_tokenize",
    functions: &[kw(
        "TokenizerIter",
        1,
        3,
        Type::Any,
        &["source", "encoding", "extra_tokens"],
    )],
    constants: &[],
};

/// `token`: CPython 3.14's numeric token constants (from `Lib/token.py`),
/// `tok_name`, the exact-operator map and the three terminal predicates.
///
/// `__all__` is part of the module surface (and therefore visible through
/// `token.__all__`), but its leading underscore keeps it out of
/// `from token import *`, matching CPython.
pub static TOKEN: ModuleSpec = ModuleSpec {
    name: "token",
    functions: &[
        f("ISTERMINAL", 1, 1, Type::Bool),
        f("ISNONTERMINAL", 1, 1, Type::Bool),
        f("ISEOF", 1, 1, Type::Bool),
    ],
    constants: &[
        c("ENDMARKER", Type::Int),
        c("NAME", Type::Int),
        c("NUMBER", Type::Int),
        c("STRING", Type::Int),
        c("NEWLINE", Type::Int),
        c("INDENT", Type::Int),
        c("DEDENT", Type::Int),
        c("LPAR", Type::Int),
        c("RPAR", Type::Int),
        c("LSQB", Type::Int),
        c("RSQB", Type::Int),
        c("COLON", Type::Int),
        c("COMMA", Type::Int),
        c("SEMI", Type::Int),
        c("PLUS", Type::Int),
        c("MINUS", Type::Int),
        c("STAR", Type::Int),
        c("SLASH", Type::Int),
        c("VBAR", Type::Int),
        c("AMPER", Type::Int),
        c("LESS", Type::Int),
        c("GREATER", Type::Int),
        c("EQUAL", Type::Int),
        c("DOT", Type::Int),
        c("PERCENT", Type::Int),
        c("LBRACE", Type::Int),
        c("RBRACE", Type::Int),
        c("EQEQUAL", Type::Int),
        c("NOTEQUAL", Type::Int),
        c("LESSEQUAL", Type::Int),
        c("GREATEREQUAL", Type::Int),
        c("TILDE", Type::Int),
        c("CIRCUMFLEX", Type::Int),
        c("LEFTSHIFT", Type::Int),
        c("RIGHTSHIFT", Type::Int),
        c("DOUBLESTAR", Type::Int),
        c("PLUSEQUAL", Type::Int),
        c("MINEQUAL", Type::Int),
        c("STAREQUAL", Type::Int),
        c("SLASHEQUAL", Type::Int),
        c("PERCENTEQUAL", Type::Int),
        c("AMPEREQUAL", Type::Int),
        c("VBAREQUAL", Type::Int),
        c("CIRCUMFLEXEQUAL", Type::Int),
        c("LEFTSHIFTEQUAL", Type::Int),
        c("RIGHTSHIFTEQUAL", Type::Int),
        c("DOUBLESTAREQUAL", Type::Int),
        c("DOUBLESLASH", Type::Int),
        c("DOUBLESLASHEQUAL", Type::Int),
        c("AT", Type::Int),
        c("ATEQUAL", Type::Int),
        c("RARROW", Type::Int),
        c("ELLIPSIS", Type::Int),
        c("COLONEQUAL", Type::Int),
        c("EXCLAMATION", Type::Int),
        c("OP", Type::Int),
        c("TYPE_IGNORE", Type::Int),
        c("TYPE_COMMENT", Type::Int),
        c("SOFT_KEYWORD", Type::Int),
        c("FSTRING_START", Type::Int),
        c("FSTRING_MIDDLE", Type::Int),
        c("FSTRING_END", Type::Int),
        c("TSTRING_START", Type::Int),
        c("TSTRING_MIDDLE", Type::Int),
        c("TSTRING_END", Type::Int),
        c("COMMENT", Type::Int),
        c("NL", Type::Int),
        c("ERRORTOKEN", Type::Int),
        c("ENCODING", Type::Int),
        c("N_TOKENS", Type::Int),
        c("NT_OFFSET", Type::Int),
        c("tok_name", Type::Dict),
        c("EXACT_TOKEN_TYPES", Type::Dict),
        c("__all__", Type::List),
    ],
};

/// `_collections_abc`: the abstract base class objects. The pure-Python
/// `collections.abc` module imports every name from here.
pub static _COLLECTIONS_ABC: ModuleSpec = ModuleSpec {
    name: "_collections_abc",
    // Native bodies of the `register` method installed on every ABC class and
    // of `Buffer`'s `__buffer__`/`__subclasshook__`. The names are
    // underscore-hidden from `import *`; `register` raises
    // `NotImplementedError` because virtual subclass registration is not
    // emulated, and `Buffer`'s methods implement PEP 688.
    functions: &[
        f("_abc_register", 1, 1, Type::None),
        f("_abc_buffer", 2, 2, Type::Any),
        f("_abc_buffer_subclasshook", 2, 2, Type::Bool),
    ],
    constants: &[
        c("Buffer", Type::Any),
        c("Awaitable", Type::Any),
        c("AsyncIterable", Type::Any),
        c("AsyncIterator", Type::Any),
        c("AsyncGenerator", Type::Any),
        c("Hashable", Type::Any),
        c("Iterable", Type::Any),
        c("Iterator", Type::Any),
        c("Generator", Type::Any),
        c("Reversible", Type::Any),
        c("Sized", Type::Any),
        c("Container", Type::Any),
        c("Callable", Type::Any),
        c("Collection", Type::Any),
        c("Set", Type::Any),
        c("MutableSet", Type::Any),
        c("Mapping", Type::Any),
        c("MutableMapping", Type::Any),
        c("Sequence", Type::Any),
        c("MutableSequence", Type::Any),
        c("Coroutine", Type::Any),
    ],
};

/// `collections.abc`: the same class objects as [`_COLLECTIONS_ABC`].
///
/// The extra `abc` constant is a frontend workaround: `import collections.abc`
/// binds `collections` to this spec, and the nested-module chain resolver only
/// knows `os.path`, so `collections.abc.Mapping` reaches the runtime as
/// `collections.abc` followed by a dynamic attribute. The constant resolves
/// that intermediate expression to this module's value.
pub static COLLECTIONS_ABC: ModuleSpec = ModuleSpec {
    name: "collections.abc",
    functions: &[],
    constants: &[
        c("abc", Type::Any),
        c("Buffer", Type::Any),
        c("Awaitable", Type::Any),
        c("AsyncIterable", Type::Any),
        c("AsyncIterator", Type::Any),
        c("AsyncGenerator", Type::Any),
        c("Hashable", Type::Any),
        c("Iterable", Type::Any),
        c("Iterator", Type::Any),
        c("Generator", Type::Any),
        c("Reversible", Type::Any),
        c("Sized", Type::Any),
        c("Container", Type::Any),
        c("Callable", Type::Any),
        c("Collection", Type::Any),
        c("Set", Type::Any),
        c("MutableSet", Type::Any),
        c("Mapping", Type::Any),
        c("MutableMapping", Type::Any),
        c("Sequence", Type::Any),
        c("MutableSequence", Type::Any),
        c("Coroutine", Type::Any),
    ],
};

/// `gc`: the collector's introspection surface. Names the current collector
/// cannot honour (`disable`, `set_threshold`, `freeze`, ...) are declared so
/// they raise a tagged `NotImplementedError` rather than a misleading
/// `AttributeError`.
pub static GC: ModuleSpec = ModuleSpec {
    name: "gc",
    functions: &[
        kw("collect", 0, 1, Type::Int, &["generation"]),
        f("isenabled", 0, 0, Type::Bool),
        f("enable", 0, 0, Type::None),
        f("disable", 0, 0, Type::None),
        f("get_count", 0, 0, Type::Tuple),
        f("get_stats", 0, 0, Type::List),
        f("get_threshold", 0, 0, Type::Tuple),
        f("set_threshold", 1, 3, Type::None),
        f("freeze", 0, 0, Type::None),
        f("unfreeze", 0, 0, Type::None),
        kw("get_objects", 0, 1, Type::List, &["generation"]),
    ],
    constants: &[],
};

/// `atexit`: LIFO shutdown callbacks.
pub static ATEXIT: ModuleSpec = ModuleSpec {
    name: "atexit",
    functions: &[
        f("register", 1, usize::MAX, Type::Any),
        f("unregister", 1, 1, Type::None),
        f("_run_exitfuncs", 0, 0, Type::None),
        f("_clear", 0, 0, Type::None),
    ],
    constants: &[],
};

/// `site`: the importable site stub. Hydro performs no site processing, so
/// `main()` is a truthful no-op and `addsitedir` reports the limitation.
pub static SITE: ModuleSpec = ModuleSpec {
    name: "site",
    functions: &[
        kw("main", 0, 1, Type::None, &["known_paths"]),
        kw("addsitedir", 1, 2, Type::None, &["path", "known_paths"]),
    ],
    constants: &[
        c("ENABLE_USER_SITE", Type::Bool),
        c("USER_SITE", Type::Any),
        c("USER_BASE", Type::Any),
        c("PREFIXES", Type::List),
    ],
};

/// Shared function table of `_contextvars` and `contextvars`.
///
/// `ContextVar` is declared as a function even though CPython exposes a type:
/// Hydro's native class constructor path only binds declared *module*
/// parameters, so a class value would reject `ContextVar(name, default=...)`.
/// The value returned by the constructor is an instance of the real native
/// `ContextVar` class, which carries `get`/`set`/`reset`. `Context` and
/// `Token` stay class constants so they work as `isinstance` targets and can
/// be constructed with the class protocol.
///
/// The `_contextvars_*` entries are the native method bodies installed on those
/// classes. They are never imported by name; they exist because
/// `FunctionBody::Native` calls are validated against this table.
const CONTEXTVARS_FUNCTIONS: &[FunctionSpec] = &[
    kw("ContextVar", 1, 2, Type::Any, &["name", "default"]),
    f("copy_context", 0, 0, Type::Any),
    f("_contextvars_var_get", 1, 1, Type::Any),
    f("_contextvars_var_set", 2, 2, Type::Any),
    f("_contextvars_var_reset", 2, 2, Type::None),
    f("_contextvars_var_repr", 1, 1, Type::Str),
    f("_contextvars_token_init", 1, usize::MAX, Type::None),
    f("_contextvars_token_repr", 1, 1, Type::Str),
    f("_contextvars_ctx_init", 1, usize::MAX, Type::None),
    f("_contextvars_ctx_run", 2, usize::MAX, Type::Any),
    f("_contextvars_ctx_copy", 1, 1, Type::Any),
    f("_contextvars_ctx_get", 2, 3, Type::Any),
    f("_contextvars_ctx_getitem", 2, 2, Type::Any),
    f("_contextvars_ctx_setitem", 3, 3, Type::None),
    f("_contextvars_ctx_delitem", 2, 2, Type::None),
    f("_contextvars_ctx_contains", 2, 2, Type::Bool),
    f("_contextvars_ctx_len", 1, 1, Type::Int),
    f("_contextvars_ctx_keys", 1, 1, Type::List),
    f("_contextvars_ctx_values", 1, 1, Type::List),
    f("_contextvars_ctx_items", 1, 1, Type::List),
    f("_contextvars_ctx_repr", 1, 1, Type::Str),
    f("_contextvars_missing_repr", 1, 1, Type::Str),
];

const CONTEXTVARS_CONSTANTS: &[ConstantSpec] = &[c("Context", Type::Any), c("Token", Type::Any)];

/// `_contextvars`: the native context-variable implementation. The pure
/// `contextvars` module in CPython is only a re-export of this module, so an
/// explicit `contextvars` table keeps both import spellings native.
pub static _CONTEXTVARS: ModuleSpec = ModuleSpec {
    name: "_contextvars",
    functions: CONTEXTVARS_FUNCTIONS,
    constants: CONTEXTVARS_CONSTANTS,
};

/// `contextvars`: `Context`, `ContextVar`, `Token` and `copy_context`.
pub static CONTEXTVARS: ModuleSpec = ModuleSpec {
    name: "contextvars",
    functions: CONTEXTVARS_FUNCTIONS,
    constants: CONTEXTVARS_CONSTANTS,
};

/// `ctypes` / `_ctypes`: arena-backed C data types, structures, pointers and
/// dynamic libraries. Every `_cdata_*`, `_cdll_*` and `_loader_*` entry is a
/// native method body reachable through the class values; they are declared
/// here because native calls are validated against this table.
const CTYPES_FUNCTIONS: &[FunctionSpec] = &[
    f("sizeof", 1, 1, Type::Int),
    f("alignment", 1, 1, Type::Int),
    f("offsetof", 2, 2, Type::Int),
    f("POINTER", 1, 1, Type::Any),
    f("pointer", 1, 1, Type::Any),
    f("byref", 1, 1, Type::Any),
    f("cast", 2, 2, Type::Any),
    f("addressof", 1, 1, Type::Int),
    f("string_at", 1, 2, Type::Bytes),
    f("wstring_at", 1, 2, Type::Str),
    f("memmove", 3, 3, Type::None),
    f("memset", 3, 3, Type::None),
    f("resize", 2, 2, Type::None),
    f("create_string_buffer", 1, 2, Type::Any),
    f("create_unicode_buffer", 1, 2, Type::Any),
    f("CFUNCTYPE", 1, usize::MAX, Type::Any),
    f("WINFUNCTYPE", 1, usize::MAX, Type::Any),
    f("PYFUNCTYPE", 1, usize::MAX, Type::Any),
    f("get_last_error", 0, 0, Type::Int),
    f("set_last_error", 1, 1, Type::None),
    f("_cdata_init", 1, usize::MAX, Type::Any),
    f("_cdata_get_value", 1, 1, Type::Any),
    f("_cdata_set_value", 2, 2, Type::None),
    f("_cdata_get_contents", 1, 1, Type::Any),
    f("_cdata_get_raw", 1, 1, Type::Bytes),
    f("_cdata_set_raw", 2, 2, Type::None),
    f("_cdata_len", 1, 1, Type::Int),
    f("_cdata_getitem", 2, 2, Type::Any),
    f("_cdata_setitem", 3, 3, Type::None),
    f("_cdata_str", 1, 1, Type::Str),
    f("_cdata_repr", 1, 1, Type::Str),
    f("_cdata_call", 1, usize::MAX, Type::Any),
    f("_cdata_add", 2, 2, Type::Any),
    f("_cdata_radd", 2, 2, Type::Any),
    f("_cdll_init", 1, 6, Type::None),
    f("_cdll_getitem", 2, 2, Type::Any),
    f("_loader_init", 1, 2, Type::None),
    f("_loader_load", 2, 2, Type::Any),
    f("_loader_getitem", 2, 2, Type::Any),
];

const CTYPES_CONSTANTS: &[ConstantSpec] = &[
    c("_CData", Type::Any),
    c("_SimpleCData", Type::Any),
    c("_Pointer", Type::Any),
    c("Array", Type::Any),
    c("Structure", Type::Any),
    c("Union", Type::Any),
    c("CFuncPtr", Type::Any),
    c("_CFuncPtr", Type::Any),
    c("CDLL", Type::Any),
    c("PyDLL", Type::Any),
    c("LibraryLoader", Type::Any),
    c("ArgumentError", Type::Any),
    c("c_bool", Type::Any),
    c("c_byte", Type::Any),
    c("c_int8", Type::Any),
    c("c_ubyte", Type::Any),
    c("c_uint8", Type::Any),
    c("c_char", Type::Any),
    c("c_wchar", Type::Any),
    c("c_short", Type::Any),
    c("c_int16", Type::Any),
    c("c_ushort", Type::Any),
    c("c_uint16", Type::Any),
    c("c_int", Type::Any),
    c("c_int32", Type::Any),
    c("c_uint", Type::Any),
    c("c_uint32", Type::Any),
    c("c_long", Type::Any),
    c("c_int64", Type::Any),
    c("c_ssize_t", Type::Any),
    c("c_ptrdiff_t", Type::Any),
    c("c_ulong", Type::Any),
    c("c_uint64", Type::Any),
    c("c_size_t", Type::Any),
    c("c_longlong", Type::Any),
    c("c_ulonglong", Type::Any),
    c("c_intp", Type::Any),
    c("c_float", Type::Any),
    c("c_double", Type::Any),
    c("c_longdouble", Type::Any),
    c("c_void_p", Type::Any),
    c("c_char_p", Type::Any),
    c("c_wchar_p", Type::Any),
    c("cdll", Type::Any),
    c("RTLD_LOCAL", Type::Int),
    c("RTLD_GLOBAL", Type::Int),
    c("DEFAULT_MODE", Type::Int),
    c("FUNCFLAG_CDECL", Type::Int),
    c("FUNCFLAG_PYTHONAPI", Type::Int),
    c("FUNCFLAG_USE_ERRNO", Type::Int),
    c("FUNCFLAG_USE_LASTERROR", Type::Int),
    c("SIZEOF_TIME_T", Type::Int),
    c("__version__", Type::Str),
];

pub static CTYPES: ModuleSpec = ModuleSpec {
    name: "ctypes",
    functions: CTYPES_FUNCTIONS,
    constants: CTYPES_CONSTANTS,
};

pub static _CTYPES: ModuleSpec = ModuleSpec {
    name: "_ctypes",
    functions: CTYPES_FUNCTIONS,
    constants: CTYPES_CONSTANTS,
};

/// `locale`: libc-backed locale queries plus the `LC_*` categories.
pub static LOCALE: ModuleSpec = ModuleSpec {
    name: "locale",
    functions: &[
        f("setlocale", 1, 2, Type::Str),
        f("getlocale", 0, 1, Type::Tuple),
        f("getpreferredencoding", 0, 1, Type::Str),
        f("getencoding", 0, 0, Type::Str),
        f("normalize", 1, 1, Type::Str),
        f("strcoll", 2, 2, Type::Int),
        f("strxfrm", 1, 1, Type::Str),
        f("bindtextdomain", 1, 2, Type::Any),
        f("textdomain", 0, 1, Type::Any),
        f("gettext", 1, 1, Type::Str),
        f("dgettext", 2, 2, Type::Str),
        f("dcgettext", 2, 3, Type::Str),
        f("_gettext", 1, 1, Type::Str),
        f("_dgettext", 2, 2, Type::Str),
        f("_dcgettext", 2, 3, Type::Str),
    ],
    constants: &[
        c("LC_CTYPE", Type::Int),
        c("LC_NUMERIC", Type::Int),
        c("LC_TIME", Type::Int),
        c("LC_COLLATE", Type::Int),
        c("LC_MONETARY", Type::Int),
        c("LC_MESSAGES", Type::Int),
        c("LC_ALL", Type::Int),
        c("Error", Type::Any),
    ],
};

/// `sysconfig`: build-time variables and installation paths.
pub static SYSCONFIG: ModuleSpec = ModuleSpec {
    name: "sysconfig",
    functions: &[
        f("get_config_var", 1, 1, Type::Any),
        f("_get_config_var", 1, 1, Type::Any),
        f("get_config_vars", 0, usize::MAX, Type::Dict),
        f("_get_config_vars", 0, usize::MAX, Type::Dict),
        f("get_paths", 0, 3, Type::Dict),
        f("_get_paths", 0, 3, Type::Dict),
        f("get_path", 1, 4, Type::Str),
        f("_get_path", 1, 4, Type::Str),
        f("get_python_version", 0, 0, Type::Str),
        f("get_platform", 0, 0, Type::Str),
        f("get_scheme_names", 0, 0, Type::Tuple),
        f("get_default_scheme", 0, 0, Type::Str),
        f("get_preferred_scheme", 1, 1, Type::Str),
        f("is_python_build", 0, 0, Type::Bool),
    ],
    constants: &[
        c("PREFIX", Type::Str),
        c("EXEC_PREFIX", Type::Str),
        c("BASE_PREFIX", Type::Str),
        c("BASE_EXEC_PREFIX", Type::Str),
        c("installed_base", Type::Str),
        c("installed_platbase", Type::Str),
        c("platbase", Type::Str),
        c("prefix", Type::Str),
        c("exec_prefix", Type::Str),
        c("py_version", Type::Str),
        c("py_version_short", Type::Str),
        c("py_version_nodot", Type::Str),
        c("abiflags", Type::Str),
        c("platform", Type::Str),
    ],
};

/// `zoneinfo`: system tzdata-backed `ZoneInfo`.
pub static ZONEINFO: ModuleSpec = ModuleSpec {
    name: "zoneinfo",
    functions: &[
        f("available_timezones", 0, 0, Type::Set),
        f("reset_tzpath", 0, 1, Type::None),
        f("_zoneinfo_init", 1, 3, Type::None),
        f("_zoneinfo_get_key", 1, 1, Type::Any),
        f("_zoneinfo_utcoffset", 2, 2, Type::Any),
        f("_zoneinfo_dst", 2, 2, Type::Any),
        f("_zoneinfo_tzname", 2, 2, Type::Any),
        f("_zoneinfo_fromutc", 2, 2, Type::Any),
        f("_zoneinfo_str", 1, 1, Type::Str),
        f("_zoneinfo_repr", 1, 1, Type::Str),
    ],
    constants: &[
        c("ZoneInfo", Type::Any),
        c("ZoneInfoNotFoundError", Type::Any),
        c("TZPATH", Type::Tuple),
    ],
};

/// Shared function/constant tables of `_decimal` and `decimal`.
const DECIMAL_FUNCTIONS: &[FunctionSpec] = &[
    f("_decimal_init", 1, 2, Type::None),
    f("_decimal_str", 1, 1, Type::Str),
    f("_decimal_repr", 1, 1, Type::Str),
    f("_decimal_as_tuple", 1, 1, Type::Tuple),
    f("_decimal_eq", 2, 2, Type::Bool),
    f("_decimal_ne", 2, 2, Type::Bool),
    f("_decimal_lt", 2, 2, Type::Bool),
    f("_decimal_le", 2, 2, Type::Bool),
    f("_decimal_gt", 2, 2, Type::Bool),
    f("_decimal_ge", 2, 2, Type::Bool),
    f("_decimal_add", 2, 2, Type::Any),
    f("_decimal_radd", 2, 2, Type::Any),
    f("_decimal_subtract", 2, 2, Type::Any),
    f("_decimal_multiply", 2, 2, Type::Any),
    f("_decimal_divide", 2, 2, Type::Any),
    f("_decimal_neg", 1, 1, Type::Any),
    f("_decimal_pos", 1, 1, Type::Any),
    f("_decimal_abs", 1, 1, Type::Any),
    f("_decimal_quantize", 2, 2, Type::Any),
    f("_decimal_compare", 2, 2, Type::Any),
    f("_decimal_copy_abs", 1, 1, Type::Any),
    f("_decimal_copy_negate", 1, 1, Type::Any),
    f("_decimal_copy_sign", 2, 2, Type::Any),
    f("_decimal_adjusted", 1, 1, Type::Int),
    f("_decimal_is_nan", 1, 1, Type::Bool),
    f("_decimal_is_qnan", 1, 1, Type::Bool),
    f("_decimal_is_snan", 1, 1, Type::Bool),
    f("_decimal_is_finite", 1, 1, Type::Bool),
    f("_decimal_is_infinite", 1, 1, Type::Bool),
    f("_decimal_is_zero", 1, 1, Type::Bool),
    f("_decimal_is_signed", 1, 1, Type::Bool),
    f("_decimal_to_integral_value", 1, 1, Type::Any),
    f("getcontext", 0, 0, Type::Any),
    f("setcontext", 1, 1, Type::None),
    f("localcontext", 0, 1, Type::Any),
    f("_context_init", 1, 9, Type::None),
    f("_context_enter", 1, 1, Type::Any),
    f("_context_exit", 1, 4, Type::None),
    f("_context_copy", 1, 1, Type::Any),
    f("_localcontext_init", 1, 2, Type::None),
];

const DECIMAL_CONSTANTS: &[ConstantSpec] = &[
    c("Decimal", Type::Any),
    c("Context", Type::Any),
    c("DefaultContext", Type::Any),
    c("DecimalException", Type::Any),
    c("InvalidOperation", Type::Any),
    c("DivisionByZero", Type::Any),
    c("Overflow", Type::Any),
    c("Underflow", Type::Any),
    c("Inexact", Type::Any),
    c("Rounded", Type::Any),
    c("Subnormal", Type::Any),
    c("Clamped", Type::Any),
    c("ConversionSyntax", Type::Any),
    c("ROUND_CEILING", Type::Str),
    c("ROUND_DOWN", Type::Str),
    c("ROUND_FLOOR", Type::Str),
    c("ROUND_HALF_DOWN", Type::Str),
    c("ROUND_HALF_EVEN", Type::Str),
    c("ROUND_HALF_UP", Type::Str),
    c("ROUND_UP", Type::Str),
    c("ROUND_05UP", Type::Str),
    c("__libmpdec_version__", Type::Str),
    c("MAX_PREC", Type::Int),
    c("MAX_EMAX", Type::Int),
    c("MIN_EMIN", Type::Int),
    c("MIN_ETINY", Type::Int),
];

pub static DECIMAL: ModuleSpec = ModuleSpec {
    name: "decimal",
    functions: DECIMAL_FUNCTIONS,
    constants: DECIMAL_CONSTANTS,
};

pub static _DECIMAL: ModuleSpec = ModuleSpec {
    name: "_decimal",
    functions: DECIMAL_FUNCTIONS,
    constants: DECIMAL_CONSTANTS,
};

/// Shared function table of `_struct` and `struct`.
pub static _STRUCT_FUNCTIONS: &[FunctionSpec] = &[
    f("calcsize", 1, 1, Type::Int),
    f("pack", 1, usize::MAX, Type::Bytes),
    f("pack_into", 3, usize::MAX, Type::None),
    f("unpack", 2, 2, Type::Tuple),
    f("unpack_from", 2, 3, Type::Tuple),
    f("iter_unpack", 2, 2, Type::Any),
    f("_clearcache", 0, 0, Type::None),
    f("Struct", 1, 1, Type::Any),
];

pub static _STRUCT: ModuleSpec = ModuleSpec {
    name: "_struct",
    functions: _STRUCT_FUNCTIONS,
    constants: &[c("error", Type::Any)],
};

pub static STRUCT: ModuleSpec = ModuleSpec {
    name: "struct",
    functions: _STRUCT_FUNCTIONS,
    constants: &[c("error", Type::Any)],
};

/// `unicodedata`: Unicode 16.0.0 character database and normalization.
pub static UNICODEDATA: ModuleSpec = ModuleSpec {
    name: "unicodedata",
    functions: &[
        f("category", 1, 1, Type::Str),
        f("combining", 1, 1, Type::Int),
        f("east_asian_width", 1, 1, Type::Str),
        f("bidirectional", 1, 1, Type::Str),
        f("decomposition", 1, 1, Type::Str),
        f("normalize", 2, 2, Type::Str),
        f("is_normalized", 2, 2, Type::Bool),
        f("name", 1, 2, Type::Str),
        f("lookup", 1, 1, Type::Str),
        f("numeric", 1, 2, Type::Float),
        f("digit", 1, 2, Type::Int),
        f("decimal", 1, 2, Type::Int),
    ],
    constants: &[c("unidata_version", Type::Str)],
};

/// `zlib`: real zlib-format compression through the system `libz`.
///
/// The public functions and constants mirror CPython 3.14. The `_zlib_*`
/// entries are the native method bodies of the `Compress`/`Decompress`
/// classes; they are internal hooks, validated through this table like every
/// other native callable.
pub static ZLIB: ModuleSpec = ModuleSpec {
    name: "zlib",
    functions: &[
        kw("compress", 1, 3, Type::Bytes, &["data", "level", "wbits"]),
        kw(
            "decompress",
            1,
            3,
            Type::Bytes,
            &["data", "wbits", "bufsize"],
        ),
        kw("crc32", 1, 2, Type::Int, &["data", "value"]),
        kw("adler32", 1, 2, Type::Int, &["data", "value"]),
        kw(
            "compressobj",
            0,
            5,
            Type::Any,
            &["level", "method", "wbits", "memLevel", "strategy"],
        ),
        kw("decompressobj", 0, 2, Type::Any, &["wbits", "zdict"]),
        // `Compress` / `Decompress` native method bodies.
        f("_zlib_compress", 2, 2, Type::Bytes),
        f("_zlib_flush", 1, 2, Type::Bytes),
        f("_zlib_copy", 1, 1, Type::Any),
        f("_zlib_decompress", 2, 3, Type::Bytes),
        f("_zlib_decompress_flush", 1, 2, Type::Bytes),
        f("_zlib_decompress_copy", 1, 1, Type::Any),
    ],
    constants: &[
        c("error", Type::Any),
        c("Z_NO_COMPRESSION", Type::Int),
        c("Z_BEST_SPEED", Type::Int),
        c("Z_BEST_COMPRESSION", Type::Int),
        c("Z_DEFAULT_COMPRESSION", Type::Int),
        c("DEFLATED", Type::Int),
        c("DEF_MEM_LEVEL", Type::Int),
        c("MAX_WBITS", Type::Int),
        c("Z_DEFAULT_STRATEGY", Type::Int),
        c("Z_FILTERED", Type::Int),
        c("Z_HUFFMAN_ONLY", Type::Int),
        c("Z_RLE", Type::Int),
        c("Z_FIXED", Type::Int),
        c("Z_NO_FLUSH", Type::Int),
        c("Z_PARTIAL_FLUSH", Type::Int),
        c("Z_SYNC_FLUSH", Type::Int),
        c("Z_FULL_FLUSH", Type::Int),
        c("Z_FINISH", Type::Int),
        c("Z_BLOCK", Type::Int),
        c("Z_TREES", Type::Int),
        c("Z_OK", Type::Int),
        c("Z_STREAM_END", Type::Int),
        c("Z_NEED_DICT", Type::Int),
        c("Z_ERRNO", Type::Int),
        c("Z_STREAM_ERROR", Type::Int),
        c("Z_DATA_ERROR", Type::Int),
        c("Z_MEM_ERROR", Type::Int),
        c("Z_BUF_ERROR", Type::Int),
        c("Z_VERSION_ERROR", Type::Int),
        c("ZLIB_VERSION", Type::Str),
        c("ZLIB_RUNTIME_VERSION", Type::Str),
    ],
};

/// `mmap`: real memory-mapped files through the platform `mmap`.
///
/// The `_mmap_*` entries are the native method bodies of the `mmap.mmap`
/// class (arity includes `self`); they are internal hooks, validated through
/// this table like every other native callable.
pub static MMAP: ModuleSpec = ModuleSpec {
    name: "mmap",
    functions: &[
        kw(
            "mmap",
            2,
            7,
            Type::Any,
            &[
                "fileno", "length", "flags", "prot", "access", "offset", "trackfd",
            ],
        ),
        f("_mmap_init", 3, 8, Type::None),
        f("_mmap_close", 1, 1, Type::None),
        f("_mmap_read", 1, 2, Type::Bytes),
        f("_mmap_read_byte", 1, 1, Type::Int),
        f("_mmap_readline", 1, 1, Type::Bytes),
        f("_mmap_write", 2, 2, Type::Int),
        f("_mmap_write_byte", 2, 2, Type::None),
        f("_mmap_seek", 2, 3, Type::Int),
        f("_mmap_seekable", 1, 1, Type::Bool),
        f("_mmap_tell", 1, 1, Type::Int),
        f("_mmap_size", 1, 1, Type::Int),
        f("_mmap_flush", 1, 3, Type::None),
        f("_mmap_resize", 2, 2, Type::None),
        f("_mmap_move", 4, 4, Type::None),
        f("_mmap_find", 2, 4, Type::Int),
        f("_mmap_rfind", 2, 4, Type::Int),
        f("_mmap_getitem", 2, 2, Type::Any),
        f("_mmap_setitem", 3, 3, Type::None),
        f("_mmap_len", 1, 1, Type::Int),
        f("_mmap_enter", 1, 1, Type::Any),
        f("_mmap_exit", 1, 4, Type::None),
        f("_mmap_repr", 1, 1, Type::Str),
    ],
    constants: &[
        c("ACCESS_DEFAULT", Type::Int),
        c("ACCESS_READ", Type::Int),
        c("ACCESS_WRITE", Type::Int),
        c("ACCESS_COPY", Type::Int),
        c("ALLOCATIONGRANULARITY", Type::Int),
        c("PAGESIZE", Type::Int),
        c("MAP_SHARED", Type::Int),
        c("MAP_PRIVATE", Type::Int),
        c("MAP_ANONYMOUS", Type::Int),
        c("PROT_READ", Type::Int),
        c("PROT_WRITE", Type::Int),
    ],
};

/// `inspect`: native introspection over the runtime's function/class/module
/// representations.
///
/// CPython implements most of `inspect` in Python on top of frame, code and
/// traceback objects. Hydro has none of those, so this table declares the
/// native implementation: predicates over runtime values, docstring helpers,
/// `Signature`/`Parameter`/`BoundArguments` construction, and argument
/// binding. See the runtime module for the precise unsupported areas.
///
/// The classes (`Signature`, `Parameter`, `ParameterKind`, `BoundArguments`,
/// `FullArgSpec`, `FrameInfo`) and the `empty` sentinel are declared as
/// constants; the runtime materialises them as real class values so
/// `isinstance` and attribute access behave like CPython.
pub static INSPECT: ModuleSpec = ModuleSpec {
    name: "inspect",
    functions: &[
        // Predicates.
        f("ismodule", 1, 1, Type::Bool),
        f("isclass", 1, 1, Type::Bool),
        f("isfunction", 1, 1, Type::Bool),
        f("ismethod", 1, 1, Type::Bool),
        f("isbuiltin", 1, 1, Type::Bool),
        f("isroutine", 1, 1, Type::Bool),
        f("isgeneratorfunction", 1, 1, Type::Bool),
        f("iscoroutinefunction", 1, 1, Type::Bool),
        // `inspect.markcoroutinefunction(func)`: tags a callable so
        // `iscoroutinefunction(func)` reports `True` without a code object.
        kw("markcoroutinefunction", 1, 1, Type::Any, &["func"]),
        f("isasyncgenfunction", 1, 1, Type::Bool),
        f("isgenerator", 1, 1, Type::Bool),
        f("iscoroutine", 1, 1, Type::Bool),
        f("isasyncgen", 1, 1, Type::Bool),
        f("isawaitable", 1, 1, Type::Bool),
        f("isframe", 1, 1, Type::Bool),
        f("iscode", 1, 1, Type::Bool),
        f("ismethoddescriptor", 1, 1, Type::Bool),
        f("isdatadescriptor", 1, 1, Type::Bool),
        f("ismemberdescriptor", 1, 1, Type::Bool),
        f("isgetsetdescriptor", 1, 1, Type::Bool),
        f("isabstract", 1, 1, Type::Bool),
        // Docstrings and source provenance.
        f("cleandoc", 1, 1, Type::Str),
        f("getdoc", 1, 1, Type::Any),
        f("getmodule", 1, 1, Type::Any),
        f("getfile", 1, 1, Type::Str),
        f("getsourcefile", 1, 1, Type::Any),
        kw("unwrap", 1, 2, Type::Any, &["func", "stop"]),
        // Signature construction and argument binding.
        kw(
            "signature",
            1,
            6,
            Type::Any,
            &[
                "obj",
                "follow_wrapped",
                "globals",
                "locals",
                "eval_str",
                "annotation_format",
            ],
        ),
        kw("getfullargspec", 1, 1, Type::Any, &["func"]),
        // `getcallargs(func, /, *positional, **named)`: extra positional
        // arguments and keyword names are the arguments being bound.
        kw_dynamic("getcallargs", 1, usize::MAX, Type::Dict, &["func"]),
        kw("getmembers", 1, 2, Type::List, &["object", "predicate"]),
        kw(
            "getattr_static",
            2,
            3,
            Type::Any,
            &["obj", "attr", "default"],
        ),
        f("currentframe", 0, 0, Type::Any),
        kw("stack", 0, 1, Type::List, &["context"]),
        kw("getclosurevars", 1, 1, Type::Any, &["func"]),
    ],
    constants: &[
        c("empty", Type::Any),
        c("_empty", Type::Any),
        c("Signature", Type::Any),
        c("Parameter", Type::Any),
        c("ParameterKind", Type::Any),
        c("_ParameterKind", Type::Any),
        c("BoundArguments", Type::Any),
        c("FullArgSpec", Type::Any),
        c("FrameInfo", Type::Any),
        c("ClosureVars", Type::Any),
        // `Parameter.kind` members are re-exported by CPython's inspect module
        // for backwards compatibility.
        c("_POSITIONAL_ONLY", Type::Any),
        c("_POSITIONAL_OR_KEYWORD", Type::Any),
        c("_VAR_POSITIONAL", Type::Any),
        c("_KEYWORD_ONLY", Type::Any),
        c("_VAR_KEYWORD", Type::Any),
        // Code-object flag bits, imported by CPython's inspect from `_opcode`.
        c("CO_OPTIMIZED", Type::Int),
        c("CO_NEWLOCALS", Type::Int),
        c("CO_VARARGS", Type::Int),
        c("CO_VARKEYWORDS", Type::Int),
        c("CO_NESTED", Type::Int),
        c("CO_GENERATOR", Type::Int),
        c("CO_NOFREE", Type::Int),
        c("CO_COROUTINE", Type::Int),
        c("CO_ITERABLE_COROUTINE", Type::Int),
        c("CO_ASYNC_GENERATOR", Type::Int),
        c("CO_HAS_DOCSTRING", Type::Int),
        c("CO_METHOD", Type::Int),
    ],
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
    fn pytest_main_accepts_a_full_session_argument_list() {
        let main = function("pytest", "main").expect("pytest.main");
        assert_eq!(main.min_args, 0);
        assert_eq!(main.max_args, usize::MAX);
        assert_eq!(main.ret, Type::None);
        assert!(
            main.params.is_empty(),
            "pytest.main(*args, **kwargs) declares no fixed parameters"
        );
        assert!(
            main.dynamic_keywords,
            "pytest.main(*args, **kwargs) forwards every keyword to the runtime"
        );
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
    fn contextlib_contextmanager_specs_are_registered() {
        let decorate = function("contextlib", "contextmanager").expect("contextmanager");
        assert_eq!((decorate.min_args, decorate.max_args), (1, 1));
        let call = function("contextlib", "_contextmanager_call").expect("_contextmanager_call");
        // The decorated function is the bound first argument; every further
        // positional and keyword argument belongs to the call.
        assert_eq!(call.min_args, 1);
        assert_eq!(call.max_args, usize::MAX);
        assert!(call.dynamic_keywords);
        for (name, arity) in [("_contextmanager_enter", 1), ("_contextmanager_exit", 4)] {
            let spec = function("contextlib", name).expect(name);
            assert_eq!((spec.min_args, spec.max_args), (arity, arity));
        }
    }

    #[test]
    fn os_and_platform_new_entries() {
        assert_eq!(function("os", "sysconf").unwrap().ret, Type::Int);
        assert_eq!(function("os", "getloadavg").unwrap().ret, Type::Tuple);
        let uname = function("os", "uname").expect("os.uname");
        assert_eq!(
            (uname.min_args, uname.max_args, uname.ret),
            (0, 0, Type::Any)
        );
        // `os.stat(path, *, dir_fd=None, follow_symlinks=True)`.
        let stat = function("os", "stat").expect("os.stat");
        assert_eq!((stat.min_args, stat.max_args, stat.ret), (1, 3, Type::Any));
        assert_eq!(stat.params, &["path", "dir_fd", "follow_symlinks"][..]);
        let isabs = function("os.path", "isabs").expect("os.path.isabs");
        assert_eq!(
            (isabs.min_args, isabs.max_args, isabs.ret),
            (1, 1, Type::Bool)
        );
        let normcase = function("os.path", "normcase").expect("os.path.normcase");
        assert_eq!(
            (normcase.min_args, normcase.max_args, normcase.ret),
            (1, 1, Type::Str)
        );
        let realpath = function("os.path", "realpath").expect("os.path.realpath");
        assert_eq!((realpath.min_args, realpath.max_args), (1, 2));
        assert!(realpath.dynamic_keywords);
        assert_eq!(function("platform", "processor").unwrap().ret, Type::Str);
        let uname = function("platform", "uname").expect("platform.uname");
        assert_eq!(
            (uname.min_args, uname.max_args, uname.ret),
            (0, 0, Type::Any)
        );
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
    fn os_environment_and_listdir_specs_are_registered() {
        for (name, arity) in [("putenv", 2), ("unsetenv", 1)] {
            let spec = function("os", name).expect(name);
            assert_eq!((spec.min_args, spec.max_args), (arity, arity), "{name}");
            assert_eq!(spec.ret, Type::None, "{name}");
            assert!(spec.params.is_empty(), "{name} is positional-only");
        }
        let listdir = function("os", "listdir").expect("os.listdir");
        assert_eq!(
            (listdir.min_args, listdir.max_args, listdir.ret),
            (0, 1, Type::List)
        );
        assert_eq!(listdir.params, &["path"][..]);
        assert!(
            listdir.dynamic_keywords,
            "listdir(path=...) is accepted by CPython's Python wrapper"
        );
    }

    #[test]
    fn sys_implementation_specs_are_registered() {
        assert_eq!(constant("sys", "implementation").unwrap().ty, Type::Any);
        assert_eq!(constant("sys", "hexversion").unwrap().ty, Type::Int);
    }

    #[test]
    fn typing_surface_is_registered() {
        assert_eq!(
            function("typing", "cast").unwrap().params,
            &["typ", "val"][..]
        );
        assert_eq!(function("typing", "cast").unwrap().min_args, 2);
        let assert_never = function("typing", "assert_never").expect("assert_never");
        assert_eq!(
            (assert_never.min_args, assert_never.max_args),
            (1, 1),
            "assert_never(arg, /) takes exactly one argument"
        );
        assert_eq!(assert_never.ret, Type::Never, "assert_never never returns");
        assert_eq!(function("typing", "runtime_checkable").unwrap().max_args, 1);
        assert_eq!(function("typing", "overload").unwrap().min_args, 1);
        let typevar = function("typing", "TypeVar").unwrap();
        assert_eq!(typevar.min_args, 1);
        assert_eq!(typevar.max_args, usize::MAX, "constraints are variadic");
        assert!(typevar.dynamic_keywords, "bound/covariant/... are keywords");
        assert_eq!(typevar.params, &["name"][..]);
        assert_eq!(function("typing", "NewType").unwrap().min_args, 2);
        let hints = function("typing", "get_type_hints").unwrap();
        assert_eq!(hints.ret, Type::Dict);
        assert!(hints.dynamic_keywords);
        assert_eq!(hints.max_args, 5);
        let defaults = function("typing", "_get_defaults").expect("_get_defaults");
        assert_eq!((defaults.min_args, defaults.max_args), (1, 1));
        assert_eq!(defaults.ret, Type::Dict);
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
            "AsyncContextManager",
            "AsyncGenerator",
            "Awaitable",
            "Coroutine",
            "AsyncIterable",
            "AsyncIterator",
            "ParamSpec",
            "TypeVarTuple",
            "_Final",
            "_SpecialForm",
            "_GenericAlias",
            "_BaseGenericAlias",
            "_SpecialGenericAlias",
            "_UnionGenericAlias",
            "_AnnotatedAlias",
            "_ConcatenateGenericAlias",
            "_ProtocolMeta",
            "_TypedDictMeta",
            "EXCLUDED_ATTRIBUTES",
            "_prohibited",
        ] {
            assert_eq!(constant("typing", name).unwrap().ty, Type::Any, "{name}");
        }
        assert_eq!(
            constant("typing", "_ASSERT_NEVER_REPR_MAX_LENGTH")
                .unwrap()
                .ty,
            Type::Int
        );
        // `typing_extensions` introspects `_type_check` and decorates with
        // `_tp_cache`; both must be real function specs. The names are parsed
        // by the runtime (dynamic keywords) because CPython's optional
        // parameters are not a fixed leading run.
        let type_check = function("typing", "_type_check").expect("_type_check");
        assert!(type_check.dynamic_keywords);
        assert_eq!(type_check.min_args, 2);
        assert_eq!(function("typing", "_tp_cache").unwrap().max_args, 1);
        assert!(function("typing", "_tp_cache").unwrap().dynamic_keywords);
        assert_eq!(
            function("typing", "_overload_dummy").unwrap().max_args,
            usize::MAX
        );
        for name in [
            "_type_convert",
            "_type_repr",
            "_eval_type",
            "_is_param_expr",
            "_deduplicate",
            "_generic_class_getitem",
            "_collect_type_vars",
            "_collect_parameters",
            "_check_generic",
        ] {
            assert!(function("typing", name).is_some(), "{name}");
        }
        assert_eq!(function("typing", "ParamSpec").unwrap().min_args, 1);
        assert_eq!(function("typing", "TypeVarTuple").unwrap().min_args, 1);
    }

    #[test]
    fn abc_and_enum_surfaces_are_registered() {
        assert_eq!(constant("abc", "ABC").unwrap().ty, Type::Any);
        assert_eq!(constant("abc", "ABCMeta").unwrap().ty, Type::Any);
        assert_eq!(function("abc", "abstractmethod").unwrap().max_args, 1);
        assert_eq!(function("abc", "get_cache_token").unwrap().ret, Type::Int);
        for name in [
            "abstractclassmethod",
            "abstractstaticmethod",
            "abstractproperty",
        ] {
            assert_eq!(function("abc", name).unwrap().max_args, 1, "{name}");
        }
        assert_eq!(constant("enum", "Enum").unwrap().ty, Type::Any);
        assert_eq!(constant("enum", "IntEnum").unwrap().ty, Type::Any);
        assert_eq!(constant("enum", "IntFlag").unwrap().ty, Type::Any);
        let auto = function("enum", "auto").unwrap();
        assert_eq!((auto.min_args, auto.max_args), (0, 0));
    }

    #[test]
    fn opcode_surface_is_truthful_for_a_bytecode_less_runtime() {
        // The predicates exist and answer `False` at runtime; the operations
        // that need CPython bytecode exist and raise `NotImplementedError`.
        for name in [
            "has_arg",
            "has_const",
            "has_name",
            "has_jump",
            "has_free",
            "has_local",
            "has_exc",
            "is_valid",
        ] {
            let spec = function("_opcode", name).unwrap();
            assert_eq!((spec.min_args, spec.max_args), (1, 1), "{name}");
            assert_eq!(spec.ret, Type::Bool, "{name}");
        }
        assert_eq!(function("_opcode", "stack_effect").unwrap().max_args, 3);
        assert_eq!(function("_opcode", "get_executor").unwrap().min_args, 2);
        assert_eq!(
            function("_opcode", "get_intrinsic1_descs").unwrap().ret,
            Type::List
        );
        assert_eq!(
            function("_opcode", "get_special_method_names").unwrap().ret,
            Type::Dict
        );
        // `opmap`/`opname` are not `_opcode` members in CPython 3.14 either.
        assert!(constant("_opcode", "opmap").is_none());
        assert!(constant("_opcode", "opname").is_none());
        assert_eq!(
            constant("_opcode", "ENABLE_SPECIALIZATION").unwrap().ty,
            Type::Int
        );
    }

    #[test]
    fn zlib_and_mmap_surfaces_are_registered() {
        // Public zlib functions plus the internal `Compress`/`Decompress`
        // method bodies that the runtime validates against this table.
        for name in [
            "compress",
            "decompress",
            "crc32",
            "adler32",
            "compressobj",
            "decompressobj",
            "_zlib_compress",
            "_zlib_flush",
            "_zlib_copy",
            "_zlib_decompress",
            "_zlib_decompress_flush",
            "_zlib_decompress_copy",
        ] {
            assert!(function("zlib", name).is_some(), "zlib.{name}");
        }
        assert_eq!(function("zlib", "compress").unwrap().ret, Type::Bytes);
        assert_eq!(function("zlib", "crc32").unwrap().ret, Type::Int);
        assert_eq!(function("zlib", "compressobj").unwrap().min_args, 0);
        for name in [
            "error",
            "Z_BEST_COMPRESSION",
            "DEFLATED",
            "Z_FINISH",
            "Z_STREAM_ERROR",
            "ZLIB_VERSION",
        ] {
            assert!(constant("zlib", name).is_some(), "zlib.{name}");
        }

        // `mmap.mmap` is both the constructor spec and the class hook; every
        // `_mmap_*` body is an internal method entry.
        assert_eq!(function("mmap", "mmap").unwrap().max_args, 7);
        assert_eq!(function("mmap", "_mmap_init").unwrap().min_args, 3);
        for name in [
            "_mmap_close",
            "_mmap_read",
            "_mmap_write",
            "_mmap_seek",
            "_mmap_size",
            "_mmap_getitem",
            "_mmap_setitem",
            "_mmap_repr",
        ] {
            assert!(function("mmap", name).is_some(), "mmap.{name}");
        }
        for name in [
            "ACCESS_DEFAULT",
            "ACCESS_READ",
            "ACCESS_WRITE",
            "ACCESS_COPY",
            "PAGESIZE",
            "MAP_SHARED",
            "PROT_READ",
        ] {
            assert_eq!(constant("mmap", name).unwrap().ty, Type::Int, "{name}");
        }
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
            "CodeType",
            "GenericAlias",
            "MappingProxyType",
            "NoneType",
            "UnionType",
        ] {
            assert_eq!(constant("types", name).unwrap().ty, Type::Any, "{name}");
        }
        let function_type = function("types", "FunctionType").unwrap();
        assert_eq!((function_type.min_args, function_type.max_args), (1, 6));
        assert_eq!(
            function_type.params,
            &[
                "code",
                "globals",
                "name",
                "argdefs",
                "closure",
                "kwdefaults"
            ][..]
        );
        let namespace = function("types", "SimpleNamespace").unwrap();
        assert_eq!((namespace.min_args, namespace.max_args), (0, 1));
        let dynamic = function("types", "DynamicClassAttribute").unwrap();
        assert_eq!((dynamic.min_args, dynamic.max_args), (0, 4));
        assert_eq!(dynamic.params, &["fget", "fset", "fdel", "doc"][..]);
        assert!(dynamic.dynamic_keywords);
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
        assert_eq!(
            (
                function("itertools", "permutations").unwrap().min_args,
                function("itertools", "permutations").unwrap().max_args
            ),
            (1, 2)
        );
        let product = function("itertools", "product").unwrap();
        assert_eq!(product.min_args, 0);
        assert_eq!(product.max_args, usize::MAX);
        assert!(product.dynamic_keywords);
        for name in ["combinations", "combinations_with_replacement"] {
            let combinator = function("itertools", name).unwrap();
            assert_eq!((combinator.min_args, combinator.max_args), (2, 2));
            assert_eq!(combinator.ret, Type::List);
            assert!(!combinator.dynamic_keywords);
        }
        assert_eq!(
            (
                function("itertools", "takewhile").unwrap().min_args,
                function("itertools", "takewhile").unwrap().max_args
            ),
            (2, 2)
        );
    }

    #[test]
    fn ast_fractions_and_importlib_are_registered() {
        assert_eq!(function("ast", "literal_eval").unwrap().max_args, 1);
        assert!(function("ast", "arg").is_some(), "ast.arg constructor");
        assert_eq!(constant("ast", "arguments").unwrap().ty, Type::Any);
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
        let wraps = function("functools", "wraps").expect("wraps");
        assert_eq!((wraps.min_args, wraps.max_args), (1, 3));
        assert_eq!(wraps.params, &["wrapped", "assigned", "updated"][..]);
        let update_wrapper = function("functools", "update_wrapper").expect("update_wrapper");
        assert_eq!((update_wrapper.min_args, update_wrapper.max_args), (2, 4));
        assert_eq!(
            update_wrapper.params,
            &["wrapper", "wrapped", "assigned", "updated"][..]
        );
        let wraps_apply = function("functools", "_wraps_apply").expect("_wraps_apply");
        assert_eq!((wraps_apply.min_args, wraps_apply.max_args), (4, 4));
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
        let bit_or = function("operator", "or_").expect("operator.or_");
        assert_eq!((bit_or.min_args, bit_or.max_args), (2, 2));
        assert_eq!(bit_or.ret, Type::Any);
        let index = function("operator", "index").expect("operator.index");
        assert_eq!((index.min_args, index.max_args), (1, 1));
        assert_eq!(index.ret, Type::Int);
        let getitem = function("operator", "getitem").expect("operator.getitem");
        assert_eq!((getitem.min_args, getitem.max_args), (2, 2));
        assert_eq!(getitem.ret, Type::Any);
        let setitem = function("operator", "setitem").expect("operator.setitem");
        assert_eq!((setitem.min_args, setitem.max_args), (3, 3));
        assert_eq!(setitem.ret, Type::None);
        let delitem = function("operator", "delitem").expect("operator.delitem");
        assert_eq!((delitem.min_args, delitem.max_args), (2, 2));
        assert_eq!(delitem.ret, Type::None);
        assert_eq!(
            function("_operator", "getitem").map(|spec| spec.min_args),
            Some(2)
        );
        assert_eq!(
            function("_operator", "index").map(|spec| spec.min_args),
            Some(1)
        );
        assert_eq!(
            function("_operator", "or_").map(|spec| spec.min_args),
            Some(2)
        );
        for name in [
            "_unwrap_partial",
            "_unwrap_partialmethod",
            "_singledispatchmethod_get",
        ] {
            assert!(function("functools", name).is_some(), "{name}");
        }
        let get = function("functools", "_singledispatchmethod_get").unwrap();
        assert_eq!((get.min_args, get.max_args), (1, 3));
        // `partialmethod` is both a real descriptor class at runtime and a
        // module-call constructor; `Placeholder` is the singleton constant.
        let partialmethod = function("functools", "partialmethod").expect("partialmethod");
        assert_eq!(partialmethod.min_args, 1);
        assert_eq!(partialmethod.max_args, usize::MAX);
        assert!(partialmethod.dynamic_keywords);
        assert_eq!(constant("functools", "Placeholder").unwrap().ty, Type::Any);
        assert_eq!(
            constant("functools", "_PlaceholderType").unwrap().ty,
            Type::Any
        );
        assert_eq!(constant("_functools", "Placeholder").unwrap().ty, Type::Any);
        for name in ["WRAPPER_ASSIGNMENTS", "WRAPPER_UPDATES"] {
            assert_eq!(constant("functools", name).unwrap().ty, Type::Any);
        }
        // `cached_property(func)` is a descriptor class and a module-call
        // constructor; the getter may also be passed by keyword.
        let cached_property = function("functools", "cached_property").expect("cached_property");
        assert_eq!((cached_property.min_args, cached_property.max_args), (1, 1));
        assert_eq!(cached_property.params, &["func"][..]);
        assert_eq!(cached_property.ret, Type::Any);
        // Keyword-dynamic so `cached_property(func=...)` reaches the module
        // dispatcher, which rejects any other name with `__init__`'s error.
        assert!(cached_property.dynamic_keywords);
        assert_eq!(
            function("_functools", "cached_property").map(|spec| spec.min_args),
            Some(1)
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

    #[test]
    fn math_log2_is_registered_as_a_float_function() {
        for name in ["log", "log10", "log2"] {
            let spec = function("math", name).unwrap_or_else(|| panic!("math.{name}"));
            assert_eq!((spec.min_args, spec.max_args), (1, 1), "{name}");
            assert_eq!(spec.ret, Type::Float, "{name}");
        }
    }

    #[test]
    fn math_prod_is_registered_with_a_keyword_only_start() {
        let prod = function("math", "prod").expect("math.prod");
        // `iterable` is the only positional slot; `start` is keyword-only.
        assert_eq!((prod.min_args, prod.max_args), (1, 1));
        assert_eq!(prod.ret, Type::Any);
        assert!(prod.dynamic_keywords);
        assert_eq!(prod.params, &["iterable"][..]);
    }

    #[test]
    fn math_classification_predicates_are_registered_as_bool_functions() {
        for name in ["isfinite", "isnan", "isinf", "isclose"] {
            let spec = function("math", name).unwrap_or_else(|| panic!("math.{name}"));
            assert_eq!(spec.ret, Type::Bool, "{name}");
        }
        for name in ["isfinite", "isnan", "isinf"] {
            let spec = function("math", name).expect(name);
            assert_eq!((spec.min_args, spec.max_args), (1, 1), "{name}");
            assert!(spec.params.is_empty(), "{name}");
            assert!(!spec.dynamic_keywords, "{name}");
        }
        let isclose = function("math", "isclose").expect("math.isclose");
        // `rel_tol` and `abs_tol` are keyword-only, so the positional arity is
        // the mandatory `a`/`b` pair.
        assert_eq!((isclose.min_args, isclose.max_args), (2, 2));
        assert_eq!(isclose.params, &["a", "b"][..]);
        assert!(isclose.dynamic_keywords);
    }

    #[test]
    fn inspect_coroutine_markers_and_nested_asyncio_specs_are_declared() {
        let marker = function("inspect", "markcoroutinefunction").expect("markcoroutinefunction");
        assert_eq!((marker.min_args, marker.max_args), (1, 1));
        assert_eq!(marker.params, &["func"][..]);
        assert_eq!(marker.ret, Type::Any);
        let predicate = function("inspect", "iscoroutinefunction").expect("iscoroutinefunction");
        assert_eq!(predicate.ret, Type::Bool);

        assert_eq!(
            crate::stdlib::module("asyncio.coroutines")
                .expect("asyncio.coroutines spec")
                .name,
            "asyncio.coroutines"
        );
        assert_eq!(
            constant("asyncio.coroutines", "_is_coroutine")
                .expect("_is_coroutine")
                .ty,
            Type::Any
        );
        // `from asyncio import coroutines` sees the nested module as a
        // dynamically typed constant.
        assert_eq!(
            constant("asyncio", "coroutines")
                .expect("asyncio.coroutines")
                .ty,
            Type::Any
        );
        // The nested module is a package submodule of `asyncio`.
        assert_eq!(
            crate::stdlib::module("asyncio").expect("asyncio spec").name,
            "asyncio"
        );
    }

    #[test]
    fn sys_registry_and_builtins_specs_are_declared() {
        for (name, ty) in [
            ("path", Type::List),
            ("path_importer_cache", Type::Dict),
            ("modules", Type::Dict),
            ("builtin_module_names", Type::Tuple),
            ("prefix", Type::Str),
            ("base_prefix", Type::Str),
            ("exec_prefix", Type::Str),
            ("last_exc", Type::Any),
            ("last_type", Type::Any),
            ("last_value", Type::Any),
            ("last_traceback", Type::Any),
        ] {
            assert_eq!(constant("sys", name).expect(name).ty, ty, "{name}");
        }
        let exc_info = function("sys", "exc_info").expect("sys.exc_info");
        assert_eq!((exc_info.min_args, exc_info.max_args), (0, 0));
        assert_eq!(exc_info.ret, Type::Tuple);
        let excepthook = function("sys", "excepthook").expect("sys.excepthook");
        assert_eq!((excepthook.min_args, excepthook.max_args), (3, 3));
        assert_eq!(excepthook.ret, Type::None);
        let get_limit = function("sys", "getrecursionlimit").expect("getrecursionlimit");
        assert_eq!(
            (get_limit.min_args, get_limit.max_args, get_limit.ret),
            (0, 0, Type::Int)
        );
        let set_limit = function("sys", "setrecursionlimit").expect("setrecursionlimit");
        assert_eq!((set_limit.min_args, set_limit.max_args), (1, 1));
        assert_eq!(set_limit.ret, Type::None);
        let frame_module = function("sys", "_getframemodulename").expect("_getframemodulename");
        assert_eq!(
            (
                frame_module.min_args,
                frame_module.max_args,
                frame_module.ret
            ),
            (0, 1, Type::Any)
        );
        let frame = function("sys", "_getframe").expect("_getframe");
        assert_eq!(
            (frame.min_args, frame.max_args, frame.ret),
            (0, 1, Type::Any)
        );
        assert_eq!(
            crate::stdlib::module("builtins")
                .expect("builtins spec")
                .name,
            "builtins"
        );
        for name in [
            "len",
            "print",
            "setattr",
            "compile",
            "__import__",
            "classmethod",
            "staticmethod",
            "property",
            "super",
            "hex",
            "bin",
            "oct",
        ] {
            assert!(function("builtins", name).is_some(), "{name}");
        }
        for name in ["hex", "bin", "oct"] {
            let conversion = function("builtins", name).expect(name);
            assert_eq!((conversion.min_args, conversion.max_args), (1, 1));
            assert_eq!(conversion.ret, Type::Str);
        }
        let classmethod = function("builtins", "classmethod").unwrap();
        assert_eq!((classmethod.min_args, classmethod.max_args), (1, 1));
        let staticmethod = function("builtins", "staticmethod").unwrap();
        assert_eq!((staticmethod.min_args, staticmethod.max_args), (1, 1));
        let property = function("builtins", "property").unwrap();
        assert_eq!((property.min_args, property.max_args), (1, 4));
        assert_eq!(property.params, &["fget", "fset", "fdel", "doc"][..]);
        for name in [
            "ValueError",
            "ImportError",
            "ModuleNotFoundError",
            "None",
            "True",
            "False",
            "Ellipsis",
            "NotImplemented",
            "__debug__",
            "object",
        ] {
            assert!(constant("builtins", name).is_some(), "{name}");
        }
    }

    #[test]
    fn warnings_marshal_and_tokenize_specs_are_registered() {
        for module in ["warnings", "_warnings"] {
            for name in [
                "warn",
                "warn_explicit",
                "filterwarnings",
                "simplefilter",
                "resetwarnings",
                "showwarning",
                "formatwarning",
            ] {
                assert!(function(module, name).is_some(), "{module}.{name}");
            }
            assert_eq!(constant(module, "filters").unwrap().ty, Type::List);
        }
        let warn = function("warnings", "warn").unwrap();
        assert_eq!((warn.min_args, warn.max_args), (1, 3));
        assert_eq!(warn.params, &["message", "category", "stacklevel"][..]);
        // `skip_file_prefixes` is keyword-only: it is not a positional slot but
        // the runtime keyword binder accepts it (and rejects unknown names).
        assert!(warn.dynamic_keywords, "skip_file_prefixes is a keyword");
        assert!(!warn.params.contains(&"skip_file_prefixes"));
        let explicit = function("warnings", "warn_explicit").unwrap();
        assert_eq!((explicit.min_args, explicit.max_args), (4, 7));

        let dumps = function("marshal", "dumps").unwrap();
        assert_eq!((dumps.min_args, dumps.max_args), (1, 2));
        assert_eq!(dumps.ret, Type::Bytes);
        assert_eq!(function("marshal", "load").unwrap().min_args, 1);
        assert_eq!(constant("marshal", "version").unwrap().ty, Type::Int);

        let tokenizer = function("_tokenize", "TokenizerIter").unwrap();
        assert_eq!(tokenizer.min_args, 1);
        assert_eq!(
            tokenizer.params,
            &["source", "encoding", "extra_tokens"][..]
        );
    }

    #[test]
    fn token_codecs_and_text_io_wrapper_specs_are_registered() {
        for name in ["ISTERMINAL", "ISNONTERMINAL", "ISEOF"] {
            assert_eq!(function("token", name).unwrap().ret, Type::Bool);
        }
        for name in ["OP", "ENCODING", "ENDMARKER", "N_TOKENS", "NT_OFFSET"] {
            assert_eq!(constant("token", name).unwrap().ty, Type::Int, "{name}");
        }
        for name in ["tok_name", "EXACT_TOKEN_TYPES"] {
            assert_eq!(constant("token", name).unwrap().ty, Type::Dict, "{name}");
        }
        assert_eq!(constant("token", "__all__").unwrap().ty, Type::List);

        // `codecs` is a distinct module from `_codecs`; both reach lookup.
        assert_eq!(function("codecs", "lookup").unwrap().max_args, 1);
        assert_eq!(function("_codecs", "lookup").unwrap().max_args, 1);
        for name in [
            "BOM_UTF8",
            "BOM_UTF16_LE",
            "BOM_UTF16_BE",
            "BOM_UTF32_LE",
            "BOM_UTF32_BE",
            "BOM_LE",
            "BOM_BE",
            "BOM",
        ] {
            assert_eq!(constant("codecs", name).unwrap().ty, Type::Bytes, "{name}");
        }

        let wrapper = function("io", "TextIOWrapper").unwrap();
        assert_eq!((wrapper.min_args, wrapper.max_args), (1, 6));
        assert_eq!(
            wrapper.params,
            &[
                "buffer",
                "encoding",
                "errors",
                "newline",
                "line_buffering",
                "write_through"
            ][..]
        );
        assert!(function("_io", "TextIOWrapper").is_some());
    }

    #[test]
    fn collections_abc_gc_atexit_and_site_specs_are_registered() {
        let required = [
            "Buffer",
            "Iterable",
            "Iterator",
            "Sized",
            "Container",
            "Collection",
            "Sequence",
            "MutableSequence",
            "Mapping",
            "MutableMapping",
            "Set",
            "MutableSet",
            "Hashable",
            "Callable",
            "Generator",
            "Coroutine",
            "Awaitable",
            "AsyncIterable",
            "AsyncIterator",
        ];
        for module in ["collections.abc", "_collections_abc"] {
            let spec = crate::stdlib::module(module).unwrap();
            if module == "_collections_abc" {
                // Native method bodies: the generic `register` plus Buffer's
                // `__buffer__` and `__subclasshook__`.
                assert_eq!(spec.functions.len(), 3);
                for name in ["_abc_register", "_abc_buffer", "_abc_buffer_subclasshook"] {
                    assert!(function(module, name).is_some(), "{name}");
                }
            } else {
                assert!(spec.functions.is_empty());
            }
            for &name in &required {
                assert_eq!(constant(module, name).unwrap().ty, Type::Any, "{name}");
            }
        }
        assert_eq!(constant("collections", "abc").unwrap().ty, Type::Any);
        let ordered = function("collections", "OrderedDict").unwrap();
        assert_eq!((ordered.min_args, ordered.max_args), (0, 1));
        assert!(ordered.dynamic_keywords);
        assert!(function("_collections", "OrderedDict").is_some());
        let counter = function("collections", "Counter").unwrap();
        assert_eq!((counter.min_args, counter.max_args), (0, 1));
        assert!(counter.dynamic_keywords);
        assert!(function("_collections", "Counter").is_some());

        for name in [
            "collect",
            "isenabled",
            "enable",
            "disable",
            "get_count",
            "get_stats",
            "get_threshold",
            "set_threshold",
            "freeze",
            "unfreeze",
            "get_objects",
        ] {
            assert!(function("gc", name).is_some(), "gc.{name}");
        }
        assert_eq!(function("gc", "collect").unwrap().max_args, 1);
        assert_eq!(function("gc", "get_stats").unwrap().ret, Type::List);
        assert_eq!(function("gc", "get_count").unwrap().ret, Type::Tuple);

        assert_eq!(function("atexit", "register").unwrap().min_args, 1);
        assert_eq!(function("atexit", "register").unwrap().max_args, usize::MAX);
        assert!(function("atexit", "_run_exitfuncs").is_some());
        assert!(function("atexit", "unregister").is_some());
        assert!(function("atexit", "_clear").is_some());

        assert_eq!(constant("site", "ENABLE_USER_SITE").unwrap().ty, Type::Bool);
        assert_eq!(constant("site", "USER_SITE").unwrap().ty, Type::Any);
        assert_eq!(constant("site", "PREFIXES").unwrap().ty, Type::List);
        assert_eq!(function("site", "main").unwrap().min_args, 0);
        assert_eq!(function("site", "addsitedir").unwrap().min_args, 1);
    }

    #[test]
    fn module_table_lists_the_new_shims_once() {
        for name in [
            "warnings",
            "_warnings",
            "marshal",
            "_tokenize",
            "_collections_abc",
            "collections.abc",
            "gc",
            "atexit",
            "site",
        ] {
            assert!(crate::stdlib::module(name).is_some(), "{name} missing");
            assert_eq!(
                crate::stdlib::MODULES
                    .iter()
                    .filter(|module| module.name == name)
                    .count(),
                1,
                "{name} registered more than once"
            );
        }
    }

    #[test]
    fn importlib_submodules_are_registered_with_their_surface() {
        for name in [
            "importlib",
            "importlib.util",
            "importlib.machinery",
            "importlib.metadata",
            "importlib.resources",
        ] {
            assert!(crate::stdlib::module(name).is_some(), "{name} missing");
            assert_eq!(
                crate::stdlib::MODULES
                    .iter()
                    .filter(|module| module.name == name)
                    .count(),
                1,
                "{name} registered more than once"
            );
        }

        // Top-level names packages use, plus the submodule attributes that
        // make `importlib.util` and friends reachable without a leaf import.
        assert_eq!(function("importlib", "reload").unwrap().max_args, 1);
        assert_eq!(
            function("importlib", "invalidate_caches").unwrap().min_args,
            0
        );
        assert_eq!(
            function("importlib", "import_module").unwrap().params,
            &["name", "package"][..]
        );
        for name in ["util", "machinery", "metadata", "resources"] {
            assert_eq!(constant("importlib", name).unwrap().ty, Type::Any, "{name}");
        }

        // `importlib.util`
        let find_spec = function("importlib.util", "find_spec").expect("find_spec");
        assert_eq!((find_spec.min_args, find_spec.max_args), (1, 2));
        assert_eq!(find_spec.params, &["name", "package"][..]);
        let from_file =
            function("importlib.util", "spec_from_file_location").expect("spec_from_file_location");
        assert_eq!((from_file.min_args, from_file.max_args), (1, 4));
        assert_eq!(
            from_file.params,
            &["name", "location", "loader", "submodule_search_locations"][..]
        );
        assert_eq!(
            function("importlib.util", "module_from_spec")
                .unwrap()
                .max_args,
            1
        );
        assert_eq!(
            function("importlib.util", "resolve_name").unwrap().ret,
            Type::Str
        );
        let decode = function("importlib.util", "decode_source").expect("decode_source");
        assert_eq!(
            (decode.min_args, decode.max_args, decode.ret),
            (1, 1, Type::Str)
        );
        assert!(decode.params.is_empty());
        assert_eq!(
            constant("importlib.util", "LazyLoader").unwrap().ty,
            Type::Any
        );

        // `importlib.machinery`
        for name in [
            "ModuleSpec",
            "Loader",
            "SourceFileLoader",
            "ExtensionFileLoader",
            "BuiltinImporter",
            "FrozenImporter",
        ] {
            assert_eq!(
                constant("importlib.machinery", name).unwrap().ty,
                Type::Any,
                "importlib.machinery.{name}"
            );
        }
        assert_eq!(
            function("importlib.machinery", "_source_init")
                .unwrap()
                .max_args,
            3
        );
        assert_eq!(
            function("importlib.machinery", "_builtin_find_spec")
                .unwrap()
                .max_args,
            4
        );
        for name in ["SOURCE_SUFFIXES", "BYTECODE_SUFFIXES", "EXTENSION_SUFFIXES"] {
            assert_eq!(
                constant("importlib.machinery", name).unwrap().ty,
                Type::List,
                "importlib.machinery.{name}"
            );
        }
        assert_eq!(
            function("importlib.machinery", "all_suffixes").unwrap().ret,
            Type::List
        );

        // `importlib.metadata`
        assert_eq!(
            function("importlib.metadata", "version").unwrap().ret,
            Type::Str
        );
        assert_eq!(
            constant("importlib.metadata", "PackageNotFoundError")
                .unwrap()
                .ty,
            Type::Any
        );
        for name in ["distributions", "entry_points"] {
            let spec = function("importlib.metadata", name).expect(name);
            assert!(spec.dynamic_keywords, "{name} keeps its keyword filters");
            assert_eq!((spec.min_args, spec.max_args), (0, 0), "{name}");
        }
        for name in ["distribution", "metadata", "requires"] {
            assert_eq!(
                function("importlib.metadata", name).unwrap().max_args,
                1,
                "{name}"
            );
        }

        // `importlib.resources`
        assert_eq!(
            function("importlib.resources", "files").unwrap().max_args,
            1
        );
        for method in [
            "_traversable_joinpath",
            "_traversable_read_text",
            "_traversable_read_bytes",
            "_traversable_iterdir",
            "_traversable_is_file",
            "_traversable_is_dir",
            "_traversable_name",
            "_traversable_fspath",
        ] {
            let spec = function("importlib.resources", method).expect(method);
            assert!(spec.min_args >= 1, "{method} binds its receiver");
        }
        assert_eq!(
            function("importlib.resources", "_traversable_read_text")
                .unwrap()
                .ret,
            Type::Str
        );
        assert_eq!(
            function("importlib.resources", "_traversable_read_bytes")
                .unwrap()
                .ret,
            Type::Bytes
        );
    }
}
