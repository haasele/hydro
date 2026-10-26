//! Static vocabulary of the IR: types, constants, operators, builtins,
//! exceptions and call targets.
//!
//! These are the value-level definitions shared by [`crate::Program`] and the
//! runtime ABI. Operator codes and exception tags are stable encodings.

use std::fmt;

/// Index into [`Function::locals`](crate::Function::locals).
pub type LocalId = u32;

/// Index into [`Program::functions`].
pub type FunctionId = u32;

/// Static type of an expression, local, parameter or return value.
///
/// `Any` means "only known at runtime" and forces codegen to use the dynamic
/// runtime operators. `Never` only appears as a return type of functions that
/// cannot return normally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    Int,
    /// Arbitrary-precision integer (values outside the tagged immediate
    /// range). Numeric, but distinct from [`Type::Int`].
    BigInt,
    Float,
    Bool,
    Str,
    /// Immutable byte string.
    Bytes,
    None,
    Any,
    Never,
    /// Untyped list (`Any` elements).
    List,
    /// Untyped dict (keys must be hashable at runtime).
    Dict,
    /// Untyped tuple (`Any` elements).
    Tuple,
    /// Insertion-ordered set (elements must be hashable).
    Set,
    /// Immutable arithmetic progression (`range(...)`).
    Range,
    /// Callable function value (named function or lambda).
    Function,
}

impl Type {
    /// True for `Int`, `BigInt` and `Float` (and `Bool`, which is numerically
    /// compatible).
    pub fn is_numeric(self) -> bool {
        matches!(self, Type::Int | Type::BigInt | Type::Float | Type::Bool)
    }

    pub fn name(self) -> &'static str {
        match self {
            Type::Int => "int",
            Type::BigInt => "BigInt",
            Type::Float => "float",
            Type::Bool => "bool",
            Type::Str => "str",
            Type::Bytes => "bytes",
            Type::None => "None",
            Type::Any => "Any",
            Type::Never => "Never",
            Type::List => "list",
            Type::Dict => "dict",
            Type::Tuple => "tuple",
            Type::Set => "set",
            Type::Range => "range",
            Type::Function => "function",
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Compile-time constant.
#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Int(i64),
    /// Decimal literal (optional leading `-`) that does not fit the tagged
    /// immediate range. Canonical: no leading zeroes beyond `0`, no `+`.
    BigInt(String),
    Float(f64),
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
    None,
    /// Python's `NotImplemented` singleton. Codegen materialises the runtime's
    /// reserved `NotImplemented` immediate; Hydro has no `NotImplementedType`,
    /// so its static type is [`Type::Any`].
    NotImplemented,
    /// Python's `...` literal. Documented limitation: Hydro has no distinct
    /// `EllipsisType` sentinel yet, so codegen materialises the runtime `None`
    /// value and the static type is [`Type::None`]. Expression statements such
    /// as `def f(): ...` therefore behave exactly like `None`, but `... is
    /// None` is `True` instead of `False`.
    Ellipsis,
    /// A fully literal tuple default (`padding=(0, 1)`). Codegen materialises
    /// a fresh runtime tuple; nested tuples, strings and scalars are allowed.
    Tuple(Vec<Const>),
    /// A module-level value referenced by name, used for defaults such as
    /// `NO_CHANGE` or `box.HEAVY_HEAD`. Codegen reads the module global table.
    Global {
        module: String,
        name: String,
    },
}

impl Const {
    pub fn ty(&self) -> Type {
        match self {
            Const::Int(_) => Type::Int,
            Const::BigInt(_) => Type::BigInt,
            Const::Float(_) => Type::Float,
            Const::Bool(_) => Type::Bool,
            Const::Str(_) => Type::Str,
            Const::Bytes(_) => Type::Bytes,
            Const::None => Type::None,
            Const::NotImplemented => Type::Any,
            Const::Ellipsis => Type::None,
            Const::Tuple(_) => Type::Tuple,
            Const::Global { .. } => Type::Any,
        }
    }
}

/// Binary operators. `code()` is the stable runtime ABI encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `left in right`
    In,
    /// `left not in right`
    NotIn,
    /// `left is right`
    Is,
    /// `left is not right`
    IsNot,
    /// `left & right`
    BitAnd,
    /// `left | right`
    BitOr,
    /// `left ^ right`
    BitXor,
    /// `left << right`
    Shl,
    /// `left >> right`
    Shr,
}

impl BinOp {
    /// Stable encoding shared with `hydro_runtime` and `docs/ARCHITECTURE.md`.
    pub fn code(self) -> u32 {
        match self {
            BinOp::Add => 0,
            BinOp::Sub => 1,
            BinOp::Mul => 2,
            BinOp::Div => 3,
            BinOp::FloorDiv => 4,
            BinOp::Mod => 5,
            BinOp::Pow => 6,
            BinOp::Eq => 7,
            BinOp::Ne => 8,
            BinOp::Lt => 9,
            BinOp::Le => 10,
            BinOp::Gt => 11,
            BinOp::Ge => 12,
            BinOp::In => 13,
            BinOp::NotIn => 14,
            BinOp::Is => 15,
            BinOp::IsNot => 16,
            BinOp::BitAnd => 17,
            BinOp::BitOr => 18,
            BinOp::BitXor => 19,
            BinOp::Shl => 20,
            BinOp::Shr => 21,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::FloorDiv => "//",
            BinOp::Mod => "%",
            BinOp::Pow => "**",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::In => "in",
            BinOp::NotIn => "not in",
            BinOp::Is => "is",
            BinOp::IsNot => "is not",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
        }
    }

    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinOp::Eq
                | BinOp::Ne
                | BinOp::Lt
                | BinOp::Le
                | BinOp::Gt
                | BinOp::Ge
                | BinOp::Is
                | BinOp::IsNot
                | BinOp::In
                | BinOp::NotIn
        )
    }
}

/// Unary operators. `code()` is the stable runtime ABI encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Pos,
    Not,
    /// `~value`: bitwise inversion of an `int` (including `bool`, like
    /// Python); any other operand raises `TypeError` at runtime.
    Invert,
}

impl UnOp {
    pub fn code(self) -> u32 {
        match self {
            UnOp::Neg => 0,
            UnOp::Pos => 1,
            UnOp::Not => 2,
            UnOp::Invert => 3,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Pos => "+",
            UnOp::Not => "not",
            UnOp::Invert => "~",
        }
    }
}

/// Short-circuiting boolean operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolOp {
    And,
    Or,
}

/// Builtins known to the compiler. `range` is special-cased by `for` lowering
/// and therefore not listed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Print,
    Len,
    Str,
    /// `repr(value)`: Python's representation string.
    Repr,
    /// `hash(value)`: a deterministic integer hash. Equal values hash equally
    /// and unhashable values (`list`, `dict`, `set`, iterator, or a tuple
    /// containing one) raise `TypeError`. The value is implementation-defined
    /// and not reproducible against CPython.
    Hash,
    Int,
    Float,
    Abs,
    /// `bool(x)`.
    Bool,
    /// `type(x)`.
    Type,
    /// `reversed(iterable)`.
    Reversed,
    /// `__import__(name, ...)`: dynamic imports are not supported at runtime.
    Import,
    /// `set()` / `set(iterable)`.
    Set,
    /// `range(...)` as a first-class value.
    Range,
    /// `list()` / `list(iterable)`.
    List,
    /// `tuple()` / `tuple(iterable)`: an immutable sequence.
    Tuple,
    /// `dict()` / `dict(iterable)`.
    Dict,
    /// `sum(iterable[, start])`.
    Sum,
    /// `enumerate(iterable[, start])`.
    Enumerate,
    /// `zip(*iterables)`.
    Zip,
    /// `map(function, iterable)` (materialised list).
    Map,
    /// `filter(function_or_none, iterable)` (materialised list).
    Filter,
    /// `bytes()` / `bytes(int)` / `bytes(iterable)` / `bytes(str, encoding)`.
    Bytes,
    /// `bytearray()` / `bytearray(bytes_like)` / `bytearray(iterable)` /
    /// `bytearray(str, encoding)`: a mutable byte sequence. The runtime owns
    /// the value kind (`KIND_BYTEARRAY`); this variant names the callable.
    ByteArray,
    /// `memoryview(bytes_like)`: a view over a bytes-like value. The runtime
    /// owns the value kind (`KIND_MEMORYVIEW`).
    MemoryView,
    /// `classmethod(function)`: a descriptor that binds the class as the
    /// first argument when accessed through a class or instance.
    Classmethod,
    /// `staticmethod(function)`: a descriptor that returns the wrapped
    /// callable unchanged.
    Staticmethod,
    /// `property(fget[, fset[, fdel[, doc]]])`: a data descriptor whose
    /// `__get__` calls the getter with the instance.
    Property,
    /// `iter(obj)`: the object's iterator (an iterator/generator is returned
    /// unchanged).
    Iter,
    /// `next(iterator[, default])`: the next element, `StopIteration` at the
    /// end unless a default is given.
    Next,
    /// `frozenset()` / `frozenset(iterable)`. Hydro values are immutable sets:
    /// the result shares the runtime set representation.
    Frozenset,
    /// `getattr(object, name)`: the attribute value, raising `AttributeError`
    /// when it is missing.
    Getattr,
    /// `hasattr(object, name)`: whether the attribute exists. Only an
    /// `AttributeError` is swallowed; any other failure propagates.
    Hasattr,
    /// `max(*args)`: the largest candidate, or the largest element of a
    /// single iterable.
    Max,
    /// `min(*args)`: the counterpart of [`Builtin::Max`].
    Min,
    /// `sorted(iterable, key=None, reverse=False)`: a new list with the
    /// elements in ascending order (stable). Direct lowering always appends
    /// the `key` and `reverse` slots.
    Sorted,
    /// `round(value[, ndigits])`.
    Round,
    /// `format(value, spec)`: Python's built-in formatting. F-string
    /// interpolation fields reuse this builtin with two extra constant
    /// arguments (the spec and conversion flag); the argument list
    /// distinguishes the two shapes.
    Format,
    /// `isinstance(object, type)`.
    Isinstance,
    /// `issubclass(class, class_or_tuple)`: whether the first class derives
    /// from the second (or from any element of a tuple). Both arguments are
    /// validated at runtime; non-classes raise `TypeError`.
    Issubclass,
    /// `all(iterable)`: `True` when every element is truthy.
    All,
    AnyValue,
    /// `ord(character)`: the Unicode scalar value of a one-character `str`.
    /// Any other string length or a non-`str` argument raises `TypeError`.
    Ord,
    /// `chr(codepoint)`: the one-character `str` for an integer in
    /// `0..=0x10FFFF`. Surrogate code points are rejected because Hydro strings
    /// are UTF-8; a non-integer raises `TypeError` and an out-of-range or
    /// surrogate code point raises `ValueError`.
    Chr,
    /// `divmod(a, b)`: the pair `(a // b, a % b)` as a 2-tuple, with Python's
    /// floor-division semantics for ints and floats. A zero divisor raises
    /// `ZeroDivisionError`; non-numeric operands raise `TypeError`.
    Divmod,
    /// `callable(object)`: `True` for functions, builtins, native module
    /// functions, classes and extension functions.
    Callable,
    /// Internal: merge two dicts into a fresh dict (left to right, last value
    /// wins, insertion order preserved). Produced by dict literals that
    /// contain `**unpacking`; not reachable from source code.
    DictMerge,
    /// `input([prompt])`: writes `prompt` to stdout (no newline), reads one
    /// line from stdin, strips the trailing newline and returns it as a `str`.
    /// End of file raises `RuntimeError`.
    Input,
    /// `get_ipython()`: interactive-shell probe used by libraries such as
    /// rich. Hydro has no IPython, so the call returns an object whose
    /// `__class__.__name__` is `"TerminalInteractiveShell"`; probing code
    /// therefore reports "not a Jupyter kernel". The bare name also resolves,
    /// so `try: get_ipython / except NameError:` never takes the handler.
    GetIpython,
    /// `globals()`: a live view of the enclosing module's global namespace.
    ///
    /// A direct call always carries the owning module's dotted name as a
    /// hidden trailing constant argument (the frontend knows the unit). The
    /// runtime returns one cached `dict` per module, refreshed from the
    /// runtime global store, so reads and item writes observe the compiled
    /// module globals. The bare name resolves as a callable value too, but
    /// calling that value without the hidden module argument raises a tagged
    /// `RuntimeError` (a function value carries no module context).
    Globals,
    /// `locals()`: a `dict` snapshot of the current frame's locals.
    ///
    /// Codegen materialises the dict from the live local slots, using the
    /// names recorded in the function's local table (compiler-private `$…`
    /// slots are omitted). At module level this is the module body's own
    /// namespace, mirroring CPython's `locals() is globals()` there.
    Locals,
    /// `dir([object])`: the sorted attribute-name list of a module, class,
    /// instance or builtin value. With no argument, codegen lists the current
    /// frame's local names instead of calling the runtime.
    Dir,
    /// `vars([object])`: the object's `__dict__` (or, without an argument, the
    /// current frame's locals). Modules return their namespace mapping.
    Vars,
    /// `eval(source[, globals[, locals]])`: evaluates a Python expression
    /// string. Hydro compiles ahead of time, so this supports the forms a
    /// bootstrap path can evaluate without a parser: literals (`ast`
    /// literal grammar) and bare/dotted names looked up in the given
    /// namespaces. Every other expression raises a tagged
    /// `NotImplementedError` naming the source, never a placeholder value.
    Eval,
    /// `exec(source[, globals[, locals]])`: Hydro has no runtime statement
    /// compiler, so this always raises a tagged `NotImplementedError`.
    Exec,
    /// `compile(source, filename, mode, ...)`: Hydro has no bytecode compiler,
    /// so this always raises a tagged `NotImplementedError`.
    Compile,
    /// Internal: build a class from a runtime namespace `dict`, the shape used
    /// by class bodies that execute statements (`del`, `if`, `for`, `with`,
    /// `try`, …). Arguments are the class name, a tuple of bases, the
    /// metaclass value or `None`, and the namespace dict. Function values in
    /// the namespace become ordinary methods, `classmethod`/`staticmethod`/
    /// `property` descriptors keep their binding kind, and every other value
    /// becomes a frozen class attribute. Not reachable from source: the name
    /// starts with `$`, which cannot appear in a Python identifier.
    ClassFromNamespace,
    /// Internal: `globals()[name] = value` writes through the module's live
    /// namespace view into the runtime global store, so later static reads and
    /// `getattr(module, name)` observe the binding. The frontend lowers that
    /// exact store shape to this builtin; arguments are the module name, the
    /// key and the value. Not reachable from source (`$`-prefixed name).
    GlobalSetDynamic,
}

impl Builtin {
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Print => "print",
            Builtin::Len => "len",
            Builtin::Str => "str",
            Builtin::Repr => "repr",
            Builtin::Hash => "hash",
            Builtin::Int => "int",
            Builtin::Float => "float",
            Builtin::Abs => "abs",
            Builtin::Bool => "bool",
            Builtin::Type => "type",
            Builtin::Reversed => "reversed",
            Builtin::Import => "__import__",
            Builtin::Set => "set",
            Builtin::Range => "range",
            Builtin::List => "list",
            Builtin::Tuple => "tuple",
            Builtin::Dict => "dict",
            Builtin::Sum => "sum",
            Builtin::Enumerate => "enumerate",
            Builtin::Zip => "zip",
            Builtin::Map => "map",
            Builtin::Filter => "filter",
            Builtin::Bytes => "bytes",
            Builtin::ByteArray => "bytearray",
            Builtin::MemoryView => "memoryview",
            Builtin::Classmethod => "classmethod",
            Builtin::Staticmethod => "staticmethod",
            Builtin::Property => "property",
            Builtin::Iter => "iter",
            Builtin::Next => "next",
            Builtin::Frozenset => "frozenset",
            Builtin::Getattr => "getattr",
            Builtin::Hasattr => "hasattr",
            Builtin::Max => "max",
            Builtin::Min => "min",
            Builtin::Sorted => "sorted",
            Builtin::Round => "round",
            Builtin::Format => "format",
            Builtin::Isinstance => "isinstance",
            Builtin::Issubclass => "issubclass",
            Builtin::All => "all",
            Builtin::AnyValue => "any",
            Builtin::Ord => "ord",
            Builtin::Chr => "chr",
            Builtin::Divmod => "divmod",
            Builtin::Callable => "callable",
            Builtin::DictMerge => "dict merge",
            Builtin::Input => "input",
            Builtin::GetIpython => "get_ipython",
            Builtin::Globals => "globals",
            Builtin::Locals => "locals",
            Builtin::Dir => "dir",
            Builtin::Vars => "vars",
            Builtin::Eval => "eval",
            Builtin::Exec => "exec",
            Builtin::Compile => "compile",
            // Internal spellings start with `$`, which cannot occur in a
            // Python identifier, so source code can never name them.
            Builtin::ClassFromNamespace => "$class_from_namespace",
            Builtin::GlobalSetDynamic => "$global_set_dynamic",
        }
    }

    /// Resolves a builtin name visible to source code.
    pub fn from_name(name: &str) -> Option<Builtin> {
        Some(match name {
            "print" => Builtin::Print,
            "len" => Builtin::Len,
            "str" => Builtin::Str,
            "repr" => Builtin::Repr,
            "hash" => Builtin::Hash,
            "int" => Builtin::Int,
            "float" => Builtin::Float,
            "abs" => Builtin::Abs,
            "bool" => Builtin::Bool,
            "type" => Builtin::Type,
            "reversed" => Builtin::Reversed,
            "__import__" => Builtin::Import,
            "set" => Builtin::Set,
            "range" => Builtin::Range,
            "list" => Builtin::List,
            "tuple" => Builtin::Tuple,
            "dict" => Builtin::Dict,
            "sum" => Builtin::Sum,
            "enumerate" => Builtin::Enumerate,
            "zip" => Builtin::Zip,
            "map" => Builtin::Map,
            "filter" => Builtin::Filter,
            "bytes" => Builtin::Bytes,
            "bytearray" => Builtin::ByteArray,
            "memoryview" => Builtin::MemoryView,
            "classmethod" => Builtin::Classmethod,
            "staticmethod" => Builtin::Staticmethod,
            "property" => Builtin::Property,
            "iter" => Builtin::Iter,
            "next" => Builtin::Next,
            "frozenset" => Builtin::Frozenset,
            "getattr" => Builtin::Getattr,
            "hasattr" => Builtin::Hasattr,
            "max" => Builtin::Max,
            "min" => Builtin::Min,
            "sorted" => Builtin::Sorted,
            "round" => Builtin::Round,
            "format" => Builtin::Format,
            "isinstance" => Builtin::Isinstance,
            "issubclass" => Builtin::Issubclass,
            "all" => Builtin::All,
            "any" => Builtin::AnyValue,
            "ord" => Builtin::Ord,
            "chr" => Builtin::Chr,
            "divmod" => Builtin::Divmod,
            "callable" => Builtin::Callable,
            "input" => Builtin::Input,
            "get_ipython" => Builtin::GetIpython,
            "globals" => Builtin::Globals,
            "locals" => Builtin::Locals,
            "dir" => Builtin::Dir,
            "vars" => Builtin::Vars,
            "eval" => Builtin::Eval,
            "exec" => Builtin::Exec,
            "compile" => Builtin::Compile,
            // Internal names (never produced by the parser).
            "$class_from_namespace" => Builtin::ClassFromNamespace,
            "$global_set_dynamic" => Builtin::GlobalSetDynamic,
            _ => return None,
        })
    }
}

/// Exception kinds supported by the compiler. Tags are the stable runtime
/// encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExcKind {
    ValueError,
    TypeError,
    ZeroDivisionError,
    RuntimeError,
    OverflowError,
    KeyboardInterrupt,
    IndexError,
    KeyError,
    StopIteration,
    AttributeError,
    OSError,
    /// `FileNotFoundError`: a subclass of `OSError` (a handler for either
    /// kind matches both at runtime).
    FileNotFoundError,
    /// `AssertionError`: raised by `assert` statements and unittest
    /// assertions.
    AssertionError,
    /// `NameError`: an undefined name. Hydro type-checks name resolution, so
    /// this mostly exists as a catchable tag for library probes
    /// (`try: get_ipython / except NameError:`) and for `del` of an unbound
    /// module global.
    NameError,
    /// `SystemExit`: raised by `sys.exit()`. It is catchable like any other
    /// exception (`except SystemExit`, `except BaseException`), but not by
    /// `except Exception` (it derives directly from `BaseException`). When it
    /// stays uncaught the existing traceback path exits with the stored code.
    SystemExit,
    /// `GeneratorExit`: injected by `generator.close()`; it is also exposed
    /// as a first-class class value.
    GeneratorExit,
    /// Base class matching every tag whose parent chain reaches it (Python's
    /// `Exception`), so not the `BaseException`-only children (`SystemExit`,
    /// `KeyboardInterrupt`, `GeneratorExit`, `CancelledError` and
    /// `BaseExceptionGroup`).
    Exception,
    /// Root exception class matching every exception (Python's
    /// `BaseException`).
    BaseException,
    /// `StopAsyncIteration`: the normal end of an asynchronous iterator,
    /// caught by `async for` lowering.
    StopAsyncIteration,
    /// `SystemError`: an internal interpreter error.
    SystemError,
    /// `MemoryError`: out of memory.
    MemoryError,
    /// `RecursionError`: the recursion limit was exceeded (a `RuntimeError`
    /// subclass).
    RecursionError,
    /// `SyntaxError`: invalid syntax; also the base of
    /// [`ExcKind::IndentationError`].
    SyntaxError,
    /// `IndentationError`: bad indentation (a `SyntaxError` subclass).
    IndentationError,
    /// `TabError`: inconsistent tabs and spaces (an `IndentationError`).
    TabError,
    /// `ArithmeticError`: base of the arithmetic failures below.
    ArithmeticError,
    /// `LookupError`: base of [`ExcKind::IndexError`] and
    /// [`ExcKind::KeyError`].
    LookupError,
    /// `ImportError`: a failed import.
    ImportError,
    /// `ModuleNotFoundError`: no such module (an `ImportError` subclass).
    ModuleNotFoundError,
    /// `NotImplementedError`: abstract method (a `RuntimeError` subclass).
    NotImplementedError,
    /// `EOFError`: end of file where input was expected.
    EOFError,
    /// `UnicodeError`: base of the Unicode codec errors (a `ValueError`).
    UnicodeError,
    UnicodeDecodeError,
    UnicodeEncodeError,
    UnicodeTranslateError,
    /// `BufferError`: a buffer operation failed.
    BufferError,
    /// `ReferenceError`: a weak reference was used after its target died.
    ReferenceError,
    /// `ExceptionGroup`: a group of exceptions (also a `BaseExceptionGroup`).
    ExceptionGroup,
    /// `BaseExceptionGroup`: a group of exceptions, including the
    /// `BaseException`-only ones.
    BaseExceptionGroup,
    /// `Warning`: base of every warning category.
    Warning,
    UserWarning,
    DeprecationWarning,
    PendingDeprecationWarning,
    SyntaxWarning,
    RuntimeWarning,
    FutureWarning,
    ImportWarning,
    UnicodeWarning,
    BytesWarning,
    ResourceWarning,
    /// `FloatingPointError`: a floating-point operation failed.
    FloatingPointError,
    /// `BlockingIOError`: an `OSError` subclass.
    BlockingIOError,
    /// `ChildProcessError`: an `OSError` subclass.
    ChildProcessError,
    /// `ConnectionError`: an `OSError` subclass and the base of the four
    /// connection failures below.
    ConnectionError,
    BrokenPipeError,
    ConnectionAbortedError,
    ConnectionRefusedError,
    ConnectionResetError,
    /// `FileExistsError`: an `OSError` subclass.
    FileExistsError,
    /// `InterruptedError`: an `OSError` subclass.
    InterruptedError,
    /// `IsADirectoryError`: an `OSError` subclass.
    IsADirectoryError,
    /// `NotADirectoryError`: an `OSError` subclass.
    NotADirectoryError,
    /// `PermissionError`: an `OSError` subclass.
    PermissionError,
    /// `ProcessLookupError`: an `OSError` subclass.
    ProcessLookupError,
    /// `TimeoutError`: an `OSError` subclass.
    TimeoutError,
    /// Alias of [`ExcKind::OSError`]: `EnvironmentError is OSError` in
    /// CPython, so it shares the `OSError` tag and class name.
    EnvironmentError,
    /// Alias of [`ExcKind::OSError`]: `IOError is OSError` in CPython, so it
    /// shares the `OSError` tag and class name.
    IOError,
}

/// Every exception kind with its stable runtime tag and CPython class name.
///
/// This is the single source of truth for [`ExcKind::tag`],
/// [`ExcKind::name`], [`ExcKind::from_name`] and
/// [`ExcKind::from_class_name`]. Tags `0..=21` are fixed by the runtime ABI
/// (`OSError` subclasses and their siblings appended from `22`); the table is
/// ordered by tag. `EnvironmentError` and `IOError` are CPython aliases of
/// `OSError` and therefore share tag `10` and the name `"OSError"`; they are
/// listed last so a name lookup finds the canonical `OSError` entry first.
const EXCEPTION_ENTRIES: &[(ExcKind, u32, &str)] = &[
    (ExcKind::ValueError, 0, "ValueError"),
    (ExcKind::TypeError, 1, "TypeError"),
    (ExcKind::ZeroDivisionError, 2, "ZeroDivisionError"),
    (ExcKind::RuntimeError, 3, "RuntimeError"),
    (ExcKind::OverflowError, 4, "OverflowError"),
    (ExcKind::KeyboardInterrupt, 5, "KeyboardInterrupt"),
    (ExcKind::IndexError, 6, "IndexError"),
    (ExcKind::KeyError, 7, "KeyError"),
    (ExcKind::StopIteration, 8, "StopIteration"),
    (ExcKind::AttributeError, 9, "AttributeError"),
    (ExcKind::OSError, 10, "OSError"),
    (ExcKind::GeneratorExit, 11, "GeneratorExit"),
    (ExcKind::Exception, 12, "Exception"),
    (ExcKind::BaseException, 13, "BaseException"),
    (ExcKind::AssertionError, 14, "AssertionError"),
    (ExcKind::FileNotFoundError, 15, "FileNotFoundError"),
    (ExcKind::NameError, 16, "NameError"),
    (ExcKind::SystemExit, 17, "SystemExit"),
    (ExcKind::StopAsyncIteration, 18, "StopAsyncIteration"),
    (ExcKind::SystemError, 22, "SystemError"),
    (ExcKind::MemoryError, 23, "MemoryError"),
    (ExcKind::RecursionError, 24, "RecursionError"),
    (ExcKind::SyntaxError, 25, "SyntaxError"),
    (ExcKind::IndentationError, 26, "IndentationError"),
    (ExcKind::TabError, 27, "TabError"),
    (ExcKind::ArithmeticError, 28, "ArithmeticError"),
    (ExcKind::LookupError, 29, "LookupError"),
    (ExcKind::ImportError, 30, "ImportError"),
    (ExcKind::ModuleNotFoundError, 31, "ModuleNotFoundError"),
    (ExcKind::NotImplementedError, 32, "NotImplementedError"),
    (ExcKind::EOFError, 33, "EOFError"),
    (ExcKind::UnicodeError, 34, "UnicodeError"),
    (ExcKind::UnicodeDecodeError, 35, "UnicodeDecodeError"),
    (ExcKind::UnicodeEncodeError, 36, "UnicodeEncodeError"),
    (ExcKind::UnicodeTranslateError, 37, "UnicodeTranslateError"),
    (ExcKind::BufferError, 38, "BufferError"),
    (ExcKind::ReferenceError, 39, "ReferenceError"),
    (ExcKind::ExceptionGroup, 40, "ExceptionGroup"),
    (ExcKind::BaseExceptionGroup, 41, "BaseExceptionGroup"),
    (ExcKind::Warning, 42, "Warning"),
    (ExcKind::UserWarning, 43, "UserWarning"),
    (ExcKind::DeprecationWarning, 44, "DeprecationWarning"),
    (
        ExcKind::PendingDeprecationWarning,
        45,
        "PendingDeprecationWarning",
    ),
    (ExcKind::SyntaxWarning, 46, "SyntaxWarning"),
    (ExcKind::RuntimeWarning, 47, "RuntimeWarning"),
    (ExcKind::FutureWarning, 48, "FutureWarning"),
    (ExcKind::ImportWarning, 49, "ImportWarning"),
    (ExcKind::UnicodeWarning, 50, "UnicodeWarning"),
    (ExcKind::BytesWarning, 51, "BytesWarning"),
    (ExcKind::ResourceWarning, 52, "ResourceWarning"),
    (ExcKind::FloatingPointError, 53, "FloatingPointError"),
    (ExcKind::BlockingIOError, 54, "BlockingIOError"),
    (ExcKind::ChildProcessError, 55, "ChildProcessError"),
    (ExcKind::ConnectionError, 56, "ConnectionError"),
    (ExcKind::BrokenPipeError, 57, "BrokenPipeError"),
    (
        ExcKind::ConnectionAbortedError,
        58,
        "ConnectionAbortedError",
    ),
    (
        ExcKind::ConnectionRefusedError,
        59,
        "ConnectionRefusedError",
    ),
    (ExcKind::ConnectionResetError, 60, "ConnectionResetError"),
    (ExcKind::FileExistsError, 61, "FileExistsError"),
    (ExcKind::InterruptedError, 62, "InterruptedError"),
    (ExcKind::IsADirectoryError, 63, "IsADirectoryError"),
    (ExcKind::NotADirectoryError, 64, "NotADirectoryError"),
    (ExcKind::PermissionError, 65, "PermissionError"),
    (ExcKind::ProcessLookupError, 66, "ProcessLookupError"),
    (ExcKind::TimeoutError, 67, "TimeoutError"),
    // CPython aliases: `EnvironmentError is OSError` and `IOError is OSError`.
    (ExcKind::EnvironmentError, 10, "OSError"),
    (ExcKind::IOError, 10, "OSError"),
];

impl ExcKind {
    /// Stable runtime tag. Legacy tags `0..=18` are never renumbered; new
    /// kinds are appended from `22` (the runtime reserves `19..=21` for its
    /// internal `CancelledError`, `Skipped` and `XFailed` tags).
    pub fn tag(self) -> u32 {
        self.entry().0
    }

    /// CPython-visible class name (`type(exc).__name__`).
    pub fn name(self) -> &'static str {
        self.entry().1
    }

    fn entry(self) -> (u32, &'static str) {
        for &(kind, tag, name) in EXCEPTION_ENTRIES {
            if kind == self {
                return (tag, name);
            }
        }
        // Unreachable: every variant is listed in `EXCEPTION_ENTRIES`.
        (12, "Exception")
    }

    /// Resolves a Python exception name used in `raise`/`except`.
    ///
    /// `IOError` and `EnvironmentError` resolve to [`ExcKind::OSError`]
    /// because CPython defines them as aliases of `OSError`. `GeneratorExit`
    /// is injected by the runtime (`generator.close()`), so it is not
    /// resolvable from source; [`ExcKind::from_class_name`] exposes it as a
    /// value.
    pub fn from_name(name: &str) -> Option<ExcKind> {
        match name {
            "IOError" | "EnvironmentError" => return Some(ExcKind::OSError),
            // Runtime-injected like the internal `CancelledError` tag; a
            // source-level `except GeneratorExit` stays unsupported.
            "GeneratorExit" => return None,
            _ => {}
        }
        EXCEPTION_ENTRIES
            .iter()
            .find(|(_, _, entry_name)| *entry_name == name)
            .map(|(kind, _, _)| *kind)
    }

    /// Resolves an unqualified built-in exception name used as a *value* in an
    /// expression (`x = ValueError`, `class E(Exception):`).
    ///
    /// Unlike [`ExcKind::from_name`], which drives `raise`/`except`, this also
    /// exposes `GeneratorExit`, which the runtime injects.
    pub fn from_class_name(name: &str) -> Option<ExcKind> {
        Some(match name {
            "GeneratorExit" => ExcKind::GeneratorExit,
            other => return ExcKind::from_name(other),
        })
    }
}

impl fmt::Display for ExcKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Call target: a program function, a builtin, or a member of a compiled
/// CPython extension module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Callee {
    Function(FunctionId),
    Builtin(Builtin),
    /// `ext.member(args...)` where `ext` is a compiled extension module. The
    /// frontend resolves `path` to the shared object at compile time; the
    /// runtime loads it lazily and caches it. `member` is the Python-visible
    /// attribute name.
    Extension {
        path: String,
        member: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_alias(kind: ExcKind) -> bool {
        matches!(kind, ExcKind::EnvironmentError | ExcKind::IOError)
    }

    #[test]
    fn legacy_tags_are_not_renumbered() {
        // The runtime ABI fixes these; appending is allowed, renumbering is
        // not.
        assert_eq!(ExcKind::ValueError.tag(), 0);
        assert_eq!(ExcKind::TypeError.tag(), 1);
        assert_eq!(ExcKind::ZeroDivisionError.tag(), 2);
        assert_eq!(ExcKind::RuntimeError.tag(), 3);
        assert_eq!(ExcKind::OverflowError.tag(), 4);
        assert_eq!(ExcKind::KeyboardInterrupt.tag(), 5);
        assert_eq!(ExcKind::IndexError.tag(), 6);
        assert_eq!(ExcKind::KeyError.tag(), 7);
        assert_eq!(ExcKind::StopIteration.tag(), 8);
        assert_eq!(ExcKind::AttributeError.tag(), 9);
        assert_eq!(ExcKind::OSError.tag(), 10);
        assert_eq!(ExcKind::GeneratorExit.tag(), 11);
        assert_eq!(ExcKind::Exception.tag(), 12);
        assert_eq!(ExcKind::BaseException.tag(), 13);
        assert_eq!(ExcKind::AssertionError.tag(), 14);
        assert_eq!(ExcKind::FileNotFoundError.tag(), 15);
        assert_eq!(ExcKind::NameError.tag(), 16);
        assert_eq!(ExcKind::SystemExit.tag(), 17);
        assert_eq!(ExcKind::StopAsyncIteration.tag(), 18);
    }

    #[test]
    fn taxonomy_covers_the_cpython_classes() {
        let expected = [
            (ExcKind::SystemError, 22u32, "SystemError"),
            (ExcKind::MemoryError, 23, "MemoryError"),
            (ExcKind::RecursionError, 24, "RecursionError"),
            (ExcKind::SyntaxError, 25, "SyntaxError"),
            (ExcKind::IndentationError, 26, "IndentationError"),
            (ExcKind::TabError, 27, "TabError"),
            (ExcKind::ArithmeticError, 28, "ArithmeticError"),
            (ExcKind::LookupError, 29, "LookupError"),
            (ExcKind::ImportError, 30, "ImportError"),
            (ExcKind::ModuleNotFoundError, 31, "ModuleNotFoundError"),
            (ExcKind::NotImplementedError, 32, "NotImplementedError"),
            (ExcKind::EOFError, 33, "EOFError"),
            (ExcKind::UnicodeError, 34, "UnicodeError"),
            (ExcKind::UnicodeDecodeError, 35, "UnicodeDecodeError"),
            (ExcKind::UnicodeEncodeError, 36, "UnicodeEncodeError"),
            (ExcKind::UnicodeTranslateError, 37, "UnicodeTranslateError"),
            (ExcKind::BufferError, 38, "BufferError"),
            (ExcKind::ReferenceError, 39, "ReferenceError"),
            (ExcKind::ExceptionGroup, 40, "ExceptionGroup"),
            (ExcKind::BaseExceptionGroup, 41, "BaseExceptionGroup"),
            (ExcKind::Warning, 42, "Warning"),
            (ExcKind::UserWarning, 43, "UserWarning"),
            (ExcKind::DeprecationWarning, 44, "DeprecationWarning"),
            (
                ExcKind::PendingDeprecationWarning,
                45,
                "PendingDeprecationWarning",
            ),
            (ExcKind::SyntaxWarning, 46, "SyntaxWarning"),
            (ExcKind::RuntimeWarning, 47, "RuntimeWarning"),
            (ExcKind::FutureWarning, 48, "FutureWarning"),
            (ExcKind::ImportWarning, 49, "ImportWarning"),
            (ExcKind::UnicodeWarning, 50, "UnicodeWarning"),
            (ExcKind::BytesWarning, 51, "BytesWarning"),
            (ExcKind::ResourceWarning, 52, "ResourceWarning"),
            (ExcKind::FloatingPointError, 53, "FloatingPointError"),
            (ExcKind::BlockingIOError, 54, "BlockingIOError"),
            (ExcKind::ChildProcessError, 55, "ChildProcessError"),
            (ExcKind::ConnectionError, 56, "ConnectionError"),
            (ExcKind::BrokenPipeError, 57, "BrokenPipeError"),
            (
                ExcKind::ConnectionAbortedError,
                58,
                "ConnectionAbortedError",
            ),
            (
                ExcKind::ConnectionRefusedError,
                59,
                "ConnectionRefusedError",
            ),
            (ExcKind::ConnectionResetError, 60, "ConnectionResetError"),
            (ExcKind::FileExistsError, 61, "FileExistsError"),
            (ExcKind::InterruptedError, 62, "InterruptedError"),
            (ExcKind::IsADirectoryError, 63, "IsADirectoryError"),
            (ExcKind::NotADirectoryError, 64, "NotADirectoryError"),
            (ExcKind::PermissionError, 65, "PermissionError"),
            (ExcKind::ProcessLookupError, 66, "ProcessLookupError"),
            (ExcKind::TimeoutError, 67, "TimeoutError"),
        ];
        for (kind, tag, name) in expected {
            assert_eq!(kind.tag(), tag, "{name} tag");
            assert_eq!(kind.name(), name);
            assert_eq!(ExcKind::from_name(name), Some(kind), "{name} from_name");
            assert_eq!(
                ExcKind::from_class_name(name),
                Some(kind),
                "{name} from_class_name"
            );
        }
    }

    #[test]
    fn table_tags_are_unique_and_preserve_tag_order() {
        let mut tags = Vec::new();
        let mut names = Vec::new();
        let mut last: i64 = -1;
        for &(kind, tag, name) in EXCEPTION_ENTRIES {
            assert_eq!(kind.tag(), tag);
            assert_eq!(kind.name(), name);
            assert!(
                tag <= 18 || tag >= 22,
                "tag {tag} collides with the runtime-internal 19..=21 range"
            );
            if is_alias(kind) {
                assert_eq!(tag, ExcKind::OSError.tag());
                assert_eq!(name, ExcKind::OSError.name());
                continue;
            }
            assert!(!tags.contains(&tag), "duplicate tag {tag}");
            assert!(!names.contains(&name), "duplicate name {name}");
            assert!(
                i64::from(tag) > last,
                "entries must stay in ascending tag order (tag {tag})"
            );
            last = i64::from(tag);
            tags.push(tag);
            names.push(name);
        }
    }

    #[test]
    fn names_roundtrip_except_runtime_injected_generator_exit() {
        for &(kind, _, name) in EXCEPTION_ENTRIES {
            if is_alias(kind) {
                continue;
            }
            if kind == ExcKind::GeneratorExit {
                assert_eq!(ExcKind::from_name(name), None);
                assert_eq!(ExcKind::from_class_name(name), Some(ExcKind::GeneratorExit));
                continue;
            }
            assert_eq!(ExcKind::from_name(name), Some(kind), "{name}");
            assert_eq!(ExcKind::from_class_name(name), Some(kind), "{name}");
        }
        assert_eq!(ExcKind::from_name("NoSuchError"), None);
        assert_eq!(ExcKind::from_class_name("NoSuchError"), None);
    }

    #[test]
    fn ioerror_and_environmenterror_are_oserror_aliases() {
        assert_eq!(ExcKind::IOError.tag(), ExcKind::OSError.tag());
        assert_eq!(ExcKind::EnvironmentError.tag(), ExcKind::OSError.tag());
        assert_eq!(ExcKind::IOError.name(), "OSError");
        assert_eq!(ExcKind::EnvironmentError.name(), "OSError");
        assert_eq!(ExcKind::from_name("IOError"), Some(ExcKind::OSError));
        assert_eq!(
            ExcKind::from_name("EnvironmentError"),
            Some(ExcKind::OSError)
        );
        assert_eq!(ExcKind::from_class_name("IOError"), Some(ExcKind::OSError));
        assert_eq!(format!("{}", ExcKind::IOError), "OSError");
    }

    #[test]
    fn display_and_debug_are_stable() {
        assert_eq!(format!("{}", ExcKind::SyntaxError), "SyntaxError");
        assert_eq!(
            format!("{}", ExcKind::ModuleNotFoundError),
            "ModuleNotFoundError"
        );
        assert_eq!(format!("{:?}", ExcKind::ValueError), "ValueError");
    }

    #[test]
    fn issubclass_is_a_source_builtin() {
        assert_eq!(Builtin::from_name("issubclass"), Some(Builtin::Issubclass));
        assert_eq!(Builtin::Issubclass.name(), "issubclass");
        // The name is distinct from `isinstance`: first-class values must
        // dispatch to the right runtime entry point.
        assert_ne!(Builtin::Issubclass.name(), Builtin::Isinstance.name());
    }
}
