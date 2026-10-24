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
}

impl UnOp {
    pub fn code(self) -> u32 {
        match self {
            UnOp::Neg => 0,
            UnOp::Pos => 1,
            UnOp::Not => 2,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Pos => "+",
            UnOp::Not => "not",
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
    Int,
    Float,
    Abs,
    /// `set()` / `set(iterable)`.
    Set,
    /// `range(...)` as a first-class value.
    Range,
    /// `list()` / `list(iterable)`.
    List,
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
    /// Internal: one f-string interpolation field lowered to
    /// `pyx_format_value`. Arguments are the value, a constant format spec
    /// string and a constant conversion flag (`0` none, `1` `!s`, `2` `!r`,
    /// `3` `!a`). Not reachable from source code.
    Format,
}

impl Builtin {
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Print => "print",
            Builtin::Len => "len",
            Builtin::Str => "str",
            Builtin::Int => "int",
            Builtin::Float => "float",
            Builtin::Abs => "abs",
            Builtin::Set => "set",
            Builtin::Range => "range",
            Builtin::List => "list",
            Builtin::Dict => "dict",
            Builtin::Sum => "sum",
            Builtin::Enumerate => "enumerate",
            Builtin::Zip => "zip",
            Builtin::Map => "map",
            Builtin::Filter => "filter",
            Builtin::Bytes => "bytes",
            Builtin::Iter => "iter",
            Builtin::Next => "next",
            Builtin::Format => "format",
        }
    }

    /// Resolves a builtin name visible to source code. `format` is an
    /// internal f-string helper and is deliberately not resolvable.
    pub fn from_name(name: &str) -> Option<Builtin> {
        Some(match name {
            "print" => Builtin::Print,
            "len" => Builtin::Len,
            "str" => Builtin::Str,
            "int" => Builtin::Int,
            "float" => Builtin::Float,
            "abs" => Builtin::Abs,
            "set" => Builtin::Set,
            "range" => Builtin::Range,
            "list" => Builtin::List,
            "dict" => Builtin::Dict,
            "sum" => Builtin::Sum,
            "enumerate" => Builtin::Enumerate,
            "zip" => Builtin::Zip,
            "map" => Builtin::Map,
            "filter" => Builtin::Filter,
            "bytes" => Builtin::Bytes,
            "iter" => Builtin::Iter,
            "next" => Builtin::Next,
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
            "IOError" => ExcKind::OSError,
            _ => return None,
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
