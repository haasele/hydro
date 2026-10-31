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
        // `modf(x)`: the `(fractional, integral)` pair, both as floats.
        f("modf", 1, 1, Type::Tuple),
        // `copysign(x, y)`: magnitude of `x` with the sign of `y`.
        f("copysign", 2, 2, Type::Float),
        // `isqrt(n)`: integer square root of a non-negative integer.
        f("isqrt", 1, 1, Type::Int),
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
    // Single-dispatch generic functions: `singledispatch(func)` returns the
    // dispatcher object (a native class instance with `register`/`dispatch`),
    // `singledispatchmethod(func)` the descriptor, and `total_ordering(cls)`
    // installs the missing comparison methods on the class. CPython accepts
    // each single argument by name, so the entries are keyword-dynamic.
    kw_dynamic("singledispatch", 1, 1, Type::Any, &["func"]),
    kw_dynamic("singledispatchmethod", 1, 1, Type::Any, &["func"]),
    kw_dynamic("total_ordering", 1, 1, Type::Any, &["cls"]),
    // Target of the decorator `register(cls)` returns: a partial binds the
    // dispatcher and the class, so the decorated function arrives last.
    f("_singledispatch_register_apply", 3, 3, Type::Any),
    // Target of the partial each generated `total_ordering` method installs:
    // the partial binds the source operation's name and the installed method
    // name, so the call supplies `(self, other)`.
    f("_total_ordering_compare", 4, 4, Type::Any),
    // `cmp_to_key(mycmp)`: wraps an old-style two-argument comparison
    // function into a key function object.
    f("cmp_to_key", 1, 1, Type::Any),
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
    // `operator.and_(a, b)` / `operator.xor(a, b)`: bitwise AND/XOR (`&`, `^`).
    f("and_", 2, 2, Type::Any),
    f("xor", 2, 2, Type::Any),
    f("neg", 1, 1, Type::Any),
    f("pos", 1, 1, Type::Any),
    f("invert", 1, 1, Type::Any),
    f("inv", 1, 1, Type::Any),
    f("abs", 1, 1, Type::Any),
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
    // `methodcaller(name, /, *args, **kwargs)`: a callable that invokes the
    // named method. The runtime currently binds the name only (the `six`
    // dictionary-view helpers); extra arguments are rejected at call time.
    f("methodcaller", 1, 1, Type::Function),
    // `_compare_digest(a, b)`: the timing-safe comparison `hmac`/`secrets`
    // import from `_operator`.
    f("_compare_digest", 2, 2, Type::Bool),
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
        // `process_time()`: consumed CPU time of the current process.
        f("process_time", 0, 0, Type::Float),
        f("sleep", 1, 1, Type::None),
        // Broken-down local/UTC time as a `struct_time` sequence; both take an
        // optional epoch-seconds argument.
        f("localtime", 0, 1, Type::Any),
        f("gmtime", 0, 1, Type::Any),
        // `strftime(format[, t])`: formats either the given `struct_time` or
        // the current local time. Both parameters may be passed by name.
        kw("strftime", 1, 2, Type::Str, &["format", "t"]),
        // `strptime(string[, format])`: CPython's default format is
        // `%a %b %d %H:%M:%S %Y`; both parameters may be passed by name.
        kw("strptime", 1, 2, Type::Any, &["string", "format"]),
        // `ctime([secs])`: the `asctime(localtime(secs))` spelling with an
        // optional epoch-seconds argument.
        f("ctime", 0, 1, Type::Str),
        // `asctime([t])`: a nine-field time sequence (or `struct_time`) in
        // C `asctime` form; omitted means the current local time.
        f("asctime", 0, 1, Type::Str),
        // `mktime(t)`: the inverse of `localtime`, an epoch-seconds float for
        // a nine-field `struct_time` sequence.
        f("mktime", 1, 1, Type::Float),
        f("tzset", 0, 0, Type::None),
    ],
    constants: &[
        // `struct_time`: the nine-field broken-down time sequence.
        c("struct_time", Type::Any),
        // `tzname`: the `(std, dst)` timezone name pair.
        c("tzname", Type::Any),
        // Non-zero when daylight saving time is in effect.
        c("daylight", Type::Int),
        c("timezone", Type::Int),
        c("altzone", Type::Int),
        // The nine `struct_time` field names (`['tm_year', ...]`), not a count.
        c("_STRUCT_TM_ITEMS", Type::List),
    ],
};

pub static THREAD: ModuleSpec = ModuleSpec {
    name: "_thread",
    functions: &[
        f("start_new_thread", 2, 2, Type::Int),
        f("join", 1, 1, Type::None),
        f("get_ident", 0, 0, Type::Int),
        f("allocate_lock", 0, 0, Type::Any),
        // `_thread.RLock`: the reentrant lock class used by `threading`.
        f("RLock", 0, 0, Type::Any),
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
        // `random.sample(population, k)`: `k` distinct elements as a list.
        f("sample", 2, 2, Type::List),
        // `getrandbits(k)`: a non-negative random int with `k` bits drawn from
        // the global SplitMix64 generator.
        f("getrandbits", 1, 1, Type::Int),
        kw("Random", 0, 1, Type::Any, &["seed"]),
        // `random.SystemRandom(seed=None)`: every draw comes from the OS
        // entropy source; the optional seed is ignored.
        kw("SystemRandom", 0, 1, Type::Any, &["seed"]),
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
    constants: &[
        // The pure-path class family. Pydantic 2.14 looks every one of these
        // up by identity (its `_handlers`/`_generic_handlers` tables) and may
        // instantiate them as `os.PathLike` constructors, so they are class
        // values rather than module functions. `Path` itself stays a function:
        // it is the concrete, filesystem-capable constructor.
        c("PurePath", Type::Any),
        c("PosixPath", Type::Any),
        c("WindowsPath", Type::Any),
        c("PurePosixPath", Type::Any),
        c("PureWindowsPath", Type::Any),
    ],
};

/// `platform`: build and uname facts. `platform(aliased=False, terse=False)`
/// takes CPython's two documented keyword arguments; `python_build()` and
/// `python_compiler()` read the same `sys.version` provenance as the CPython
/// pure-Python implementation.
pub static PLATFORM: ModuleSpec = ModuleSpec {
    name: "platform",
    functions: &[
        f("system", 0, 0, Type::Str),
        f("machine", 0, 0, Type::Str),
        f("release", 0, 0, Type::Str),
        f("version", 0, 0, Type::Str),
        f("node", 0, 0, Type::Str),
        kw("platform", 0, 2, Type::Str, &["aliased", "terse"]),
        f("python_version", 0, 0, Type::Str),
        f("python_implementation", 0, 0, Type::Str),
        // `python_build()` -> `(buildno, builddate)` and
        // `python_compiler()` -> the compiler identification.
        f("python_build", 0, 0, Type::Tuple),
        f("python_compiler", 0, 0, Type::Str),
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
    constants: &[c("JSONDecodeError", Type::Any)],
};

pub static CSV: ModuleSpec = ModuleSpec {
    name: "csv",
    functions: &[
        kw("reader", 1, 1, Type::Any, &["file"]),
        kw("writer", 1, 1, Type::Any, &["file"]),
        kw("DictReader", 1, 1, Type::Any, &["file"]),
        kw("DictWriter", 2, 2, Type::Any, &["file", "fieldnames"]),
    ],
    constants: &[
        c("QUOTE_MINIMAL", Type::Int),
        c("QUOTE_ALL", Type::Int),
        c("QUOTE_NONNUMERIC", Type::Int),
        c("QUOTE_NONE", Type::Int),
        c("QUOTE_STRINGS", Type::Int),
        c("QUOTE_NOTNULL", Type::Int),
    ],
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
        // `getsize(filename)`/`getmtime(filename)`/`getatime(filename)`/
        // `getctime(filename)`: the `os.stat` fields of the path.
        f("getsize", 1, 1, Type::Int),
        f("getmtime", 1, 1, Type::Float),
        f("getatime", 1, 1, Type::Float),
        f("getctime", 1, 1, Type::Float),
        // `islink(path)`: whether the final path component is a symlink.
        f("islink", 1, 1, Type::Bool),
        // `samefile(path1, path2)`: same device and inode.
        f("samefile", 2, 2, Type::Bool),
        // `expanduser(path)`: `~`/`~user` expanded against the home directory.
        f("expanduser", 1, 1, Type::Str),
        f("abspath", 1, 1, Type::Str),
        f("splitext", 1, 1, Type::Tuple),
        f("split", 1, 1, Type::Tuple),
        // `normpath` is purely lexical and returns `bytes` for a `bytes`
        // operand, so the result type is dynamic. `splitdrive` always reports
        // an empty drive on POSIX, preserving a `bytes` operand's flavour.
        f("normpath", 1, 1, Type::Any),
        f("splitdrive", 1, 1, Type::Tuple),
        // `splitroot(path)`: the `(drive, root, tail)` triple (3.12+). Bytes
        // operands keep their flavour, so the type is dynamic.
        f("splitroot", 1, 1, Type::Any),
        // `normcase` is the identity on POSIX; `realpath(filename, *,
        // strict=False)` resolves symlinks component by component (the
        // keyword is parsed by the runtime).
        f("normcase", 1, 1, Type::Str),
        kw_dynamic("realpath", 1, 2, Type::Str, &["filename", "strict"]),
        // `expandvars(path)`: `$name`/`${name}` expansion against the
        // environment; str in, str out (bytes in, bytes out).
        f("expandvars", 1, 1, Type::Any),
        // `lexists(path)`: whether the path exists, without following a
        // broken final symlink.
        f("lexists", 1, 1, Type::Bool),
        // `commonpath(paths)`/`commonprefix(paths)`: the result follows the
        // operand flavour (`str` or `bytes`), so the type is dynamic.
        f("commonpath", 1, 1, Type::Any),
        f("commonprefix", 1, 1, Type::Any),
        // `relpath(path, start=os.curdir)`: a relative path; bytes operands
        // return bytes, hence the dynamic result type. `start` binds by name.
        kw("relpath", 1, 2, Type::Any, &["path", "start"]),
    ],
    constants: &[
        // The path-flavour constants, shared with `os` on POSIX.
        c("sep", Type::Str),
        // `altsep` is `None` on POSIX and `/` on Windows.
        c("altsep", Type::Any),
        c("pathsep", Type::Str),
        c("extsep", Type::Str),
        c("curdir", Type::Str),
        c("pardir", Type::Str),
        // CPython 3.14's `os.path.ALLOW_MISSING` sentinel: a unique instance
        // used to distinguish "argument omitted" from an explicit `None`.
        c("ALLOW_MISSING", Type::Any),
    ],
};

pub static OS: ModuleSpec = ModuleSpec {
    name: "os",
    functions: &[
        f("getpid", 0, 0, Type::Int),
        f("getppid", 0, 0, Type::Int),
        f("cpu_count", 0, 0, Type::Int),
        f("getcwd", 0, 0, Type::Str),
        // `getcwdb()`: the current directory as bytes.
        f("getcwdb", 0, 0, Type::Bytes),
        f("chdir", 1, 1, Type::None),
        // Linux glibc device-number helpers used by pandas' terminal checks.
        f("makedev", 2, 2, Type::Int),
        f("major", 1, 1, Type::Int),
        f("minor", 1, 1, Type::Int),
        f("getenv", 1, 2, Type::Any),
        // `os.fspath(path)`: str/bytes pass through, other objects are asked
        // for `__fspath__`. The result is str or bytes, which the runtime
        // reports as the dynamic type.
        kw("fspath", 1, 1, Type::Any, &["path"]),
        f("fork", 0, 0, Type::Int),
        f("waitpid", 2, 2, Type::Tuple),
        f("_exit", 1, 1, Type::None),
        f("remove", 1, 1, Type::None),
        // `unlink(path)`: the file-removal spelling. The runtime additionally
        // accepts `dir_fd` through its supplemental member spec, but only the
        // portable single-argument form is declared here.
        f("unlink", 1, 1, Type::None),
        f("rename", 2, 2, Type::None),
        // `symlink(src, dst, target_is_directory=False, *, dir_fd=None)` and
        // `link(src, dst, *, src_dir_fd=None, dst_dir_fd=None,
        // follow_symlinks=True)`: POSIX link creation. The Windows-only
        // `target_is_directory` slot is accepted and ignored on POSIX.
        kw(
            "symlink",
            2,
            3,
            Type::None,
            &["src", "dst", "target_is_directory"],
        ),
        kw("link", 2, 2, Type::None, &["src", "dst"]),
        f("access", 2, 2, Type::Bool),
        // `os.urandom(n)`: `n` bytes from the OS entropy source.
        f("urandom", 1, 1, Type::Bytes),
        // `os.stat(path, *, dir_fd=None, follow_symlinks=True)`: an
        // `os.stat_result`-shaped namespace with the documented attributes.
        kw(
            "stat",
            1,
            3,
            Type::Any,
            &["path", "dir_fd", "follow_symlinks"],
        ),
        // `os.lstat(path, *, dir_fd=None)`: `stat` without following the final
        // symbolic link. `dir_fd` is declared so keyword calls bind, but the
        // runtime rejects anything except `None` (there is no descriptor-
        // relative lookup).
        kw("lstat", 1, 2, Type::Any, &["path", "dir_fd"]),
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
        // `os.makedirs(name, mode=0o777, exist_ok=False)`: recursive directory
        // creation. `exist_ok` tolerates only an existing directory.
        kw("makedirs", 1, 3, Type::None, &["name", "mode", "exist_ok"]),
        // `os.mkdir(path, mode=0o777, *, dir_fd=None)`: a single directory
        // level, created through `mkdir(2)` so `mode` is honoured.
        kw("mkdir", 1, 3, Type::None, &["path", "mode", "dir_fd"]),
        // `os.rmdir(path, *, dir_fd=None)`: remove one empty directory.
        // `dir_fd` binds into the second positional slot; only `None` is
        // accepted (Hydro has no descriptor-relative path lookup).
        kw("rmdir", 1, 2, Type::None, &["path", "dir_fd"]),
        // `chmod(path, mode, *, dir_fd=None, follow_symlinks=True)` and
        // `mknod(path, mode=0o600, device=0, *, dir_fd=None)`: the keyword-only
        // options keep positional slots for the frontend binder; the runtime
        // accepts them at those slots (`dir_fd` must be `None`).
        kw(
            "chmod",
            1,
            4,
            Type::None,
            &["path", "mode", "dir_fd", "follow_symlinks"],
        ),
        kw(
            "mknod",
            1,
            4,
            Type::None,
            &["path", "mode", "device", "dir_fd"],
        ),
        // `chown(path, uid, gid, *, dir_fd=None, follow_symlinks=True)`.
        kw(
            "chown",
            3,
            5,
            Type::None,
            &["path", "uid", "gid", "dir_fd", "follow_symlinks"],
        ),
        // `utime(path, times=None, *, ns=None, dir_fd=None,
        // follow_symlinks=True)`.
        kw(
            "utime",
            1,
            5,
            Type::None,
            &["path", "times", "ns", "dir_fd", "follow_symlinks"],
        ),
        // `fsync(fd)`: flush a descriptor to stable storage; `fdatasync(fd)`.
        f("fsync", 1, 1, Type::None),
        f("fdatasync", 1, 1, Type::None),
        // `sched_getaffinity(pid)`: the CPU set as a `set[int]`.
        f("sched_getaffinity", 1, 1, Type::Set),
        // `os.system(command)`: runs a shell command and returns its wait
        // status.
        f("system", 1, 1, Type::Any),
        // `fsencode`/`fsdecode`: UTF-8 encoding under the surrogateescape
        // error handler; the result is `bytes` and `str` respectively.
        f("fsencode", 1, 1, Type::Any),
        f("fsdecode", 1, 1, Type::Any),
        // Process credentials; every one is positional-only in CPython.
        f("getuid", 0, 0, Type::Int),
        f("geteuid", 0, 0, Type::Int),
        f("getgid", 0, 0, Type::Int),
        f("getegid", 0, 0, Type::Int),
        // `os.close(fd)` and `os.fstat(fd)`: the latter returns an
        // `os.stat_result`-shaped namespace.
        f("close", 1, 1, Type::None),
        f("fstat", 1, 1, Type::Any),
        // `isatty(fd)`/`read(fd, n)`/`write(fd, data)`: descriptor-level
        // queries and I/O. Every parameter of these C-level functions is
        // positional-only, so no keyword names are declared.
        f("isatty", 1, 1, Type::Bool),
        f("read", 2, 2, Type::Bytes),
        f("write", 2, 2, Type::Int),
        // `fdopen(fd, mode='r', buffering=-1, encoding=None, errors=None,
        // newline=None, closefd=True, opener=None)`: CPython's Python-level
        // wrapper exposes all eight names. The runtime consumes the first
        // three and ignores the rest, which stay declared so call sites bind.
        kw(
            "fdopen",
            1,
            8,
            Type::Any,
            &[
                "fd",
                "mode",
                "buffering",
                "encoding",
                "errors",
                "newline",
                "closefd",
                "opener",
            ],
        ),
        // `readlink(path, *, dir_fd=None)`: the link target. `dir_fd` has a
        // positional slot for the frontend binder; the runtime accepts it at
        // that slot but only supports `None` (no descriptor-relative lookup).
        kw("readlink", 1, 2, Type::Any, &["path", "dir_fd"]),
        // `strerror(code)`: the system message for an errno value.
        f("strerror", 1, 1, Type::Str),
        // `scandir(path='.')`: an iterator of `os.DirEntry` values. The
        // argument may be passed by name (CPython's wrapper is Python code).
        kw("scandir", 0, 1, Type::Any, &["path"]),
        // Descriptor inheritance flags.
        // `lseek(fd, offset, whence)`: the descriptor's new absolute offset.
        f("lseek", 3, 3, Type::Int),
        // `openpty()`: the `(master, slave)` pseudo-terminal pair.
        f("openpty", 0, 0, Type::Tuple),
        // `setsid()`: create a session and return its id.
        f("setsid", 0, 0, Type::Int),
        f("login_tty", 1, 1, Type::None),
        // `get_blocking(fd)` / `readinto(fd, buffer[, n])`.
        f("get_blocking", 1, 1, Type::Bool),
        f("set_blocking", 2, 2, Type::None),
        f("readinto", 2, 3, Type::Int),
        f("ftruncate", 2, 2, Type::None),
        // `device_encoding(fd)`: the locale encoding for a terminal, else None.
        f("device_encoding", 1, 1, Type::Any),
        // `execlp(file, arg0, *args)`: replaces the process image.
        f("execlp", 2, usize::MAX, Type::Any),
        f("forkpty", 0, 0, Type::Tuple),
        // `walk(top)`: directory entries as a materialised list of
        // `(path, dirnames, filenames)` triples.
        f("walk", 1, 2, Type::List),
        f("set_inheritable", 2, 2, Type::None),
        f("get_inheritable", 1, 1, Type::Bool),
    ],
    constants: &[
        c("name", Type::Str),
        c("sep", Type::Str),
        c("pathsep", Type::Str),
        c("curdir", Type::Str),
        c("pardir", Type::Str),
        c("environ", Type::Dict),
        c("PathLike", Type::Any),
        c("devnull", Type::Str),
        c("linesep", Type::Str),
        c("O_BINARY", Type::Int),
        // `altsep` is `None` on POSIX; `defpath` is the default executable
        // search path.
        c("altsep", Type::Any),
        c("defpath", Type::Str),
        // `os.extsep`: the extension separator (`.`), and the seek whence
        // values shared with `io`.
        c("extsep", Type::Str),
        c("SEEK_SET", Type::Int),
        c("SEEK_CUR", Type::Int),
        c("SEEK_END", Type::Int),
        c("SEEK_DATA", Type::Int),
        c("SEEK_HOLE", Type::Int),
        // `os.WNOHANG`: the non-blocking flag for the `waitpid` family.
        c("WNOHANG", Type::Int),
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
        c("O_NOCTTY", Type::Int),
        // The remaining `os.open` flags.
        c("O_EXCL", Type::Int),
        c("O_NOFOLLOW", Type::Int),
        c("O_DIRECTORY", Type::Int),
        c("O_SYNC", Type::Int),
        c("O_DSYNC", Type::Int),
        c("O_ASYNC", Type::Int),
        c("O_NONBLOCK", Type::Int),
        c("O_NDELAY", Type::Int),
        c("O_ACCMODE", Type::Int),
    ],
};

/// `pwd`: the Unix password database. Every query returns a `struct_passwd`
/// namespace (`pw_name`, `pw_passwd`, `pw_uid`, `pw_gid`, `pw_gecos`,
/// `pw_dir`, `pw_shell`), which is why the result type is dynamic.
pub static PWD: ModuleSpec = ModuleSpec {
    name: "pwd",
    functions: &[
        // `struct_passwd(sequence)`: builds the namespace from the seven
        // documented fields, which the runtime requires as one sequence.
        f("struct_passwd", 1, 1, Type::Any),
        f("getpwnam", 1, 1, Type::Any),
        f("getpwuid", 1, 1, Type::Any),
        f("getpwall", 0, 0, Type::List),
    ],
    constants: &[],
};

/// `grp`: the Unix group database. Queries return a `struct_group` namespace
/// (`gr_name`, `gr_passwd`, `gr_gid`, `gr_mem`); `getgrall` returns a list of
/// them.
pub static GRP: ModuleSpec = ModuleSpec {
    name: "grp",
    functions: &[
        f("struct_group", 1, 1, Type::Any),
        f("getgrnam", 1, 1, Type::Any),
        f("getgrgid", 1, 1, Type::Any),
        f("getgrall", 0, 0, Type::List),
    ],
    constants: &[],
};

/// `resource`: process resource limits and usage. `getrlimit` returns the
/// `(soft, hard)` pair and `getrusage` a `struct_rusage` namespace; every
/// constant is an integer selector or limit value.
pub static RESOURCE: ModuleSpec = ModuleSpec {
    name: "resource",
    functions: &[
        f("getrlimit", 1, 1, Type::Tuple),
        f("setrlimit", 2, 2, Type::None),
        f("getrusage", 1, 1, Type::Any),
        f("getpagesize", 0, 0, Type::Int),
    ],
    constants: &[
        c("RLIM_INFINITY", Type::Int),
        // Limits available on every supported Unix.
        c("RLIMIT_CPU", Type::Int),
        c("RLIMIT_FSIZE", Type::Int),
        c("RLIMIT_DATA", Type::Int),
        c("RLIMIT_STACK", Type::Int),
        c("RLIMIT_CORE", Type::Int),
        c("RLIMIT_RSS", Type::Int),
        c("RLIMIT_NPROC", Type::Int),
        c("RLIMIT_NOFILE", Type::Int),
        c("RLIMIT_MEMLOCK", Type::Int),
        c("RLIMIT_AS", Type::Int),
        // Linux/Android extras.
        c("RLIMIT_LOCKS", Type::Int),
        c("RLIMIT_SIGPENDING", Type::Int),
        c("RLIMIT_MSGQUEUE", Type::Int),
        c("RLIMIT_NICE", Type::Int),
        c("RLIMIT_RTPRIO", Type::Int),
        c("RLIMIT_RTTIME", Type::Int),
        // `getrusage` selectors.
        c("RUSAGE_SELF", Type::Int),
        c("RUSAGE_CHILDREN", Type::Int),
        c("RUSAGE_THREAD", Type::Int),
    ],
};

/// `fcntl`: descriptor control, advisory locks and `ioctl`. The commands are
/// integer constants from the platform C library.
pub static FCNTL: ModuleSpec = ModuleSpec {
    name: "fcntl",
    functions: &[
        f("fcntl", 2, 3, Type::Any),
        f("ioctl", 2, 4, Type::Any),
        f("flock", 2, 2, Type::None),
        f("lockf", 2, 5, Type::None),
    ],
    constants: &[
        c("F_DUPFD", Type::Int),
        c("F_GETFD", Type::Int),
        c("F_SETFD", Type::Int),
        c("F_GETFL", Type::Int),
        c("F_SETFL", Type::Int),
        c("F_GETLK", Type::Int),
        c("F_SETLK", Type::Int),
        c("F_SETLKW", Type::Int),
        c("F_GETOWN", Type::Int),
        // Linux exposes the STREAMS push operation even though it fails at
        // runtime; `pty` imports it unconditionally.
        c("I_PUSH", Type::Int),
        c("F_SETOWN", Type::Int),
        // Linux/Android extras beyond the portable set.
        c("F_GETSIG", Type::Int),
        c("F_SETSIG", Type::Int),
        c("F_SETLEASE", Type::Int),
        c("F_GETLEASE", Type::Int),
        c("F_NOTIFY", Type::Int),
        c("F_DUPFD_CLOEXEC", Type::Int),
        c("F_SETPIPE_SZ", Type::Int),
        c("F_GETPIPE_SZ", Type::Int),
        c("F_ADD_SEALS", Type::Int),
        c("F_GET_SEALS", Type::Int),
        // Descriptor flag and `flock(2)` operation bits.
        c("FD_CLOEXEC", Type::Int),
        c("LOCK_SH", Type::Int),
        c("LOCK_EX", Type::Int),
        c("LOCK_NB", Type::Int),
        c("LOCK_UN", Type::Int),
    ],
};

/// `syslog`: the Unix system logger. Constants are the priorities, facilities
/// and `openlog` options.
pub static SYSLOG: ModuleSpec = ModuleSpec {
    name: "syslog",
    functions: &[
        f("openlog", 0, 3, Type::None),
        f("syslog", 1, 2, Type::None),
        f("closelog", 0, 0, Type::None),
        f("setlogmask", 1, 1, Type::Int),
    ],
    constants: &[
        // Priorities.
        c("LOG_EMERG", Type::Int),
        c("LOG_ALERT", Type::Int),
        c("LOG_CRIT", Type::Int),
        c("LOG_ERR", Type::Int),
        c("LOG_WARNING", Type::Int),
        c("LOG_NOTICE", Type::Int),
        c("LOG_INFO", Type::Int),
        c("LOG_DEBUG", Type::Int),
        // Facilities.
        c("LOG_KERN", Type::Int),
        c("LOG_USER", Type::Int),
        c("LOG_MAIL", Type::Int),
        c("LOG_DAEMON", Type::Int),
        c("LOG_AUTH", Type::Int),
        c("LOG_SYSLOG", Type::Int),
        c("LOG_LPR", Type::Int),
        c("LOG_NEWS", Type::Int),
        c("LOG_UUCP", Type::Int),
        c("LOG_CRON", Type::Int),
        c("LOG_AUTHPRIV", Type::Int),
        c("LOG_FTP", Type::Int),
        c("LOG_LOCAL0", Type::Int),
        c("LOG_LOCAL1", Type::Int),
        c("LOG_LOCAL2", Type::Int),
        c("LOG_LOCAL3", Type::Int),
        c("LOG_LOCAL4", Type::Int),
        c("LOG_LOCAL5", Type::Int),
        c("LOG_LOCAL6", Type::Int),
        c("LOG_LOCAL7", Type::Int),
        // `openlog` options.
        c("LOG_PID", Type::Int),
        c("LOG_CONS", Type::Int),
        c("LOG_ODELAY", Type::Int),
        c("LOG_NDELAY", Type::Int),
        c("LOG_NOWAIT", Type::Int),
        c("LOG_PERROR", Type::Int),
    ],
};

/// `_signal`: the C accelerator behind `signal`. All functions are
/// positional-only, as in CPython's C module; `SIGRTMIN`/`SIGRTMAX`/`NSIG` are
/// computed by the runtime, and `ItimerError` is an exception class.
pub static _SIGNAL: ModuleSpec = ModuleSpec {
    name: "_signal",
    functions: &[
        f("signal", 2, 2, Type::Any),
        f("getsignal", 1, 1, Type::Any),
        f("raise_signal", 1, 1, Type::None),
        f("strsignal", 1, 1, Type::Any),
        f("valid_signals", 0, 0, Type::Set),
        f("pthread_sigmask", 2, 2, Type::Set),
        // `sigwait(sigset)`: blocks until a signal from the set arrives and
        // returns its number.
        f("sigwait", 1, 1, Type::Int),
        f("alarm", 1, 1, Type::Int),
        f("pause", 0, 0, Type::None),
        f("setitimer", 2, 3, Type::Tuple),
        f("getitimer", 1, 1, Type::Tuple),
        f("siginterrupt", 2, 2, Type::None),
        // `sigpending()`: the set of signals pending delivery to the thread.
        f("sigpending", 0, 0, Type::Set),
        // `default_int_handler(signum, frame)` always raises
        // `KeyboardInterrupt`, so it never returns.
        f("default_int_handler", 2, 2, Type::Never),
    ],
    constants: &[
        c("SIGHUP", Type::Int),
        c("SIGINT", Type::Int),
        c("SIGQUIT", Type::Int),
        c("SIGILL", Type::Int),
        c("SIGTRAP", Type::Int),
        c("SIGABRT", Type::Int),
        c("SIGIOT", Type::Int),
        c("SIGBUS", Type::Int),
        c("SIGFPE", Type::Int),
        c("SIGKILL", Type::Int),
        c("SIGUSR1", Type::Int),
        c("SIGSEGV", Type::Int),
        c("SIGUSR2", Type::Int),
        c("SIGPIPE", Type::Int),
        c("SIGALRM", Type::Int),
        c("SIGTERM", Type::Int),
        c("SIGSTKFLT", Type::Int),
        c("SIGCHLD", Type::Int),
        c("SIGCLD", Type::Int),
        c("SIGCONT", Type::Int),
        c("SIGSTOP", Type::Int),
        c("SIGTSTP", Type::Int),
        c("SIGTTIN", Type::Int),
        c("SIGTTOU", Type::Int),
        c("SIGURG", Type::Int),
        c("SIGXCPU", Type::Int),
        c("SIGXFSZ", Type::Int),
        c("SIGVTALRM", Type::Int),
        c("SIGPROF", Type::Int),
        c("SIGWINCH", Type::Int),
        c("SIGIO", Type::Int),
        c("SIGPOLL", Type::Int),
        c("SIGPWR", Type::Int),
        c("SIGSYS", Type::Int),
        c("SIG_DFL", Type::Int),
        c("SIG_IGN", Type::Int),
        c("SIG_BLOCK", Type::Int),
        c("SIG_UNBLOCK", Type::Int),
        c("SIG_SETMASK", Type::Int),
        c("ITIMER_REAL", Type::Int),
        c("ITIMER_VIRTUAL", Type::Int),
        c("ITIMER_PROF", Type::Int),
        c("SIGRTMIN", Type::Int),
        c("SIGRTMAX", Type::Int),
        c("NSIG", Type::Int),
        c("ItimerError", Type::Any),
    ],
};

/// `termios`: the POSIX terminal control interface. Every flag, speed and
/// control-character index is an integer constant; `error` is the exception
/// class raised for terminal failures.
pub static TERMIOS: ModuleSpec = ModuleSpec {
    name: "termios",
    functions: &[
        f("tcgetattr", 1, 1, Type::List),
        f("tcsetattr", 3, 3, Type::None),
        f("tcsendbreak", 2, 2, Type::None),
        f("tcdrain", 1, 1, Type::None),
        f("tcflush", 2, 2, Type::None),
        f("tcflow", 2, 2, Type::None),
        f("tcgetwinsize", 1, 1, Type::Tuple),
        f("tcsetwinsize", 2, 2, Type::None),
    ],
    constants: &[
        // Input flags.
        c("IGNBRK", Type::Int),
        c("BRKINT", Type::Int),
        c("IGNPAR", Type::Int),
        c("PARMRK", Type::Int),
        c("INPCK", Type::Int),
        c("ISTRIP", Type::Int),
        c("INLCR", Type::Int),
        c("IGNCR", Type::Int),
        c("ICRNL", Type::Int),
        c("IUCLC", Type::Int),
        c("IXON", Type::Int),
        c("IXANY", Type::Int),
        c("IXOFF", Type::Int),
        c("IMAXBEL", Type::Int),
        c("IUTF8", Type::Int),
        c("OPOST", Type::Int),
        // Output flags and delays.
        c("OLCUC", Type::Int),
        c("ONLCR", Type::Int),
        c("OCRNL", Type::Int),
        c("ONOCR", Type::Int),
        c("ONLRET", Type::Int),
        c("OFILL", Type::Int),
        c("OFDEL", Type::Int),
        c("NLDLY", Type::Int),
        c("NL0", Type::Int),
        c("NL1", Type::Int),
        c("CRDLY", Type::Int),
        c("CR0", Type::Int),
        c("CR1", Type::Int),
        c("CR2", Type::Int),
        c("CR3", Type::Int),
        c("TABDLY", Type::Int),
        c("TAB0", Type::Int),
        c("TAB1", Type::Int),
        c("TAB2", Type::Int),
        c("TAB3", Type::Int),
        c("XTABS", Type::Int),
        c("BSDLY", Type::Int),
        c("BS0", Type::Int),
        c("BS1", Type::Int),
        c("VTDLY", Type::Int),
        c("VT0", Type::Int),
        c("VT1", Type::Int),
        c("FFDLY", Type::Int),
        c("FF0", Type::Int),
        c("FF1", Type::Int),
        c("CBAUD", Type::Int),
        c("CBAUDEX", Type::Int),
        c("CIBAUD", Type::Int),
        c("CSIZE", Type::Int),
        c("CS5", Type::Int),
        c("CS6", Type::Int),
        c("CS7", Type::Int),
        c("CS8", Type::Int),
        c("CSTOPB", Type::Int),
        c("CREAD", Type::Int),
        // Control modes.
        c("PARENB", Type::Int),
        c("PARODD", Type::Int),
        c("HUPCL", Type::Int),
        c("CLOCAL", Type::Int),
        c("CRTSCTS", Type::Int),
        c("ISIG", Type::Int),
        c("ICANON", Type::Int),
        c("XCASE", Type::Int),
        c("ECHO", Type::Int),
        c("ECHOE", Type::Int),
        c("ECHOK", Type::Int),
        c("ECHONL", Type::Int),
        c("NOFLSH", Type::Int),
        c("TOSTOP", Type::Int),
        c("ECHOCTL", Type::Int),
        c("ECHOPRT", Type::Int),
        c("ECHOKE", Type::Int),
        c("FLUSHO", Type::Int),
        c("PENDIN", Type::Int),
        c("IEXTEN", Type::Int),
        c("EXTPROC", Type::Int),
        c("VINTR", Type::Int),
        c("VQUIT", Type::Int),
        c("VERASE", Type::Int),
        // Control-character indices.
        c("VKILL", Type::Int),
        c("VEOF", Type::Int),
        c("VTIME", Type::Int),
        c("VMIN", Type::Int),
        c("VSWTC", Type::Int),
        c("VSWTCH", Type::Int),
        c("VSTART", Type::Int),
        c("VSTOP", Type::Int),
        c("VSUSP", Type::Int),
        c("VEOL", Type::Int),
        c("VREPRINT", Type::Int),
        c("VDISCARD", Type::Int),
        c("VWERASE", Type::Int),
        c("VLNEXT", Type::Int),
        c("VEOL2", Type::Int),
        c("CINTR", Type::Int),
        c("CQUIT", Type::Int),
        c("CERASE", Type::Int),
        c("CKILL", Type::Int),
        c("CEOF", Type::Int),
        c("CEOL", Type::Int),
        c("CEOT", Type::Int),
        c("CSTART", Type::Int),
        c("CSTOP", Type::Int),
        c("CSUSP", Type::Int),
        c("CDSUSP", Type::Int),
        c("CRPRNT", Type::Int),
        c("CWERASE", Type::Int),
        c("CFLUSH", Type::Int),
        c("CLNEXT", Type::Int),
        c("_POSIX_VDISABLE", Type::Int),
        c("B0", Type::Int),
        // Baud rates.
        c("B50", Type::Int),
        c("B75", Type::Int),
        c("B110", Type::Int),
        c("B134", Type::Int),
        c("B150", Type::Int),
        c("B200", Type::Int),
        c("B300", Type::Int),
        c("B600", Type::Int),
        c("B1200", Type::Int),
        c("B1800", Type::Int),
        c("B2400", Type::Int),
        c("B4800", Type::Int),
        c("B9600", Type::Int),
        c("B19200", Type::Int),
        c("B38400", Type::Int),
        c("EXTA", Type::Int),
        c("EXTB", Type::Int),
        c("B57600", Type::Int),
        c("B115200", Type::Int),
        c("B230400", Type::Int),
        c("B460800", Type::Int),
        c("B500000", Type::Int),
        c("B576000", Type::Int),
        c("B921600", Type::Int),
        c("B1000000", Type::Int),
        c("B1152000", Type::Int),
        c("B1500000", Type::Int),
        c("B2000000", Type::Int),
        c("B2500000", Type::Int),
        c("B3000000", Type::Int),
        c("B3500000", Type::Int),
        c("B4000000", Type::Int),
        c("B7200", Type::Int),
        c("B14400", Type::Int),
        c("B28800", Type::Int),
        c("B76800", Type::Int),
        // Line disciplines, queue selectors and ioctl request numbers.
        c("NCCS", Type::Int),
        c("N_TTY", Type::Int),
        c("N_SLIP", Type::Int),
        c("N_MOUSE", Type::Int),
        c("N_PPP", Type::Int),
        c("N_STRIP", Type::Int),
        c("TCSANOW", Type::Int),
        c("TCSADRAIN", Type::Int),
        c("TCSAFLUSH", Type::Int),
        c("TCIFLUSH", Type::Int),
        c("TCOFLUSH", Type::Int),
        c("TCIOFLUSH", Type::Int),
        c("TCOOFF", Type::Int),
        c("TCOON", Type::Int),
        c("TCIOFF", Type::Int),
        c("TCION", Type::Int),
        c("TCGETS", Type::Int),
        c("TCSETS", Type::Int),
        c("TCSETSW", Type::Int),
        c("TCSETSF", Type::Int),
        c("TCGETA", Type::Int),
        c("TCSETA", Type::Int),
        c("TCSETAW", Type::Int),
        c("TCSETAF", Type::Int),
        c("TCSBRK", Type::Int),
        c("TCXONC", Type::Int),
        c("TCFLSH", Type::Int),
        c("TCSBRKP", Type::Int),
        c("TIOCEXCL", Type::Int),
        c("TIOCNXCL", Type::Int),
        c("TIOCSCTTY", Type::Int),
        c("TIOCGPGRP", Type::Int),
        c("TIOCSPGRP", Type::Int),
        c("TIOCOUTQ", Type::Int),
        c("TIOCSTI", Type::Int),
        c("TIOCGWINSZ", Type::Int),
        c("TIOCSWINSZ", Type::Int),
        c("TIOCNOTTY", Type::Int),
        c("TIOCMGET", Type::Int),
        c("TIOCMSET", Type::Int),
        c("FIONREAD", Type::Int),
        c("TIOCINQ", Type::Int),
        c("FIONBIO", Type::Int),
        c("FIOCLEX", Type::Int),
        c("FIONCLEX", Type::Int),
        c("FIOASYNC", Type::Int),
        // `termios.error`: the exception class for terminal failures.
        c("error", Type::Any),
    ],
};

/// `array`: the compact homogeneous array type. The constructor is keyword
/// dynamic (`array(typecode[, initializer])`), and `typecodes` is the module's
/// supported type-code string.
pub static ARRAY: ModuleSpec = ModuleSpec {
    name: "array",
    functions: &[kw_dynamic("array", 1, 2, Type::Any, &[])],
    constants: &[c("typecodes", Type::Str)],
};

/// `cmath`: complex-number versions of the `math` functions. Unary functions
/// return `complex` (even for real results), `phase` returns a float,
/// `polar` a `(r, phi)` pair and `rect` a complex value; the constants mirror
/// `math` plus the imaginary `infj`/`nanj`.
pub static CMATH: ModuleSpec = ModuleSpec {
    name: "cmath",
    functions: &[
        f("acos", 1, 1, Type::Complex),
        f("acosh", 1, 1, Type::Complex),
        f("asin", 1, 1, Type::Complex),
        f("asinh", 1, 1, Type::Complex),
        f("atan", 1, 1, Type::Complex),
        f("atanh", 1, 1, Type::Complex),
        f("cos", 1, 1, Type::Complex),
        f("cosh", 1, 1, Type::Complex),
        f("exp", 1, 1, Type::Complex),
        f("sin", 1, 1, Type::Complex),
        f("sinh", 1, 1, Type::Complex),
        f("sqrt", 1, 1, Type::Complex),
        f("tan", 1, 1, Type::Complex),
        f("tanh", 1, 1, Type::Complex),
        f("log10", 1, 1, Type::Complex),
        // `log(z[, base])`: the optional base keeps the complex result type.
        f("log", 1, 2, Type::Complex),
        f("phase", 1, 1, Type::Float),
        f("polar", 1, 1, Type::Tuple),
        f("rect", 2, 2, Type::Complex),
        f("isfinite", 1, 1, Type::Bool),
        f("isinf", 1, 1, Type::Bool),
        f("isnan", 1, 1, Type::Bool),
        // `isclose(a, b, *, rel_tol=1e-09, abs_tol=0.0)`: both tolerances are
        // keyword-only, so the runtime binds them (`dynamic_keywords`).
        kw_dynamic("isclose", 2, 4, Type::Bool, &[]),
    ],
    constants: &[
        c("pi", Type::Float),
        c("e", Type::Float),
        c("tau", Type::Float),
        c("inf", Type::Float),
        c("nan", Type::Float),
        c("infj", Type::Complex),
        c("nanj", Type::Complex),
    ],
};

pub static THREADING: ModuleSpec = ModuleSpec {
    name: "threading",
    functions: &[
        kw("Thread", 1, 3, Type::Any, &["target", "args", "name"]),
        f("Lock", 0, 0, Type::Any),
        f("RLock", 0, 0, Type::Any),
        f("Event", 0, 0, Type::Any),
        // `Condition(lock=None)`: lock/notify surface used by
        // `concurrent.futures`; the runtime backs it with the Event object.
        f("Condition", 0, 1, Type::Any),
        f("get_ident", 0, 0, Type::Int),
        f("active_count", 0, 0, Type::Int),
        // `current_thread()`/`main_thread()`: `Thread` objects representing the
        // running and the interpreter's main thread.
        f("current_thread", 0, 0, Type::Any),
        f("main_thread", 0, 0, Type::Any),
        f("_threadbase_init", 1, 4, Type::None),
        f("_threadbase_start", 1, 1, Type::None),
        f("_threadbase_join", 1, 2, Type::None),
        f("_threadbase_is_alive", 1, 1, Type::Bool),
        f("_threadbase_run", 1, 1, Type::None),
        // `threading.settrace(func)`: install the per-thread trace hook. It
        // exists for imports; Hydro has no trace callback machinery.
        f("settrace", 1, 1, Type::None),
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
        // `get_context(method=None)`: the process-start context. Hydro's
        // single-process runtime exposes the module's own factories.
        kw("get_context", 0, 1, Type::Any, &["method"]),
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
        // `Pickler(file, protocol=None, *, fix_imports=True,
        // buffer_callback=None)`: the streaming writer class. Declared as a
        // module function so `pickle.Pickler(...)` lowers as a module call;
        // the runtime value is the class.
        kw("Pickler", 1, 2, Type::Any, &["file", "protocol"]),
        // `pickle._Pickler`: CPython's pure-Python writer; HYD1 has one
        // writer, so both spellings are the same class.
        kw("_Pickler", 1, 2, Type::Any, &["file", "protocol"]),
        // `Unpickler(file, *, fix_imports=True, encoding="ASCII",
        // errors="strict", buffers=None)`: the streaming reader. Declared as
        // a module function so `pickle.Unpickler(...)` lowers as a module
        // call; the runtime value is the cached class and applies its own
        // defaults for the keyword-only options.
        kw(
            "Unpickler",
            1,
            5,
            Type::Any,
            &["file", "fix_imports", "encoding", "errors", "buffers"],
        ),
        // `decode_long(data, bytes=None)`: the `pickletools` helper that
        // rebuilds a little-endian two's-complement integer. `bytes` is a
        // keyword-only compatibility slot.
        f("decode_long", 1, 2, Type::Any),
    ],
    constants: &[
        // `pickle.DEFAULT_PROTOCOL`: the protocol `dump`/`dumps` use when none
        // is given; `HIGHEST_PROTOCOL` is the newest one this runtime writes.
        c("DEFAULT_PROTOCOL", Type::Int),
        c("HIGHEST_PROTOCOL", Type::Int),
        // `pickle.__all__`: the exported name list (`from pickle import *`).
        c("__all__", Type::List),
        // The pickling exception classes; the runtime maps all three to its
        // cached pickling-error class.
        c("PickleError", Type::Any),
        c("PicklingError", Type::Any),
        c("UnpicklingError", Type::Any),
        // `pickle.bytes_types`: the byte-like types `loads` accepts
        // (`(bytes, bytearray)` in CPython).
        c("bytes_types", Type::Any),
    ],
};

pub static SYS: ModuleSpec = ModuleSpec {
    name: "sys",
    functions: &[
        f("exit", 0, 1, Type::None),
        // `sys.exc_info()` returns the `(type, value, traceback)` triple of
        // the exception being handled; `sys.excepthook` is the default hook
        // `sys.excepthook(type, value, traceback)`.
        f("exc_info", 0, 0, Type::Tuple),
        // `sys.exception()`: the instance of the exception being handled
        // (`None` outside a handler), the `exc_info()[1]` shorthand.
        f("exception", 0, 0, Type::Any),
        f("excepthook", 3, 3, Type::None),
        // Process-wide recursion limit. `setrecursionlimit` validates the new
        // limit like CPython (integer, above the current recursion depth) and
        // stores it for the whole process.
        f("getrecursionlimit", 0, 0, Type::Int),
        // `getsizeof(obj, default=None)`: the object's shallow size. Hydro
        // reports its own representation sizes, not CPython's.
        f("getsizeof", 1, 2, Type::Int),
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
        f("getdefaultencoding", 0, 0, Type::Str),
        // `sys.audit(event, *args)`: raises if a hook vetoes the event; the
        // trailing arguments are event-specific.
        f("audit", 1, usize::MAX, Type::None),
        // `sys.addaudithook(hook)`: appends a callable audit hook.
        f("addaudithook", 1, 1, Type::None),
        f("getfilesystemencoding", 0, 0, Type::Str),
        // Name of the filesystem error handler (`surrogateescape` on POSIX).
        f("getfilesystemencodeerrors", 0, 0, Type::Str),
        // Maximum number of decimal digits used when converting an int to
        // text (`sys.set_int_max_str_digits`'s companion).
        f("get_int_max_str_digits", 0, 0, Type::Int),
        f("getswitchinterval", 0, 0, Type::Float),
        f("setswitchinterval", 1, 1, Type::None),
        // `settrace(function)` / `setprofile(function)`: install (or clear,
        // with `None`) the global trace/profile callbacks. Hydro's runtime
        // has no callback hooks yet, so the names exist for imports.
        f("settrace", 1, 1, Type::None),
        f("setprofile", 1, 1, Type::None),
    ],
    constants: &[
        c("argv", Type::List),
        // `sys.float_info`: the platform IEEE-754 binary64 limits.
        c("float_info", Type::Any),
        // `sys.hash_info`: the stable numeric hash parameters.
        c("hash_info", Type::Any),
        // Process-wide importer cache; native modules and path entries may
        // stay absent, but the dict identity is stable.
        c("path_importer_cache", Type::Dict),
        c("executable", Type::Str),
        c("_base_executable", Type::Str),
        c("__excepthook__", Type::Any),
        // Interactive prompt strings read by `code`/`pdb`.
        c("ps1", Type::Str),
        c("ps2", Type::Str),
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
        // `sys.stdlib_module_names`: the frozenset of standard-library module
        // names this interpreter knows.
        c("stdlib_module_names", Type::Set),
        // `sys.path_hooks`: the path-entry finder hook list.
        c("path_hooks", Type::List),
        // `sys.flags`: the interpreter's command-line flag namespace.
        c("flags", Type::Any),
        // `sys.monitoring`/`sys.meta_path`: the monitor tool registry and the
        // ordered list of meta path finders.
        c("monitoring", Type::Any),
        c("meta_path", Type::Any),
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
        // `int(x=0, base=10)`: `base` is keyword-bindable (CPython rejects a
        // keyword `x`, but the slot binder cannot express that split).
        kw("int", 0, 2, Type::Int, &["x", "base"]),
        f("float", 0, 1, Type::Float),
        f("abs", 1, 1, Type::Any),
        f("bool", 0, 1, Type::Bool),
        // `type(x)` reports an object's type; `type(name, bases, ns, **kwds)`
        // creates a class and forwards the extra names to the metaclass
        // resolved from the bases, so they are not declared parameters.
        kw_dynamic("type", 1, 3, Type::Any, &[]),
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
        // `dict([source], **kwargs)`: the single positional operand is a
        // mapping or an iterable of pairs, and every keyword name becomes an
        // entry layered on top (`dict(a=1)`, `dict({'a': 1}, b=2)`).
        kw_dynamic("dict", 0, 1, Type::Dict, &[]),
        f("sum", 1, 2, Type::Any),
        // `enumerate(iterable, start=0)`: the starting count may be passed by
        // name.
        kw("enumerate", 1, 2, Type::Any, &["iterable", "start"]),
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
        // `format(value, format_spec='')`: the specification defaults to the
        // empty string.
        kw("format", 1, 2, Type::Str, &["value", "format_spec"]),
        f("isinstance", 2, 2, Type::Bool),
        f("all", 1, 1, Type::Bool),
        f("any", 1, 1, Type::Bool),
        f("ord", 1, 1, Type::Int),
        f("chr", 1, 1, Type::Str),
        f("divmod", 2, 2, Type::Tuple),
        // `pow(base, exp, mod=None)`: two-argument exponentiation (the `**`
        // operator) and integer modular exponentiation. CPython 3.14 binds all
        // three parameters by name, and an explicit `mod=None` selects the
        // two-argument form.
        kw("pow", 2, 3, Type::Any, &["base", "exp", "mod"]),
        // Integer-base spellings: `hex`, `bin` and `oct` take exactly one
        // integer (including arbitrary-precision values) and return the
        // prefixed literal text.
        f("hex", 1, 1, Type::Str),
        f("bin", 1, 1, Type::Str),
        f("oct", 1, 1, Type::Str),
        f("callable", 1, 1, Type::Bool),
        // Descriptor factories (unqualified compiler builtins). `property`
        // keeps CPython's parameter names; the runtime accepts and does not
        // store the `doc` argument. Every slot defaults to `None`, so the
        // zero-argument form is valid.
        f("classmethod", 1, 1, Type::Any),
        f("staticmethod", 1, 1, Type::Any),
        kw(
            "property",
            0,
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
        // `compile(source, filename, mode, flags=0, dont_inherit=False,
        // optimize=-1, *, _feature_version=-1)`: every parameter binds by
        // name.
        kw(
            "compile",
            3,
            6,
            Type::Any,
            &[
                "source",
                "filename",
                "mode",
                "flags",
                "dont_inherit",
                "optimize",
            ],
        ),
        f("issubclass", 2, 2, Type::Bool),
        // The builtins spelling of `io.open`; same CPython parameter list.
        kw(
            "open",
            1,
            8,
            Type::Any,
            &[
                "file",
                "mode",
                "buffering",
                "encoding",
                "errors",
                "newline",
                "closefd",
                "opener",
            ],
        ),
        // Internal native method bodies attached to the cached object/scalar
        // classes. They are not part of CPython's builtins namespace, but the
        // module dispatcher must be able to resolve a bound method value.
        f("__object_new__", 0, 1, Type::Any),
        f("__object_init__", 0, 1, Type::None),
        f("__object_str__", 1, 1, Type::Str),
        f("__object_repr__", 1, 1, Type::Str),
        f("__object_format__", 1, 2, Type::Str),
        f("__object_eq__", 2, 2, Type::Bool),
        f("__object_ne__", 2, 2, Type::Bool),
        f("__object_hash__", 1, 1, Type::Int),
        f("__object_dir__", 1, 1, Type::List),
        f("__object_sizeof__", 1, 1, Type::Int),
        f("__object_reduce_ex__", 1, 2, Type::Any),
        f("__object_getattribute__", 2, 2, Type::Any),
        f("__scalar_new__", 1, 2, Type::Any),
        f("__scalar_format__", 1, 2, Type::Str),
        f("__scalar_repr__", 1, 1, Type::Str),
        f("__scalar_str__", 1, 1, Type::Str),
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
        // Python 3.10's encoding warning, also re-exported by `warnings`.
        c("EncodingWarning", Type::Any),
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
        // Python 3.15's immutable mapping. Pydantic 2.14 imports the name
        // behind a `sys.version_info >= (3, 15)` guard and keeps it in its
        // handler tables; the runtime provides a real constructible class so
        // the import and identity comparisons resolve (see the runtime's
        // `frozendict` implementation for the documented limitations).
        c("frozendict", Type::Any),
        // `object` is CPython's universal base class. It is a class value, not
        // a native function: `class C(object)`, `isinstance(x, object)` and
        // `object()` all need the runtime class. The bare name resolves like
        // every other `builtins` constant.
        c("object", Type::Any),
    ],
};

/// `io` and `_io` share the same function table.
const IO_FUNCTIONS: &[FunctionSpec] = &[
    // `open(file, mode='r', buffering=-1, encoding=None, errors=None,
    // newline=None, closefd=True, opener=None)`: every CPython parameter is
    // declared in signature order so keyword calls bind to the right slot.
    kw(
        "open",
        1,
        8,
        Type::Any,
        &[
            "file",
            "mode",
            "buffering",
            "encoding",
            "errors",
            "newline",
            "closefd",
            "opener",
        ],
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
    // `text_encoding(encoding, stacklevel=2)`: reports the encoding a call
    // would use, falling back to the locale when `encoding` is `None`.
    kw("text_encoding", 1, 2, Type::Str, &["encoding"]),
    // `open_code(path)`: opens a binary source file for the import system.
    f("open_code", 1, 1, Type::Any),
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
        c("UnsupportedOperation", Type::Any),
        // `io.Reader`/`io.Writer` are the ABCs from `Lib/io.py`;
        // `typing_extensions` uses them as its generic I/O protocols.
        c("Reader", Type::Any),
        c("Writer", Type::Any),
        // `seek` whence values shared with `os.SEEK_*`.
        c("SEEK_SET", Type::Int),
        c("SEEK_CUR", Type::Int),
        c("SEEK_END", Type::Int),
        c("SEEK_DATA", Type::Int),
        c("SEEK_HOLE", Type::Int),
        // The exported name list and the default `BufferedReader`/Writer
        // buffer size.
        c("__all__", Type::List),
        c("DEFAULT_BUFFER_SIZE", Type::Int),
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
        // `argparse.REMAINDER`: the `nargs` sentinel string that collects all
        // remaining arguments.
        c("REMAINDER", Type::Str),
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
        // `defaultdict(default_factory=None, /, initial=(), **kwargs)`: the
        // factory is positional-only and the trailing keywords are data keys,
        // so every keyword name is dynamic. A second positional argument
        // initialises the mapping; CPython rejects a third with "dict expected
        // at most 1 argument".
        kw_dynamic("defaultdict", 0, 2, Type::Any, &[]),
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
    constants: &[
        c("abc", Type::Any),
        c("deque", Type::Any),
        c("ChainMap", Type::Any),
        // `collections.UserDict`: the pure-Python dict wrapper used as a base
        // class by libraries that need dict behaviour with extra state.
        c("UserDict", Type::Any),
        // The list/str wrappers from the same `collections` surface; the
        // runtime serves them as class values with native methods.
        c("UserList", Type::Any),
        c("UserString", Type::Any),
    ],
};

/// Shared function table of `socket` and its `_socket` accelerator alias.
const SOCKET_FUNCTIONS: &[FunctionSpec] = &[
    // The runtime binds these keyword names itself, so the spec keeps the
    // parameter list empty (the frontend still validates the arity).
    kw_dynamic("socket", 0, 3, Type::Any, &[]),
    kw_dynamic(
        "create_connection",
        1,
        3,
        Type::Any,
        &["address", "timeout", "source_address"],
    ),
    // `create_server(address, *, family=AF_INET, backlog=None,
    // reuse_port=False, dualstack_ipv6=False)`: a listening socket. The
    // runtime binds the keyword-only options itself, like
    // `create_connection`.
    kw_dynamic("create_server", 1, 1, Type::Any, &[]),
    f("gethostname", 0, 0, Type::Str),
    f("socketpair", 0, 3, Type::Any),
    f("getaddrinfo", 2, 6, Type::List),
    f("gethostbyname", 1, 1, Type::Str),
    // `getfqdn([name])`: the fully qualified domain name; the optional name
    // binds by keyword in CPython's Python-level wrapper.
    kw("getfqdn", 0, 1, Type::Str, &["name"]),
    f("inet_pton", 2, 2, Type::Bytes),
    f("inet_ntop", 2, 2, Type::Str),
];

/// Shared constants: the option/protocol numbers plus the class spellings.
///
/// `SocketType` is the class `type()` reports for a socket; `error`,
/// `gaierror` and `timeout` are exception classes. Hydro has one tagged
/// `OSError` family for socket failures (including resolver and timeout
/// errors), so all three names resolve to that class.
const SOCKET_CONSTANTS: &[ConstantSpec] = &[
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
    // TCP-level options (`setsockopt(IPPROTO_TCP, ...)`). The Linux/Android
    // extras are the knobs long-lived clients tune.
    c("TCP_NODELAY", Type::Int),
    c("TCP_MAXSEG", Type::Int),
    c("TCP_CORK", Type::Int),
    c("TCP_KEEPIDLE", Type::Int),
    c("TCP_KEEPINTVL", Type::Int),
    c("TCP_KEEPCNT", Type::Int),
    c("TCP_SYNCNT", Type::Int),
    c("TCP_LINGER2", Type::Int),
    c("TCP_DEFER_ACCEPT", Type::Int),
    c("TCP_WINDOW_CLAMP", Type::Int),
    c("TCP_INFO", Type::Int),
    c("TCP_QUICKACK", Type::Int),
    c("TCP_CONGESTION", Type::Int),
    c("TCP_USER_TIMEOUT", Type::Int),
    c("TCP_FASTOPEN", Type::Int),
    c("TCP_FASTOPEN_CONNECT", Type::Int),
    c("TCP_ULP", Type::Int),
    c("TCP_NOTSENT_LOWAT", Type::Int),
    c("TCP_INQ", Type::Int),
    c("SHUT_RD", Type::Int),
    c("SHUT_WR", Type::Int),
    c("SHUT_RDWR", Type::Int),
    c("MSG_OOB", Type::Int),
    c("MSG_PEEK", Type::Int),
    c("MSG_DONTWAIT", Type::Int),
    c("MSG_WAITALL", Type::Int),
    c("MSG_NOSIGNAL", Type::Int),
    c("SocketType", Type::Any),
    // `socket._GLOBAL_DEFAULT_TIMEOUT`: the sentinel socket wrappers treat as
    // "use the global default".
    c("_GLOBAL_DEFAULT_TIMEOUT", Type::Any),
    c("error", Type::Any),
    c("gaierror", Type::Any),
    c("timeout", Type::Any),
];

pub static SOCKET: ModuleSpec = ModuleSpec {
    name: "socket",
    functions: SOCKET_FUNCTIONS,
    constants: SOCKET_CONSTANTS,
};

/// `_socket`: CPython's C accelerator.
///
/// pydantic/typing_extensions import it directly (`import _socket;
/// getattr(_socket, "CAPI", None)`), so it is a real native module with the
/// same surface as `socket` and no fake `CAPI` capsule.
pub static _SOCKET: ModuleSpec = ModuleSpec {
    name: "_socket",
    functions: SOCKET_FUNCTIONS,
    constants: SOCKET_CONSTANTS,
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
        // The private context factories `urllib.request`/`http.client` use:
        // `_create_stdlib_context`/`_create_unverified_context` accept the
        // `SSLContext` keyword set, `_create_default_https_context` the
        // `create_default_context` options.
        kw_dynamic("_create_stdlib_context", 0, 9, Type::Any, &[]),
        kw_dynamic("_create_unverified_context", 0, 9, Type::Any, &[]),
        kw_dynamic("_create_default_https_context", 0, 4, Type::Any, &[]),
    ],
    constants: &[
        // `ssl.Purpose` (the `SERVER_AUTH`/`CLIENT_AUTH` namespace) and the
        // `SSLSocket` class value.
        c("Purpose", Type::Any),
        c("SSLSocket", Type::Any),
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
        // `search`/`match`/`fullmatch(pattern, string, flags=0)` and
        // `findall(pattern, string, flags=0)`: the optional trailing flags
        // argument is positional (`re.search(pat, text, re.I)`), which the
        // runtime forwards to the regex builder.
        f("search", 2, 3, Type::Any),
        f("match", 2, 3, Type::Any),
        f("fullmatch", 2, 3, Type::Any),
        f("findall", 2, 3, Type::List),
        // `sub(pattern, repl, string, count=0, flags=0)`: the result is the
        // same kind as the input string, so a bytes pattern/subject returns
        // bytes. The static table has no overload channel, so the return is
        // dynamic (the runtime returns `bytes` for bytes patterns).
        f("sub", 3, 5, Type::Any),
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
        // The compiled-pattern and match classes. They are the cached classes
        // `type()` reports for those values, so `isinstance(p, re.Pattern)`
        // and `isinstance(m, re.Match)` hold; neither can be instantiated.
        c("Pattern", Type::Any),
        c("Match", Type::Any),
        // The private namespaces the deprecated `sre_compile`/
        // `sre_constants`/`sre_parse` wrappers import.
        c("_compiler", Type::Any),
        c("_constants", Type::Any),
        c("_parser", Type::Any),
    ],
};

/// Constants shared by the three private `re` namespaces.
///
/// CPython's `re/_compiler.py` and `re/_parser.py` start with
/// `from ._constants import *`, so the SRE limits, the flag bits and the
/// error class are present in all of them.
const RE_CONSTANTS: &[ConstantSpec] = &[
    c("MAGIC", Type::Int),
    c("MAXREPEAT", Type::Int),
    c("MAXGROUPS", Type::Int),
    c("SRE_FLAG_IGNORECASE", Type::Int),
    c("SRE_FLAG_LOCALE", Type::Int),
    c("SRE_FLAG_MULTILINE", Type::Int),
    c("SRE_FLAG_DOTALL", Type::Int),
    c("SRE_FLAG_UNICODE", Type::Int),
    c("SRE_FLAG_VERBOSE", Type::Int),
    c("SRE_FLAG_DEBUG", Type::Int),
    c("SRE_FLAG_ASCII", Type::Int),
    c("SRE_INFO_PREFIX", Type::Int),
    c("SRE_INFO_LITERAL", Type::Int),
    c("SRE_INFO_CHARSET", Type::Int),
    c("PatternError", Type::Any),
    c("error", Type::Any),
];

/// `re._constants`: the SRE limits, flag bits and error class.
pub static RE_CONSTANTS_MODULE: ModuleSpec = ModuleSpec {
    name: "re._constants",
    functions: &[],
    constants: RE_CONSTANTS,
};

/// `re._compiler`: the pattern compiler facade. `compile` is the engine
/// `re.compile` exposes and `isstring` the pattern-type predicate; the SRE
/// parse-tree builders are not provided.
pub static RE_COMPILER: ModuleSpec = ModuleSpec {
    name: "re._compiler",
    functions: &[
        f("compile", 1, 2, Type::Any),
        f("isstring", 1, 1, Type::Bool),
    ],
    constants: RE_CONSTANTS,
};

/// `re._parser`: the SRE parser facade. Hydro hands patterns straight to its
/// regex engine and builds no SRE parse tree, so the module exposes the shared
/// constants only.
pub static RE_PARSER: ModuleSpec = ModuleSpec {
    name: "re._parser",
    functions: &[],
    constants: RE_CONSTANTS,
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
        // The common POSIX names; the runtime resolves the platform value
        // from the Linux table.
        c("ENOENT", Type::Int),
        c("EINVAL", Type::Int),
        c("EACCES", Type::Int),
        c("EEXIST", Type::Int),
        c("ENOTDIR", Type::Int),
        c("EISDIR", Type::Int),
        c("ENOSPC", Type::Int),
        c("EROFS", Type::Int),
        c("EPIPE", Type::Int),
        c("ESPIPE", Type::Int),
        c("ECONNREFUSED", Type::Int),
        c("ENOTCONN", Type::Int),
        c("ETIMEDOUT", Type::Int),
        c("EAGAIN", Type::Int),
        c("EWOULDBLOCK", Type::Int),
        c("EINTR", Type::Int),
        c("ENOMEM", Type::Int),
        c("EBADF", Type::Int),
        c("EIO", Type::Int),
        c("ENOSYS", Type::Int),
        c("EOPNOTSUPP", Type::Int),
        c("ENOTSUP", Type::Int),
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
        // The uuencode helpers: `a2b_uu(data)` and `b2a_uu(data,
        // backtick=False)`.
        f("a2b_uu", 1, 1, Type::Bytes),
        kw("b2a_uu", 1, 2, Type::Bytes, &["data", "backtick"]),
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
        kw("urlsafe_b64decode", 1, 2, Type::Bytes, &["s", "padded"]),
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
        // `_translate(pat, star, question_mark)`: the two-wildcard translator
        // `glob` uses; returns `(fragments, star_indices)`.
        f("_translate", 3, 3, Type::Tuple),
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
    constants: &[
        // `GetoptError`: the `Exception` subclass `getopt` raises; `error` is
        // CPython's additional module-level alias for the same class.
        c("GetoptError", Type::Any),
        c("error", Type::Any),
    ],
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
        // `Popen(args, cwd=None, stdout=None, stderr=None, text=None,
        // env=None, close_fds=True, start_new_session=False, stdin=None,
        // encoding=None, errors=None, preexec_fn=None)`: the trailing names
        // are keyword-only in CPython and declared so call sites bind. The
        // runtime consumes `close_fds`/`start_new_session`; `stdin`,
        // `encoding`, `errors` and `preexec_fn` are accepted for signature
        // compatibility.
        kw(
            "Popen",
            1,
            14,
            Type::Any,
            &[
                "args",
                "cwd",
                "stdout",
                "stderr",
                "text",
                "env",
                "close_fds",
                "start_new_session",
                "stdin",
                "encoding",
                "errors",
                "preexec_fn",
                "bufsize",
                "shell",
            ],
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
        // `mkstemp(suffix=None, prefix=None, dir=None, text=False)`: an open
        // descriptor plus the path.
        kw(
            "mkstemp",
            0,
            4,
            Type::Tuple,
            &["suffix", "prefix", "dir", "text"],
        ),
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
        // `NamedTemporaryFile(mode='w+b', buffering=-1, encoding=None,
        // newline=None, suffix=None, prefix=None, dir=None, delete=True,
        // errors=None)`: the file object is deleted when it is closed.
        kw(
            "NamedTemporaryFile",
            0,
            9,
            Type::Any,
            &[
                "mode",
                "buffering",
                "encoding",
                "newline",
                "suffix",
                "prefix",
                "dir",
                "delete",
                "errors",
            ],
        ),
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
        // Streams between file-like objects; the buffer size defaults to
        // shutil's 256 KiB (`length=0`).
        kw("copyfileobj", 2, 3, Type::None, &["fsrc", "fdst", "length"]),
        kw("copy", 2, 2, Type::Str, &["src", "dst"]),
        kw("copy2", 2, 2, Type::Str, &["src", "dst"]),
        kw("rmtree", 1, 2, Type::None, &["path", "ignore_errors"]),
        kw("move", 2, 2, Type::Str, &["src", "dst"]),
        f("copymode", 2, 2, Type::None),
    ],
    constants: &[],
};

/// `textwrap`: text wrapping and indentation helpers.
///
/// `wrap`/`fill`/`shorten` accept the full `TextWrapper` keyword set, and
/// `TextWrapper` is declared as a constructor function (its runtime value is
/// the cached native class). Parameter order matches the runtime slot layout:
/// `width`, `initial_indent`, `subsequent_indent`, `expand_tabs`,
/// `replace_whitespace`, `fix_sentence_endings`, `break_long_words`,
/// `break_on_hyphens`, `drop_whitespace`, `tabsize`, `max_lines`,
/// `placeholder`. The maximum arity equals the parameter count because
/// compiled keyword calls pad every slot with `None`.
pub static TEXTWRAP: ModuleSpec = ModuleSpec {
    name: "textwrap",
    functions: &[
        f("dedent", 1, 1, Type::Str),
        kw("indent", 2, 3, Type::Str, &["text", "prefix", "predicate"]),
        kw(
            "wrap",
            1,
            13,
            Type::List,
            &[
                "text",
                "width",
                "initial_indent",
                "subsequent_indent",
                "expand_tabs",
                "replace_whitespace",
                "fix_sentence_endings",
                "break_long_words",
                "break_on_hyphens",
                "drop_whitespace",
                "tabsize",
                "max_lines",
                "placeholder",
            ],
        ),
        kw(
            "fill",
            1,
            13,
            Type::Str,
            &[
                "text",
                "width",
                "initial_indent",
                "subsequent_indent",
                "expand_tabs",
                "replace_whitespace",
                "fix_sentence_endings",
                "break_long_words",
                "break_on_hyphens",
                "drop_whitespace",
                "tabsize",
                "max_lines",
                "placeholder",
            ],
        ),
        kw(
            "shorten",
            2,
            13,
            Type::Str,
            &[
                "text",
                "width",
                "initial_indent",
                "subsequent_indent",
                "expand_tabs",
                "replace_whitespace",
                "fix_sentence_endings",
                "break_long_words",
                "break_on_hyphens",
                "drop_whitespace",
                "tabsize",
                "max_lines",
                "placeholder",
            ],
        ),
        kw(
            "TextWrapper",
            0,
            12,
            Type::Any,
            &[
                "width",
                "initial_indent",
                "subsequent_indent",
                "expand_tabs",
                "replace_whitespace",
                "fix_sentence_endings",
                "break_long_words",
                "break_on_hyphens",
                "drop_whitespace",
                "tabsize",
                "max_lines",
                "placeholder",
            ],
        ),
        // `TextWrapper.wrap`/`fill` instance methods: the receiver and the
        // text are the only arguments; the options come from the instance.
        f("_textwrapper_wrap", 2, 2, Type::List),
        f("_textwrapper_fill", 2, 2, Type::Str),
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
/// field's `field(...)` call from the class-body AST. The names still need a
/// runtime value: `dataclass` is inert, `field` returns a real `Field`
/// instance so executing the class body succeeds, and `MISSING` is the shared
/// sentinel the omitted field settings carry.
///
/// `replace(obj, **changes)` takes its single positional argument plus
/// arbitrary keyword names, so it forwards every keyword to the runtime
/// ([`kw_dynamic`]); the runtime shallow-copies the instance attributes.
/// `fields(class_or_instance)` returns the declared `Field` objects as a
/// tuple, and `asdict(obj, *, dict_factory=dict)` walks those fields to build
/// the nested mapping.
pub static DATACLASSES: ModuleSpec = ModuleSpec {
    name: "dataclasses",
    functions: &[
        // `field(*, default=MISSING, default_factory=MISSING, init=True, ...)`:
        // CPython takes no positional arguments; all names are keyword-only,
        // so the generic binder must not shift `default_factory` into a
        // `default` slot.
        kw_dynamic("field", 0, 0, Type::Any, &[]),
        kw_dynamic("replace", 1, 1, Type::Any, &["obj"]),
        // `asdict(obj, *, dict_factory=dict)`: recursively converts a dataclass
        // instance to a mapping; `dict_factory` is keyword-only, so keywords
        // are forwarded to the runtime.
        kw_dynamic("asdict", 1, 1, Type::Dict, &["obj"]),
        // `is_dataclass(obj)`: true for a dataclass or one of its instances.
        f("is_dataclass", 1, 1, Type::Bool),
        // `fields(class_or_instance)`: tuple of the declared `Field` objects.
        f("fields", 1, 1, Type::Tuple),
    ],
    constants: &[
        c("dataclass", Type::Any),
        c("InitVar", Type::Any),
        // `Field`: the descriptor `field()` builds.
        c("Field", Type::Any),
        // `MISSING`: the sentinel omitted `field()` settings carry.
        c("MISSING", Type::Any),
        c("_HAS_DEFAULT_FACTORY", Type::Any),
    ],
};

/// `datetime`: the `date`/`time`/`datetime`/`timedelta`/`tzinfo` classes plus
/// the constructors and methods the runtime exposes as native functions.
///
/// The class names are constants (`Type::Any`); the internal entries are the
/// `FunctionBody::Native` names their method tables carry, so the runtime's
/// arity validation knows every one of them. `_datetime_replace` takes
/// arbitrary keyword names (`replace(year=..., tzinfo=...)`), which is why it
/// is the one `kw_dynamic` entry.
pub static DATETIME: ModuleSpec = ModuleSpec {
    name: "datetime",
    functions: &[
        // Class-level constructors (`@staticmethod`s in CPython).
        f("_datetime_now", 0, 1, Type::Any),
        f("_datetime_utcnow", 0, 0, Type::Any),
        f("_date_today", 0, 0, Type::Any),
        f("_datetime_today", 0, 0, Type::Any),
        f("_datetime_fromtimestamp", 1, 2, Type::Any),
        f("_datetime_combine", 2, 2, Type::Any),
        // Instance methods.
        f("_datetime_strftime", 2, 2, Type::Str),
        f("_datetime_isoformat", 1, 2, Type::Str),
        f("_datetime_date", 1, 1, Type::Any),
        f("_datetime_time", 1, 1, Type::Any),
        f("_datetime_timestamp", 1, 1, Type::Float),
        f("_datetime_weekday", 1, 1, Type::Int),
        f("_datetime_isoweekday", 1, 1, Type::Int),
        f("_datetime_toordinal", 1, 1, Type::Int),
        f("_datetime_total_seconds", 1, 1, Type::Float),
        kw_dynamic("_datetime_replace", 1, 1, Type::Any, &[]),
        f("_datetime_utcoffset", 1, 1, Type::Any),
        f("_datetime_dst", 1, 1, Type::Any),
        f("_datetime_tzname", 1, 1, Type::Any),
        f("_datetime_tzinfo_utcoffset", 1, 2, Type::Any),
        f("_datetime_tzinfo_dst", 1, 2, Type::Any),
        f("_datetime_tzinfo_tzname", 1, 2, Type::Any),
        f("_datetime_repr", 1, 1, Type::Str),
        f("_datetime_str", 1, 1, Type::Str),
        // Rich comparisons and arithmetic.
        f("_datetime_eq", 2, 2, Type::Any),
        f("_datetime_lt", 2, 2, Type::Any),
        f("_datetime_le", 2, 2, Type::Any),
        f("_datetime_gt", 2, 2, Type::Any),
        f("_datetime_ge", 2, 2, Type::Any),
        f("_datetime_add", 2, 2, Type::Any),
        f("_datetime_radd", 2, 2, Type::Any),
        f("_datetime_sub", 2, 2, Type::Any),
        // `datetime.timezone` instance methods (receiver first).
        f("_datetime_timezone_utcoffset", 2, 2, Type::Any),
        f("_datetime_timezone_dst", 2, 2, Type::Any),
        f("_datetime_timezone_tzname", 2, 2, Type::Any),
        f("_datetime_timezone_fromutc", 2, 2, Type::Any),
        f("_datetime_timezone_eq", 2, 2, Type::Any),
    ],
    constants: &[
        c("date", Type::Any),
        c("time", Type::Any),
        c("datetime", Type::Any),
        c("timedelta", Type::Any),
        c("tzinfo", Type::Any),
        // `datetime.timezone`: the fixed-offset `tzinfo` subclass.
        c("timezone", Type::Any),
        // `datetime.UTC` (PEP 615): the very same object as
        // `datetime.timezone.utc`, so `UTC is timezone.utc` holds.
        c("UTC", Type::Any),
        c("MINYEAR", Type::Int),
        c("MAXYEAR", Type::Int),
        c("datetime_CAPI", Type::Any),
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
        // `no_type_check(arg)`: the identity decorator that stamps
        // `__no_type_check__` on a function (recursively on a class's own
        // members) and returns it unchanged. pydantic applies it to helper
        // functions. `no_type_check_decorator(decorator)` gives another
        // decorator the same effect and emits a DeprecationWarning (removed
        // in CPython 3.15).
        kw("no_type_check", 1, 1, Type::Any, &["arg"]),
        kw("no_type_check_decorator", 1, 1, Type::Any, &["decorator"]),
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
        // `typing_extensions` takes `get_overloads`/`clear_overloads` from
        // `typing` when they exist (Python 3.11+). They read and reset the
        // registry filled by `@overload`.
        f("get_overloads", 1, 1, Type::List),
        f("clear_overloads", 0, 0, Type::None),
        // `disjoint_base(cls)` marks a class for the 3.15 inheritance check.
        // Hydro keeps no compiler flag, so the decorator returns the class.
        f("disjoint_base", 1, 1, Type::Any),
        // PEP 695 helpers and runtime type assertions. `assert_type` and
        // `reveal_type` are runtime no-ops that return their argument;
        // `override` is an identity marker. `is_typeddict`/`is_protocol`
        // inspect the runtime class shape, and `get_protocol_members`
        // answers from the protocol class's own method/attribute names.
        f("assert_type", 2, 2, Type::Any),
        f("reveal_type", 1, 1, Type::Any),
        f("override", 1, 1, Type::Any),
        f("is_typeddict", 1, 1, Type::Bool),
        f("is_protocol", 1, 1, Type::Bool),
        f("get_protocol_members", 1, 1, Type::Set),
        // CPython's private `_ConcatenateGenericAlias(origin, args, *,
        // _typevar_types, _paramspec_tvars)`. The attribute value is the real
        // alias class (see `typing::class_value`); declaring the call form
        // lets `typing_extensions`' fallback path construct it with the
        // keyword-only metadata instead of failing on an unknown keyword.
        kw_dynamic(
            "_ConcatenateGenericAlias",
            2,
            2,
            Type::Any,
            &["origin", "args"],
        ),
    ],
    constants: &[
        c("TYPE_CHECKING", Type::Bool),
        // `typing.__all__`: the module's public export list (built by the
        // runtime from this spec so `from typing import *` and
        // `typing_extensions`' name probing agree with it).
        c("__all__", Type::List),
        // CPython 3.13+ sentinels imported by `typing_extensions` and
        // pydantic. They are identity-stable singleton instances; no
        // `builtins.sentinel` (3.15-only) is added.
        c("NoDefault", Type::Any),
        c("NoExtraItems", Type::Any),
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
        c("Hashable", Type::Any),
        c("Reversible", Type::Any),
        c("Sized", Type::Any),
        c("Container", Type::Any),
        c("AbstractSet", Type::Any),
        c("MutableSet", Type::Any),
        c("MutableMapping", Type::Any),
        c("MutableSequence", Type::Any),
        c("MappingView", Type::Any),
        c("KeysView", Type::Any),
        c("ItemsView", Type::Any),
        c("ValuesView", Type::Any),
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

/// `annotationlib`: CPython 3.14 annotation introspection.
pub static ANNOTATIONLIB: ModuleSpec = ModuleSpec {
    name: "annotationlib",
    functions: &[
        kw_dynamic("get_annotations", 1, 1, Type::Dict, &["obj"]),
        kw_dynamic(
            "call_annotate_function",
            2,
            2,
            Type::Dict,
            &["annotate", "format"],
        ),
        kw_dynamic(
            "call_evaluate_function",
            2,
            2,
            Type::Dict,
            &["evaluate", "format"],
        ),
        f("get_annotate_from_class_namespace", 1, 1, Type::Any),
        f("type_repr", 1, 1, Type::Str),
        f("annotations_to_string", 1, 1, Type::Dict),
    ],
    constants: &[
        c("Format", Type::Any),
        c("VALUE", Type::Int),
        c("VALUE_WITH_FAKE_GLOBALS", Type::Int),
        c("FORWARDREF", Type::Int),
        c("STRING", Type::Int),
        c("ForwardRef", Type::Any),
        c("__all__", Type::List),
    ],
};

/// `__future__`: the compile-time feature flags as booleans. The parser handles
/// the actual feature semantics; the values only serve direct lookups.
pub static FUTURE: ModuleSpec = ModuleSpec {
    name: "__future__",
    functions: &[],
    constants: &[
        // The ordered list of every feature name, in CPython 3.14 order.
        c("all_feature_names", Type::List),
        c("annotations", Type::Bool),
        c("barry_as_FLUFL", Type::Bool),
        c("division", Type::Bool),
        c("absolute_import", Type::Bool),
        c("print_function", Type::Bool),
        c("generator_stop", Type::Bool),
        c("unicode_literals", Type::Bool),
        // Legacy features retained for `__future__` compatibility.
        c("nested_scopes", Type::Bool),
        c("generators", Type::Bool),
        c("with_statement", Type::Bool),
        // The release-record class `pydoc` uses as an `isinstance` target.
        c("_Feature", Type::Any),
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
        kw_dynamic("SimpleNamespace", 0, 1, Type::Any, &[]),
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
        // `get_original_bases(cls, /)`: the class's `__orig_bases__` when
        // present, else its real bases (Hydro records the declared bases, so
        // generic aliases stay visible).
        f("get_original_bases", 1, 1, Type::Tuple),
        kw(
            "prepare_class",
            2,
            3,
            Type::Tuple,
            &["name", "bases", "kwds"],
        ),
        f("resolve_bases", 1, 1, Type::Any),
        // `MappingProxyType(mapping)`: the read-only wrapper constructor.
        f("MappingProxyType", 1, 1, Type::Any),
    ],
    constants: &[
        c("CodeType", Type::Any),
        // CPython removed `ClassType` in 3 but the `six` compatibility layer
        // still references it in a version-guarded branch; keep the alias so
        // the reference resolves.
        c("ClassType", Type::Any),
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
        c("NotImplementedType", Type::Any),
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
        // Python-2 compatibility aliases (`six.moves`).
        f("imap", 2, usize::MAX, Type::List),
        f("izip", 0, usize::MAX, Type::List),
        f("izip_longest", 0, usize::MAX, Type::List),
        f("ifilter", 2, 2, Type::List),
        f("ifilterfalse", 2, 2, Type::List),
        f("filterfalse", 2, 2, Type::List),
        f("chain", 0, usize::MAX, Type::List),
        kw_dynamic("zip_longest", 0, usize::MAX, Type::List, &[]),
        f("pairwise", 1, 1, Type::List),
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
        f("dropwhile", 2, 2, Type::List),
        // `groupby(iterable, key=None)`: consecutive keys. `key` may be
        // passed by name.
        kw("groupby", 1, 2, Type::Any, &["iterable", "key"]),
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
        kw_dynamic("AnnAssign", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("FunctionDef", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("AsyncFunctionDef", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("ClassDef", 0, usize::MAX, Type::Any, &[]),
        // Statement and helper nodes. The runtime serves them as real classes
        // (with CPython field order) and accepts every field by keyword, so
        // each constructor is keyword-dynamic.
        kw_dynamic("Return", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("If", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Else", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("For", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("While", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Try", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("ExceptHandler", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Raise", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Assert", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Import", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("ImportFrom", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("With", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("AsyncWith", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Match", 0, usize::MAX, Type::Any, &[]),
        // CPython spells this one lowercase (`ast.match_case`); `MatchCase`
        // is the alias the runtime maps to the same class.
        kw_dynamic("match_case", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("MatchCase", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("AugAssign", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Delete", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Global", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Nonlocal", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Pass", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Break", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Continue", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Yield", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("YieldFrom", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Await", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Lambda", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("JoinedStr", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("FormattedValue", 0, usize::MAX, Type::Any, &[]),
        // Helper nodes (`comprehension`, `arguments`, `alias`, `withitem`)
        // and the PEP 695 type-parameter nodes.
        kw_dynamic("comprehension", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("Comprehension", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("arguments", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("alias", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("withitem", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("TypeAlias", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("TypeVar", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("ParamSpec", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("TypeVarTuple", 0, usize::MAX, Type::Any, &[]),
        // Assignment, walrus, type-comment and comprehension nodes.
        kw_dynamic("Assign", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("NamedExpr", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("TypeIgnore", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("TryStar", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("AsyncFor", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("ListComp", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("SetComp", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("DictComp", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("GeneratorExp", 0, usize::MAX, Type::Any, &[]),
        // `match` pattern nodes.
        kw_dynamic("MatchValue", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("MatchSingleton", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("MatchSequence", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("MatchMapping", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("MatchClass", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("MatchStar", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("MatchAs", 0, usize::MAX, Type::Any, &[]),
        kw_dynamic("MatchOr", 0, usize::MAX, Type::Any, &[]),
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
        // `pattern` (the `match` base) and `type_ignore` (type-comment base).
        c("pattern", Type::Any),
        c("type_ignore", Type::Any),
        c("comprehension", Type::Any),
        c("excepthandler", Type::Any),
        c("arguments", Type::Any),
        c("alias", Type::Any),
        // PEP 695's `ast.type_param` union base for `TypeVar`/`ParamSpec`/
        // `TypeVarTuple`.
        c("type_param", Type::Any),
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
        // `_gcd_import(name, package=None, level=0)`: the bootstrap primitive
        // behind `import_module` (also reachable as
        // `_frozen_importlib._gcd_import`).
        f("_gcd_import", 1, 3, Type::Any),
        // Internal frozen-bootstrap hooks `importlib/__init__.py` and `pydoc`
        // reach through `_frozen_importlib`.
        f("_find_spec", 2, 3, Type::Any),
        f("_exec", 2, 2, Type::None),
        f("_setup", 2, 2, Type::None),
    ],
    constants: &[
        c("util", Type::Any),
        c("machinery", Type::Any),
        c("metadata", Type::Any),
        c("resources", Type::Any),
        // `importlib._bootstrap`: the frozen bootstrap module `pkgutil` reads
        // (`_bootstrap._find_spec`); the runtime serves the module value.
        c("_bootstrap", Type::Any),
        // `importlib._bootstrap_external`: the frozen external bootstrap
        // module, whose public surface is `importlib.machinery`.
        c("_bootstrap_external", Type::Any),
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
        kw(
            "spec_from_loader",
            2,
            4,
            Type::Any,
            &["name", "loader", "origin", "is_package"],
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
        // `_find_spec_from_path(name, path=None)`: the internal helper
        // `pyclbr` calls with the two positional arguments.
        f("_find_spec_from_path", 2, 2, Type::Any),
        // `cache_from_source(path, debug_override=None, *,
        // optimization=None)`: the `__pycache__` `.pyc` path.
        kw(
            "cache_from_source",
            1,
            3,
            Type::Str,
            &["path", "debug_override", "optimization"],
        ),
        // `source_hash(source_bytes)`: the SipHash-1-3 digest a `.pyc` header
        // records.
        f("source_hash", 1, 1, Type::Bytes),
    ],
    constants: &[
        c("LazyLoader", Type::Any),
        // `MAGIC_NUMBER`: the four-byte pyc magic of the running interpreter.
        c("MAGIC_NUMBER", Type::Bytes),
    ],
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
        // `_module_spec(name, origin, is_package)`: the `ModuleSpec` binding
        // every compiled unit publishes as `__spec__`.
        f("_module_spec", 3, 3, Type::Any),
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
        // `_install(bootstrap_module)`: registers the path finder with
        // `sys.meta_path` and publishes the bootstrap module.
        f("_install", 1, 1, Type::None),
        // `_bootstrap_external._classify_pyc(data, name, exc_details)`: the
        // pyc header validator `modulefinder` calls.
        f("_classify_pyc", 3, 3, Type::Any),
        f("_set_bootstrap_module", 1, 1, Type::None),
        // `_bootstrap_external`'s pyc helpers (`py_compile`/`compileall`):
        // header packing and the atomic bytecode writer.
        f("_pack_uint32", 1, 1, Type::Bytes),
        f("_unpack_uint32", 1, 1, Type::Int),
        f("_calc_mode", 1, 1, Type::Int),
        f("_write_atomic", 2, 3, Type::None),
        f("_code_to_timestamp_pyc", 1, 3, Type::Any),
        f("_code_to_hash_pyc", 2, 3, Type::Any),
        // `PathFinder.find_spec(fullname, path=None, target=None)` and its
        // cache hook.
        f("_path_finder_find_spec", 2, 4, Type::Any),
        f("_path_finder_invalidate_caches", 1, 1, Type::None),
        // `FileFinder(path, *loader_details)` and its instance methods. The
        // receiver is the first argument, as for every native method.
        f("_file_finder_init", 2, usize::MAX, Type::None),
        f("_file_finder_path_hook", 1, usize::MAX, Type::Any),
        f("_file_finder_hook", 1, 1, Type::Any),
        f("_file_finder_find_spec", 2, 3, Type::Any),
        f("_file_finder_invalidate_caches", 1, 1, Type::None),
    ],
    constants: &[
        c("ModuleSpec", Type::Any),
        c("Loader", Type::Any),
        c("SourceFileLoader", Type::Any),
        c("ExtensionFileLoader", Type::Any),
        c("BuiltinImporter", Type::Any),
        c("FrozenImporter", Type::Any),
        // The path-based finder and the loaders `pkgutil`/`importlib` expose
        // from this module.
        c("PathFinder", Type::Any),
        c("FileFinder", Type::Any),
        c("SourcelessFileLoader", Type::Any),
        c("AppleFrameworkLoader", Type::Any),
        // `zipimport._zip_directory_cache`: the shared zip directory cache
        // (empty, because Hydro imports no zip archives).
        c("_zip_directory_cache", Type::Dict),
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
        // The `importlib.abc.MetaPathFinder` implementation that discovers
        // installed distributions; imported by `importlib.metadata` users and
        // by `importlib.util.find_spec` shims.
        c("MetadataPathFinder", Type::Any),
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
///
/// `closing(thing)` wraps an object's `close()` method, and `ExitStack`
/// registers exits dynamically (`enter_context`/`push`/`callback`), unwinds
/// them LIFO and transfers them with `pop_all`. `callback` keeps its call-site
/// keywords for the registered callable, so its spec also declares dynamic
/// keywords.
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
        // `closing(thing)`: a context manager whose `__exit__` closes the
        // wrapped object and never suppresses the block exception.
        f("closing", 1, 1, Type::Any),
        f("_closing_enter", 1, 1, Type::Any),
        f("_closing_exit", 4, 4, Type::Bool),
        // `ExitStack()`: the native stack class. Its methods take the stack
        // first, so the central table declares the receiver-inclusive
        // arities. `callback(callback, *args, **kwargs)` forwards the
        // call-site keywords to the registered callback, hence the dynamic
        // keyword support.
        f("ExitStack", 0, 0, Type::Any),
        f("_exitstack_enter", 1, 1, Type::Any),
        f("_exitstack_exit", 4, 4, Type::Bool),
        f("_exitstack_close", 1, 1, Type::None),
        f("_exitstack_enter_context", 2, 2, Type::Any),
        f("_exitstack_push", 2, 2, Type::Any),
        kw_dynamic("_exitstack_callback", 2, usize::MAX, Type::Any, &[]),
        f("_exitstack_pop_all", 1, 1, Type::Any),
        // The `AbstractContextManager`/`AbstractAsyncContextManager` protocol
        // bodies. `typing_extensions` imports both classes for its
        // `ContextManager`/`AsyncContextManager` aliases; the methods are the
        // CPython definitions (enter hands back the manager, exit never
        // suppresses, the subclass hook answers structurally).
        f("_abstract_cm_enter", 1, 1, Type::Any),
        f("_abstract_cm_exit", 4, 4, Type::None),
        f("_abstract_cm_subclasshook", 2, 2, Type::Any),
        f("_abstract_acm_aenter", 1, 1, Type::Any),
        f("_abstract_acm_aexit", 4, 4, Type::None),
        f("_abstract_acm_subclasshook", 2, 2, Type::Any),
    ],
    constants: &[
        // Real class values at runtime; see `contextlib_class_value`.
        c("AbstractContextManager", Type::Any),
        c("AbstractAsyncContextManager", Type::Any),
    ],
};

/// `_collections`: accelerator alias table for `collections`. `defaultdict`,
/// `OrderedDict` and `Counter` are implemented; `deque` is deliberately absent
/// instead of being faked.
pub static _COLLECTIONS: ModuleSpec = ModuleSpec {
    name: "_collections",
    functions: &[
        kw_dynamic("defaultdict", 0, 2, Type::Any, &[]),
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
    constants: &[
        // The accelerator exports the same `CodecInfo` class object as
        // `codecs`.
        c("CodecInfo", Type::Any),
    ],
};

/// `codecs`: the pure-Python wrapper surface over the native `_codecs`
/// registry. `lookup` is the same native implementation the accelerator
/// exports; the byte-order marks carry their real CPython values.
pub static CODECS: ModuleSpec = ModuleSpec {
    name: "codecs",
    functions: &[
        f("lookup", 1, 1, Type::Any),
        // `encode(obj, encoding='utf-8', errors='strict')` and
        // `decode(obj, encoding='utf-8', errors='strict')`.
        f("encode", 1, 3, Type::Bytes),
        f("decode", 1, 3, Type::Str),
        // The codec-search registry. `register` takes a callable search
        // function; `unregister` removes matching callables.
        f("register", 1, 1, Type::None),
        f("unregister", 1, 1, Type::None),
        // The error-handler registry.
        f("register_error", 2, 2, Type::None),
        f("lookup_error", 1, 1, Type::Any),
        // `escape_decode(data, errors='strict')`: the `unicode_escape` /
        // `raw_unicode_escape` decoder shared with `_codecs`.
        f("escape_decode", 1, 2, Type::Tuple),
        f("getreader", 1, 1, Type::Function),
        f("getwriter", 1, 1, Type::Function),
        // `getdecoder(encoding)`/`getencoder(encoding)`: the matching
        // `CodecInfo` member, used by pandas' string accessors.
        f("getdecoder", 1, 1, Type::Function),
        f("getencoder", 1, 1, Type::Function),
        // `getincrementaldecoder`/`getincrementalencoder`: the incremental
        // codec classes used by `io.TextIOWrapper`.
        f("getincrementaldecoder", 1, 1, Type::Any),
        f("getincrementalencoder", 1, 1, Type::Any),
    ],
    constants: &[
        // `CodecInfo(encode, decode, streamreader=None, streamwriter=None,
        // incrementalencoder=None, incrementaldecoder=None, name=None, *,
        // _is_text_encoding=None)`: the registry-entry class `encodings`
        // constructs and checks with `isinstance`.
        c("CodecInfo", Type::Any),
        c("StreamReader", Type::Any),
        c("StreamWriter", Type::Any),
        c("StreamReaderWriter", Type::Any),
        c("StreamRecoder", Type::Any),
        c("IncrementalEncoder", Type::Any),
        c("IncrementalDecoder", Type::Any),
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
    // `warnings.deprecated` is a class (usable in type aliases and as a
    // decorator), exposed as a dynamic constant so calls take the runtime
    // class path with keyword arguments. `_DEPRECATED_MSG` is the format
    // string its implementation builds deprecation messages from, and
    // `EncodingWarning` is re-exported here by CPython's module.
    constants: &[
        c("filters", Type::List),
        c("deprecated", Type::Any),
        c("_DEPRECATED_MSG", Type::Str),
        c("EncodingWarning", Type::Any),
    ],
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
        c("MappingView", Type::Any),
        c("KeysView", Type::Any),
        c("ItemsView", Type::Any),
        c("ValuesView", Type::Any),
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
        c("MappingView", Type::Any),
        c("KeysView", Type::Any),
        c("ItemsView", Type::Any),
        c("ValuesView", Type::Any),
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
        // `get_referrers(*objs)`: objects referring to any argument. The
        // runtime has no heap enumeration, so it raises NotImplementedError;
        // the name is declared so imports and attribute reads resolve.
        f("get_referrers", 0, usize::MAX, Type::List),
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
    f("get_errno", 0, 0, Type::Int),
    f("set_errno", 1, 1, Type::None),
    f("dlopen", 1, 2, Type::Int),
    f("FreeLibrary", 1, 1, Type::None),
    f("GetLastError", 0, 0, Type::Int),
    f("WinError", 0, 2, Type::Any),
    f("_cfield_init", 1, usize::MAX, Type::None),
    f("_cfield_get", 2, 3, Type::Any),
    f("_cfield_set", 3, 3, Type::None),
    f("_cfield_repr", 1, 1, Type::Str),
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
    c("CField", Type::Any),
    c("wintypes", Type::Any),
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
        // `localeconv()`: the C locale's numeric/monetary formatting data.
        f("localeconv", 0, 0, Type::Dict),
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
        c("CHAR_MAX", Type::Int),
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
        // Native method bodies of the frozen `ucd_3_2_0` database; the
        // receiver is the first argument.
        f("_ucd32_category", 2, 2, Type::Str),
        f("_ucd32_bidirectional", 2, 2, Type::Str),
        f("_ucd32_combining", 2, 2, Type::Int),
        f("_ucd32_decomposition", 2, 2, Type::Str),
        f("_ucd32_east_asian_width", 2, 2, Type::Str),
        f("_ucd32_mirrored", 2, 2, Type::Int),
        f("_ucd32_name", 2, 3, Type::Str),
        f("_ucd32_lookup", 2, 2, Type::Str),
        f("_ucd32_normalize", 3, 3, Type::Str),
        f("_ucd32_is_normalized", 3, 3, Type::Bool),
        f("_ucd32_numeric", 2, 3, Type::Float),
        f("_ucd32_digit", 2, 3, Type::Int),
        f("_ucd32_decimal", 2, 3, Type::Int),
    ],
    constants: &[
        c("unidata_version", Type::Str),
        // `ucd_3_2_0`: the frozen Unicode 3.2 database used by the `idna`
        // codec.
        c("ucd_3_2_0", Type::Any),
    ],
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
        // `_ZlibDecompressor`: the raw decompressor class `gzip` uses as its
        // `_decomp_factory` (`_ZlibDecompressor(wbits=-MAX_WBITS)`). It is a
        // class value, not a module function, so calls take the class
        // construction path.
        c("_ZlibDecompressor", Type::Any),
        // `_ZlibCompressor`: the mirror incremental-compression class, also
        // routed by the runtime module.
        c("_ZlibCompressor", Type::Any),
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

/// `http`: native HTTP/1.1 server used by the benchmark app.
///
/// `serve(port, routes, handler, max_connections)` binds a listener and blocks
/// for the process lifetime. `routes` is a `dict[str, bytes]` keyed by
/// `"METHOD /path"`; a matching request is answered from native memory without
/// a Python call. Every other request is passed to `handler(method, path, body)`,
/// which must return the complete response bytes. Parameter names are
/// declared so the usual keyword spelling (`http.serve(port=..., ...)`)
/// lowers to the same positional native call.
pub static HTTP: ModuleSpec = ModuleSpec {
    name: "http",
    functions: &[kw(
        "serve",
        4,
        4,
        Type::None,
        &["port", "routes", "handler", "max_connections"],
    )],
    constants: &[],
};

/// `http.server`: the classes `pydoc.serve` builds on.
///
/// Hydro's `http` module is the native server, so the standard-library
/// `http/server.py` package cannot be linked underneath it. The nested native
/// module exposes real class values with the protocol methods the standard
/// library uses: `BaseHTTPRequestHandler` (request parsing, `do_<METHOD>`
/// dispatch, status/header emission through `wfile`) and `HTTPServer`
/// (`socket.create_server` binding, accept loop, close), plus
/// `ThreadingHTTPServer` with CPython's `daemon_threads = True`.
pub static HTTP_SERVER: ModuleSpec = ModuleSpec {
    name: "http.server",
    functions: &[],
    constants: &[
        c("BaseHTTPRequestHandler", Type::Any),
        c("HTTPServer", Type::Any),
        c("ThreadingHTTPServer", Type::Any),
    ],
};

/// `postgres`: native PostgreSQL v3 client used by the benchmark app.
///
/// Connections are capsule-backed and reconnect lazily. `insert` returns the
/// integer `id` from `RETURNING id`; `insert_device` lets the server generate
/// the UUID and returns the `(id, uuid)` tuple from `RETURNING id, uuid`.
/// SQL identifiers are restricted and string values are escaped by the
/// runtime, so callers do not splice SQL.
pub static POSTGRES: ModuleSpec = ModuleSpec {
    name: "postgres",
    functions: &[
        f("connect", 4, 4, Type::Any),
        f("insert", 5, 5, Type::Int),
        f("insert_device", 4, 4, Type::Any),
    ],
    constants: &[],
};

/// `memcached`: native ASCII protocol client used by the benchmark app.
///
/// A connection is a capsule; `set` returns `True` after a `STORED` reply and
/// reconnects lazily after I/O or protocol failure.
pub static MEMCACHED: ModuleSpec = ModuleSpec {
    name: "memcached",
    functions: &[f("connect", 2, 2, Type::Any), f("set", 4, 4, Type::Bool)],
    constants: &[],
};

/// `_hydro_sidecar`: native transport for foreign-runtime sidecar workers.
///
/// A sidecar is a child process speaking a length-prefixed binary protocol on
/// stdin/stdout. `spawn(program, args=None)` launches it and performs the
/// protocol handshake; the returned handle is a capsule-backed worker reused by
/// `call(handle, method, *args)`. `restart` relaunches a worker, `close`
/// terminates it and `alive` reports whether it is still running. The protocol
/// constants are exposed as module constants.
pub static HYDRO_SIDECAR: ModuleSpec = ModuleSpec {
    name: "_hydro_sidecar",
    functions: &[
        f("spawn", 1, 2, Type::Any),
        f("call", 2, usize::MAX, Type::Any),
        f("restart", 1, 1, Type::None),
        f("close", 1, 1, Type::None),
        f("alive", 1, 1, Type::Bool),
    ],
    constants: &[c("PROTOCOL_VERSION", Type::Int), c("MAX_FRAME", Type::Int)],
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
        f("istraceback", 1, 1, Type::Bool),
        f("ismethodwrapper", 1, 1, Type::Bool),
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
        f("getabsfile", 1, 1, Type::Str),
        f("getcomments", 1, 1, Type::Any),
        f("getmro", 1, 1, Type::Tuple),
        f("classify_class_attrs", 1, 1, Type::List),
        // `getclasstree(classes, unique=False)`.
        kw("getclasstree", 1, 2, Type::List, &["classes", "unique"]),
        f("getfile", 1, 1, Type::Str),
        f("getsourcefile", 1, 1, Type::Any),
        f("findsource", 1, 1, Type::Tuple),
        f("getsourcelines", 1, 1, Type::Tuple),
        f("getblock", 1, 1, Type::List),
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
        // `getmodulename(path)`: the module name for a file name, or `None`
        // when the suffix is not an import suffix (`path` may be passed by
        // name, like the Python-level wrapper).
        kw("getmodulename", 1, 1, Type::Any, &["path"]),
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
        c("Attribute", Type::Any),
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

// ---------------------------------------------------------------------------
// Hashing, HMAC and UUID
// ---------------------------------------------------------------------------
//
// `hashlib` and the C accelerator surfaces (`_hashlib`, `_sha1`, `_sha2`,
// `_sha3`, `_md5`, `_blake2`, `_hmac`, `_uuid`) are implemented by one runtime
// module.
// The constructors accept their documented keyword names; the digest objects
// themselves are `HASH`/`HMAC` class instances whose methods are runtime
// supplemental entries.

/// `hashlib`: digest constructors, PBKDF2, HMAC helpers and the UUID
/// generators the runtime implements natively.
pub static HASHLIB: ModuleSpec = ModuleSpec {
    name: "hashlib",
    functions: &[
        // `new(name, data=b'', *, usedforsecurity=True)`: the named-constructor
        // form. Keyword-dynamic so `usedforsecurity` (and future options) reach
        // the runtime, which validates the names itself.
        kw_dynamic("new", 1, 2, Type::Any, &["name", "data"]),
        // `md5(data=b'', *, usedforsecurity=True)` and the other constructors.
        // BLAKE2 additionally accepts its full parameter set, so the entries
        // are keyword-dynamic rather than a fixed keyword list.
        kw_dynamic("md5", 0, 1, Type::Any, &["data"]),
        kw_dynamic("sha1", 0, 1, Type::Any, &["data"]),
        kw_dynamic("sha224", 0, 1, Type::Any, &["data"]),
        kw_dynamic("sha256", 0, 1, Type::Any, &["data"]),
        kw_dynamic("sha384", 0, 1, Type::Any, &["data"]),
        kw_dynamic("sha512", 0, 1, Type::Any, &["data"]),
        kw_dynamic("blake2b", 0, 1, Type::Any, &["data"]),
        kw_dynamic("blake2s", 0, 1, Type::Any, &["data"]),
        // SHA-3 (FIPS 202) and the extendable-output SHAKE functions; both
        // accept the `usedforsecurity` keyword through the dynamic binder.
        kw_dynamic("sha3_224", 0, 1, Type::Any, &["data"]),
        kw_dynamic("sha3_256", 0, 1, Type::Any, &["data"]),
        kw_dynamic("sha3_384", 0, 1, Type::Any, &["data"]),
        kw_dynamic("sha3_512", 0, 1, Type::Any, &["data"]),
        kw_dynamic("shake_128", 0, 1, Type::Any, &["data"]),
        kw_dynamic("shake_256", 0, 1, Type::Any, &["data"]),
        kw(
            "pbkdf2_hmac",
            4,
            5,
            Type::Bytes,
            &["hash_name", "password", "salt", "iterations", "dklen"],
        ),
        kw("compare_digest", 2, 2, Type::Bool, &["a", "b"]),
        kw("hmac_new", 1, 3, Type::Any, &["key", "msg", "digestmod"]),
        kw(
            "hmac_digest",
            3,
            3,
            Type::Bytes,
            &["key", "msg", "digestmod"],
        ),
        kw("file_digest", 2, 2, Type::Any, &["fileobj", "digest"]),
        f("get_fips_mode", 0, 0, Type::Int),
        f("_is_fips_mode", 0, 0, Type::Bool),
        kw("uuid1", 0, 2, Type::Bytes, &["node", "clock_seq"]),
        f("uuid4", 0, 0, Type::Bytes),
        kw("uuid5", 2, 2, Type::Bytes, &["namespace", "name"]),
        f("generate_time_safe", 0, 0, Type::Tuple),
    ],
    constants: &[
        c("algorithms_guaranteed", Type::Set),
        c("algorithms_available", Type::Set),
        c("HASH", Type::Any),
        c("HMAC", Type::Any),
        c("openssl_md_meth_names", Type::Set),
        c("UnsupportedDigestmodError", Type::Any),
    ],
};

/// `_hashlib`: the OpenSSL-backed accelerator. It exposes the named
/// constructor, the `openssl_*` constructors, HMAC helpers, FIPS queries and
/// PBKDF2.
pub static _HASHLIB: ModuleSpec = ModuleSpec {
    name: "_hashlib",
    functions: &[
        kw("new", 1, 2, Type::Any, &["name", "data"]),
        kw("hmac_new", 1, 3, Type::Any, &["key", "msg", "digestmod"]),
        kw(
            "hmac_digest",
            3,
            3,
            Type::Bytes,
            &["key", "msg", "digestmod"],
        ),
        kw("compare_digest", 2, 2, Type::Bool, &["a", "b"]),
        f("get_fips_mode", 0, 0, Type::Int),
        f("_is_fips_mode", 0, 0, Type::Bool),
        kw(
            "pbkdf2_hmac",
            4,
            5,
            Type::Bytes,
            &["hash_name", "password", "salt", "iterations", "dklen"],
        ),
        kw("openssl_md5", 0, 1, Type::Any, &["data"]),
        kw("openssl_sha1", 0, 1, Type::Any, &["data"]),
        kw("openssl_sha224", 0, 1, Type::Any, &["data"]),
        kw("openssl_sha256", 0, 1, Type::Any, &["data"]),
        kw("openssl_sha384", 0, 1, Type::Any, &["data"]),
        kw("openssl_sha512", 0, 1, Type::Any, &["data"]),
        kw("openssl_blake2b512", 0, 1, Type::Any, &["data"]),
        kw("openssl_blake2s256", 0, 1, Type::Any, &["data"]),
        kw("openssl_sha3_224", 0, 1, Type::Any, &["data"]),
        kw("openssl_sha3_256", 0, 1, Type::Any, &["data"]),
        kw("openssl_sha3_384", 0, 1, Type::Any, &["data"]),
        kw("openssl_sha3_512", 0, 1, Type::Any, &["data"]),
        kw("openssl_shake_128", 0, 1, Type::Any, &["data"]),
        kw("openssl_shake_256", 0, 1, Type::Any, &["data"]),
    ],
    constants: &[
        c("HASH", Type::Any),
        c("HMAC", Type::Any),
        c("UnsupportedDigestmodError", Type::Any),
        c("openssl_md_meth_names", Type::Set),
    ],
};

/// `_sha1`: the SHA-1 accelerator constructor.
pub static _SHA1: ModuleSpec = ModuleSpec {
    name: "_sha1",
    functions: &[kw("sha1", 0, 1, Type::Any, &["data"])],
    constants: &[],
};

/// `_sha2`: the SHA-2 accelerator constructors.
pub static _SHA2: ModuleSpec = ModuleSpec {
    name: "_sha2",
    functions: &[
        kw("sha224", 0, 1, Type::Any, &["data"]),
        kw("sha256", 0, 1, Type::Any, &["data"]),
        kw("sha384", 0, 1, Type::Any, &["data"]),
        kw("sha512", 0, 1, Type::Any, &["data"]),
    ],
    constants: &[],
};

/// `_sha3`: the SHA-3 (FIPS 202) and SHAKE accelerator constructors. CPython's
/// `hashlib` imports exactly these names from it when the OpenSSL accelerator
/// is unavailable; the runtime routes the alias to the `hashlib`
/// implementation.
pub static _SHA3: ModuleSpec = ModuleSpec {
    name: "_sha3",
    functions: &[
        kw("sha3_224", 0, 1, Type::Any, &["data"]),
        kw("sha3_256", 0, 1, Type::Any, &["data"]),
        kw("sha3_384", 0, 1, Type::Any, &["data"]),
        kw("sha3_512", 0, 1, Type::Any, &["data"]),
        kw("shake_128", 0, 1, Type::Any, &["data"]),
        kw("shake_256", 0, 1, Type::Any, &["data"]),
    ],
    constants: &[],
};

/// `_md5`: the MD5 accelerator constructor.
pub static _MD5: ModuleSpec = ModuleSpec {
    name: "_md5",
    functions: &[kw("md5", 0, 1, Type::Any, &["data"])],
    constants: &[],
};

/// `_blake2`: the BLAKE2 accelerator constructors.
pub static _BLAKE2: ModuleSpec = ModuleSpec {
    name: "_blake2",
    functions: &[
        kw("blake2b", 0, 1, Type::Any, &["data"]),
        kw("blake2s", 0, 1, Type::Any, &["data"]),
    ],
    constants: &[],
};

/// `_hmac`: the HMAC accelerator. `new` builds the streaming object,
/// `compute_digest` the one-shot form of `hmac_digest`; the unknown digest
/// error is exposed as a class value.
pub static _HMAC: ModuleSpec = ModuleSpec {
    name: "_hmac",
    functions: &[
        kw("new", 1, 3, Type::Any, &["key", "msg", "digestmod"]),
        // `compute_digest(key, msg, digestmod)`: the one-shot digest.
        f("compute_digest", 3, 3, Type::Bytes),
    ],
    constants: &[
        c("UnknownHashError", Type::Any),
        // `_hmac.HMAC`: the cached HMAC class value.
        c("HMAC", Type::Any),
    ],
};

/// `_uuid`: the native UUID time generator and its capability flags.
pub static _UUID: ModuleSpec = ModuleSpec {
    name: "_uuid",
    functions: &[f("generate_time_safe", 0, 0, Type::Tuple)],
    constants: &[
        c("has_uuid_generate_time_safe", Type::Bool),
        c("has_stable_extractable_node", Type::Bool),
        // The RFC 4122 namespace UUIDs, rendered as canonical strings.
        c("NAMESPACE_DNS", Type::Str),
        c("NAMESPACE_URL", Type::Str),
        c("NAMESPACE_OID", Type::Str),
        c("NAMESPACE_X500", Type::Str),
    ],
};

// ---------------------------------------------------------------------------
// Compression, XML and SQLite native modules
// ---------------------------------------------------------------------------

/// `_bz2`: the libbz2 codec classes and their native method bodies. No module
/// constants are exposed.
pub static _BZ2: ModuleSpec = ModuleSpec {
    name: "_bz2",
    functions: &[
        kw("BZ2Compressor", 0, 1, Type::Any, &["compresslevel"]),
        f("BZ2Decompressor", 0, 0, Type::Any),
        f("_bz2_compress", 2, 2, Type::Bytes),
        f("_bz2_flush", 1, 1, Type::Bytes),
        f("_bz2_decompress", 2, 3, Type::Bytes),
        f("_bz2_compressor_init", 1, 2, Type::None),
        f("_bz2_decompressor_init", 1, 1, Type::None),
    ],
    constants: &[],
};

/// `_lzma`: the liblzma codec classes, filter helpers and the format/check
/// constants.
pub static _LZMA: ModuleSpec = ModuleSpec {
    name: "_lzma",
    functions: &[
        kw(
            "LZMACompressor",
            0,
            4,
            Type::Any,
            &["format", "check", "preset", "filters"],
        ),
        kw(
            "LZMADecompressor",
            0,
            3,
            Type::Any,
            &["format", "memlimit", "filters"],
        ),
        f("_lzma_compress", 2, 2, Type::Bytes),
        f("_lzma_flush", 1, 1, Type::Bytes),
        f("_lzma_decompress", 2, 3, Type::Bytes),
        f("_lzma_compressor_init", 1, 5, Type::None),
        f("_lzma_decompressor_init", 1, 4, Type::None),
        f("is_check_supported", 1, 1, Type::Bool),
        f("_encode_filter_properties", 1, 1, Type::Bytes),
        f("_decode_filter_properties", 2, 2, Type::Any),
    ],
    constants: &[
        c("LZMAError", Type::Any),
        c("FORMAT_AUTO", Type::Int),
        c("FORMAT_XZ", Type::Int),
        c("FORMAT_ALONE", Type::Int),
        c("FORMAT_RAW", Type::Int),
        c("CHECK_NONE", Type::Int),
        c("CHECK_CRC32", Type::Int),
        c("CHECK_CRC64", Type::Int),
        c("CHECK_SHA256", Type::Int),
        c("CHECK_ID_MAX", Type::Int),
        c("CHECK_UNKNOWN", Type::Int),
        c("FILTER_LZMA1", Type::Int),
        c("FILTER_LZMA2", Type::Int),
        c("FILTER_DELTA", Type::Int),
        c("FILTER_X86", Type::Int),
        c("FILTER_POWERPC", Type::Int),
        c("FILTER_IA64", Type::Int),
        c("FILTER_ARM", Type::Int),
        c("FILTER_ARMTHUMB", Type::Int),
        c("FILTER_SPARC", Type::Int),
        c("MF_HC3", Type::Int),
        c("MF_HC4", Type::Int),
        c("MF_BT2", Type::Int),
        c("MF_BT3", Type::Int),
        c("MF_BT4", Type::Int),
        c("MODE_FAST", Type::Int),
        c("MODE_NORMAL", Type::Int),
        c("PRESET_DEFAULT", Type::Int),
        c("PRESET_EXTREME", Type::Int),
    ],
};

/// `pyexpat`: libexpat's XML parser. `ParserCreate` builds an `XMLParserType`
/// instance; the `_pyexpat_*` entries are the native method/property bodies
/// installed on that class (the receiver is included in each arity).
pub static PYEXPAT: ModuleSpec = ModuleSpec {
    name: "pyexpat",
    functions: &[
        kw(
            "ParserCreate",
            0,
            3,
            Type::Any,
            &["encoding", "namespace_separator", "intern"],
        ),
        f("ErrorString", 1, 1, Type::Str),
        f("_pyexpat_init", 1, 4, Type::None),
        f("_pyexpat_parse", 2, 3, Type::Int),
        f("_pyexpat_parse_file", 2, 2, Type::Int),
        f("_pyexpat_set_base", 2, 2, Type::None),
        f("_pyexpat_get_base", 1, 1, Type::Any),
        f("_pyexpat_external_entity_parser_create", 2, 3, Type::Any),
        f("_pyexpat_set_param_entity_parsing", 2, 2, Type::Bool),
        f("_pyexpat_use_foreign_dtd", 1, 2, Type::None),
        f("_pyexpat_get_buffer_text", 1, 1, Type::Bool),
        f("_pyexpat_set_buffer_text", 2, 2, Type::None),
        f("_pyexpat_get_buffer_size", 1, 1, Type::Int),
        f("_pyexpat_set_buffer_size", 2, 2, Type::None),
        f("_pyexpat_get_buffer_used", 1, 1, Type::Int),
        f("_pyexpat_get_namespace_prefixes", 1, 1, Type::Bool),
        f("_pyexpat_set_namespace_prefixes", 2, 2, Type::None),
        f("_pyexpat_get_ordered_attributes", 1, 1, Type::Bool),
        f("_pyexpat_set_ordered_attributes", 2, 2, Type::None),
        f("_pyexpat_get_specified_attributes", 1, 1, Type::Bool),
        f("_pyexpat_set_specified_attributes", 2, 2, Type::None),
        f("_pyexpat_get_error_code", 1, 1, Type::Int),
        f("_pyexpat_get_current_line", 1, 1, Type::Int),
        f("_pyexpat_get_current_column", 1, 1, Type::Int),
        f("_pyexpat_get_current_byte", 1, 1, Type::Int),
    ],
    constants: &[
        c("ExpatError", Type::Any),
        c("error", Type::Any),
        c("XMLParserType", Type::Any),
        c("errors", Type::Any),
        c("model", Type::Any),
        c("EXPAT_VERSION", Type::Str),
        c("version_info", Type::Tuple),
        c("native_encoding", Type::Str),
        c("features", Type::List),
        c("XML_PARAM_ENTITY_PARSING_NEVER", Type::Int),
        c("XML_PARAM_ENTITY_PARSING_UNLESS_STANDALONE", Type::Int),
        c("XML_PARAM_ENTITY_PARSING_ALWAYS", Type::Int),
    ],
};

/// `sqlite3`: the DB-API 2.0 module backed by libsqlite3. `Connection` and
/// `Cursor` are declared both as constructors (module-call lowering) and as
/// constants (class values for `isinstance`/attribute reads).
pub static SQLITE3: ModuleSpec = ModuleSpec {
    name: "sqlite3",
    functions: &[
        // `connect(database, ..., uri=False)`: keyword-only `database=` is
        // valid, so the entry is dynamic with zero fixed slots.
        kw_dynamic("connect", 0, 8, Type::Any, &[]),
        f("complete_statement", 1, 1, Type::Bool),
        f("Connection", 1, 8, Type::Any),
        f("Cursor", 1, 1, Type::Any),
    ],
    constants: &[
        c("Connection", Type::Any),
        c("Cursor", Type::Any),
        c("Error", Type::Any),
        c("Warning", Type::Any),
        c("InterfaceError", Type::Any),
        c("DatabaseError", Type::Any),
        c("DataError", Type::Any),
        c("OperationalError", Type::Any),
        c("IntegrityError", Type::Any),
        c("InternalError", Type::Any),
        c("ProgrammingError", Type::Any),
        c("NotSupportedError", Type::Any),
        c("apilevel", Type::Str),
        c("threadsafety", Type::Int),
        c("paramstyle", Type::Str),
        c("sqlite_version", Type::Str),
        c("sqlite_version_info", Type::Tuple),
        c("PARSE_DECLTYPES", Type::Int),
        c("PARSE_COLNAMES", Type::Int),
    ],
};

/// `_sqlite3`: the C accelerator spelling of `sqlite3`; same surface.
pub static _SQLITE3: ModuleSpec = ModuleSpec {
    name: "_sqlite3",
    functions: SQLITE3.functions,
    constants: SQLITE3.constants,
};

/// `_gdbm`: the libgdbm backend behind `dbm.gnu`. `open` returns a `gdbm`
/// mapping object; the `_gdbm_*` entries are that object's native method
/// bodies (the receiver is included in each arity).
pub static GDBM: ModuleSpec = ModuleSpec {
    name: "_gdbm",
    functions: &[
        f("open", 1, 3, Type::Any),
        f("_gdbm_close", 1, 1, Type::None),
        f("_gdbm_keys", 1, 1, Type::List),
        f("_gdbm_get", 1, 3, Type::Any),
        f("_gdbm_setdefault", 1, 3, Type::Any),
        f("_gdbm_firstkey", 1, 1, Type::Any),
        f("_gdbm_nextkey", 2, 2, Type::Any),
        f("_gdbm_reorganize", 1, 1, Type::None),
        f("_gdbm_sync", 1, 1, Type::None),
        f("_gdbm_clear", 1, 1, Type::None),
        f("_gdbm_contains", 2, 2, Type::Bool),
        f("_gdbm_len", 1, 1, Type::Int),
        f("_gdbm_bool", 1, 1, Type::Bool),
        f("_gdbm_getitem", 2, 2, Type::Any),
        f("_gdbm_setitem", 3, 3, Type::None),
        f("_gdbm_delitem", 2, 2, Type::None),
        f("_gdbm_enter", 1, 1, Type::Any),
        f("_gdbm_exit", 1, 4, Type::None),
    ],
    constants: &[
        c("error", Type::Any),
        c("open_flags", Type::Str),
        c("_GDBM_VERSION", Type::Tuple),
    ],
};

/// `_dbm`: the NDBM backend behind `dbm.ndbm`. Its mapping object has no
/// `firstkey`/`nextkey`/`sync`, matching CPython.
pub static DBM: ModuleSpec = ModuleSpec {
    name: "_dbm",
    functions: &[
        f("open", 1, 3, Type::Any),
        f("_dbm_close", 1, 1, Type::None),
        f("_dbm_keys", 1, 1, Type::List),
        f("_dbm_get", 1, 3, Type::Any),
        f("_dbm_setdefault", 1, 3, Type::Any),
        f("_dbm_clear", 1, 1, Type::None),
        f("_dbm_contains", 2, 2, Type::Bool),
        f("_dbm_len", 1, 1, Type::Int),
        f("_dbm_bool", 1, 1, Type::Bool),
        f("_dbm_getitem", 2, 2, Type::Any),
        f("_dbm_setitem", 3, 3, Type::None),
        f("_dbm_delitem", 2, 2, Type::None),
        f("_dbm_enter", 1, 1, Type::Any),
        f("_dbm_exit", 1, 4, Type::None),
    ],
    constants: &[c("error", Type::Any), c("library", Type::Str)],
};

// ---------------------------------------------------------------------------
// Terminal and codec accelerator modules
// ---------------------------------------------------------------------------

/// `readline`: GNU readline command-line editing and history. The module is
/// only importable when the real library is present; every entry is
/// positional as in CPython's C module.
pub static READLINE: ModuleSpec = ModuleSpec {
    name: "readline",
    functions: &[
        f("parse_and_bind", 1, 1, Type::None),
        f("get_line_buffer", 0, 0, Type::Str),
        f("insert_text", 1, 1, Type::None),
        f("read_history_file", 0, 1, Type::None),
        f("write_history_file", 0, 1, Type::None),
        f("add_history", 1, 1, Type::None),
        f("clear_history", 0, 0, Type::None),
        f("get_history_length", 0, 0, Type::Int),
        f("set_history_length", 1, 1, Type::None),
        f("get_completer", 0, 0, Type::Any),
        f("set_completer", 0, 1, Type::None),
        f("get_begidx", 0, 0, Type::Int),
        f("get_endidx", 0, 0, Type::Int),
        f("set_completer_delims", 1, 1, Type::None),
        f("get_completer_delims", 0, 0, Type::Str),
        f("set_auto_history", 1, 1, Type::None),
        f("get_current_history_length", 0, 0, Type::Int),
        // `redisplay()`: the readline front end is always non-interactive
        // here; the call succeeds and redraws nothing.
        f("redisplay", 0, 1, Type::None),
    ],
    constants: &[c("backend", Type::Str)],
};

/// `_curses`: the ncurses screen surface. `window` and `error` are class
/// values; the `KEY_*`/`A_*`/`ACS_*` constants are resolved dynamically by the
/// runtime (state-dependent ones exist only after `initscr`/`start_color`).
pub static CURSES: ModuleSpec = ModuleSpec {
    name: "_curses",
    functions: &[
        f("initscr", 0, 0, Type::Any),
        f("endwin", 0, 0, Type::None),
        f("isendwin", 0, 0, Type::Bool),
        f("newwin", 4, 4, Type::Any),
        f("cbreak", 0, 1, Type::None),
        f("nocbreak", 0, 1, Type::None),
        f("echo", 0, 1, Type::None),
        f("noecho", 0, 1, Type::None),
        f("raw", 0, 1, Type::None),
        f("noraw", 0, 1, Type::None),
        f("nl", 0, 1, Type::None),
        f("nonl", 0, 1, Type::None),
        f("getch", 0, 0, Type::Int),
        f("ungetch", 1, 1, Type::None),
        f("addstr", 1, 4, Type::None),
        f("addnstr", 2, 5, Type::None),
        f("addch", 1, 3, Type::None),
        f("getstr", 0, 3, Type::Bytes),
        f("move", 2, 2, Type::None),
        f("erase", 0, 0, Type::None),
        f("clear", 0, 0, Type::None),
        f("clrtobot", 0, 0, Type::None),
        f("clrtoeol", 0, 0, Type::None),
        f("standout", 0, 0, Type::None),
        f("standend", 0, 0, Type::None),
        f("refresh", 0, 0, Type::None),
        f("doupdate", 0, 0, Type::None),
        kw("setupterm", 0, 2, Type::None, &["term", "fd"]),
        f("tigetstr", 1, 1, Type::Bytes),
        f("tigetnum", 1, 1, Type::Int),
        f("tigetflag", 1, 1, Type::Int),
        f("keyname", 1, 1, Type::Str),
        f("has_key", 1, 1, Type::Bool),
        f("putp", 1, 1, Type::None),
        f("termname", 0, 0, Type::Any),
        f("longname", 0, 0, Type::Any),
        f("unctrl", 1, 1, Type::Any),
        f("start_color", 0, 0, Type::None),
        f("has_colors", 0, 0, Type::Bool),
        f("can_change_color", 0, 0, Type::Bool),
        f("init_pair", 3, 3, Type::None),
        f("init_color", 4, 4, Type::None),
        f("color_content", 1, 1, Type::Tuple),
        f("pair_content", 1, 1, Type::Tuple),
        f("curs_set", 1, 1, Type::Int),
        f("beep", 0, 0, Type::None),
        f("flash", 0, 0, Type::None),
        f("flushinp", 0, 0, Type::None),
        f("savetty", 0, 0, Type::None),
        f("resetty", 0, 0, Type::None),
        f("delay_output", 1, 1, Type::None),
        f("use_env", 0, 1, Type::None),
        f("filter", 0, 0, Type::None),
        f("get_escdelay", 0, 0, Type::Int),
        f("set_escdelay", 1, 1, Type::None),
    ],
    constants: &[
        // `KEY_*` from ncurses.
        c("KEY_CODE_YES", Type::Int),
        c("KEY_MIN", Type::Int),
        c("KEY_BREAK", Type::Int),
        c("KEY_DOWN", Type::Int),
        c("KEY_UP", Type::Int),
        c("KEY_LEFT", Type::Int),
        c("KEY_RIGHT", Type::Int),
        c("KEY_HOME", Type::Int),
        c("KEY_BACKSPACE", Type::Int),
        c("KEY_F0", Type::Int),
        c("KEY_DL", Type::Int),
        c("KEY_IL", Type::Int),
        c("KEY_DC", Type::Int),
        c("KEY_IC", Type::Int),
        c("KEY_EIC", Type::Int),
        c("KEY_CLEAR", Type::Int),
        c("KEY_EOS", Type::Int),
        c("KEY_EOL", Type::Int),
        c("KEY_SF", Type::Int),
        c("KEY_SR", Type::Int),
        c("KEY_NPAGE", Type::Int),
        c("KEY_PPAGE", Type::Int),
        c("KEY_STAB", Type::Int),
        c("KEY_CTAB", Type::Int),
        c("KEY_CATAB", Type::Int),
        c("KEY_ENTER", Type::Int),
        c("KEY_SRESET", Type::Int),
        c("KEY_RESET", Type::Int),
        c("KEY_PRINT", Type::Int),
        c("KEY_LL", Type::Int),
        c("KEY_A1", Type::Int),
        c("KEY_A3", Type::Int),
        c("KEY_B2", Type::Int),
        c("KEY_C1", Type::Int),
        c("KEY_C3", Type::Int),
        c("KEY_BTAB", Type::Int),
        c("KEY_BEG", Type::Int),
        c("KEY_CANCEL", Type::Int),
        c("KEY_CLOSE", Type::Int),
        c("KEY_COMMAND", Type::Int),
        c("KEY_COPY", Type::Int),
        c("KEY_CREATE", Type::Int),
        c("KEY_END", Type::Int),
        c("KEY_EXIT", Type::Int),
        c("KEY_FIND", Type::Int),
        c("KEY_HELP", Type::Int),
        c("KEY_MARK", Type::Int),
        c("KEY_MESSAGE", Type::Int),
        c("KEY_MOVE", Type::Int),
        c("KEY_NEXT", Type::Int),
        c("KEY_OPEN", Type::Int),
        c("KEY_OPTIONS", Type::Int),
        c("KEY_PREVIOUS", Type::Int),
        c("KEY_REDO", Type::Int),
        c("KEY_REFERENCE", Type::Int),
        c("KEY_REFRESH", Type::Int),
        c("KEY_REPLACE", Type::Int),
        c("KEY_RESTART", Type::Int),
        c("KEY_RESUME", Type::Int),
        c("KEY_SAVE", Type::Int),
        c("KEY_SBEG", Type::Int),
        c("KEY_SCANCEL", Type::Int),
        c("KEY_SCOMMAND", Type::Int),
        c("KEY_SCOPY", Type::Int),
        c("KEY_SCREATE", Type::Int),
        c("KEY_SDC", Type::Int),
        c("KEY_SDL", Type::Int),
        c("KEY_SELECT", Type::Int),
        c("KEY_SEND", Type::Int),
        c("KEY_SEOL", Type::Int),
        c("KEY_SEXIT", Type::Int),
        c("KEY_SFIND", Type::Int),
        c("KEY_SHELP", Type::Int),
        c("KEY_SHOME", Type::Int),
        c("KEY_SIC", Type::Int),
        c("KEY_SLEFT", Type::Int),
        c("KEY_SMESSAGE", Type::Int),
        c("KEY_SMOVE", Type::Int),
        c("KEY_SNEXT", Type::Int),
        c("KEY_SOPTIONS", Type::Int),
        c("KEY_SPREVIOUS", Type::Int),
        c("KEY_SPRINT", Type::Int),
        c("KEY_SREDO", Type::Int),
        c("KEY_SREPLACE", Type::Int),
        c("KEY_SRIGHT", Type::Int),
        c("KEY_SRSUME", Type::Int),
        c("KEY_SSAVE", Type::Int),
        c("KEY_SSUSPEND", Type::Int),
        c("KEY_SUNDO", Type::Int),
        c("KEY_SUSPEND", Type::Int),
        c("KEY_UNDO", Type::Int),
        c("KEY_MOUSE", Type::Int),
        c("KEY_RESIZE", Type::Int),
        c("KEY_MAX", Type::Int),
        // `KEY_F1`..`KEY_F63` (dynamic in the runtime).
        c("KEY_F1", Type::Int),
        c("KEY_F2", Type::Int),
        c("KEY_F3", Type::Int),
        c("KEY_F4", Type::Int),
        c("KEY_F5", Type::Int),
        c("KEY_F6", Type::Int),
        c("KEY_F7", Type::Int),
        c("KEY_F8", Type::Int),
        c("KEY_F9", Type::Int),
        c("KEY_F10", Type::Int),
        c("KEY_F11", Type::Int),
        c("KEY_F12", Type::Int),
        c("KEY_F13", Type::Int),
        c("KEY_F14", Type::Int),
        c("KEY_F15", Type::Int),
        c("KEY_F16", Type::Int),
        c("KEY_F17", Type::Int),
        c("KEY_F18", Type::Int),
        c("KEY_F19", Type::Int),
        c("KEY_F20", Type::Int),
        c("KEY_F21", Type::Int),
        c("KEY_F22", Type::Int),
        c("KEY_F23", Type::Int),
        c("KEY_F24", Type::Int),
        c("KEY_F25", Type::Int),
        c("KEY_F26", Type::Int),
        c("KEY_F27", Type::Int),
        c("KEY_F28", Type::Int),
        c("KEY_F29", Type::Int),
        c("KEY_F30", Type::Int),
        c("KEY_F31", Type::Int),
        c("KEY_F32", Type::Int),
        c("KEY_F33", Type::Int),
        c("KEY_F34", Type::Int),
        c("KEY_F35", Type::Int),
        c("KEY_F36", Type::Int),
        c("KEY_F37", Type::Int),
        c("KEY_F38", Type::Int),
        c("KEY_F39", Type::Int),
        c("KEY_F40", Type::Int),
        c("KEY_F41", Type::Int),
        c("KEY_F42", Type::Int),
        c("KEY_F43", Type::Int),
        c("KEY_F44", Type::Int),
        c("KEY_F45", Type::Int),
        c("KEY_F46", Type::Int),
        c("KEY_F47", Type::Int),
        c("KEY_F48", Type::Int),
        c("KEY_F49", Type::Int),
        c("KEY_F50", Type::Int),
        c("KEY_F51", Type::Int),
        c("KEY_F52", Type::Int),
        c("KEY_F53", Type::Int),
        c("KEY_F54", Type::Int),
        c("KEY_F55", Type::Int),
        c("KEY_F56", Type::Int),
        c("KEY_F57", Type::Int),
        c("KEY_F58", Type::Int),
        c("KEY_F59", Type::Int),
        c("KEY_F60", Type::Int),
        c("KEY_F61", Type::Int),
        c("KEY_F62", Type::Int),
        c("KEY_F63", Type::Int),
        // `A_*` attributes.
        c("A_NORMAL", Type::Int),
        c("A_ATTRIBUTES", Type::Int),
        c("A_CHARTEXT", Type::Int),
        c("A_COLOR", Type::Int),
        c("A_STANDOUT", Type::Int),
        c("A_UNDERLINE", Type::Int),
        c("A_REVERSE", Type::Int),
        c("A_BLINK", Type::Int),
        c("A_DIM", Type::Int),
        c("A_BOLD", Type::Int),
        c("A_ALTCHARSET", Type::Int),
        c("A_INVIS", Type::Int),
        c("A_PROTECT", Type::Int),
        c("A_HORIZONTAL", Type::Int),
        c("A_LEFT", Type::Int),
        c("A_LOW", Type::Int),
        c("A_RIGHT", Type::Int),
        c("A_TOP", Type::Int),
        c("A_VERTICAL", Type::Int),
        c("A_ITALIC", Type::Int),
        // `ACS_*` line-drawing characters.
        c("ACS_ULCORNER", Type::Int),
        c("ACS_LLCORNER", Type::Int),
        c("ACS_URCORNER", Type::Int),
        c("ACS_LRCORNER", Type::Int),
        c("ACS_LTEE", Type::Int),
        c("ACS_RTEE", Type::Int),
        c("ACS_BTEE", Type::Int),
        c("ACS_TTEE", Type::Int),
        c("ACS_HLINE", Type::Int),
        c("ACS_VLINE", Type::Int),
        c("ACS_PLUS", Type::Int),
        c("ACS_S1", Type::Int),
        c("ACS_S9", Type::Int),
        c("ACS_DIAMOND", Type::Int),
        c("ACS_CKBOARD", Type::Int),
        c("ACS_DEGREE", Type::Int),
        c("ACS_PLMINUS", Type::Int),
        c("ACS_BULLET", Type::Int),
        c("ACS_LARROW", Type::Int),
        c("ACS_RARROW", Type::Int),
        c("ACS_DARROW", Type::Int),
        c("ACS_UARROW", Type::Int),
        c("ACS_BOARD", Type::Int),
        c("ACS_LANTERN", Type::Int),
        c("ACS_BLOCK", Type::Int),
        c("ACS_S3", Type::Int),
        c("ACS_S7", Type::Int),
        c("ACS_LEQUAL", Type::Int),
        c("ACS_GEQUAL", Type::Int),
        c("ACS_PI", Type::Int),
        c("ACS_NEQUAL", Type::Int),
        c("ACS_STERLING", Type::Int),
        // Screen state (present only after the matching call, like CPython).
        c("LINES", Type::Int),
        c("COLS", Type::Int),
        c("COLORS", Type::Int),
        c("COLOR_PAIRS", Type::Int),
        // Class values.
        c("error", Type::Any),
        c("window", Type::Any),
    ],
};

/// The per-language codec accelerators: each exposes `getcodec(name)`, the
/// entry point `encodings/*.py` uses to obtain a codec object.
pub static _CODECS_CN: ModuleSpec = ModuleSpec {
    name: "_codecs_cn",
    functions: &[f("getcodec", 1, 1, Type::Any)],
    constants: &[],
};

pub static _CODECS_JP: ModuleSpec = ModuleSpec {
    name: "_codecs_jp",
    functions: &[f("getcodec", 1, 1, Type::Any)],
    constants: &[],
};

pub static _CODECS_KR: ModuleSpec = ModuleSpec {
    name: "_codecs_kr",
    functions: &[f("getcodec", 1, 1, Type::Any)],
    constants: &[],
};

pub static _CODECS_TW: ModuleSpec = ModuleSpec {
    name: "_codecs_tw",
    functions: &[f("getcodec", 1, 1, Type::Any)],
    constants: &[],
};

pub static _CODECS_HK: ModuleSpec = ModuleSpec {
    name: "_codecs_hk",
    functions: &[f("getcodec", 1, 1, Type::Any)],
    constants: &[],
};

pub static _CODECS_ISO2022: ModuleSpec = ModuleSpec {
    name: "_codecs_iso2022",
    functions: &[f("getcodec", 1, 1, Type::Any)],
    constants: &[],
};

/// `_multibytecodec`: the incremental codec base classes the `encodings`
/// wrappers derive from, plus `__create_codec` (wraps a codec object in a
/// `MultibyteCodec`).
pub static MULTIBYTECODEC: ModuleSpec = ModuleSpec {
    name: "_multibytecodec",
    functions: &[f("__create_codec", 1, 1, Type::Any)],
    constants: &[
        c("MultibyteCodec", Type::Any),
        c("MultibyteIncrementalEncoder", Type::Any),
        c("MultibyteIncrementalDecoder", Type::Any),
        c("MultibyteStreamReader", Type::Any),
        c("MultibyteStreamWriter", Type::Any),
    ],
};

/// `_zstd`: the compiled Zstandard codec behind `compression.zstd`. The
/// `Zstd*` classes are cached class values; the `_zstd_*` entries are their
/// native method/descriptor bodies (receiver included in the arity).
pub static _ZSTD: ModuleSpec = ModuleSpec {
    name: "_zstd",
    functions: &[
        f("get_frame_size", 1, 1, Type::Int),
        f("get_frame_info", 1, 1, Type::Tuple),
        f("train_dict", 3, 3, Type::Bytes),
        f("finalize_dict", 5, 5, Type::Bytes),
        f("get_param_bounds", 2, 2, Type::Tuple),
        f("set_parameter_types", 2, 2, Type::Any),
        f("_zstd_compressor_init", 1, 4, Type::Any),
        f("_zstd_compress", 2, 3, Type::Bytes),
        f("_zstd_flush", 1, 2, Type::Bytes),
        f("_zstd_set_pledged_input_size", 2, 2, Type::Any),
        f("_zstd_decompressor_init", 1, 3, Type::Any),
        f("_zstd_decompress", 2, 3, Type::Bytes),
        f("_zstd_dict_init", 2, 3, Type::Any),
        f("_zstd_dict_content", 1, 1, Type::Any),
        f("_zstd_dict_id", 1, 1, Type::Int),
        f("_zstd_dict_digested", 1, 1, Type::Tuple),
        f("_zstd_dict_undigested", 1, 1, Type::Tuple),
        f("_zstd_dict_prefix", 1, 1, Type::Tuple),
        f("_zstd_dict_repr", 1, 1, Type::Str),
    ],
    constants: &[
        c("ZstdCompressor", Type::Any),
        c("ZstdDecompressor", Type::Any),
        c("ZstdDict", Type::Any),
        c("ZstdError", Type::Any),
        c("zstd_version", Type::Str),
        c("zstd_version_number", Type::Int),
        c("ZSTD_CLEVEL_DEFAULT", Type::Int),
        c("ZSTD_DStreamOutSize", Type::Int),
        c("ZSTD_c_compressionLevel", Type::Int),
        c("ZSTD_c_windowLog", Type::Int),
        c("ZSTD_c_hashLog", Type::Int),
        c("ZSTD_c_chainLog", Type::Int),
        c("ZSTD_c_searchLog", Type::Int),
        c("ZSTD_c_minMatch", Type::Int),
        c("ZSTD_c_targetLength", Type::Int),
        c("ZSTD_c_strategy", Type::Int),
        c("ZSTD_c_enableLongDistanceMatching", Type::Int),
        c("ZSTD_c_ldmHashLog", Type::Int),
        c("ZSTD_c_ldmMinMatch", Type::Int),
        c("ZSTD_c_ldmBucketSizeLog", Type::Int),
        c("ZSTD_c_ldmHashRateLog", Type::Int),
        c("ZSTD_c_contentSizeFlag", Type::Int),
        c("ZSTD_c_checksumFlag", Type::Int),
        c("ZSTD_c_dictIDFlag", Type::Int),
        c("ZSTD_c_nbWorkers", Type::Int),
        c("ZSTD_c_jobSize", Type::Int),
        c("ZSTD_c_overlapLog", Type::Int),
        c("ZSTD_d_windowLogMax", Type::Int),
        c("ZSTD_fast", Type::Int),
        c("ZSTD_dfast", Type::Int),
        c("ZSTD_greedy", Type::Int),
        c("ZSTD_lazy", Type::Int),
        c("ZSTD_lazy2", Type::Int),
        c("ZSTD_btlazy2", Type::Int),
        c("ZSTD_btopt", Type::Int),
        c("ZSTD_btultra", Type::Int),
        c("ZSTD_btultra2", Type::Int),
    ],
};

// ---------------------------------------------------------------------------
// Module aliases
// ---------------------------------------------------------------------------
//
// CPython exposes several standard modules under two spellings: the C
// accelerator and its Python wrapper (`_abc`/`abc`, `_datetime`/`datetime`),
// a platform-specific path module (`posixpath`), or the frozen bootstrap
// modules (`_frozen_importlib`). Each alias reuses the target's tables so the
// two spellings never drift; the module name is the only difference. Runtime
// dispatch must forward the alias name to the target implementation.

/// `_abc`: the C accelerator behind `abc`.
pub static _ABC: ModuleSpec = ModuleSpec {
    name: "_abc",
    functions: ABC.functions,
    constants: ABC.constants,
};

/// `_datetime`: the C accelerator behind `datetime`.
pub static _DATETIME: ModuleSpec = ModuleSpec {
    name: "_datetime",
    functions: DATETIME.functions,
    constants: DATETIME.constants,
};

/// `_locale`: the C accelerator behind `locale`.
pub static _LOCALE: ModuleSpec = ModuleSpec {
    name: "_locale",
    functions: LOCALE.functions,
    constants: LOCALE.constants,
};

/// `_types`: the C accelerator behind `types`.
pub static _TYPES: ModuleSpec = ModuleSpec {
    name: "_types",
    functions: TYPES.functions,
    constants: TYPES.constants,
};

/// `_typing`: the C accelerator behind `typing`.
pub static _TYPING: ModuleSpec = ModuleSpec {
    name: "_typing",
    functions: TYPING.functions,
    constants: TYPING.constants,
};

/// `_sre`: the C implementation behind `re`. The expression-building
/// functions and the compiled-pattern surface are shared with `re`.
pub static _SRE: ModuleSpec = ModuleSpec {
    name: "_sre",
    functions: RE.functions,
    constants: RE.constants,
};

/// `_ssl`: the C module `ssl` wraps. Hydro implements TLS natively and has no
/// separate C layer, so the alias reuses `ssl`'s tables (the class/constant
/// values must stay identical and calls must reach one implementation).
pub static _SSL: ModuleSpec = ModuleSpec {
    name: "_ssl",
    functions: SSL.functions,
    constants: SSL.constants,
};

/// `posix`: the Unix flavour of `os`. The table is `os`'s, so the platform
/// spelling resolves the same names.
pub static POSIX: ModuleSpec = ModuleSpec {
    name: "posix",
    functions: OS.functions,
    constants: OS.constants,
};

/// `posixpath`: the POSIX `os.path` implementation. `os.path` already is the
/// POSIX module, so the names are shared. (`ntpath` below aliases the same
/// table: the function surface is identical; only Windows path syntax would
/// differ.)
pub static POSIXPATH: ModuleSpec = ModuleSpec {
    name: "posixpath",
    functions: OS_PATH.functions,
    constants: OS_PATH.constants,
};

/// `ntpath`: the Windows `os.path` implementation, aliased to the same
/// function surface as `posixpath`.
pub static NTPATH: ModuleSpec = ModuleSpec {
    name: "ntpath",
    functions: OS_PATH.functions,
    constants: OS_PATH.constants,
};

/// `_frozen_importlib`: the frozen `importlib` bootstrap module.
pub static _FROZEN_IMPORTLIB: ModuleSpec = ModuleSpec {
    name: "_frozen_importlib",
    functions: IMPORTLIB.functions,
    constants: IMPORTLIB.constants,
};

/// `_frozen_importlib_external`: the frozen `importlib._bootstrap_external`
/// module, whose public surface is `importlib.machinery`.
pub static _FROZEN_IMPORTLIB_EXTERNAL: ModuleSpec = ModuleSpec {
    name: "_frozen_importlib_external",
    functions: IMPORTLIB_MACHINERY.functions,
    constants: IMPORTLIB_MACHINERY.constants,
};

/// `importlib._bootstrap_external`: the Python spelling of the frozen external
/// bootstrap module. Literal imports of this name must compile, so it shares
/// the `importlib.machinery` surface like `_frozen_importlib_external`.
pub static IMPORTLIB_BOOTSTRAP_EXTERNAL: ModuleSpec = ModuleSpec {
    name: "importlib._bootstrap_external",
    functions: IMPORTLIB_MACHINERY.functions,
    constants: IMPORTLIB_MACHINERY.constants,
};

/// `zipimport`: the zip-file importer. Hydro has no zip import machinery, so
/// the table exposes the shared finder/loader surface from
/// `importlib.machinery` and nothing extra.
pub static ZIPIMPORT: ModuleSpec = ModuleSpec {
    name: "zipimport",
    functions: IMPORTLIB_MACHINERY.functions,
    constants: IMPORTLIB_MACHINERY.constants,
};

/// `_sysconfig`: the C accelerator behind `sysconfig`.
pub static _SYSCONFIG: ModuleSpec = ModuleSpec {
    name: "_sysconfig",
    functions: SYSCONFIG.functions,
    constants: SYSCONFIG.constants,
};

/// `faulthandler`: registered so imports resolve. Hydro installs no signal
/// handlers and has no fault-reporting API, so the module exposes no names.
pub static FAULTHANDLER: ModuleSpec = ModuleSpec {
    name: "faulthandler",
    functions: &[],
    constants: &[],
};

/// `_suggestions`: the typo-suggestion helper `traceback`/`name_error` use to
/// propose the closest matching name.
pub static _SUGGESTIONS: ModuleSpec = ModuleSpec {
    name: "_suggestions",
    functions: &[f("_generate_suggestions", 2, 2, Type::Any)],
    constants: &[],
};

/// `runpy`: the module runner. Hydro has no module execution ABI, so
/// `run_module` is the only declared entry point; the runtime raises an
/// explicit error when it cannot execute the target.
pub static RUNPY: ModuleSpec = ModuleSpec {
    name: "runpy",
    functions: &[
        kw(
            "run_module",
            1,
            4,
            Type::Any,
            &["mod_name", "init_globals", "run_name", "alter_sys"],
        ),
        // `_get_module_details(mod_name)`: the internal helper that resolves
        // the module metadata `run_module` executes.
        f("_get_module_details", 1, 1, Type::Any),
    ],
    constants: &[],
};

// Accelerator aliases for the standard-library modules whose C implementation
// shares the Python surface. Each reuses the target's tables so the two
// spellings cannot drift. (`_bisect`, `_heapq` and `_struct` already have
// their own alias specs above.)

/// `_json`: the C accelerator behind `json`. Only the encoder/decoder helpers
/// are C-level in CPython; sharing the whole table keeps both spellings
/// consistent here.
pub static _JSON: ModuleSpec = ModuleSpec {
    name: "_json",
    functions: JSON.functions,
    constants: JSON.constants,
};

/// `_csv`: the C accelerator behind `csv`.
pub static _CSV: ModuleSpec = ModuleSpec {
    name: "_csv",
    functions: CSV.functions,
    constants: CSV.constants,
};

/// `_pickle`: the C accelerator behind `pickle`.
pub static _PICKLE: ModuleSpec = ModuleSpec {
    name: "_pickle",
    functions: PICKLE.functions,
    constants: PICKLE.constants,
};

/// `_random`: the C accelerator behind `random`.
pub static _RANDOM: ModuleSpec = ModuleSpec {
    name: "_random",
    functions: RANDOM.functions,
    constants: RANDOM.constants,
};

/// `_statistics`: the C accelerator behind `statistics`.
pub static _STATISTICS: ModuleSpec = ModuleSpec {
    name: "_statistics",
    functions: STATISTICS.functions,
    constants: STATISTICS.constants,
};

/// `_queue`: the C accelerator behind `queue`.
pub static _QUEUE: ModuleSpec = ModuleSpec {
    name: "_queue",
    functions: QUEUE.functions,
    constants: QUEUE.constants,
};

/// `_asyncio`: the C accelerator behind `asyncio`.
pub static _ASYNCIO: ModuleSpec = ModuleSpec {
    name: "_asyncio",
    functions: ASYNCIO.functions,
    constants: ASYNCIO.constants,
};

/// `_multiprocessing`: the C accelerator behind `multiprocessing`.
pub static _MULTIPROCESSING: ModuleSpec = ModuleSpec {
    name: "_multiprocessing",
    functions: MULTIPROCESSING.functions,
    constants: MULTIPROCESSING.constants,
};

/// `_zoneinfo`: the C accelerator behind `zoneinfo`.
pub static _ZONEINFO: ModuleSpec = ModuleSpec {
    name: "_zoneinfo",
    functions: ZONEINFO.functions,
    constants: ZONEINFO.constants,
};

/// `_ast`: the C accelerator behind `ast`.
pub static _AST: ModuleSpec = ModuleSpec {
    name: "_ast",
    functions: AST.functions,
    constants: AST.constants,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stdlib::{constant, function, module};

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
        assert_eq!((field.min_args, field.max_args), (0, 0));
        assert!(
            field.params.is_empty(),
            "field() is keyword-only: no positional slots"
        );
        assert!(
            field.dynamic_keywords,
            "field() accepts its CPython keyword names dynamically"
        );
        let replace = function("dataclasses", "replace").expect("dataclasses.replace");
        assert_eq!((replace.min_args, replace.max_args), (1, 1));
        assert_eq!(replace.params, &["obj"][..]);
        assert!(
            replace.dynamic_keywords,
            "replace() forwards arbitrary keyword names to the runtime"
        );
        let asdict = function("dataclasses", "asdict").expect("dataclasses.asdict");
        assert_eq!((asdict.min_args, asdict.max_args), (1, 1));
        assert_eq!(asdict.ret, Type::Dict);
        assert_eq!(asdict.params, &["obj"][..]);
        assert!(
            asdict.dynamic_keywords,
            "asdict() accepts the keyword-only dict_factory"
        );
        assert_eq!(constant("dataclasses", "MISSING").unwrap().ty, Type::Any);
        let fields = function("dataclasses", "fields").expect("dataclasses.fields");
        assert_eq!((fields.min_args, fields.max_args), (1, 1));
        assert_eq!(fields.ret, Type::Tuple);
        assert!(
            !fields.dynamic_keywords,
            "fields() takes one positional argument and no keywords"
        );
    }

    #[test]
    fn datetime_value_classes_and_methods_are_registered() {
        for name in ["date", "time", "datetime", "timedelta", "tzinfo"] {
            assert_eq!(
                constant("datetime", name).expect(name).ty,
                Type::Any,
                "datetime.{name} is a class value"
            );
        }
        // Static constructors take no receiver.
        for name in [
            "_datetime_now",
            "_datetime_utcnow",
            "_date_today",
            "_datetime_today",
        ] {
            let spec = function("datetime", name).expect(name);
            assert_eq!(spec.min_args, 0, "{name} is static");
        }
        assert_eq!(
            function("datetime", "_datetime_fromtimestamp")
                .expect("fromtimestamp")
                .max_args,
            2
        );
        assert_eq!(
            function("datetime", "_datetime_combine")
                .expect("combine")
                .min_args,
            2
        );
        // `replace()` is keyword-only and forwards arbitrary names.
        let replace = function("datetime", "_datetime_replace").expect("replace");
        assert_eq!((replace.min_args, replace.max_args), (1, 1));
        assert!(replace.dynamic_keywords);
        // Comparisons/arithmetic take both operands.
        for name in [
            "_datetime_eq",
            "_datetime_lt",
            "_datetime_le",
            "_datetime_gt",
            "_datetime_ge",
            "_datetime_add",
            "_datetime_radd",
            "_datetime_sub",
        ] {
            let spec = function("datetime", name).expect(name);
            assert_eq!((spec.min_args, spec.max_args), (2, 2), "{name}");
        }
    }

    #[test]
    fn re_pattern_and_match_are_registered() {
        assert_eq!(constant("re", "Pattern").expect("Pattern").ty, Type::Any);
        assert_eq!(constant("re", "Match").expect("Match").ty, Type::Any);
    }

    #[test]
    fn dict_accepts_dynamic_keywords() {
        let spec = function("builtins", "dict").expect("builtins.dict");
        assert_eq!((spec.min_args, spec.max_args), (0, 1));
        assert!(
            spec.dynamic_keywords,
            "every keyword name of dict() is an entry"
        );
        assert!(spec.params.is_empty());
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
    fn contextlib_closing_and_exitstack_specs_are_registered() {
        let closing = function("contextlib", "closing").expect("closing");
        assert_eq!(
            (closing.min_args, closing.max_args, closing.ret),
            (1, 1, Type::Any)
        );

        let stack = function("contextlib", "ExitStack").expect("ExitStack");
        assert_eq!((stack.min_args, stack.max_args), (0, 0));

        // The native methods take the receiver first, so their arities
        // include it; `callback` additionally forwards keyword names.
        for (name, min, max) in [
            ("_closing_enter", 1, 1),
            ("_closing_exit", 4, 4),
            ("_exitstack_enter", 1, 1),
            ("_exitstack_exit", 4, 4),
            ("_exitstack_close", 1, 1),
            ("_exitstack_enter_context", 2, 2),
            ("_exitstack_push", 2, 2),
            ("_exitstack_pop_all", 1, 1),
        ] {
            let spec = function("contextlib", name).expect(name);
            assert_eq!((spec.min_args, spec.max_args), (min, max), "{name}");
        }
        let callback = function("contextlib", "_exitstack_callback").expect("callback");
        assert_eq!((callback.min_args, callback.max_args), (2, usize::MAX));
        assert!(callback.dynamic_keywords);

        // The abstract protocol classes `typing_extensions` imports for its
        // `ContextManager`/`AsyncContextManager` aliases.
        for name in ["AbstractContextManager", "AbstractAsyncContextManager"] {
            assert_eq!(constant("contextlib", name).expect(name).ty, Type::Any);
        }
        for (name, min, max) in [
            ("_abstract_cm_enter", 1, 1),
            ("_abstract_cm_exit", 4, 4),
            ("_abstract_cm_subclasshook", 2, 2),
            ("_abstract_acm_aenter", 1, 1),
            ("_abstract_acm_aexit", 4, 4),
            ("_abstract_acm_subclasshook", 2, 2),
        ] {
            let spec = function("contextlib", name).expect(name);
            assert_eq!((spec.min_args, spec.max_args), (min, max), "{name}");
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
        // `os.lstat(path, *, dir_fd=None)`.
        let lstat = function("os", "lstat").expect("os.lstat");
        assert_eq!(
            (lstat.min_args, lstat.max_args, lstat.ret),
            (1, 2, Type::Any)
        );
        assert_eq!(lstat.params, &["path", "dir_fd"][..]);
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
        assert_eq!(constant("os", "pathsep").unwrap().ty, Type::Str);
        assert_eq!(constant("os", "curdir").unwrap().ty, Type::Str);
        for name in [
            "O_RDONLY",
            "O_WRONLY",
            "O_RDWR",
            "O_CREAT",
            "O_TRUNC",
            "O_APPEND",
            "O_CLOEXEC",
            "O_NOCTTY",
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
    fn os_path_normpath_splitdrive_makedirs_and_copyfileobj_specs() {
        let normpath = function("os.path", "normpath").expect("os.path.normpath");
        assert_eq!(
            (normpath.min_args, normpath.max_args, normpath.ret),
            (1, 1, Type::Any)
        );
        let splitdrive = function("os.path", "splitdrive").expect("os.path.splitdrive");
        assert_eq!(
            (splitdrive.min_args, splitdrive.max_args, splitdrive.ret),
            (1, 1, Type::Tuple)
        );

        let makedirs = function("os", "makedirs").expect("os.makedirs");
        assert_eq!(
            (makedirs.min_args, makedirs.max_args, makedirs.ret),
            (1, 3, Type::None)
        );
        assert_eq!(makedirs.params, &["name", "mode", "exist_ok"][..]);
        assert_eq!(constant("os", "pardir").unwrap().ty, Type::Str);

        let copyfileobj = function("shutil", "copyfileobj").expect("shutil.copyfileobj");
        assert_eq!(
            (copyfileobj.min_args, copyfileobj.max_args, copyfileobj.ret),
            (2, 3, Type::None)
        );
        assert_eq!(copyfileobj.params, &["fsrc", "fdst", "length"][..]);
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
        for name in ["no_type_check", "no_type_check_decorator"] {
            let decorator = function("typing", name).expect(name);
            assert_eq!((decorator.min_args, decorator.max_args), (1, 1));
            assert_eq!(decorator.ret, Type::Any);
        }
        assert_eq!(constant("typing", "__all__").unwrap().ty, Type::List);
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
        // CPython 3.13+ sentinels used by typing_extensions/pydantic.
        for name in ["NoDefault", "NoExtraItems"] {
            assert_eq!(constant("typing", name).unwrap().ty, Type::Any, "{name}");
        }
        // Runtime type-assertion/introspection helpers added for pydantic.
        assert_eq!(
            (
                function("typing", "assert_type").unwrap().min_args,
                function("typing", "assert_type").unwrap().max_args
            ),
            (2, 2)
        );
        for name in ["reveal_type", "override", "is_typeddict", "is_protocol"] {
            let spec = function("typing", name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!((spec.min_args, spec.max_args), (1, 1), "{name}");
        }
        let members = function("typing", "get_protocol_members").expect("get_protocol_members");
        assert_eq!((members.min_args, members.max_args), (1, 1));
        assert_eq!(members.ret, Type::Set);
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
        // `typing_extensions` takes the 3.11 overload introspection helpers
        // when they exist, and uses `disjoint_base` when the runtime has it.
        let get_overloads = function("typing", "get_overloads").expect("get_overloads");
        assert_eq!(
            (
                get_overloads.min_args,
                get_overloads.max_args,
                get_overloads.ret
            ),
            (1, 1, Type::List)
        );
        let clear_overloads = function("typing", "clear_overloads").expect("clear_overloads");
        assert_eq!((clear_overloads.min_args, clear_overloads.max_args), (0, 0));
        assert_eq!(
            function("typing", "disjoint_base")
                .expect("disjoint_base")
                .max_args,
            1
        );
        // The private alias constructor is declared as a callable with the
        // keyword-only metadata names, while its attribute value stays the
        // real class.
        let alias = function("typing", "_ConcatenateGenericAlias").expect("alias ctor");
        assert_eq!((alias.min_args, alias.max_args), (2, 2));
        assert!(alias.dynamic_keywords);
        assert_eq!(alias.params, &["origin", "args"][..]);
    }

    #[test]
    fn pydantic_stdlib_surface_is_registered() {
        for name in ["Reader", "Writer"] {
            assert_eq!(constant("io", name).unwrap().ty, Type::Any, "{name}");
        }
        for name in ["SEEK_SET", "SEEK_CUR", "SEEK_END"] {
            assert_eq!(constant("io", name).unwrap().ty, Type::Int, "{name}");
        }
        let original_bases = function("types", "get_original_bases").expect("get_original_bases");
        assert_eq!((original_bases.min_args, original_bases.max_args), (1, 1));
        assert_eq!(original_bases.ret, Type::Tuple);
        assert_eq!(constant("warnings", "deprecated").unwrap().ty, Type::Any);
    }

    #[test]
    fn abc_surface_is_registered_and_enum_is_not_native() {
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
        // `enum` is the vendored CPython `enum.py`, not a native module: the
        // central table must not claim it, so the loader resolves `stdlib/enum.py`.
        assert!(module("enum").is_none(), "enum must not be a native module");
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
        assert_eq!(
            constant("__future__", "all_feature_names")
                .expect("all_feature_names")
                .ty,
            Type::List
        );
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
        assert!(namespace.dynamic_keywords);
        let dynamic = function("types", "DynamicClassAttribute").unwrap();
        assert_eq!((dynamic.min_args, dynamic.max_args), (0, 4));
        assert_eq!(dynamic.params, &["fget", "fset", "fdel", "doc"][..]);
        assert!(dynamic.dynamic_keywords);
    }

    #[test]
    fn errno_common_constants_are_registered() {
        for name in [
            "ENOENT",
            "EINVAL",
            "EACCES",
            "EEXIST",
            "ENOTDIR",
            "EISDIR",
            "ENOSPC",
            "EPIPE",
            "ECONNREFUSED",
            "ETIMEDOUT",
            "EAGAIN",
            "EWOULDBLOCK",
            "EINTR",
            "ENOMEM",
            "EBADF",
            "EIO",
            "ENOSYS",
            "EOPNOTSUPP",
            "ENOTSUP",
        ] {
            assert_eq!(constant("errno", name).unwrap().ty, Type::Int, "{name}");
        }
        assert_eq!(constant("errno", "EDOM").unwrap().ty, Type::Int);
        assert_eq!(constant("errno", "ERANGE").unwrap().ty, Type::Int);
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
        let pairwise = function("itertools", "pairwise").unwrap();
        assert_eq!((pairwise.min_args, pairwise.max_args), (1, 1));
        assert_eq!(pairwise.ret, Type::List);
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
        for name in ["takewhile", "dropwhile"] {
            let predicate_combinator = function("itertools", name).unwrap();
            assert_eq!(
                (predicate_combinator.min_args, predicate_combinator.max_args),
                (2, 2)
            );
            assert_eq!(predicate_combinator.ret, Type::List);
            assert!(!predicate_combinator.dynamic_keywords);
        }
    }

    #[test]
    fn ast_fractions_and_importlib_are_registered() {
        assert_eq!(function("ast", "literal_eval").unwrap().max_args, 1);
        assert!(function("ast", "arg").is_some(), "ast.arg constructor");
        assert_eq!(constant("ast", "arguments").unwrap().ty, Type::Any);
        // Statement/helper node constructors are keyword-dynamic and shared
        // with `_ast`.
        for name in [
            "Return",
            "If",
            "Else",
            "For",
            "While",
            "Try",
            "ExceptHandler",
            "Raise",
            "Assert",
            "Import",
            "ImportFrom",
            "FunctionDef",
            "AsyncFunctionDef",
            "ClassDef",
            "With",
            "AsyncWith",
            "Match",
            "match_case",
            "MatchCase",
            "AugAssign",
            "Delete",
            "Global",
            "Nonlocal",
            "Pass",
            "Break",
            "Continue",
            "Yield",
            "YieldFrom",
            "Await",
            "Lambda",
            "JoinedStr",
            "FormattedValue",
            "comprehension",
            "Comprehension",
            "arguments",
            "alias",
            "withitem",
            "TypeAlias",
            "TypeVar",
            "ParamSpec",
            "TypeVarTuple",
            "Assign",
            "NamedExpr",
            "TypeIgnore",
            "TryStar",
            "AsyncFor",
            "MatchValue",
            "MatchSingleton",
            "MatchSequence",
            "MatchMapping",
            "MatchClass",
            "MatchStar",
            "MatchAs",
            "MatchOr",
            "ListComp",
            "SetComp",
            "DictComp",
            "GeneratorExp",
        ] {
            for module in ["ast", "_ast"] {
                let spec = function(module, name).unwrap_or_else(|| panic!("{module}.{name}"));
                assert_eq!(
                    (spec.min_args, spec.max_args, spec.ret),
                    (0, usize::MAX, Type::Any),
                    "{module}.{name}"
                );
                assert!(spec.dynamic_keywords, "{module}.{name} accepts keywords");
            }
        }
        assert_eq!(
            constant("ast", "type_param").expect("type_param").ty,
            Type::Any
        );
        for name in ["pattern", "type_ignore"] {
            assert_eq!(
                constant("ast", name).expect(name).ty,
                Type::Any,
                "ast.{name}"
            );
        }
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
        // Single-dispatch decorators and `total_ordering` take exactly one
        // positional-or-keyword argument; the decorator helper the register
        // path uses takes `(dispatcher, cls, function)`.
        for (name, parameter) in [
            ("singledispatch", "func"),
            ("singledispatchmethod", "func"),
            ("total_ordering", "cls"),
        ] {
            let spec = function("functools", name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!((spec.min_args, spec.max_args), (1, 1), "{name}");
            assert_eq!(spec.params.len(), 1, "{name}");
            assert_eq!(spec.params[0], parameter, "{name}");
            assert!(spec.dynamic_keywords, "{name}");
            assert_eq!(spec.ret, Type::Any, "{name}");
        }
        let apply =
            function("functools", "_singledispatch_register_apply").expect("register helper");
        assert_eq!((apply.min_args, apply.max_args), (3, 3));
        assert!(!apply.dynamic_keywords);
        let compare = function("functools", "_total_ordering_compare").expect("ordering helper");
        assert_eq!((compare.min_args, compare.max_args), (4, 4));
        assert!(!compare.dynamic_keywords);
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
        // `re.search(pat, text, re.I)`: the flags argument is positional on
        // every matching entry point (`findall` has included it since 3.11).
        for (name, ret) in [
            ("search", Type::Any),
            ("match", Type::Any),
            ("fullmatch", Type::Any),
            ("findall", Type::List),
        ] {
            let spec = function("re", name).expect(name);
            assert_eq!(
                (spec.min_args, spec.max_args, spec.ret),
                (2, 3, ret),
                "{name}"
            );
        }
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
        assert_eq!((property.min_args, property.max_args), (0, 4));
        assert_eq!(property.params, &["fget", "fset", "fdel", "doc"][..]);
        // `type(name, bases, ns, **kwds)` forwards the class-header keywords
        // to the metaclass, so they are not declared parameters.
        let type_spec = function("builtins", "type").unwrap();
        assert_eq!((type_spec.min_args, type_spec.max_args), (1, 3));
        assert!(type_spec.dynamic_keywords);
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
        let module_spec = function("importlib.machinery", "_module_spec").expect("_module_spec");
        assert_eq!((module_spec.min_args, module_spec.max_args), (3, 3));
        assert_eq!(module_spec.ret, Type::Any);
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

    #[test]
    fn socket_accelerator_alias_mirrors_the_socket_spec() {
        let socket = crate::stdlib::module("socket").expect("socket spec");
        let accelerator = crate::stdlib::module("_socket").expect("_socket spec");
        assert_eq!(accelerator.name, "_socket");

        // Same functions, same arities and return types.
        assert_eq!(accelerator.functions.len(), socket.functions.len());
        for spec in socket.functions {
            let mirrored = function("_socket", spec.name).expect(spec.name);
            assert_eq!(
                (mirrored.min_args, mirrored.max_args, mirrored.ret),
                (spec.min_args, spec.max_args, spec.ret),
                "{}",
                spec.name
            );
            assert_eq!(mirrored.params, spec.params, "{}", spec.name);
            assert_eq!(
                mirrored.dynamic_keywords, spec.dynamic_keywords,
                "{}",
                spec.name
            );
        }

        // Same constants, including the class spellings pydantic probes.
        assert_eq!(accelerator.constants.len(), socket.constants.len());
        for name in [
            "AF_INET",
            "SOCK_STREAM",
            "SOCK_DGRAM",
            "IPPROTO_TCP",
            "SocketType",
            "error",
            "gaierror",
            "timeout",
        ] {
            assert!(
                constant("_socket", name).is_some(),
                "_socket.{name} is declared"
            );
        }
        for spec in socket.constants {
            assert_eq!(
                constant("_socket", spec.name).expect(spec.name).ty,
                spec.ty,
                "{}",
                spec.name
            );
        }
    }

    #[test]
    fn frozendict_and_pathlib_class_constants_are_registered() {
        // Pydantic 2.14 imports `frozendict` from `builtins` and looks up the
        // whole pure-path family on `pathlib`; both are class values.
        assert_eq!(
            constant("builtins", "frozendict").expect("frozendict").ty,
            Type::Any
        );
        for name in [
            "PurePath",
            "PosixPath",
            "WindowsPath",
            "PurePosixPath",
            "PureWindowsPath",
        ] {
            assert_eq!(
                constant("pathlib", name).expect(name).ty,
                Type::Any,
                "pathlib.{name} is a class constant"
            );
        }

        // `Path` remains a constructor function with its optional `path`
        // keyword; the class constants do not shadow it.
        let path = function("pathlib", "Path").expect("pathlib.Path");
        assert_eq!((path.min_args, path.max_args), (0, 1));
        assert_eq!(path.params, &["path"][..]);
        assert!(function("pathlib", "PurePath").is_none());
    }

    #[test]
    fn import_matrix_tier_one_declarations_are_registered() {
        // os: descriptor helpers and the bytes spelling of getcwd.
        for (name, min, max, ret) in [
            ("isatty", 1, 1, Type::Bool),
            ("getcwdb", 0, 0, Type::Bytes),
            ("read", 2, 2, Type::Bytes),
            ("write", 2, 2, Type::Int),
        ] {
            let spec = function("os", name).unwrap_or_else(|| panic!("os.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (min, max, ret));
            assert!(spec.params.is_empty(), "os.{name} is positional-only");
        }
        let fdopen = function("os", "fdopen").expect("os.fdopen");
        assert_eq!((fdopen.min_args, fdopen.max_args), (1, 8));
        assert_eq!(
            fdopen.params,
            &[
                "fd",
                "mode",
                "buffering",
                "encoding",
                "errors",
                "newline",
                "closefd",
                "opener",
            ][..]
        );
        assert_eq!(fdopen.ret, Type::Any);
        let mkdir = function("os", "mkdir").expect("os.mkdir");
        assert_eq!(
            (mkdir.min_args, mkdir.max_args, mkdir.ret),
            (1, 3, Type::None)
        );
        assert_eq!(mkdir.params, &["path", "mode", "dir_fd"][..]);
        let chmod = function("os", "chmod").expect("os.chmod");
        assert_eq!(
            (chmod.min_args, chmod.max_args, chmod.ret),
            (1, 4, Type::None)
        );
        assert_eq!(
            chmod.params,
            &["path", "mode", "dir_fd", "follow_symlinks"][..]
        );
        let mknod = function("os", "mknod").expect("os.mknod");
        assert_eq!(
            (mknod.min_args, mknod.max_args, mknod.ret),
            (1, 4, Type::None)
        );
        assert_eq!(mknod.params, &["path", "mode", "device", "dir_fd"][..]);
        let splitroot = function("os.path", "splitroot").expect("os.path.splitroot");
        assert_eq!(
            (splitroot.min_args, splitroot.max_args, splitroot.ret),
            (1, 1, Type::Any)
        );
        let gcd_import = function("importlib", "_gcd_import").expect("importlib._gcd_import");
        assert_eq!(
            (gcd_import.min_args, gcd_import.max_args, gcd_import.ret),
            (1, 3, Type::Any)
        );
        assert!(
            function("_frozen_importlib", "_gcd_import").is_some(),
            "_frozen_importlib shares the importlib table"
        );
        for (name, max) in [
            ("_create_stdlib_context", 9),
            ("_create_unverified_context", 9),
            ("_create_default_https_context", 4),
        ] {
            let spec = function("ssl", name).unwrap_or_else(|| panic!("ssl.{name}"));
            assert_eq!(
                (spec.min_args, spec.max_args, spec.ret),
                (0, max, Type::Any)
            );
            assert!(spec.dynamic_keywords, "ssl.{name} accepts keywords");
        }
        let a2b_uu = function("binascii", "a2b_uu").expect("binascii.a2b_uu");
        assert_eq!(
            (a2b_uu.min_args, a2b_uu.max_args, a2b_uu.ret),
            (1, 1, Type::Bytes)
        );
        let b2a_uu = function("binascii", "b2a_uu").expect("binascii.b2a_uu");
        assert_eq!(
            (b2a_uu.min_args, b2a_uu.max_args, b2a_uu.ret),
            (1, 2, Type::Bytes)
        );
        assert_eq!(b2a_uu.params, &["data", "backtick"][..]);
        let readlink = function("os", "readlink").expect("os.readlink");
        assert_eq!(
            (readlink.min_args, readlink.max_args, readlink.ret),
            (1, 2, Type::Any)
        );
        assert_eq!(readlink.params, &["path", "dir_fd"][..]);
        let strerror = function("os", "strerror").expect("os.strerror");
        assert_eq!(
            (strerror.min_args, strerror.max_args, strerror.ret),
            (1, 1, Type::Str)
        );
        let scandir = function("os", "scandir").expect("os.scandir");
        assert_eq!(
            (scandir.min_args, scandir.max_args, scandir.ret),
            (0, 1, Type::Any)
        );
        assert_eq!(scandir.params, &["path"][..]);
        for (name, min, max, ret) in [
            ("set_inheritable", 2, 2, Type::None),
            ("get_inheritable", 1, 1, Type::Bool),
        ] {
            let spec = function("os", name).unwrap_or_else(|| panic!("os.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (min, max, ret));
            assert!(spec.params.is_empty(), "os.{name} is positional-only");
        }
        let lexists = function("os.path", "lexists").expect("os.path.lexists");
        assert_eq!(
            (lexists.min_args, lexists.max_args, lexists.ret),
            (1, 1, Type::Bool)
        );
        for name in ["commonpath", "commonprefix"] {
            let spec = function("os.path", name).unwrap_or_else(|| panic!("os.path.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (1, 1, Type::Any));
        }
        let relpath = function("os.path", "relpath").expect("os.path.relpath");
        assert_eq!(
            (relpath.min_args, relpath.max_args, relpath.ret),
            (1, 2, Type::Any)
        );
        assert_eq!(relpath.params, &["path", "start"][..]);
        // The file-removal spelling and the remaining `os.open` flags.
        let unlink = function("os", "unlink").expect("os.unlink");
        assert_eq!(
            (unlink.min_args, unlink.max_args, unlink.ret),
            (1, 1, Type::None)
        );
        for name in [
            "O_EXCL",
            "O_NOFOLLOW",
            "O_DIRECTORY",
            "O_SYNC",
            "O_DSYNC",
            "O_ASYNC",
            "O_NONBLOCK",
            "O_NDELAY",
            "O_ACCMODE",
        ] {
            assert_eq!(constant("os", name).expect(name).ty, Type::Int, "os.{name}");
        }
        // Path-flavour constants resolve through both `os` and `os.path`.
        for name in ["sep", "pathsep", "extsep", "curdir", "pardir"] {
            assert_eq!(constant("os", name).expect(name).ty, Type::Str, "os.{name}");
            assert_eq!(
                constant("os.path", name).expect(name).ty,
                Type::Str,
                "os.path.{name}"
            );
        }
        for module in ["os", "os.path"] {
            assert_eq!(
                constant(module, "altsep").expect("altsep").ty,
                Type::Any,
                "{module}.altsep is None on POSIX"
            );
        }
        assert_eq!(
            constant("os.path", "ALLOW_MISSING")
                .expect("ALLOW_MISSING")
                .ty,
            Type::Any
        );

        // io/builtins open uses CPython's full parameter list.
        let cpython_open = [
            "file",
            "mode",
            "buffering",
            "encoding",
            "errors",
            "newline",
            "closefd",
            "opener",
        ];
        for module in ["io", "_io", "builtins"] {
            let spec = function(module, "open").unwrap_or_else(|| panic!("{module}.open"));
            assert_eq!((spec.min_args, spec.max_args), (1, 8), "{module}");
            assert_eq!(spec.params, &cpython_open[..], "{module}");
        }

        // builtins keyword spellings.
        let int = function("builtins", "int").expect("builtins.int");
        assert_eq!((int.min_args, int.max_args, int.ret), (0, 2, Type::Int));
        assert_eq!(int.params, &["x", "base"][..]);
        let enumerate = function("builtins", "enumerate").expect("builtins.enumerate");
        assert_eq!((enumerate.min_args, enumerate.max_args), (1, 2));
        assert_eq!(enumerate.params, &["iterable", "start"][..]);
        let format = function("builtins", "format").expect("builtins.format");
        assert_eq!((format.min_args, format.max_args), (1, 2));
        assert_eq!(format.params, &["value", "format_spec"][..]);
        let compile = function("builtins", "compile").expect("builtins.compile");
        assert_eq!(
            compile.params,
            &[
                "source",
                "filename",
                "mode",
                "flags",
                "dont_inherit",
                "optimize"
            ][..]
        );
        assert_eq!(
            constant("builtins", "EncodingWarning")
                .expect("EncodingWarning")
                .ty,
            Type::Any
        );

        // socket/time/math.
        let create_server = function("socket", "create_server").expect("socket.create_server");
        assert_eq!((create_server.min_args, create_server.max_args), (1, 1));
        assert!(create_server.dynamic_keywords);
        for (name, min, max, ret) in [("ctime", 0, 1, Type::Str), ("mktime", 1, 1, Type::Float)] {
            let spec = function("time", name).unwrap_or_else(|| panic!("time.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (min, max, ret));
        }
        let modf = function("math", "modf").expect("math.modf");
        assert_eq!(
            (modf.min_args, modf.max_args, modf.ret),
            (1, 1, Type::Tuple)
        );

        // Object models and serialisation.
        let getmodulename = function("inspect", "getmodulename").expect("inspect.getmodulename");
        assert_eq!((getmodulename.min_args, getmodulename.max_args), (1, 1));
        assert_eq!(getmodulename.params, &["path"][..]);
        let pickler = function("pickle", "Pickler").expect("pickle.Pickler");
        assert_eq!((pickler.min_args, pickler.max_args), (1, 2));
        assert_eq!(pickler.params, &["file", "protocol"][..]);
        // `zlib._ZlibDecompressor` is the class value `gzip` instantiates.
        assert_eq!(
            constant("zlib", "_ZlibDecompressor")
                .expect("zlib._ZlibDecompressor")
                .ty,
            Type::Any
        );
        // `code_page_encode` is Windows-only in CPython and is deliberately
        // absent here.
        let escape_decode = function("codecs", "escape_decode").expect("codecs.escape_decode");
        assert_eq!(
            (
                escape_decode.min_args,
                escape_decode.max_args,
                escape_decode.ret
            ),
            (1, 2, Type::Tuple)
        );
        assert!(function("codecs", "code_page_encode").is_none());

        // Constants and tracing hooks.
        for (module, name, ty) in [
            ("importlib.metadata", "MetadataPathFinder", Type::Any),
            ("warnings", "_DEPRECATED_MSG", Type::Str),
            ("warnings", "EncodingWarning", Type::Any),
            ("errno", "ENOTCONN", Type::Int),
            ("pickle", "HIGHEST_PROTOCOL", Type::Int),
            ("pickle", "PickleError", Type::Any),
            ("pickle", "PicklingError", Type::Any),
            ("pickle", "UnpicklingError", Type::Any),
            ("importlib", "_bootstrap_external", Type::Any),
            ("sys", "stdlib_module_names", Type::Set),
            ("pickle", "bytes_types", Type::Any),
            ("ssl", "Purpose", Type::Any),
            ("ssl", "SSLSocket", Type::Any),
            ("collections", "UserDict", Type::Any),
            ("collections", "UserList", Type::Any),
            ("collections", "UserString", Type::Any),
            ("zlib", "_ZlibCompressor", Type::Any),
        ] {
            assert_eq!(
                constant(module, name)
                    .unwrap_or_else(|| panic!("{module}.{name}"))
                    .ty,
                ty,
                "{module}.{name}"
            );
        }
        for (module, name) in [
            ("sys", "settrace"),
            ("sys", "setprofile"),
            ("threading", "settrace"),
        ] {
            let spec = function(module, name).unwrap_or_else(|| panic!("{module}.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (1, 1, Type::None));
        }
        let sys_exception = function("sys", "exception").expect("sys.exception");
        assert_eq!(
            (
                sys_exception.min_args,
                sys_exception.max_args,
                sys_exception.ret
            ),
            (0, 0, Type::Any),
            "sys.exception() returns the handled instance or None"
        );
        let unpickler = function("pickle", "Unpickler").expect("pickle.Unpickler");
        assert_eq!((unpickler.min_args, unpickler.max_args), (1, 5));
        assert_eq!(
            unpickler.params,
            &["file", "fix_imports", "encoding", "errors", "buffers"][..]
        );
        assert!(function("pickle", "Pickler").is_some());
        let referrers = function("gc", "get_referrers").expect("gc.get_referrers");
        assert_eq!(
            (referrers.min_args, referrers.max_args, referrers.ret),
            (0, usize::MAX, Type::List)
        );

        // The Unix password/group database queries return `struct_*`
        // namespaces (`getpwall`/`getgrall` return lists of them).
        for (module, name, min, max, ret) in [
            ("pwd", "struct_passwd", 1, 1, Type::Any),
            ("pwd", "getpwnam", 1, 1, Type::Any),
            ("pwd", "getpwuid", 1, 1, Type::Any),
            ("pwd", "getpwall", 0, 0, Type::List),
            ("grp", "struct_group", 1, 1, Type::Any),
            ("grp", "getgrnam", 1, 1, Type::Any),
            ("grp", "getgrgid", 1, 1, Type::Any),
            ("grp", "getgrall", 0, 0, Type::List),
            ("resource", "getrlimit", 1, 1, Type::Tuple),
            ("resource", "setrlimit", 2, 2, Type::None),
            ("resource", "getrusage", 1, 1, Type::Any),
            ("resource", "getpagesize", 0, 0, Type::Int),
            ("fcntl", "fcntl", 2, 3, Type::Any),
            ("fcntl", "ioctl", 2, 4, Type::Any),
            ("fcntl", "flock", 2, 2, Type::None),
            ("fcntl", "lockf", 2, 5, Type::None),
            ("syslog", "openlog", 0, 3, Type::None),
            ("syslog", "syslog", 1, 2, Type::None),
            ("syslog", "closelog", 0, 0, Type::None),
            ("syslog", "setlogmask", 1, 1, Type::Int),
        ] {
            let spec = function(module, name).unwrap_or_else(|| panic!("{module}.{name}"));
            assert_eq!(
                (spec.min_args, spec.max_args, spec.ret),
                (min, max, ret),
                "{module}.{name}"
            );
            assert!(spec.params.is_empty(), "{module}.{name} is positional-only");
        }

        // Every `resource`, `fcntl` and `syslog` constant is an integer
        // selector/flag.
        for (module, names) in [
            (
                "resource",
                &[
                    "RLIM_INFINITY",
                    "RLIMIT_CPU",
                    "RLIMIT_FSIZE",
                    "RLIMIT_DATA",
                    "RLIMIT_STACK",
                    "RLIMIT_CORE",
                    "RLIMIT_RSS",
                    "RLIMIT_NPROC",
                    "RLIMIT_NOFILE",
                    "RLIMIT_MEMLOCK",
                    "RLIMIT_AS",
                    "RLIMIT_LOCKS",
                    "RLIMIT_SIGPENDING",
                    "RLIMIT_MSGQUEUE",
                    "RLIMIT_NICE",
                    "RLIMIT_RTPRIO",
                    "RLIMIT_RTTIME",
                    "RUSAGE_SELF",
                    "RUSAGE_CHILDREN",
                    "RUSAGE_THREAD",
                ][..],
            ),
            (
                "fcntl",
                &[
                    "F_DUPFD",
                    "F_GETFD",
                    "F_SETFD",
                    "F_GETFL",
                    "F_SETFL",
                    "F_GETLK",
                    "F_SETLK",
                    "F_SETLKW",
                    "F_GETOWN",
                    "F_SETOWN",
                    "F_GETSIG",
                    "F_SETSIG",
                    "F_SETLEASE",
                    "F_GETLEASE",
                    "F_NOTIFY",
                    "F_DUPFD_CLOEXEC",
                    "F_SETPIPE_SZ",
                    "F_GETPIPE_SZ",
                    "F_ADD_SEALS",
                    "F_GET_SEALS",
                    "FD_CLOEXEC",
                    "LOCK_SH",
                    "LOCK_EX",
                    "LOCK_NB",
                    "LOCK_UN",
                ][..],
            ),
            (
                "syslog",
                &[
                    "LOG_EMERG",
                    "LOG_ALERT",
                    "LOG_CRIT",
                    "LOG_ERR",
                    "LOG_WARNING",
                    "LOG_NOTICE",
                    "LOG_INFO",
                    "LOG_DEBUG",
                    "LOG_KERN",
                    "LOG_USER",
                    "LOG_MAIL",
                    "LOG_DAEMON",
                    "LOG_AUTH",
                    "LOG_SYSLOG",
                    "LOG_LPR",
                    "LOG_NEWS",
                    "LOG_UUCP",
                    "LOG_CRON",
                    "LOG_AUTHPRIV",
                    "LOG_FTP",
                    "LOG_LOCAL0",
                    "LOG_LOCAL1",
                    "LOG_LOCAL2",
                    "LOG_LOCAL3",
                    "LOG_LOCAL4",
                    "LOG_LOCAL5",
                    "LOG_LOCAL6",
                    "LOG_LOCAL7",
                    "LOG_PID",
                    "LOG_CONS",
                    "LOG_ODELAY",
                    "LOG_NDELAY",
                    "LOG_NOWAIT",
                    "LOG_PERROR",
                ][..],
            ),
        ] {
            for &name in names {
                assert_eq!(
                    constant(module, name)
                        .unwrap_or_else(|| panic!("{module}.{name}"))
                        .ty,
                    Type::Int,
                    "{module}.{name}"
                );
            }
        }

        // `subprocess.Popen` declares the keyword-only options after the
        // leading six slots.
        let popen = function("subprocess", "Popen").expect("subprocess.Popen");
        assert_eq!((popen.min_args, popen.max_args), (1, 14));
        assert_eq!(
            popen.params,
            &[
                "args",
                "cwd",
                "stdout",
                "stderr",
                "text",
                "env",
                "close_fds",
                "start_new_session",
                "stdin",
                "encoding",
                "errors",
                "preexec_fn",
                "bufsize",
                "shell",
            ][..]
        );
        assert_eq!(
            constant("argparse", "REMAINDER")
                .expect("argparse.REMAINDER")
                .ty,
            Type::Str
        );
        assert_eq!(
            constant("pickle", "__all__").expect("pickle.__all__").ty,
            Type::List
        );

        // `socket.getfqdn` and the TCP-level option constants (shared with
        // `_socket`).
        let getfqdn = function("socket", "getfqdn").expect("socket.getfqdn");
        assert_eq!(
            (getfqdn.min_args, getfqdn.max_args, getfqdn.ret),
            (0, 1, Type::Str)
        );
        assert_eq!(getfqdn.params, &["name"][..]);
        for name in [
            "TCP_NODELAY",
            "TCP_MAXSEG",
            "TCP_CORK",
            "TCP_KEEPIDLE",
            "TCP_KEEPINTVL",
            "TCP_KEEPCNT",
            "TCP_SYNCNT",
            "TCP_LINGER2",
            "TCP_DEFER_ACCEPT",
            "TCP_WINDOW_CLAMP",
            "TCP_INFO",
            "TCP_QUICKACK",
            "TCP_CONGESTION",
            "TCP_USER_TIMEOUT",
            "TCP_FASTOPEN",
            "TCP_FASTOPEN_CONNECT",
            "TCP_ULP",
            "TCP_NOTSENT_LOWAT",
            "TCP_INQ",
        ] {
            for module in ["socket", "_socket"] {
                assert_eq!(
                    constant(module, name).expect(name).ty,
                    Type::Int,
                    "{module}.{name}"
                );
            }
        }
    }

    #[test]
    fn alias_modules_share_their_target_tables() {
        for (alias, target) in [
            ("_abc", "abc"),
            ("_datetime", "datetime"),
            ("_locale", "locale"),
            ("_types", "types"),
            ("_typing", "typing"),
            ("_sre", "re"),
            ("_ssl", "ssl"),
            ("posix", "os"),
            ("posixpath", "os.path"),
            ("ntpath", "os.path"),
            ("_frozen_importlib", "importlib"),
            ("_frozen_importlib_external", "importlib.machinery"),
            ("importlib._bootstrap_external", "importlib.machinery"),
            ("zipimport", "importlib.machinery"),
            ("_sysconfig", "sysconfig"),
            ("_json", "json"),
            ("_csv", "csv"),
            ("_pickle", "pickle"),
            ("_random", "random"),
            ("_statistics", "statistics"),
            ("_queue", "queue"),
            ("_asyncio", "asyncio"),
            ("_multiprocessing", "multiprocessing"),
            ("_zoneinfo", "zoneinfo"),
            ("_ast", "ast"),
        ] {
            let alias_spec = module(alias).unwrap_or_else(|| panic!("{alias}"));
            let target_spec = module(target).unwrap_or_else(|| panic!("{target}"));
            assert_eq!(alias_spec.name, alias);
            assert!(
                std::ptr::eq(alias_spec.functions, target_spec.functions),
                "{alias}.functions must be the {target} table"
            );
            assert!(
                std::ptr::eq(alias_spec.constants, target_spec.constants),
                "{alias}.constants must be the {target} table"
            );
        }
        // `_ssl` imports and member access resolve the `ssl` surface the
        // standard library uses (`http.client`, `urllib.request`, `smtplib`).
        let context = function("_ssl", "SSLContext").expect("_ssl.SSLContext");
        assert_eq!((context.min_args, context.max_args), (0, 1));
        assert!(function("_ssl", "_ssl_context_wrap_socket").is_some());
        assert!(constant("_ssl", "SSLSocket").is_some());
        assert!(constant("_ssl", "CERT_REQUIRED").is_some());
        let name = "faulthandler";
        let spec = module(name).unwrap_or_else(|| panic!("{name}"));
        assert!(spec.functions.is_empty(), "{name}");
        assert!(spec.constants.is_empty(), "{name}");
        // `_suggestions` now declares its real helper.
        let suggestions =
            function("_suggestions", "_generate_suggestions").expect("_suggestions helper");
        assert_eq!(
            (suggestions.min_args, suggestions.max_args, suggestions.ret),
            (2, 2, Type::Any)
        );
        // `runpy` is no longer empty: it declares its real entry points.
        let run_module = function("runpy", "run_module").expect("runpy.run_module");
        assert_eq!((run_module.min_args, run_module.max_args), (1, 4));
        assert_eq!(
            run_module.params,
            &["mod_name", "init_globals", "run_name", "alter_sys"][..]
        );
        let details = function("runpy", "_get_module_details").expect("runpy._get_module_details");
        assert_eq!(
            (details.min_args, details.max_args, details.ret),
            (1, 1, Type::Any)
        );
        // Every alias is registered exactly once.
        for name in [
            "_abc",
            "_datetime",
            "_locale",
            "_types",
            "_typing",
            "_sre",
            "_ssl",
            "posix",
            "posixpath",
            "ntpath",
            "_frozen_importlib",
            "_frozen_importlib_external",
            "importlib._bootstrap_external",
            "zipimport",
            "_sysconfig",
            "faulthandler",
            "_suggestions",
            "runpy",
            "_json",
            "_csv",
            "_pickle",
            "_random",
            "_statistics",
            "_queue",
            "_asyncio",
            "_multiprocessing",
            "_zoneinfo",
            "_ast",
            // Pre-existing accelerators, kept here to prove they are not
            // registered a second time.
            "_bisect",
            "_heapq",
            "_struct",
            "_sha3",
        ] {
            let count = crate::stdlib::MODULES
                .iter()
                .filter(|spec| spec.name == name)
                .count();
            assert_eq!(count, 1, "{name} must be registered exactly once");
        }
    }

    #[test]
    fn module_and_member_names_are_unique() {
        let mut modules: Vec<&str> = crate::stdlib::MODULES
            .iter()
            .map(|spec| spec.name)
            .collect();
        modules.sort_unstable();
        let total = modules.len();
        modules.dedup();
        assert_eq!(modules.len(), total, "duplicate module name in MODULES");
        for spec in crate::stdlib::MODULES {
            let mut functions: Vec<&str> = spec.functions.iter().map(|f| f.name).collect();
            functions.sort_unstable();
            let total = functions.len();
            functions.dedup();
            assert_eq!(
                functions.len(),
                total,
                "duplicate function in {}",
                spec.name
            );
            let mut constants: Vec<&str> = spec.constants.iter().map(|c| c.name).collect();
            constants.sort_unstable();
            let total = constants.len();
            constants.dedup();
            assert_eq!(
                constants.len(),
                total,
                "duplicate constant in {}",
                spec.name
            );
        }
    }

    #[test]
    fn unix_signal_termios_and_operator_specs_are_registered() {
        for (name, min, max, ret) in [
            ("signal", 2, 2, Type::Any),
            ("getsignal", 1, 1, Type::Any),
            ("raise_signal", 1, 1, Type::None),
            ("strsignal", 1, 1, Type::Any),
            ("valid_signals", 0, 0, Type::Set),
            ("pthread_sigmask", 2, 2, Type::Set),
            ("alarm", 1, 1, Type::Int),
            ("pause", 0, 0, Type::None),
            ("setitimer", 2, 3, Type::Tuple),
            ("getitimer", 1, 1, Type::Tuple),
            ("siginterrupt", 2, 2, Type::None),
            ("sigpending", 0, 0, Type::Set),
            ("default_int_handler", 2, 2, Type::Never),
        ] {
            let spec = function("_signal", name).unwrap_or_else(|| panic!("_signal.{name}"));
            assert_eq!(
                (spec.min_args, spec.max_args, spec.ret),
                (min, max, ret),
                "_signal.{name}"
            );
            assert!(spec.params.is_empty(), "_signal.{name} is positional-only");
        }
        for name in [
            "SIGHUP",
            "SIGINT",
            "SIGKILL",
            "SIGTERM",
            "SIG_DFL",
            "SIG_IGN",
            "SIG_BLOCK",
            "SIG_SETMASK",
            "ITIMER_REAL",
            "ITIMER_PROF",
            "SIGRTMIN",
            "SIGRTMAX",
            "NSIG",
        ] {
            assert_eq!(
                constant("_signal", name).expect(name).ty,
                Type::Int,
                "{name}"
            );
        }
        assert_eq!(
            constant("_signal", "ItimerError").expect("ItimerError").ty,
            Type::Any
        );

        for (name, arity, ret) in [
            ("tcgetattr", 1, Type::List),
            ("tcsetattr", 3, Type::None),
            ("tcsendbreak", 2, Type::None),
            ("tcdrain", 1, Type::None),
            ("tcflush", 2, Type::None),
            ("tcflow", 2, Type::None),
            ("tcgetwinsize", 1, Type::Tuple),
            ("tcsetwinsize", 2, Type::None),
        ] {
            let spec = function("termios", name).unwrap_or_else(|| panic!("termios.{name}"));
            assert_eq!(
                (spec.min_args, spec.max_args, spec.ret),
                (arity, arity, ret)
            );
        }
        // Every constant the runtime exposes (194 integers) plus `error`.
        let termios = module("termios").expect("termios");
        assert_eq!(termios.constants.len(), 195);
        for &name in &["IGNBRK", "ICANON", "VINTR", "B9600", "TCSANOW", "FIOASYNC"] {
            assert_eq!(
                constant("termios", name).expect(name).ty,
                Type::Int,
                "{name}"
            );
        }
        assert_eq!(constant("termios", "error").expect("error").ty, Type::Any);

        for name in ["and_", "xor"] {
            let spec = function("operator", name).unwrap_or_else(|| panic!("operator.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (2, 2, Type::Any));
        }
        let groupby = function("itertools", "groupby").expect("itertools.groupby");
        assert_eq!((groupby.min_args, groupby.max_args), (1, 2));
        assert_eq!(groupby.params, &["iterable", "key"][..]);
    }

    #[test]
    fn array_and_cmath_specs_are_registered() {
        let constructor = function("array", "array").expect("array.array");
        assert_eq!(
            (constructor.min_args, constructor.max_args, constructor.ret),
            (1, 2, Type::Any)
        );
        assert!(
            constructor.dynamic_keywords,
            "array(typecode[, initializer]) forwards keyword names"
        );
        assert_eq!(
            constant("array", "typecodes").expect("typecodes").ty,
            Type::Str
        );

        for name in [
            "acos", "acosh", "asin", "asinh", "atan", "atanh", "cos", "cosh", "exp", "sin", "sinh",
            "sqrt", "tan", "tanh", "log10", "rect",
        ] {
            let spec = function("cmath", name).unwrap_or_else(|| panic!("cmath.{name}"));
            assert_eq!(spec.ret, Type::Complex, "cmath.{name}");
        }
        let log = function("cmath", "log").expect("cmath.log");
        assert_eq!((log.min_args, log.max_args, log.ret), (1, 2, Type::Complex));
        let phase = function("cmath", "phase").expect("cmath.phase");
        assert_eq!(
            (phase.min_args, phase.max_args, phase.ret),
            (1, 1, Type::Float)
        );
        let polar = function("cmath", "polar").expect("cmath.polar");
        assert_eq!(
            (polar.min_args, polar.max_args, polar.ret),
            (1, 1, Type::Tuple)
        );
        let rect = function("cmath", "rect").expect("cmath.rect");
        assert_eq!(
            (rect.min_args, rect.max_args, rect.ret),
            (2, 2, Type::Complex)
        );
        for name in ["isfinite", "isinf", "isnan"] {
            let spec = function("cmath", name).unwrap_or_else(|| panic!("cmath.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (1, 1, Type::Bool));
        }
        let isclose = function("cmath", "isclose").expect("cmath.isclose");
        assert_eq!(
            (isclose.min_args, isclose.max_args, isclose.ret),
            (2, 4, Type::Bool)
        );
        assert!(isclose.dynamic_keywords);
        for (name, ty) in [
            ("pi", Type::Float),
            ("e", Type::Float),
            ("tau", Type::Float),
            ("inf", Type::Float),
            ("nan", Type::Float),
            ("infj", Type::Complex),
            ("nanj", Type::Complex),
        ] {
            assert_eq!(
                constant("cmath", name)
                    .unwrap_or_else(|| panic!("cmath.{name}"))
                    .ty,
                ty,
                "cmath.{name}"
            );
        }
    }

    #[test]
    fn textwrap_specs_cover_the_textwrapper_keyword_surface() {
        const OPTIONS: &[&str] = &[
            "width",
            "initial_indent",
            "subsequent_indent",
            "expand_tabs",
            "replace_whitespace",
            "fix_sentence_endings",
            "break_long_words",
            "break_on_hyphens",
            "drop_whitespace",
            "tabsize",
            "max_lines",
            "placeholder",
        ];

        let dedent = function("textwrap", "dedent").expect("textwrap.dedent");
        assert_eq!(
            (dedent.min_args, dedent.max_args, dedent.ret),
            (1, 1, Type::Str)
        );
        let indent = function("textwrap", "indent").expect("textwrap.indent");
        assert_eq!((indent.min_args, indent.max_args), (2, 3));
        assert_eq!(indent.params, &["text", "prefix", "predicate"][..]);

        for (name, ret) in [("wrap", Type::List), ("fill", Type::Str)] {
            let spec = function("textwrap", name).unwrap_or_else(|| panic!("textwrap.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (1, 13, ret));
            assert_eq!(spec.params.len(), 13, "{name} pads every option slot");
            assert_eq!(spec.params[0], "text");
            assert_eq!(&spec.params[1..], OPTIONS, "{name}");
        }
        let shorten = function("textwrap", "shorten").expect("textwrap.shorten");
        assert_eq!(
            (shorten.min_args, shorten.max_args, shorten.ret),
            (2, 13, Type::Str)
        );
        assert_eq!(&shorten.params[1..], OPTIONS);

        let wrapper = function("textwrap", "TextWrapper").expect("textwrap.TextWrapper");
        assert_eq!(
            (wrapper.min_args, wrapper.max_args, wrapper.ret),
            (0, 12, Type::Any)
        );
        assert_eq!(wrapper.params, OPTIONS);

        let method_wrap = function("textwrap", "_textwrapper_wrap").expect("_textwrapper_wrap");
        assert_eq!(
            (method_wrap.min_args, method_wrap.max_args, method_wrap.ret),
            (2, 2, Type::List)
        );
        let method_fill = function("textwrap", "_textwrapper_fill").expect("_textwrapper_fill");
        assert_eq!(
            (method_fill.min_args, method_fill.max_args, method_fill.ret),
            (2, 2, Type::Str)
        );
    }

    #[test]
    fn compression_xml_and_sqlite_specs_are_registered() {
        for (name, min, max, ret) in [
            ("BZ2Compressor", 0, 1, Type::Any),
            ("BZ2Decompressor", 0, 0, Type::Any),
            ("_bz2_compress", 2, 2, Type::Bytes),
            ("_bz2_flush", 1, 1, Type::Bytes),
            ("_bz2_decompress", 2, 3, Type::Bytes),
            ("_bz2_compressor_init", 1, 2, Type::None),
            ("_bz2_decompressor_init", 1, 1, Type::None),
        ] {
            let spec = function("_bz2", name).unwrap_or_else(|| panic!("_bz2.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (min, max, ret));
        }
        assert!(module("_bz2").expect("_bz2").constants.is_empty());

        for (name, min, max, ret) in [
            ("LZMACompressor", 0, 4, Type::Any),
            ("LZMADecompressor", 0, 3, Type::Any),
            ("_lzma_compress", 2, 2, Type::Bytes),
            ("_lzma_flush", 1, 1, Type::Bytes),
            ("_lzma_decompress", 2, 3, Type::Bytes),
            ("_lzma_compressor_init", 1, 5, Type::None),
            ("_lzma_decompressor_init", 1, 4, Type::None),
            ("is_check_supported", 1, 1, Type::Bool),
            ("_encode_filter_properties", 1, 1, Type::Bytes),
            ("_decode_filter_properties", 2, 2, Type::Any),
        ] {
            let spec = function("_lzma", name).unwrap_or_else(|| panic!("_lzma.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (min, max, ret));
        }
        assert_eq!(
            constant("_lzma", "LZMAError").expect("LZMAError").ty,
            Type::Any
        );
        for name in [
            "FORMAT_AUTO",
            "FORMAT_XZ",
            "FORMAT_ALONE",
            "FORMAT_RAW",
            "CHECK_NONE",
            "CHECK_CRC32",
            "CHECK_CRC64",
            "CHECK_SHA256",
            "CHECK_ID_MAX",
            "CHECK_UNKNOWN",
            "FILTER_LZMA1",
            "FILTER_LZMA2",
            "FILTER_DELTA",
            "FILTER_X86",
            "FILTER_POWERPC",
            "FILTER_IA64",
            "FILTER_ARM",
            "FILTER_ARMTHUMB",
            "FILTER_SPARC",
            "MF_HC3",
            "MF_HC4",
            "MF_BT2",
            "MF_BT3",
            "MF_BT4",
            "MODE_FAST",
            "MODE_NORMAL",
            "PRESET_DEFAULT",
            "PRESET_EXTREME",
        ] {
            assert_eq!(constant("_lzma", name).expect(name).ty, Type::Int, "{name}");
        }

        // pyexpat.
        let create = function("pyexpat", "ParserCreate").expect("ParserCreate");
        assert_eq!(
            (create.min_args, create.max_args, create.ret),
            (0, 3, Type::Any)
        );
        assert_eq!(
            create.params,
            &["encoding", "namespace_separator", "intern"][..]
        );
        let error_string = function("pyexpat", "ErrorString").expect("ErrorString");
        assert_eq!(
            (
                error_string.min_args,
                error_string.max_args,
                error_string.ret
            ),
            (1, 1, Type::Str)
        );
        for (name, min, max) in [
            ("_pyexpat_init", 1, 4),
            ("_pyexpat_parse", 2, 3),
            ("_pyexpat_parse_file", 2, 2),
            ("_pyexpat_set_base", 2, 2),
            ("_pyexpat_get_base", 1, 1),
            ("_pyexpat_external_entity_parser_create", 2, 3),
            ("_pyexpat_set_param_entity_parsing", 2, 2),
            ("_pyexpat_use_foreign_dtd", 1, 2),
            ("_pyexpat_get_buffer_text", 1, 1),
            ("_pyexpat_set_buffer_text", 2, 2),
            ("_pyexpat_get_buffer_size", 1, 1),
            ("_pyexpat_set_buffer_size", 2, 2),
            ("_pyexpat_get_buffer_used", 1, 1),
            ("_pyexpat_get_namespace_prefixes", 1, 1),
            ("_pyexpat_set_namespace_prefixes", 2, 2),
            ("_pyexpat_get_ordered_attributes", 1, 1),
            ("_pyexpat_set_ordered_attributes", 2, 2),
            ("_pyexpat_get_specified_attributes", 1, 1),
            ("_pyexpat_set_specified_attributes", 2, 2),
            ("_pyexpat_get_error_code", 1, 1),
            ("_pyexpat_get_current_line", 1, 1),
            ("_pyexpat_get_current_column", 1, 1),
            ("_pyexpat_get_current_byte", 1, 1),
        ] {
            let spec = function("pyexpat", name).unwrap_or_else(|| panic!("pyexpat.{name}"));
            assert_eq!((spec.min_args, spec.max_args), (min, max), "{name}");
        }
        for (name, ty) in [
            ("ExpatError", Type::Any),
            ("error", Type::Any),
            ("XMLParserType", Type::Any),
            ("errors", Type::Any),
            ("model", Type::Any),
            ("EXPAT_VERSION", Type::Str),
            ("version_info", Type::Tuple),
            ("native_encoding", Type::Str),
            ("features", Type::List),
            ("XML_PARAM_ENTITY_PARSING_NEVER", Type::Int),
            ("XML_PARAM_ENTITY_PARSING_UNLESS_STANDALONE", Type::Int),
            ("XML_PARAM_ENTITY_PARSING_ALWAYS", Type::Int),
        ] {
            assert_eq!(
                constant("pyexpat", name).expect(name).ty,
                ty,
                "pyexpat.{name}"
            );
        }

        // sqlite3 / _sqlite3.
        for module_name in ["sqlite3", "_sqlite3"] {
            let connect = function(module_name, "connect").expect("connect");
            assert_eq!(
                (connect.min_args, connect.max_args, connect.ret),
                (0, 8, Type::Any)
            );
            assert!(connect.dynamic_keywords);
            let complete = function(module_name, "complete_statement").expect("complete_statement");
            assert_eq!(
                (complete.min_args, complete.max_args, complete.ret),
                (1, 1, Type::Bool)
            );
            for name in ["Connection", "Cursor"] {
                assert!(
                    function(module_name, name).is_some(),
                    "{module_name}.{name}"
                );
                assert_eq!(
                    constant(module_name, name).expect(name).ty,
                    Type::Any,
                    "{module_name}.{name}"
                );
            }
            for name in [
                "Error",
                "Warning",
                "InterfaceError",
                "DatabaseError",
                "DataError",
                "OperationalError",
                "IntegrityError",
                "InternalError",
                "ProgrammingError",
                "NotSupportedError",
            ] {
                assert_eq!(
                    constant(module_name, name).expect(name).ty,
                    Type::Any,
                    "{module_name}.{name}"
                );
            }
            for name in ["apilevel", "paramstyle", "sqlite_version"] {
                assert_eq!(
                    constant(module_name, name).expect(name).ty,
                    Type::Str,
                    "{module_name}.{name}"
                );
            }
            for name in ["threadsafety", "PARSE_DECLTYPES", "PARSE_COLNAMES"] {
                assert_eq!(
                    constant(module_name, name).expect(name).ty,
                    Type::Int,
                    "{module_name}.{name}"
                );
            }
            assert_eq!(
                constant(module_name, "sqlite_version_info")
                    .expect("sqlite_version_info")
                    .ty,
                Type::Tuple,
                "{module_name}.sqlite_version_info"
            );
        }
        let sqlite = module("sqlite3").expect("sqlite3");
        let accelerator = module("_sqlite3").expect("_sqlite3");
        assert!(std::ptr::eq(sqlite.functions, accelerator.functions));
        assert!(std::ptr::eq(sqlite.constants, accelerator.constants));
    }

    #[test]
    fn readline_curses_and_codec_specs_are_registered() {
        for (name, min, max, ret) in [
            ("parse_and_bind", 1, 1, Type::None),
            ("get_line_buffer", 0, 0, Type::Str),
            ("read_history_file", 0, 1, Type::None),
            ("set_completer", 0, 1, Type::None),
            ("get_completer", 0, 0, Type::Any),
            ("get_begidx", 0, 0, Type::Int),
            ("get_completer_delims", 0, 0, Type::Str),
            ("get_current_history_length", 0, 0, Type::Int),
            ("redisplay", 0, 1, Type::None),
        ] {
            let spec = function("readline", name).unwrap_or_else(|| panic!("readline.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (min, max, ret));
        }
        let backend = constant("readline", "backend").expect("readline.backend");
        assert_eq!(backend.ty, Type::Str);

        let initscr = function("_curses", "initscr").expect("_curses.initscr");
        assert_eq!(
            (initscr.min_args, initscr.max_args, initscr.ret),
            (0, 0, Type::Any)
        );
        let newwin = function("_curses", "newwin").expect("_curses.newwin");
        assert_eq!(
            (newwin.min_args, newwin.max_args, newwin.ret),
            (4, 4, Type::Any)
        );
        let setupterm = function("_curses", "setupterm").expect("_curses.setupterm");
        assert_eq!((setupterm.min_args, setupterm.max_args), (0, 2));
        assert_eq!(setupterm.params, &["term", "fd"][..]);
        for (name, min, max) in [
            ("addstr", 1, 4),
            ("addnstr", 2, 5),
            ("addch", 1, 3),
            ("getstr", 0, 3),
            ("getch", 0, 0),
            ("move", 2, 2),
            ("cbreak", 0, 1),
            ("has_key", 1, 1),
        ] {
            let spec = function("_curses", name).unwrap_or_else(|| panic!("_curses.{name}"));
            assert_eq!((spec.min_args, spec.max_args), (min, max), "{name}");
        }
        for name in [
            "KEY_UP",
            "KEY_RESIZE",
            "KEY_MAX",
            "KEY_F1",
            "KEY_F63",
            "A_BOLD",
            "A_ITALIC",
            "ACS_HLINE",
            "LINES",
            "COLS",
            "COLORS",
            "COLOR_PAIRS",
        ] {
            assert_eq!(
                constant("_curses", name).expect(name).ty,
                Type::Int,
                "_curses.{name}"
            );
        }
        for name in ["error", "window"] {
            assert_eq!(
                constant("_curses", name).expect(name).ty,
                Type::Any,
                "_curses.{name}"
            );
        }

        for module_name in [
            "_codecs_cn",
            "_codecs_jp",
            "_codecs_kr",
            "_codecs_tw",
            "_codecs_hk",
            "_codecs_iso2022",
        ] {
            let getcodec = function(module_name, "getcodec").expect("getcodec");
            assert_eq!(
                (getcodec.min_args, getcodec.max_args, getcodec.ret),
                (1, 1, Type::Any),
                "{module_name}.getcodec"
            );
            assert!(module(module_name).expect(module_name).constants.is_empty());
        }
        let create = function("_multibytecodec", "__create_codec").expect("__create_codec");
        assert_eq!(
            (create.min_args, create.max_args, create.ret),
            (1, 1, Type::Any)
        );
        for name in [
            "MultibyteCodec",
            "MultibyteIncrementalEncoder",
            "MultibyteIncrementalDecoder",
            "MultibyteStreamReader",
            "MultibyteStreamWriter",
        ] {
            assert_eq!(
                constant("_multibytecodec", name).expect(name).ty,
                Type::Any,
                "_multibytecodec.{name}"
            );
        }
    }

    #[test]
    fn zstd_specs_are_registered() {
        for (name, min, max, ret) in [
            ("get_frame_size", 1, 1, Type::Int),
            ("get_frame_info", 1, 1, Type::Tuple),
            ("train_dict", 3, 3, Type::Bytes),
            ("finalize_dict", 5, 5, Type::Bytes),
            ("get_param_bounds", 2, 2, Type::Tuple),
            ("set_parameter_types", 2, 2, Type::Any),
            ("_zstd_compressor_init", 1, 4, Type::Any),
            ("_zstd_compress", 2, 3, Type::Bytes),
            ("_zstd_flush", 1, 2, Type::Bytes),
            ("_zstd_set_pledged_input_size", 2, 2, Type::Any),
            ("_zstd_decompressor_init", 1, 3, Type::Any),
            ("_zstd_decompress", 2, 3, Type::Bytes),
            ("_zstd_dict_init", 2, 3, Type::Any),
            ("_zstd_dict_content", 1, 1, Type::Any),
            ("_zstd_dict_id", 1, 1, Type::Int),
            ("_zstd_dict_digested", 1, 1, Type::Tuple),
            ("_zstd_dict_undigested", 1, 1, Type::Tuple),
            ("_zstd_dict_prefix", 1, 1, Type::Tuple),
            ("_zstd_dict_repr", 1, 1, Type::Str),
        ] {
            let spec = function("_zstd", name).unwrap_or_else(|| panic!("_zstd.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (min, max, ret));
        }
        for name in [
            "ZstdCompressor",
            "ZstdDecompressor",
            "ZstdDict",
            "ZstdError",
        ] {
            assert_eq!(
                constant("_zstd", name).expect(name).ty,
                Type::Any,
                "_zstd.{name}"
            );
        }
        assert_eq!(
            constant("_zstd", "zstd_version").expect("zstd_version").ty,
            Type::Str
        );
        for name in [
            "zstd_version_number",
            "ZSTD_CLEVEL_DEFAULT",
            "ZSTD_DStreamOutSize",
            "ZSTD_c_compressionLevel",
            "ZSTD_c_strategy",
            "ZSTD_d_windowLogMax",
            "ZSTD_fast",
            "ZSTD_btultra2",
        ] {
            assert_eq!(
                constant("_zstd", name).expect(name).ty,
                Type::Int,
                "_zstd.{name}"
            );
        }
    }

    #[test]
    fn dbm_backend_specs_are_registered() {
        let gdbm = function("_gdbm", "open").expect("_gdbm.open");
        assert_eq!((gdbm.min_args, gdbm.max_args, gdbm.ret), (1, 3, Type::Any));
        for (name, min, max, ret) in [
            ("_gdbm_close", 1, 1, Type::None),
            ("_gdbm_keys", 1, 1, Type::List),
            ("_gdbm_get", 1, 3, Type::Any),
            ("_gdbm_setdefault", 1, 3, Type::Any),
            ("_gdbm_firstkey", 1, 1, Type::Any),
            ("_gdbm_nextkey", 2, 2, Type::Any),
            ("_gdbm_reorganize", 1, 1, Type::None),
            ("_gdbm_sync", 1, 1, Type::None),
            ("_gdbm_clear", 1, 1, Type::None),
            ("_gdbm_contains", 2, 2, Type::Bool),
            ("_gdbm_len", 1, 1, Type::Int),
            ("_gdbm_bool", 1, 1, Type::Bool),
            ("_gdbm_getitem", 2, 2, Type::Any),
            ("_gdbm_setitem", 3, 3, Type::None),
            ("_gdbm_delitem", 2, 2, Type::None),
            ("_gdbm_enter", 1, 1, Type::Any),
            ("_gdbm_exit", 1, 4, Type::None),
        ] {
            let spec = function("_gdbm", name).unwrap_or_else(|| panic!("_gdbm.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (min, max, ret));
        }
        assert_eq!(constant("_gdbm", "error").expect("error").ty, Type::Any);
        assert_eq!(
            constant("_gdbm", "open_flags").expect("open_flags").ty,
            Type::Str
        );
        assert_eq!(
            constant("_gdbm", "_GDBM_VERSION")
                .expect("_GDBM_VERSION")
                .ty,
            Type::Tuple
        );

        let dbm = function("_dbm", "open").expect("_dbm.open");
        assert_eq!((dbm.min_args, dbm.max_args, dbm.ret), (1, 3, Type::Any));
        for (name, min, max, ret) in [
            ("_dbm_close", 1, 1, Type::None),
            ("_dbm_keys", 1, 1, Type::List),
            ("_dbm_get", 1, 3, Type::Any),
            ("_dbm_setdefault", 1, 3, Type::Any),
            ("_dbm_clear", 1, 1, Type::None),
            ("_dbm_contains", 2, 2, Type::Bool),
            ("_dbm_len", 1, 1, Type::Int),
            ("_dbm_bool", 1, 1, Type::Bool),
            ("_dbm_getitem", 2, 2, Type::Any),
            ("_dbm_setitem", 3, 3, Type::None),
            ("_dbm_delitem", 2, 2, Type::None),
            ("_dbm_enter", 1, 1, Type::Any),
            ("_dbm_exit", 1, 4, Type::None),
        ] {
            let spec = function("_dbm", name).unwrap_or_else(|| panic!("_dbm.{name}"));
            assert_eq!((spec.min_args, spec.max_args, spec.ret), (min, max, ret));
        }
        assert_eq!(constant("_dbm", "error").expect("error").ty, Type::Any);
        assert_eq!(constant("_dbm", "library").expect("library").ty, Type::Str);
    }

    #[test]
    fn hashlib_accelerator_specs_are_registered() {
        for (name, min, max, ret, params) in [
            ("new", 1, 2, Type::Any, &["name", "data"][..]),
            ("md5", 0, 1, Type::Any, &["data"][..]),
            ("sha1", 0, 1, Type::Any, &["data"][..]),
            ("sha224", 0, 1, Type::Any, &["data"][..]),
            ("sha256", 0, 1, Type::Any, &["data"][..]),
            ("sha384", 0, 1, Type::Any, &["data"][..]),
            ("sha512", 0, 1, Type::Any, &["data"][..]),
            ("blake2b", 0, 1, Type::Any, &["data"][..]),
            ("blake2s", 0, 1, Type::Any, &["data"][..]),
            ("sha3_224", 0, 1, Type::Any, &["data"][..]),
            ("sha3_256", 0, 1, Type::Any, &["data"][..]),
            ("sha3_384", 0, 1, Type::Any, &["data"][..]),
            ("sha3_512", 0, 1, Type::Any, &["data"][..]),
            ("shake_128", 0, 1, Type::Any, &["data"][..]),
            ("shake_256", 0, 1, Type::Any, &["data"][..]),
            (
                "pbkdf2_hmac",
                4,
                5,
                Type::Bytes,
                &["hash_name", "password", "salt", "iterations", "dklen"][..],
            ),
            ("compare_digest", 2, 2, Type::Bool, &["a", "b"][..]),
            (
                "hmac_new",
                1,
                3,
                Type::Any,
                &["key", "msg", "digestmod"][..],
            ),
            (
                "hmac_digest",
                3,
                3,
                Type::Bytes,
                &["key", "msg", "digestmod"][..],
            ),
            ("file_digest", 2, 2, Type::Any, &["fileobj", "digest"][..]),
            ("get_fips_mode", 0, 0, Type::Int, &[][..]),
            ("_is_fips_mode", 0, 0, Type::Bool, &[][..]),
            ("uuid1", 0, 2, Type::Bytes, &["node", "clock_seq"][..]),
            ("uuid4", 0, 0, Type::Bytes, &[][..]),
            ("uuid5", 2, 2, Type::Bytes, &["namespace", "name"][..]),
            ("generate_time_safe", 0, 0, Type::Tuple, &[][..]),
        ] {
            let spec = function("hashlib", name).unwrap_or_else(|| panic!("hashlib.{name}"));
            assert_eq!(
                (spec.min_args, spec.max_args, spec.ret),
                (min, max, ret),
                "hashlib.{name}"
            );
            assert_eq!(spec.params, params, "hashlib.{name}");
        }
        // `usedforsecurity` (and BLAKE2's extra parameters) are accepted as
        // dynamic keywords by the runtime binder.
        for name in [
            "new",
            "md5",
            "sha1",
            "sha224",
            "sha256",
            "sha384",
            "sha512",
            "blake2b",
            "blake2s",
            "sha3_224",
            "sha3_256",
            "sha3_384",
            "sha3_512",
            "shake_128",
            "shake_256",
        ] {
            assert!(
                function("hashlib", name).expect(name).dynamic_keywords,
                "hashlib.{name} accepts usedforsecurity"
            );
        }
        for (name, ty) in [
            ("algorithms_guaranteed", Type::Set),
            ("algorithms_available", Type::Set),
            ("HASH", Type::Any),
            ("HMAC", Type::Any),
            ("openssl_md_meth_names", Type::Set),
            ("UnsupportedDigestmodError", Type::Any),
        ] {
            assert_eq!(
                constant("hashlib", name).expect(name).ty,
                ty,
                "hashlib.{name}"
            );
        }

        // `_hashlib`: named/openssl constructors, HMAC helpers and FIPS.
        for name in [
            "new",
            "hmac_new",
            "hmac_digest",
            "compare_digest",
            "get_fips_mode",
            "_is_fips_mode",
            "pbkdf2_hmac",
            "openssl_md5",
            "openssl_sha1",
            "openssl_sha224",
            "openssl_sha256",
            "openssl_sha384",
            "openssl_sha512",
            "openssl_blake2b512",
            "openssl_blake2s256",
            "openssl_sha3_224",
            "openssl_sha3_256",
            "openssl_sha3_384",
            "openssl_sha3_512",
            "openssl_shake_128",
            "openssl_shake_256",
        ] {
            assert!(
                function("_hashlib", name).is_some(),
                "_hashlib.{name} is declared"
            );
        }
        for name in ["HASH", "HMAC", "UnsupportedDigestmodError"] {
            assert_eq!(
                constant("_hashlib", name).expect(name).ty,
                Type::Any,
                "_hashlib.{name}"
            );
        }
        assert_eq!(
            constant("_hashlib", "openssl_md_meth_names")
                .expect("openssl_md_meth_names")
                .ty,
            Type::Set
        );

        // Per-algorithm accelerators.
        for (module, names) in [
            ("_sha1", &["sha1"][..]),
            ("_sha2", &["sha224", "sha256", "sha384", "sha512"][..]),
            (
                "_sha3",
                &[
                    "sha3_224",
                    "sha3_256",
                    "sha3_384",
                    "sha3_512",
                    "shake_128",
                    "shake_256",
                ][..],
            ),
            ("_md5", &["md5"][..]),
            ("_blake2", &["blake2b", "blake2s"][..]),
        ] {
            for name in names {
                let spec = function(module, name).unwrap_or_else(|| panic!("{module}.{name}"));
                assert_eq!((spec.min_args, spec.max_args, spec.ret), (0, 1, Type::Any));
                assert_eq!(spec.params, &["data"][..]);
            }
        }
        let hmac_new = function("_hmac", "new").expect("_hmac.new");
        assert_eq!(
            (hmac_new.min_args, hmac_new.max_args, hmac_new.ret),
            (1, 3, Type::Any)
        );
        assert_eq!(hmac_new.params, &["key", "msg", "digestmod"][..]);
        let compute_digest = function("_hmac", "compute_digest").expect("_hmac.compute_digest");
        assert_eq!(
            (
                compute_digest.min_args,
                compute_digest.max_args,
                compute_digest.ret
            ),
            (3, 3, Type::Bytes)
        );
        assert_eq!(
            constant("_hmac", "UnknownHashError")
                .expect("UnknownHashError")
                .ty,
            Type::Any
        );
        assert_eq!(constant("_hmac", "HMAC").expect("_hmac.HMAC").ty, Type::Any);
        let time_safe = function("_uuid", "generate_time_safe").expect("_uuid.generate_time_safe");
        assert_eq!(
            (time_safe.min_args, time_safe.max_args, time_safe.ret),
            (0, 0, Type::Tuple)
        );
        for name in ["has_uuid_generate_time_safe", "has_stable_extractable_node"] {
            assert_eq!(
                constant("_uuid", name).expect(name).ty,
                Type::Bool,
                "_uuid.{name}"
            );
        }
        for name in [
            "NAMESPACE_DNS",
            "NAMESPACE_URL",
            "NAMESPACE_OID",
            "NAMESPACE_X500",
        ] {
            assert_eq!(
                constant("_uuid", name).expect(name).ty,
                Type::Str,
                "_uuid.{name}"
            );
        }

        for name in [
            "hashlib", "_hashlib", "_sha1", "_sha2", "_md5", "_blake2", "_hmac", "_uuid",
        ] {
            assert!(module(name).is_some(), "{name} is registered");
        }
    }

    #[test]
    fn re_private_submodule_specs_are_registered() {
        // `re._constants` / `re._compiler` / `re._parser` share one constant
        // table (`from ._constants import *` in CPython).
        let constants = module("re._constants").expect("re._constants");
        let compiler = module("re._compiler").expect("re._compiler");
        let parser = module("re._parser").expect("re._parser");
        assert!(constants.functions.is_empty());
        assert!(parser.functions.is_empty());
        assert!(std::ptr::eq(constants.constants, compiler.constants));
        assert!(std::ptr::eq(constants.constants, parser.constants));

        for (name, ty) in [
            ("MAGIC", Type::Int),
            ("MAXREPEAT", Type::Int),
            ("MAXGROUPS", Type::Int),
            ("SRE_FLAG_IGNORECASE", Type::Int),
            ("SRE_FLAG_ASCII", Type::Int),
            ("SRE_INFO_PREFIX", Type::Int),
            ("SRE_INFO_CHARSET", Type::Int),
            ("PatternError", Type::Any),
            ("error", Type::Any),
        ] {
            for module_name in ["re._constants", "re._compiler", "re._parser"] {
                assert_eq!(
                    constant(module_name, name)
                        .unwrap_or_else(|| panic!("{module_name}.{name}"))
                        .ty,
                    ty,
                    "{module_name}.{name}"
                );
            }
        }

        let compile = function("re._compiler", "compile").expect("re._compiler.compile");
        assert_eq!(
            (compile.min_args, compile.max_args, compile.ret),
            (1, 2, Type::Any)
        );
        let isstring = function("re._compiler", "isstring").expect("re._compiler.isstring");
        assert_eq!(
            (isstring.min_args, isstring.max_args, isstring.ret),
            (1, 1, Type::Bool)
        );

        // `re` re-exports the three private namespaces as module values.
        for name in ["_compiler", "_constants", "_parser"] {
            assert_eq!(constant("re", name).expect(name).ty, Type::Any, "re.{name}");
        }
    }

    #[test]
    fn importlib_machinery_and_util_specs_cover_pkgutil() {
        for (name, min, max, ret) in [
            ("_install", 1, 1, Type::None),
            ("_path_finder_find_spec", 2, 4, Type::Any),
            ("_path_finder_invalidate_caches", 1, 1, Type::None),
            ("_file_finder_init", 2, usize::MAX, Type::None),
            ("_file_finder_path_hook", 1, usize::MAX, Type::Any),
            ("_file_finder_hook", 1, 1, Type::Any),
            ("_file_finder_find_spec", 2, 3, Type::Any),
            ("_file_finder_invalidate_caches", 1, 1, Type::None),
        ] {
            let spec = function("importlib.machinery", name)
                .unwrap_or_else(|| panic!("importlib.machinery.{name}"));
            assert_eq!(
                (spec.min_args, spec.max_args, spec.ret),
                (min, max, ret),
                "importlib.machinery.{name}"
            );
        }
        for name in [
            "PathFinder",
            "FileFinder",
            "SourcelessFileLoader",
            "AppleFrameworkLoader",
        ] {
            assert_eq!(
                constant("importlib.machinery", name).expect(name).ty,
                Type::Any,
                "{name}"
            );
        }
        assert_eq!(
            constant("importlib.machinery", "_zip_directory_cache")
                .expect("_zip_directory_cache")
                .ty,
            Type::Dict
        );

        let cache_from_source =
            function("importlib.util", "cache_from_source").expect("cache_from_source");
        assert_eq!(
            (
                cache_from_source.min_args,
                cache_from_source.max_args,
                cache_from_source.ret
            ),
            (1, 3, Type::Str)
        );
        assert_eq!(
            cache_from_source.params,
            &["path", "debug_override", "optimization"][..]
        );
        let source_hash = function("importlib.util", "source_hash").expect("source_hash");
        assert_eq!(
            (source_hash.min_args, source_hash.max_args, source_hash.ret),
            (1, 1, Type::Bytes)
        );

        assert_eq!(
            constant("sys", "path_hooks").expect("path_hooks").ty,
            Type::List
        );
        assert_eq!(
            constant("importlib", "_bootstrap").expect("_bootstrap").ty,
            Type::Any
        );
    }
}
