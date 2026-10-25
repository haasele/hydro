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
    Float,
    Bool,
    Str,
    /// Immutable byte string.
    Bytes,
    None,
    Any,
    Never,
    /// Untyped list (elements are `Any` in Phase 2.0).
    List,
    /// Untyped dict (keys must be hashable at runtime).
    Dict,
    /// Untyped tuple (elements are `Any` in Phase 2.5).
    Tuple,
    /// Insertion-ordered set (elements must be hashable).
    Set,
    /// Immutable arithmetic progression (`range(...)`).
    Range,
    /// Callable function value (named function or lambda).
    Function,
}

impl Type {
    /// True for `Int` and `Float` (and `Bool`, which is numerically compatible).
    pub fn is_numeric(self) -> bool {
        matches!(self, Type::Int | Type::Float | Type::Bool)
    }

    pub fn name(self) -> &'static str {
        match self {
            Type::Int => "int",
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
    Float(f64),
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
    None,
    /// Python's `NotImplemented` singleton. Codegen materialises the runtime's
    /// reserved `NotImplemented` immediate; PyX has no `NotImplementedType`,
    /// so its static type is [`Type::Any`].
    NotImplemented,
    /// Python's `...` literal. Documented limitation: PyX has no distinct
    /// `EllipsisType` sentinel yet, so codegen materialises the runtime `None`
    /// value and the static type is [`Type::None`]. Expression statements such
    /// as `def f(): ...` therefore behave exactly like `None`, but `... is
    /// None` is `True` instead of `False`.
    Ellipsis,
}

impl Const {
    pub fn ty(&self) -> Type {
        match self {
            Const::Int(_) => Type::Int,
            Const::Float(_) => Type::Float,
            Const::Bool(_) => Type::Bool,
            Const::Str(_) => Type::Str,
            Const::Bytes(_) => Type::Bytes,
            Const::None => Type::None,
            Const::NotImplemented => Type::Any,
            Const::Ellipsis => Type::None,
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
    /// Stable encoding shared with `pyx_runtime` and `docs/ARCHITECTURE.md`.
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

/// Builtins known to Phase 1. `range` is special-cased by `for` lowering and
/// therefore not listed here.
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
    /// `iter(obj)`: the object's iterator (an iterator/generator is returned
    /// unchanged).
    Iter,
    /// `next(iterator[, default])`: the next element, `StopIteration` at the
    /// end unless a default is given.
    Next,
    /// `frozenset()` / `frozenset(iterable)`. PyX values are immutable sets:
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
    /// `all(iterable)`: `True` when every element is truthy.
    All,
    AnyValue,
    /// `ord(character)`: the Unicode scalar value of a one-character `str`.
    /// Any other string length or a non-`str` argument raises `TypeError`.
    Ord,
    /// `chr(codepoint)`: the one-character `str` for an integer in
    /// `0..=0x10FFFF`. Surrogate code points are rejected because PyX strings
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
    /// rich. PyX has no IPython, so the call returns an object whose
    /// `__class__.__name__` is `"TerminalInteractiveShell"`; probing code
    /// therefore reports "not a Jupyter kernel". The bare name also resolves,
    /// so `try: get_ipython / except NameError:` never takes the handler.
    GetIpython,
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
            Builtin::All => "all",
            Builtin::AnyValue => "any",
            Builtin::Ord => "ord",
            Builtin::Chr => "chr",
            Builtin::Divmod => "divmod",
            Builtin::Callable => "callable",
            Builtin::DictMerge => "dict merge",
            Builtin::Input => "input",
            Builtin::GetIpython => "get_ipython",
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
            "all" => Builtin::All,
            "any" => Builtin::AnyValue,
            "ord" => Builtin::Ord,
            "chr" => Builtin::Chr,
            "divmod" => Builtin::Divmod,
            "callable" => Builtin::Callable,
            "input" => Builtin::Input,
            "get_ipython" => Builtin::GetIpython,
            _ => return None,
        })
    }
}

/// Exception kinds supported in Phase 1. Tags are the stable runtime encoding.
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
    /// `NameError`: an undefined name. PyX type-checks name resolution, so
    /// this mostly exists as a catchable tag for library probes
    /// (`try: get_ipython / except NameError:`) and for `del` of an unbound
    /// module global.
    NameError,
    /// `SystemExit`: raised by `sys.exit()`. It is catchable like any other
    /// exception (`except SystemExit`, `except Exception`); when it stays
    /// uncaught the existing traceback path exits with status 1.
    SystemExit,
    /// `GeneratorExit`: injected by `generator.close()`; it is also exposed
    /// as a first-class class value.
    GeneratorExit,
    /// Base class matching every exception except `KeyboardInterrupt` and
    /// `GeneratorExit` (Python's `Exception`).
    Exception,
    /// Root exception class matching every exception (Python's
    /// `BaseException`).
    BaseException,
}

impl ExcKind {
    pub fn tag(self) -> u32 {
        match self {
            ExcKind::ValueError => 0,
            ExcKind::TypeError => 1,
            ExcKind::ZeroDivisionError => 2,
            ExcKind::RuntimeError => 3,
            ExcKind::OverflowError => 4,
            ExcKind::KeyboardInterrupt => 5,
            ExcKind::IndexError => 6,
            ExcKind::KeyError => 7,
            ExcKind::StopIteration => 8,
            ExcKind::AttributeError => 9,
            ExcKind::OSError => 10,
            // `GeneratorExit`, injected by `generator.close()`.
            ExcKind::GeneratorExit => 11,
            ExcKind::Exception => 12,
            ExcKind::BaseException => 13,
            // `AssertionError`, raised by `assert` statements.
            ExcKind::AssertionError => 14,
            ExcKind::FileNotFoundError => 15,
            ExcKind::NameError => 16,
            ExcKind::SystemExit => 17,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ExcKind::ValueError => "ValueError",
            ExcKind::TypeError => "TypeError",
            ExcKind::ZeroDivisionError => "ZeroDivisionError",
            ExcKind::RuntimeError => "RuntimeError",
            ExcKind::OverflowError => "OverflowError",
            ExcKind::KeyboardInterrupt => "KeyboardInterrupt",
            ExcKind::IndexError => "IndexError",
            ExcKind::KeyError => "KeyError",
            ExcKind::StopIteration => "StopIteration",
            ExcKind::AttributeError => "AttributeError",
            ExcKind::OSError => "OSError",
            ExcKind::FileNotFoundError => "FileNotFoundError",
            ExcKind::AssertionError => "AssertionError",
            ExcKind::NameError => "NameError",
            ExcKind::SystemExit => "SystemExit",
            ExcKind::GeneratorExit => "GeneratorExit",
            ExcKind::Exception => "Exception",
            ExcKind::BaseException => "BaseException",
        }
    }

    /// Resolve a Python exception name used in `raise`/`except`.
    pub fn from_name(name: &str) -> Option<ExcKind> {
        Some(match name {
            "ValueError" => ExcKind::ValueError,
            "TypeError" => ExcKind::TypeError,
            "ZeroDivisionError" => ExcKind::ZeroDivisionError,
            "RuntimeError" => ExcKind::RuntimeError,
            "OverflowError" => ExcKind::OverflowError,
            "KeyboardInterrupt" => ExcKind::KeyboardInterrupt,
            "IndexError" => ExcKind::IndexError,
            "KeyError" => ExcKind::KeyError,
            "StopIteration" => ExcKind::StopIteration,
            "AttributeError" => ExcKind::AttributeError,
            "OSError" => ExcKind::OSError,
            "FileNotFoundError" => ExcKind::FileNotFoundError,
            "AssertionError" => ExcKind::AssertionError,
            "NameError" => ExcKind::NameError,
            "SystemExit" => ExcKind::SystemExit,
            "Exception" => ExcKind::Exception,
            "BaseException" => ExcKind::BaseException,
            "IOError" => ExcKind::OSError,
            _ => return None,
        })
    }

    /// Resolves an unqualified built-in exception name used as a *value* in an
    /// expression (`x = ValueError`, `class E(Exception):`).
    ///
    /// Unlike [`ExcKind::from_name`], which drives `raise`/`except` and stays
    /// unchanged, this also exposes the hierarchy tags reachable only through
    /// the runtime (`AssertionError` and `GeneratorExit`).
    pub fn from_class_name(name: &str) -> Option<ExcKind> {
        Some(match name {
            "AssertionError" => ExcKind::AssertionError,
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
