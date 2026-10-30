//! Type inference (unification over type variables) and typed IR emission.
//!
//! Inference is deliberately simple: every expression owns a fresh type
//! variable, assignments/reads/calls/returns add equality constraints, and
//! operators are resolved against the numeric lattice (`bool` < `int` <
//! `float`). `str` supports `+` and repetition with an `int` count on either
//! side of `*`. Variables that never resolve become [`Type::Any`], which makes
//! the codegen use the dynamic runtime operators.

use std::collections::{HashMap, HashSet};

use hydro_ir::stdlib;
use hydro_ir::{
    BinOp, Builtin, Callee, ClassAttr, ClassMethod, CompClause, CompTarget, Const, DeleteTarget,
    Expr as IrExpr, ExprKind, Function as IrFunction, Handler as IrHandler, LocalInfo, Param,
    ParamKind, Program, Stmt as IrStmt, StmtKind, Type, UnOp, UnpackTarget,
};

use crate::diag::{self, codes, Diagnostic};
use crate::lower::{
    HCompClause, HCompTarget, HDeleteTarget, HExpr, HExprKind, HFunction, HParam, HStmt, HStmtKind,
    HUnpackTarget, LHir,
};

mod str_acc;

/// A type slot: either a concrete type or an inference variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Concrete(Type),
    Var(u32),
}

/// Union-find over inference variables plus numeric widening.
pub struct Infer {
    parent: Vec<u32>,
    binding: Vec<Option<Type>>,
    /// Roots of inference variables that belong to unannotated parameters.
    ///
    /// Such parameters are gradual: a call argument may narrow them, a
    /// conflicting call site widens the slot to `Any`, and a body use that
    /// the narrowed type cannot serve keeps its operation dynamic instead of
    /// reporting an error. Union-find merges keep the mark on the surviving
    /// root, and [`Infer::inherit_gradual`] carries it onto the open
    /// variables a body use reads the slot through, so locals that receive a
    /// parameter value inherit it as well.
    gradual: HashSet<u32>,
}

impl Infer {
    pub fn new() -> Infer {
        Infer {
            parent: Vec::new(),
            binding: Vec::new(),
            gradual: HashSet::new(),
        }
    }

    /// Mark a type slot as an unannotated parameter. Only inference variables
    /// can carry provenance; an explicitly annotated parameter is concrete
    /// and keeps its static checks.
    pub fn mark_gradual(&mut self, t: Ty) {
        if let Ty::Var(v) = t {
            let root = self.root(v);
            self.gradual.insert(root);
        }
    }

    /// Whether the slot is an unannotated parameter (or was unified with
    /// one), whether or not a call site already narrowed it to a concrete
    /// type.
    pub fn is_gradual(&mut self, t: Ty) -> bool {
        match t {
            Ty::Concrete(_) => false,
            Ty::Var(v) => {
                let root = self.root(v);
                self.gradual.contains(&root)
            }
        }
    }

    /// Carry unannotated-parameter provenance onto a still-open inference
    /// variable that is unified with a gradual slot.
    ///
    /// A call site narrows an unannotated parameter by binding its slot, and
    /// every body use reads the slot through its own variable. Without this
    /// transfer the read resolves to the concrete argument type and loses the
    /// gradual contract, so `len(parameters)` and `parameters < 'x'` would be
    /// reported against the caller's `bool` instead of staying dynamic.
    fn inherit_gradual(&mut self, from: Ty, to: Ty) {
        let Ty::Var(v) = to else {
            return;
        };
        let root = self.root(v);
        // A slot that is already pinned to a concrete type was typed by
        // something other than the gradual value; leave it concrete.
        if self.binding[root as usize].is_some() {
            return;
        }
        if !self.is_gradual(from) {
            return;
        }
        self.gradual.insert(root);
    }

    pub fn fresh(&mut self) -> Ty {
        let id = self.parent.len() as u32;
        self.parent.push(id);
        self.binding.push(None);
        Ty::Var(id)
    }

    fn root(&mut self, mut v: u32) -> u32 {
        let mut root = v;
        while self.parent[root as usize] != root {
            root = self.parent[root as usize];
        }
        while self.parent[v as usize] != root {
            let next = self.parent[v as usize];
            self.parent[v as usize] = root;
            v = next;
        }
        root
    }

    pub fn resolve(&mut self, t: Ty) -> Ty {
        match t {
            Ty::Concrete(_) => t,
            Ty::Var(v) => {
                let root = self.root(v);
                match self.binding[root as usize] {
                    Some(ty) => Ty::Concrete(ty),
                    None => Ty::Var(root),
                }
            }
        }
    }

    /// Unify two type slots, widening numeric types. On failure the two
    /// resolved concrete types are returned without mutating the store.
    pub fn unify(&mut self, a: Ty, b: Ty) -> Result<Ty, (Type, Type)> {
        // Provenance must survive the narrowing that `resolve` collapses: a
        // parameter slot can already be bound to the type one call site
        // supplied while the other side is a fresh use of that slot.
        self.inherit_gradual(a, b);
        self.inherit_gradual(b, a);
        let ra = self.resolve(a);
        let rb = self.resolve(b);
        match (ra, rb) {
            (Ty::Concrete(x), Ty::Concrete(y)) => match join(x, y) {
                Some(ty) => {
                    // Keep bound variables in sync with the widened type.
                    self.widen_binding(a, ty);
                    self.widen_binding(b, ty);
                    Ok(Ty::Concrete(ty))
                }
                None => Err((x, y)),
            },
            (Ty::Var(x), Ty::Var(y)) => {
                if x == y {
                    return Ok(Ty::Var(x));
                }
                self.parent[y as usize] = x;
                // Gradual provenance follows the surviving root.
                if self.gradual.remove(&y) {
                    self.gradual.insert(x);
                }
                Ok(Ty::Var(x))
            }
            (Ty::Var(x), Ty::Concrete(y)) => {
                self.binding[x as usize] = Some(y);
                Ok(Ty::Concrete(y))
            }
            (Ty::Concrete(x), Ty::Var(y)) => {
                self.binding[y as usize] = Some(x);
                Ok(Ty::Concrete(x))
            }
        }
    }

    fn widen_binding(&mut self, slot: Ty, ty: Type) {
        if let Ty::Var(v) = slot {
            let root = self.root(v);
            if let Some(bound) = self.binding[root as usize] {
                if let Some(joined) = join(bound, ty) {
                    self.binding[root as usize] = Some(joined);
                }
            }
        }
    }

    pub fn name_of(&mut self, t: Ty) -> String {
        match self.resolve(t) {
            Ty::Concrete(ty) => ty.name().to_string(),
            Ty::Var(_) => Type::Any.name().to_string(),
        }
    }

    pub fn type_of(&mut self, t: Ty) -> Type {
        match self.resolve(t) {
            Ty::Concrete(ty) => ty,
            Ty::Var(_) => Type::Any,
        }
    }
}

fn join(a: Type, b: Type) -> Option<Type> {
    if a == b {
        return Some(a);
    }
    if a == Type::Any || b == Type::Any {
        return Some(Type::Any);
    }
    if a == Type::Never {
        return Some(b);
    }
    if b == Type::Never {
        return Some(a);
    }
    if a.is_numeric() && b.is_numeric() {
        return Some(numeric_result(a, b));
    }
    if is_complex_mix(a, b) {
        return Some(Type::Complex);
    }
    None
}

/// Result type of mixing two numeric types: `complex` dominates, then
/// `float`, then the arbitrary-precision integer, then the tagged integer
/// (`bool` behaves as an `int`).
fn numeric_result(a: Type, b: Type) -> Type {
    if a == Type::Complex || b == Type::Complex {
        Type::Complex
    } else if a == Type::Float || b == Type::Float {
        Type::Float
    } else if a == Type::BigInt || b == Type::BigInt {
        Type::BigInt
    } else {
        Type::Int
    }
}

/// Whether `a`/`b` mix to a complex result: at least one operand is
/// `complex`, and the other is real-numeric or complex as well.
fn is_complex_mix(a: Type, b: Type) -> bool {
    let real = |t: Type| t == Type::Complex || t.is_numeric();
    (a == Type::Complex || b == Type::Complex) && real(a) && real(b)
}

/// Whether a value of type `got` satisfies an explicit annotation `expected`.
/// Annotations are checked strictly, except for the documented numeric
/// subtyping (`bool` -> `int`, and every real number -> `complex`).
fn annotation_accepts(expected: Type, got: Type) -> bool {
    if expected == got || expected == Type::Any || got == Type::Any || got == Type::Never {
        return true;
    }
    matches!(
        (expected, got),
        (Type::Int, Type::Bool)
            | (Type::Int, Type::BigInt)
            | (Type::BigInt, Type::Int)
            | (Type::BigInt, Type::Bool)
            | (Type::Float, Type::Int)
            | (Type::Float, Type::BigInt)
            | (Type::Float, Type::Bool)
            | (Type::Complex, Type::Int)
            | (Type::Complex, Type::BigInt)
            | (Type::Complex, Type::Bool)
            | (Type::Complex, Type::Float)
    )
}

/// Expected static kind of an argument passed to a built-in method.
#[derive(Clone, Copy)]
enum ArgKind {
    Any,
    Str,
    /// `str.strip`/`lstrip`/`rstrip` take an optional character set: either a
    /// `str` or `None` (the latter meaning Unicode whitespace).
    StrOrNone,
    Bytes,
    Int,
    /// `dict.update` operand: a mapping or any iterable of key/value pairs.
    /// The static checks cannot tell a dict from a list of pairs, so every
    /// iterable is accepted and the runtime validates the element shapes.
    MappingOrPairs,
    /// Any value Python can hash: containers are rejected recursively.
    Hashable,
    /// Any type whose values can be iterated: `str.join`/`bytes.join`,
    /// `list.extend` and the set methods (`union`, `update`, ...) accept every
    /// iterable, not just lists (CPython iterates the argument).
    Iterable,
}

impl ArgKind {
    /// Whether a value of type `got` may be passed for this parameter.
    fn accepts(self, got: Type) -> bool {
        if got == Type::Any || got == Type::Never {
            return true;
        }
        match self {
            ArgKind::Any => true,
            ArgKind::Str => got == Type::Str,
            ArgKind::StrOrNone => matches!(got, Type::Str | Type::None),
            ArgKind::Bytes => got == Type::Bytes,
            ArgKind::Int => got == Type::Int || got == Type::BigInt || got == Type::Bool,
            // Detailed hashability is checked on the expression itself so
            // tuples with unhashable elements are rejected recursively.
            ArgKind::Hashable => true,
            ArgKind::MappingOrPairs | ArgKind::Iterable => matches!(
                got,
                Type::List
                    | Type::Tuple
                    | Type::Set
                    | Type::Dict
                    | Type::Range
                    | Type::Str
                    | Type::Bytes
            ),
        }
    }

    fn expected_name(self) -> &'static str {
        match self {
            ArgKind::Any => "Any",
            ArgKind::Str => "str",
            ArgKind::StrOrNone => "str or None",
            ArgKind::Bytes => "bytes",
            ArgKind::Int => "int",
            ArgKind::MappingOrPairs => "mapping or iterable of pairs",
            ArgKind::Hashable => "hashable",
            ArgKind::Iterable => "iterable",
        }
    }

    /// Type an unresolved argument is bound to, when narrowing is safe.
    fn bound_type(self) -> Option<Type> {
        match self {
            ArgKind::Any | ArgKind::Hashable => None,
            ArgKind::Str => Some(Type::Str),
            // `None` is also valid, so an unresolved argument cannot be
            // narrowed to `str`.
            ArgKind::StrOrNone => None,
            ArgKind::Bytes => Some(Type::Bytes),
            ArgKind::Int => Some(Type::Int),
            // A mapping and a pair iterable are both accepted, so an
            // unresolved argument cannot be narrowed to either shape.
            ArgKind::MappingOrPairs => None,
            // Lists are the common `join` operand; keep the previous
            // narrowing so later list operations on the same unresolved
            // variable are still checked.
            ArgKind::Iterable => Some(Type::List),
        }
    }
}

/// Statically known signature of a built-in method.
struct MethodSig {
    min_args: usize,
    max_args: usize,
    /// Expected kinds of the leading positional arguments; trailing `Any`
    /// parameters may be omitted from the table.
    arg_kinds: &'static [ArgKind],
    result: Type,
}

impl MethodSig {
    const fn new(
        min_args: usize,
        max_args: usize,
        arg_kinds: &'static [ArgKind],
        result: Type,
    ) -> MethodSig {
        MethodSig {
            min_args,
            max_args,
            arg_kinds,
            result,
        }
    }
}

const STR_METHODS: &[(&str, MethodSig)] = &[
    ("upper", MethodSig::new(0, 0, &[], Type::Str)),
    ("lower", MethodSig::new(0, 0, &[], Type::Str)),
    (
        "strip",
        MethodSig::new(0, 1, &[ArgKind::StrOrNone], Type::Str),
    ),
    (
        "lstrip",
        MethodSig::new(0, 1, &[ArgKind::StrOrNone], Type::Str),
    ),
    (
        "rstrip",
        MethodSig::new(0, 1, &[ArgKind::StrOrNone], Type::Str),
    ),
    ("isdigit", MethodSig::new(0, 0, &[], Type::Bool)),
    ("isalpha", MethodSig::new(0, 0, &[], Type::Bool)),
    ("isalnum", MethodSig::new(0, 0, &[], Type::Bool)),
    ("isspace", MethodSig::new(0, 0, &[], Type::Bool)),
    ("isascii", MethodSig::new(0, 0, &[], Type::Bool)),
    ("isupper", MethodSig::new(0, 0, &[], Type::Bool)),
    ("islower", MethodSig::new(0, 0, &[], Type::Bool)),
    (
        "split",
        MethodSig::new(0, 2, &[ArgKind::Str, ArgKind::Int], Type::List),
    ),
    (
        "rsplit",
        MethodSig::new(0, 2, &[ArgKind::Str, ArgKind::Int], Type::List),
    ),
    (
        "partition",
        MethodSig::new(1, 1, &[ArgKind::Str], Type::Tuple),
    ),
    (
        "rpartition",
        MethodSig::new(1, 1, &[ArgKind::Str], Type::Tuple),
    ),
    (
        "splitlines",
        MethodSig::new(0, 1, &[ArgKind::Any], Type::List),
    ),
    (
        "rjust",
        MethodSig::new(1, 2, &[ArgKind::Int, ArgKind::Str], Type::Str),
    ),
    (
        "ljust",
        MethodSig::new(1, 2, &[ArgKind::Int, ArgKind::Str], Type::Str),
    ),
    (
        "center",
        MethodSig::new(1, 2, &[ArgKind::Int, ArgKind::Str], Type::Str),
    ),
    ("zfill", MethodSig::new(1, 1, &[ArgKind::Int], Type::Str)),
    (
        "join",
        MethodSig::new(1, 1, &[ArgKind::Iterable], Type::Str),
    ),
    (
        "replace",
        MethodSig::new(2, 3, &[ArgKind::Str, ArgKind::Str, ArgKind::Int], Type::Str),
    ),
    (
        "startswith",
        MethodSig::new(
            1,
            3,
            &[ArgKind::Any, ArgKind::Int, ArgKind::Int],
            Type::Bool,
        ),
    ),
    (
        "endswith",
        MethodSig::new(
            1,
            3,
            &[ArgKind::Any, ArgKind::Int, ArgKind::Int],
            Type::Bool,
        ),
    ),
    ("find", MethodSig::new(1, 1, &[ArgKind::Str], Type::Int)),
    (
        "count",
        MethodSig::new(1, 3, &[ArgKind::Str, ArgKind::Int, ArgKind::Int], Type::Int),
    ),
    (
        "removeprefix",
        MethodSig::new(1, 1, &[ArgKind::Str], Type::Str),
    ),
    (
        "removesuffix",
        MethodSig::new(1, 1, &[ArgKind::Str], Type::Str),
    ),
    // `str.maketrans` is a static method on the type, so instances accept the
    // same table operands; the receiver is not part of the ABI.
    (
        "maketrans",
        MethodSig::new(
            1,
            3,
            &[ArgKind::Any, ArgKind::Str, ArgKind::Str],
            Type::Dict,
        ),
    ),
    (
        "encode",
        MethodSig::new(0, 2, &[ArgKind::Str, ArgKind::Str], Type::Bytes),
    ),
    (
        "translate",
        MethodSig::new(1, 1, &[ArgKind::Any], Type::Str),
    ),
    // `str.format(*args)` accepts any number of positional arguments; field
    // count and types are validated at runtime by the format parser.
    ("format", MethodSig::new(0, usize::MAX, &[], Type::Str)),
];

/// Static signatures of `bytes` methods. Byte-oriented arguments are byte
/// strings; `decode` takes an optional string encoding.
const BYTES_METHODS: &[(&str, MethodSig)] = &[
    ("decode", MethodSig::new(0, 1, &[ArgKind::Str], Type::Str)),
    ("hex", MethodSig::new(0, 0, &[], Type::Str)),
    // `bytes.maketrans` is a static method: two buffer operands, receiver
    // ignored, and always a 256-byte table.
    (
        "maketrans",
        MethodSig::new(2, 2, &[ArgKind::Any, ArgKind::Any], Type::Bytes),
    ),
    (
        "count",
        MethodSig::new(
            1,
            3,
            &[ArgKind::Bytes, ArgKind::Int, ArgKind::Int],
            Type::Int,
        ),
    ),
    ("find", MethodSig::new(1, 1, &[ArgKind::Bytes], Type::Int)),
    (
        "startswith",
        MethodSig::new(
            1,
            3,
            &[ArgKind::Any, ArgKind::Int, ArgKind::Int],
            Type::Bool,
        ),
    ),
    (
        "endswith",
        MethodSig::new(
            1,
            3,
            &[ArgKind::Any, ArgKind::Int, ArgKind::Int],
            Type::Bool,
        ),
    ),
    (
        "replace",
        MethodSig::new(
            2,
            3,
            &[ArgKind::Bytes, ArgKind::Bytes, ArgKind::Int],
            Type::Bytes,
        ),
    ),
    ("split", MethodSig::new(1, 1, &[ArgKind::Bytes], Type::List)),
    (
        "splitlines",
        MethodSig::new(0, 1, &[ArgKind::Any], Type::List),
    ),
    (
        "join",
        MethodSig::new(1, 1, &[ArgKind::Iterable], Type::Bytes),
    ),
    (
        "strip",
        MethodSig::new(0, 1, &[ArgKind::Bytes], Type::Bytes),
    ),
    (
        "lstrip",
        MethodSig::new(0, 1, &[ArgKind::Bytes], Type::Bytes),
    ),
    (
        "rstrip",
        MethodSig::new(0, 1, &[ArgKind::Bytes], Type::Bytes),
    ),
    ("upper", MethodSig::new(0, 0, &[], Type::Bytes)),
    ("lower", MethodSig::new(0, 0, &[], Type::Bytes)),
];

const LIST_METHODS: &[(&str, MethodSig)] = &[
    ("append", MethodSig::new(1, 1, &[ArgKind::Any], Type::None)),
    (
        "sort",
        MethodSig::new(0, 2, &[ArgKind::Any, ArgKind::Any], Type::None),
    ),
    (
        "extend",
        MethodSig::new(1, 1, &[ArgKind::Iterable], Type::None),
    ),
    (
        "insert",
        MethodSig::new(2, 2, &[ArgKind::Int, ArgKind::Any], Type::None),
    ),
    ("remove", MethodSig::new(1, 1, &[ArgKind::Any], Type::None)),
    ("pop", MethodSig::new(0, 1, &[ArgKind::Int], Type::Any)),
    ("clear", MethodSig::new(0, 0, &[], Type::None)),
    ("reverse", MethodSig::new(0, 0, &[], Type::None)),
    ("copy", MethodSig::new(0, 0, &[], Type::List)),
    ("count", MethodSig::new(1, 1, &[ArgKind::Any], Type::Int)),
    ("index", MethodSig::new(1, 1, &[ArgKind::Any], Type::Int)),
];

const DICT_METHODS: &[(&str, MethodSig)] = &[
    (
        "get",
        MethodSig::new(1, 2, &[ArgKind::Any, ArgKind::Any], Type::Any),
    ),
    // `keys()`/`values()`/`items()` return live runtime views, not lists:
    // `dict_keys`/`dict_items` compare and combine as sets, while
    // `dict_values` is only iterable. Their element kind is not tracked, so
    // the static type stays dynamic and the runtime protocol decides.
    ("keys", MethodSig::new(0, 0, &[], Type::Any)),
    ("values", MethodSig::new(0, 0, &[], Type::Any)),
    ("items", MethodSig::new(0, 0, &[], Type::Any)),
    (
        "pop",
        MethodSig::new(1, 2, &[ArgKind::Any, ArgKind::Any], Type::Any),
    ),
    (
        "update",
        MethodSig::new(1, 1, &[ArgKind::MappingOrPairs], Type::None),
    ),
    (
        "setdefault",
        MethodSig::new(1, 2, &[ArgKind::Any, ArgKind::Any], Type::Any),
    ),
    ("clear", MethodSig::new(0, 0, &[], Type::None)),
];

const TUPLE_METHODS: &[(&str, MethodSig)] = &[
    ("count", MethodSig::new(1, 1, &[ArgKind::Any], Type::Int)),
    ("index", MethodSig::new(1, 1, &[ArgKind::Any], Type::Int)),
];

const RANGE_METHODS: &[(&str, MethodSig)] = &[
    // `range.index(value)` returns the position of `value`, raising
    // ValueError when it is absent. The result is always an int.
    ("index", MethodSig::new(1, 1, &[ArgKind::Any], Type::Int)),
];

const SET_METHODS: &[(&str, MethodSig)] = &[
    (
        "add",
        MethodSig::new(1, 1, &[ArgKind::Hashable], Type::None),
    ),
    (
        "remove",
        MethodSig::new(1, 1, &[ArgKind::Hashable], Type::None),
    ),
    (
        "discard",
        MethodSig::new(1, 1, &[ArgKind::Hashable], Type::None),
    ),
    ("pop", MethodSig::new(0, 0, &[], Type::Any)),
    ("clear", MethodSig::new(0, 0, &[], Type::None)),
    ("copy", MethodSig::new(0, 0, &[], Type::Set)),
    // `update(*others)` takes any number of iterables (CPython's variadic
    // signature); only the leading operand can be kind-checked here.
    (
        "update",
        MethodSig::new(0, usize::MAX, &[ArgKind::Iterable], Type::None),
    ),
    (
        "union",
        MethodSig::new(0, usize::MAX, &[ArgKind::Iterable], Type::Set),
    ),
    (
        "intersection",
        MethodSig::new(0, usize::MAX, &[ArgKind::Iterable], Type::Set),
    ),
    (
        "difference",
        MethodSig::new(0, usize::MAX, &[ArgKind::Iterable], Type::Set),
    ),
    (
        "symmetric_difference",
        MethodSig::new(1, 1, &[ArgKind::Iterable], Type::Set),
    ),
    (
        "intersection_update",
        MethodSig::new(0, usize::MAX, &[ArgKind::Iterable], Type::None),
    ),
    (
        "difference_update",
        MethodSig::new(0, usize::MAX, &[ArgKind::Iterable], Type::None),
    ),
    (
        "symmetric_difference_update",
        MethodSig::new(1, 1, &[ArgKind::Iterable], Type::None),
    ),
    (
        "issubset",
        MethodSig::new(1, 1, &[ArgKind::Iterable], Type::Bool),
    ),
    (
        "issuperset",
        MethodSig::new(1, 1, &[ArgKind::Iterable], Type::Bool),
    ),
    (
        "isdisjoint",
        MethodSig::new(1, 1, &[ArgKind::Iterable], Type::Bool),
    ),
];

/// Closest method name in `table` to the misspelled `name`.
///
/// Short typo'd names can sit just outside the regular two-edit window (for
/// example `nope` vs. `upper`); a slightly wider fallback keeps the hint
/// useful without suggesting remote names.
fn closest_method(name: &str, table: &'static [(&'static str, MethodSig)]) -> Option<&'static str> {
    let candidates = || table.iter().map(|(candidate, _)| *candidate);
    diag::closest(name, candidates()).or_else(|| diag::closest_within(name, candidates(), 3))
}

/// Whether a method lookup through `kind` is decided at runtime even when the
/// inferred receiver type is a concrete builtin.
///
/// Python resolves `receiver.method` from the value's runtime class. An
/// unannotated local, parameter, closure cell, global or any derived value can
/// hold a subclass, or a value that an `isinstance` check narrowed at runtime
/// while the frontend still tracks its assignment type (`missing_values or ()`
/// stays a tuple even inside `isinstance(missing_values, bytes)`). Reporting a
/// missing builtin method for those receivers rejects valid Python, so the
/// lookup stays dynamic and the runtime raises `AttributeError` on a real miss.
///
/// Literal containers and `str`/`bytes` constants are written out with their
/// exact type, so an unknown method on them is always a static error.
fn is_gradual_receiver(kind: &HExprKind) -> bool {
    !matches!(
        kind,
        HExprKind::List(_)
            | HExprKind::Tuple(_)
            | HExprKind::Set(_)
            | HExprKind::Dict(_)
            | HExprKind::Const(Const::Str(_) | Const::Bytes(_))
    )
}

/// Whether the HIR value of `builtin` denotes a builtin class object
/// (`int`, `str`, `bytes`, ...), so `builtin.method(...)` names a class-level
/// method rather than an instance method on a function value.
fn is_builtin_class_value(builtin: Builtin) -> bool {
    matches!(
        builtin,
        Builtin::Int
            | Builtin::Float
            | Builtin::Bool
            | Builtin::Str
            | Builtin::Bytes
            | Builtin::ByteArray
            | Builtin::List
            | Builtin::Tuple
            | Builtin::Dict
            | Builtin::Set
            | Builtin::Frozenset
            | Builtin::Range
            | Builtin::MemoryView
            | Builtin::Type
    )
}

/// Whether the HIR value of `builtin` is a class that accepts type
/// parameters (`tuple[int, ...]`, `list[str]`, `dict[str, int]`, `set[T]`,
/// `frozenset[T]`, `type[T]`).
///
/// CPython serves these through `__class_getitem__`; Hydro has no generic
/// substitution, so the subscription only has to typecheck (the runtime
/// builds an alias value) and any other builtin class value keeps rejecting
/// `builtin[...]` with `'function' object is not subscriptable`.
fn is_generic_builtin(builtin: Builtin) -> bool {
    matches!(
        builtin,
        Builtin::Tuple
            | Builtin::List
            | Builtin::Dict
            | Builtin::Set
            | Builtin::Frozenset
            | Builtin::Type
    )
}

/// Result type of the class-level methods Hydro models on builtin class
/// values. Every other class-level method is dispatched at runtime, so a
/// missing entry only means "dynamic result", not "unknown attribute".
fn builtin_class_method_result(builtin: Builtin, method: &str) -> Option<Type> {
    Some(match (builtin, method) {
        (Builtin::Int, "from_bytes") => Type::Int,
        (Builtin::Int, "to_bytes") => Type::Bytes,
        (Builtin::Int, "bit_length") => Type::Int,
        (Builtin::Str, "maketrans") => Type::Dict,
        (Builtin::Bytes, "fromhex") => Type::Bytes,
        (Builtin::Bytes, "maketrans") => Type::Bytes,
        // `bytearray` shares the byte-sequence static type; the type lattice
        // has no separate mutable-bytes variant.
        (Builtin::ByteArray, "fromhex") => Type::Bytes,
        (Builtin::ByteArray, "maketrans") => Type::Bytes,
        (Builtin::Dict, "fromkeys") => Type::Dict,
        (Builtin::Float, "fromhex") => Type::Float,
        // `type` is the metaclass every class value carries. Its class-level
        // methods are the C-level type operations CPython exposes
        // (`type.__new__`, `type.__subclasscheck__`, …); typing stubs and
        // metaclasses call them directly on the `type` builtin.
        (Builtin::Type, "__new__" | "__call__" | "__getattribute__") => Type::Any,
        (Builtin::Type, "__subclasscheck__" | "__instancecheck__") => Type::Bool,
        (Builtin::Type, "__hash__") => Type::Int,
        (Builtin::Type, "mro") => Type::List,
        (Builtin::Type, "__repr__") => Type::Str,
        (Builtin::Type, "__prepare__") => Type::Dict,
        _ => return None,
    })
}

/// Result type of `a <op> b`, or `None` if the combination is invalid.
fn op_result(op: BinOp, a: Type, b: Type) -> Option<Type> {
    let any = a == Type::Any || b == Type::Any;
    match op {
        BinOp::Eq | BinOp::Ne => {
            // Python equality never fails: different types simply compare
            // unequal. `Never` marks a call that cannot return, so it cannot
            // be compared at all.
            if a == Type::Never || b == Type::Never {
                None
            } else {
                Some(Type::Bool)
            }
        }
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            if any
                || (a.is_numeric() && b.is_numeric())
                || (a == Type::Str && b == Type::Str)
                || (a == Type::Bytes && b == Type::Bytes)
                || (a == Type::Tuple && b == Type::Tuple)
                || (a == Type::List && b == Type::List)
                || (a == Type::Set && b == Type::Set)
            {
                Some(Type::Bool)
            } else {
                None
            }
        }
        // `is`/`is not` accept any pair of values: the runtime compares the
        // tagged representation, so no operand type combination is invalid.
        BinOp::Is | BinOp::IsNot => {
            if a == Type::Never || b == Type::Never {
                None
            } else {
                Some(Type::Bool)
            }
        }
        BinOp::Add => {
            if any {
                Some(Type::Any)
            } else if is_complex_mix(a, b) {
                Some(Type::Complex)
            } else if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else if a == Type::Str && b == Type::Str {
                Some(Type::Str)
            } else if a == Type::Bytes && b == Type::Bytes {
                Some(Type::Bytes)
            } else if a == Type::List && b == Type::List {
                Some(Type::List)
            } else if a == Type::Tuple && b == Type::Tuple {
                Some(Type::Tuple)
            } else {
                None
            }
        }
        BinOp::Mul => {
            if any {
                Some(Type::Any)
            } else if is_complex_mix(a, b) {
                Some(Type::Complex)
            } else if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else if (a == Type::Str && matches!(b, Type::Int | Type::BigInt | Type::Bool))
                || (b == Type::Str && matches!(a, Type::Int | Type::BigInt | Type::Bool))
            {
                // `str * int` repeats the string. `bool` is an `int` subtype.
                Some(Type::Str)
            } else if (a == Type::Bytes && matches!(b, Type::Int | Type::BigInt | Type::Bool))
                || (b == Type::Bytes && matches!(a, Type::Int | Type::BigInt | Type::Bool))
            {
                // `bytes * int` repeats the byte string.
                Some(Type::Bytes)
            } else if (a == Type::Tuple && matches!(b, Type::Int | Type::BigInt | Type::Bool))
                || (b == Type::Tuple && matches!(a, Type::Int | Type::BigInt | Type::Bool))
            {
                Some(Type::Tuple)
            } else if (a == Type::List && matches!(b, Type::Int | Type::BigInt | Type::Bool))
                || (b == Type::List && matches!(a, Type::Int | Type::BigInt | Type::Bool))
            {
                Some(Type::List)
            } else {
                None
            }
        }
        BinOp::In | BinOp::NotIn => {
            // The right operand decides whether the test is valid; the left
            // operand only has to be hashable for dict/set and a string for
            // string membership.
            match b {
                Type::Any => Some(Type::Bool),
                Type::Str => {
                    if matches!(a, Type::Str | Type::Any) {
                        Some(Type::Bool)
                    } else {
                        None
                    }
                }
                Type::Bytes => {
                    if matches!(
                        a,
                        Type::Bytes | Type::Int | Type::Bool | Type::Any | Type::Never
                    ) {
                        Some(Type::Bool)
                    } else {
                        None
                    }
                }
                Type::List | Type::Tuple => Some(Type::Bool),
                Type::Range => {
                    if matches!(
                        a,
                        Type::Int | Type::BigInt | Type::Bool | Type::Any | Type::Never
                    ) {
                        Some(Type::Bool)
                    } else {
                        None
                    }
                }
                Type::Dict | Type::Set => {
                    if matches!(a, Type::List | Type::Dict | Type::Set) {
                        None
                    } else {
                        Some(Type::Bool)
                    }
                }
                _ => None,
            }
        }
        BinOp::Sub => {
            if any {
                Some(Type::Any)
            } else if is_complex_mix(a, b) {
                Some(Type::Complex)
            } else if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else if a == Type::Set && b == Type::Set {
                Some(Type::Set)
            } else {
                None
            }
        }
        BinOp::Mod => {
            if a == Type::Str || a == Type::Bytes {
                // `str % value_or_tuple` and `bytes % value_or_tuple` are
                // printf-style formatting; the runtime validates the format
                // string and argument count.
                Some(a)
            } else if any {
                Some(Type::Any)
            } else if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else {
                None
            }
        }
        // Python has no floor division for complex values; `pow` does.
        BinOp::FloorDiv => {
            if any {
                Some(Type::Any)
            } else if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else {
                None
            }
        }
        BinOp::Pow => {
            if any {
                Some(Type::Any)
            } else if is_complex_mix(a, b) {
                Some(Type::Complex)
            } else if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else {
                None
            }
        }
        // `@` is numeric multiplication for scalars and a protocol hook
        // (`__matmul__`/`__rmatmul__`/`__imatmul__`) for user types. Hydro
        // cannot see dunders of a statically known non-numeric operand, so
        // the runtime decides and reports `TypeError`; never reject here.
        BinOp::MatMul | BinOp::MatMulInPlace => {
            if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else {
                Some(Type::Any)
            }
        }
        BinOp::Div => {
            if any {
                Some(Type::Any)
            } else if is_complex_mix(a, b) {
                Some(Type::Complex)
            } else if a.is_numeric() && b.is_numeric() {
                Some(Type::Float)
            } else {
                None
            }
        }
        // Bitwise operators are defined for integers (and the `bool`
        // subtype); every other operand combination is only known at runtime,
        // where it raises `TypeError`.
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
            if matches!(a, Type::Int | Type::BigInt | Type::Bool)
                && matches!(b, Type::Int | Type::BigInt | Type::Bool)
            {
                Some(numeric_result(a, b))
            } else {
                Some(Type::Any)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum OpKind {
    Bin(BinOp),
    Un(UnOp),
}

#[derive(Clone, Copy)]
struct Op {
    kind: OpKind,
    a: Ty,
    b: Ty,
    out: Ty,
    line: u32,
    col: u32,
    done: bool,
}

enum Check {
    Builtin {
        builtin: Builtin,
        args: Vec<(Ty, u32, u32)>,
        out: Ty,
        line: u32,
        col: u32,
    },
    RaiseMsg {
        ty: Ty,
        line: u32,
        col: u32,
    },
}

/// End column of expressions whose source spelling has a known width.
fn simple_expr_end(expr: &HExpr, f: &HFunction) -> Option<u32> {
    let width = match &expr.kind {
        HExprKind::Local(id) | HExprKind::CellGet(id) => {
            let name = &f.locals.get(*id as usize)?.name;
            if name.starts_with('$') {
                return None;
            }
            name.len()
        }
        HExprKind::BuiltinRef(builtin) => builtin.name().len(),
        HExprKind::Call {
            callee: Callee::Builtin(builtin),
            ..
        } => builtin.name().len(),
        HExprKind::ExceptionClass(kind) => kind.name().len(),
        HExprKind::GlobalGet { name, .. } => name.len(),
        HExprKind::Const(Const::None) => 4,
        HExprKind::Const(Const::Bool(true)) => 4,
        HExprKind::Const(Const::Bool(false)) => 5,
        HExprKind::Const(Const::NotImplemented) => 14,
        HExprKind::Const(Const::Ellipsis) => 3,
        _ => return None,
    };
    Some(expr.col.saturating_add(u32::try_from(width).ok()?))
}

/// Statically known binding requirements of a user function at a call site.
///
/// A call reaches the checker in one of two shapes. The binder in `lower`
/// fills every parameter slot of a direct call (literal/non-literal defaults,
/// the `*args` list, keyword-only values and the `**kwargs` dict), while a
/// cross-module export call forwards exactly the positional arguments of the
/// source. A positionally supplied call therefore fills at least the required
/// positional parameters and at most one slot per parameter, and parameters
/// that carry a default are never required.
struct CallArity {
    name: String,
    /// Parameters bound from the positional stream, in signature order:
    /// name, binding kind and whether a default can fill it.
    positional: Vec<(String, ParamKind, bool)>,
    /// Keyword-only parameters: name and whether a default can fill it.
    kwonly: Vec<(String, bool)>,
    has_vararg: bool,
    /// Number of parameter slots after the closure captures.
    slots: usize,
}

impl CallArity {
    /// Signature of a function of this unit; closure captures are not
    /// arguments, so they are not part of the signature.
    fn of(function: &HFunction) -> CallArity {
        CallArity::from_params(&function.name, &function.params, function.capture_count)
    }

    fn from_params(name: &str, params: &[HParam], capture_count: usize) -> CallArity {
        let params = &params[capture_count.min(params.len())..];
        let mut arity = CallArity {
            name: name.to_string(),
            positional: Vec::new(),
            kwonly: Vec::new(),
            has_vararg: false,
            slots: params.len(),
        };
        for param in params {
            // A default, literal or not, makes the parameter optional; the
            // binder evaluates it when the argument is omitted.
            let optional = param.default.is_some() || param.default_expr.is_some();
            match param.kind {
                ParamKind::Positional | ParamKind::PosOnly => {
                    arity
                        .positional
                        .push((param.name.clone(), param.kind, optional));
                }
                ParamKind::KwOnly => arity.kwonly.push((param.name.clone(), optional)),
                ParamKind::VarArg => arity.has_vararg = true,
                // `**kwargs` accepts every keyword name; it constrains neither
                // the positional arity nor the required parameters.
                ParamKind::KwArg => {}
            }
        }
        arity
    }

    fn missing_message(&self, param: &str, kind: ParamKind) -> String {
        let label = match kind {
            ParamKind::PosOnly => "positional argument",
            ParamKind::KwOnly => "keyword-only argument",
            _ => "argument",
        };
        format!("{}() missing required {} '{}'", self.name, label, param)
    }

    /// Diagnostic for a call that supplies only positional arguments, or
    /// `None` when the call is acceptable.
    ///
    /// Keyword-only parameters do not gate this shape: a direct call always
    /// carries their slots (the binder filled defaults or reported the
    /// omission), and a cross-module call cannot name them at all. Only the
    /// required positional parameters and the slot count bound the call.
    fn positional_error(&self, given: usize) -> Option<String> {
        for (index, (param, kind, optional)) in self.positional.iter().enumerate() {
            if *optional || index < given {
                continue;
            }
            return Some(self.missing_message(param, *kind));
        }
        if self.has_vararg || given <= self.slots {
            return None;
        }
        Some(format!(
            "{}() takes {} positional argument(s) but {} were given",
            self.name, self.slots, given
        ))
    }

    /// Diagnostic for a call that carries keyword arguments, or `None` when
    /// the call is acceptable.
    ///
    /// `given` counts the positional arguments and `keywords` the supplied
    /// keyword names. A required parameter is satisfied by either stream; a
    /// positional-only parameter only by the positional one.
    fn keyword_error(&self, given: usize, keywords: &[&str]) -> Option<String> {
        let bound_positionally = given.min(self.positional.len());
        for (index, (param, kind, optional)) in self.positional.iter().enumerate() {
            if *optional || index < bound_positionally {
                continue;
            }
            if *kind != ParamKind::PosOnly && keywords.contains(&param.as_str()) {
                continue;
            }
            return Some(self.missing_message(param, *kind));
        }
        for (param, optional) in &self.kwonly {
            if !optional && !keywords.contains(&param.as_str()) {
                return Some(self.missing_message(param, ParamKind::KwOnly));
            }
        }
        if self.has_vararg || given <= self.positional.len() {
            return None;
        }
        Some(format!(
            "{}() takes {} positional argument(s) but {} were given",
            self.name,
            self.positional.len(),
            given
        ))
    }
}

struct Checker<'a> {
    prog: &'a LHir,
    inf: &'a mut Infer,
    filename: &'a str,
    diags: Vec<Diagnostic>,
    ops: Vec<Op>,
    checks: Vec<Check>,
    /// Known end columns keyed by expression start.
    spans: HashMap<(u32, u32), u32>,
}

impl<'a> Checker<'a> {
    /// Type/operand conflicts are the common case and carry the conversion
    /// hint.
    fn error(&mut self, line: u32, col: u32, message: impl Into<String>) {
        self.error_with(
            line,
            col,
            message,
            codes::TYPE,
            Some("check the operand types or add an explicit conversion"),
        );
    }

    fn error_code(&mut self, line: u32, col: u32, code: &'static str, message: impl Into<String>) {
        self.error_with(line, col, message, code, None);
    }

    fn error_with(
        &mut self,
        line: u32,
        col: u32,
        message: impl Into<String>,
        code: &'static str,
        help: Option<&str>,
    ) {
        let mut diagnostic = Diagnostic::new(self.filename, line, col, message).with_code(code);
        if let Some(help) = help {
            diagnostic = diagnostic.with_help(help);
        }
        self.diags.push(diagnostic);
    }

    /// Reports an unavailable method and suggests the closest entry of the
    /// receiver's method table when one is near enough.
    fn error_no_method(
        &mut self,
        expr: &HExpr,
        receiver: Type,
        method: &str,
        table: Option<&'static [(&'static str, MethodSig)]>,
    ) {
        let mut diagnostic = Diagnostic::new(
            self.filename,
            expr.line,
            expr.col,
            format!("type '{}' has no method '{}'", receiver.name(), method),
        )
        .with_code(codes::ATTRIBUTE);
        if let Some(table) = table {
            if let Some(candidate) = closest_method(method, table) {
                diagnostic = diagnostic.with_help(format!("did you mean `.{}()`?", candidate));
            }
        }
        self.diags.push(diagnostic);
    }

    fn check_function(&mut self, f: &HFunction) {
        // An unannotated return type starts as a fresh inference variable and
        // an annotation starts concrete; only the inferred slot may be
        // widened by the body, so record its shape before the walk binds it.
        let inferred_return = matches!(f.ret_ty, Ty::Var(_));
        let mut has_value = false;
        let mut has_bare = false;
        for stmt in &f.body {
            self.check_stmt(stmt, f, &mut has_value, &mut has_bare);
        }

        if f.is_module {
            return;
        }
        if f.is_generator {
            // A generator's annotation describes the yielded values, and an
            // explicit `return` value becomes the generator result, so the
            // ordinary return checks do not apply.
            return;
        }
        if has_value {
            if has_bare && inferred_return {
                // A bare `return` returns `None`, and CPython allows a
                // function to mix it with value returns. An inferred result
                // type is a single static type, so the join of the value type
                // and `None` is dynamic; a result that is already `None`
                // (only `return None` plus bare returns) stays precise.
                // Declared return types are left as written: their value
                // returns were checked against the annotation above.
                if !matches!(self.inf.resolve(f.ret_ty), Ty::Concrete(Type::None)) {
                    self.force_any(f.ret_ty);
                }
            }
        } else {
            // A stub body (`...`, `pass`, or just a docstring) returns `None`
            // at runtime, so a non-`None` return annotation on it states the
            // contract its implementations honour (typing protocols, abstract
            // placeholders) rather than a missing return value. An
            // unannotated stub used as a value still reports below.
            //
            // A body whose only executable statement is `raise` cannot fall
            // through at all, so it never misses a return value and no
            // annotation can be wrong about one (NumPy protocol methods
            // spelled as `raise NotImplementedError` do this).
            let declared_stub = matches!(f.ret_ty, Ty::Concrete(_)) && Self::is_stub_body(&f.body);
            let never_returns = Self::is_raise_only_body(&f.body);
            if self.inf.unify(f.ret_ty, Ty::Concrete(Type::None)).is_err()
                && !declared_stub
                && !never_returns
            {
                match f.ret_ty {
                    Ty::Concrete(declared) => {
                        self.error_code(
                            f.line,
                            f.col,
                            codes::CONTROL,
                            format!(
                                "function '{}' is declared to return {}, but has no return value",
                                f.name,
                                declared.name()
                            ),
                        );
                    }
                    Ty::Var(_) => {
                        self.error_code(
                            f.line,
                            f.col,
                            codes::CONTROL,
                            format!(
                                "function '{}' has no return value but is used as a value-returning function",
                                f.name
                            ),
                        );
                    }
                }
            }
        }
    }

    /// The constant of an expression statement that is a lone literal.
    fn literal_stmt(stmt: &HStmt) -> Option<&Const> {
        match &stmt.kind {
            HStmtKind::Expr(expr) => match &expr.kind {
                HExprKind::Const(constant) => Some(constant),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether a statement is a stub marker: `pass` or an expression holding
    /// only `...`.
    fn is_stub_marker(stmt: &HStmt) -> bool {
        match &stmt.kind {
            HStmtKind::Pass => true,
            _ => matches!(Self::literal_stmt(stmt), Some(Const::Ellipsis)),
        }
    }

    /// The statements of `body` with a leading docstring removed.
    fn body_without_docstring(body: &[HStmt]) -> &[HStmt] {
        // A leading string literal is the docstring, not code.
        if let Some(Const::Str(_)) = body.first().and_then(|stmt| Self::literal_stmt(stmt)) {
            &body[1..]
        } else {
            body
        }
    }

    /// Whether a body only declares a stub: an optional docstring followed by
    /// `pass` or `...` (a typing stub or an abstract placeholder), or just the
    /// docstring itself.
    ///
    /// Lowering keeps the markers as statements, so detection works on the
    /// HIR. At runtime such a body returns `None`; the declared return type
    /// describes what an implementation must return.
    fn is_stub_body(body: &[HStmt]) -> bool {
        match Self::body_without_docstring(body) {
            [] => true,
            [stmt] => Self::is_stub_marker(stmt),
            _ => false,
        }
    }

    /// Whether a body only raises: an optional docstring followed by a single
    /// `raise ...` statement.
    ///
    /// Such a body cannot fall through, so it never produces a return value
    /// and a return annotation on it states what the raising function would
    /// have returned rather than a value it is missing.
    fn is_raise_only_body(body: &[HStmt]) -> bool {
        matches!(
            Self::body_without_docstring(body),
            [stmt] if matches!(stmt.kind, HStmtKind::Raise { .. })
        )
    }

    fn check_stmts(
        &mut self,
        stmts: &[HStmt],
        f: &HFunction,
        has_value: &mut bool,
        has_bare: &mut bool,
    ) {
        for stmt in stmts {
            self.check_stmt(stmt, f, has_value, has_bare);
        }
    }

    /// Bind a value to a function-local slot.
    ///
    /// An inferred slot is a type variable: Python locals may hold a value of
    /// any type, so a slot assigned incompatible types across branches or
    /// iterations widens to `Any` (the runtime operators then decide).
    /// Assigning a value whose type is not resolved yet must not pin it to the
    /// slot's previous type (`x = None` followed by `x = maybe_value`); the
    /// slot becomes dynamic instead, so later uses keep checking dynamically.
    /// Annotated parameter slots are stored concrete and keep their strict
    /// check.
    fn assign_local(&mut self, local_ty: Ty, value_ty: Ty, name: &str, line: u32, col: u32) {
        if matches!(local_ty, Ty::Concrete(_)) {
            if let Err((expected, got)) = self.inf.unify(local_ty, value_ty) {
                if self.inf.is_gradual(value_ty) {
                    // The value comes from an unannotated parameter: its
                    // runtime type is unknown, so it is compatible with the
                    // annotated slot.
                    self.force_any(value_ty);
                } else if (expected == Type::None) != (got == Type::None) {
                    self.force_any(value_ty);
                } else {
                    self.error(
                        line,
                        col,
                        format!(
                            "cannot assign type {} to variable '{}' of type {}",
                            got.name(),
                            name,
                            expected.name()
                        ),
                    );
                }
            }
            return;
        }
        match (self.inf.resolve(local_ty), self.inf.resolve(value_ty)) {
            (Ty::Concrete(Type::None), Ty::Var(_)) => {
                // The slot is an optional slot (`x = None` on another path)
                // and this value is not resolved yet. Do not pin the value to
                // `None`; the slot stays dynamic instead.
                self.force_any(local_ty);
            }
            _ => {
                if self.inf.unify(local_ty, value_ty).is_err() {
                    // The slot is dynamic; the assigned expression keeps its
                    // own type so other reads of it stay precise.
                    self.force_any(local_ty);
                }
            }
        }
    }

    /// Widen the element slots of a yielded tuple whose element types differ.
    ///
    /// A generator may yield tuples from several branches/iterations whose
    /// corresponding elements have different types (for example `arg` is an
    /// `int` on one path and `None` on another). Keeping the elements dynamic
    /// prevents one path's type from constraining the other.
    fn widen_yielded_tuple(&mut self, value: &HExpr) {
        let HExprKind::Tuple(elems) = &value.kind else {
            return;
        };
        let differs = {
            let mut concrete = elems
                .iter()
                .filter_map(|elem| match self.inf.resolve(elem.ty) {
                    Ty::Concrete(ty) if ty != Type::Any => Some(ty),
                    _ => None,
                });
            match concrete.next() {
                Some(first) => concrete.any(|ty| ty != first),
                None => false,
            }
        };
        if differs {
            // Only inferred slots can be widened; literal/expression types
            // that are already concrete stay as they are.
            for elem in elems {
                if matches!(elem.ty, Ty::Var(_)) {
                    self.force_any(elem.ty);
                }
            }
        }
    }

    fn check_stmt(
        &mut self,
        stmt: &HStmt,
        f: &HFunction,
        has_value: &mut bool,
        has_bare: &mut bool,
    ) {
        match &stmt.kind {
            HStmtKind::Expr(e) => self.check_expr(e, f),
            HStmtKind::Assign { target, value } => {
                self.check_expr(value, f);
                let local = &f.locals[*target as usize];
                self.assign_local(local.ty, value.ty, &local.name, stmt.line, stmt.col);
            }
            HStmtKind::CellInit { value, .. } => {
                if let Some(value) = value {
                    self.check_expr(value, f);
                }
            }
            HStmtKind::CellAssign { target, value } => {
                self.check_expr(value, f);
                let local = &f.locals[*target as usize];
                self.assign_local(local.ty, value.ty, &local.name, stmt.line, stmt.col);
            }
            HStmtKind::GlobalSet { value, .. } => {
                // The global table stores dynamically typed values.
                self.check_expr(value, f);
            }
            HStmtKind::Unpack { targets, value } => {
                self.check_expr(value, f);
                if self.inf.is_gradual(value.ty) {
                    self.force_any(value.ty);
                }
                match self.inf.resolve(value.ty) {
                    // The runtime accepts every iterable; lists, tuples,
                    // strings and bytes are unpacked without materialisation.
                    Ty::Concrete(Type::List)
                    | Ty::Concrete(Type::Tuple)
                    | Ty::Concrete(Type::Set)
                    | Ty::Concrete(Type::Str)
                    | Ty::Concrete(Type::Bytes)
                    | Ty::Concrete(Type::Dict)
                    | Ty::Concrete(Type::Range)
                    | Ty::Concrete(Type::Any)
                    | Ty::Concrete(Type::Never) => {}
                    Ty::Var(_) => {
                        let _ = self.inf.unify(value.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(got) => {
                        self.error(
                            value.line,
                            value.col,
                            format!("cannot unpack {}", got.name()),
                        );
                    }
                }
                // Unpacked elements are only known at runtime.
                for target in targets {
                    self.check_unpack_target(target, f);
                }
            }
            HStmtKind::AugAssign { target, op, value } => {
                self.check_expr(value, f);
                let local_ty = f.locals[*target as usize].ty;
                self.ops.push(Op {
                    kind: OpKind::Bin(*op),
                    a: local_ty,
                    b: value.ty,
                    out: local_ty,
                    line: stmt.line,
                    col: stmt.col,
                    done: false,
                });
            }
            HStmtKind::SetSubscript {
                object,
                index,
                value,
            } => {
                self.check_expr(value, f);
                self.check_expr(object, f);
                self.check_expr(index, f);
                self.check_subscript_assignment(object, index);
            }
            HStmtKind::SetSlice {
                object,
                start,
                stop,
                step,
                value,
            } => {
                self.check_expr(value, f);
                self.check_expr(object, f);
                self.check_slice_bound(start, f);
                self.check_slice_bound(stop, f);
                self.check_slice_bound(step, f);
                self.check_slice_assignment_target(object);
            }
            HStmtKind::Closure { target, .. } => {
                // The closure value is a runtime function object without a
                // static type.
                let local_ty = f.locals[*target as usize].ty;
                let _ = self.inf.unify(local_ty, Ty::Concrete(Type::Any));
            }
            HStmtKind::ClassDef {
                target,
                bases,
                metaclass,
                attributes,
                ..
            } => {
                for base in bases {
                    self.check_expr(base, f);
                }
                if let Some(metaclass) = metaclass {
                    self.check_expr(metaclass, f);
                }
                // Attribute values are ordinary expressions evaluated when the
                // class statement runs; they do not need a static type.
                for (_, value) in attributes {
                    self.check_expr(value, f);
                }
                // A class object is a runtime value without a static type.
                let local_ty = f.locals[*target as usize].ty;
                let _ = self.inf.unify(local_ty, Ty::Concrete(Type::Any));
            }
            HStmtKind::SetAttr { object, value, .. } => {
                self.check_expr(value, f);
                self.check_expr(object, f);
            }
            HStmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                self.check_expr(cond, f);
                self.check_stmts(then_body, f, has_value, has_bare);
                self.check_stmts(else_body, f, has_value, has_bare);
            }
            HStmtKind::While {
                cond,
                body,
                else_body,
            } => {
                self.check_expr(cond, f);
                self.check_stmts(body, f, has_value, has_bare);
                self.check_stmts(else_body, f, has_value, has_bare);
            }
            HStmtKind::ForRange {
                var,
                start,
                stop,
                step,
                body,
                else_body,
            } => {
                let var_ty = f.locals[*var as usize].ty;
                match self.inf.resolve(var_ty) {
                    Ty::Concrete(Type::Int) | Ty::Concrete(Type::Any) => {}
                    Ty::Var(_) => {
                        let _ = self.inf.unify(var_ty, Ty::Concrete(Type::Int));
                    }
                    Ty::Concrete(got) => {
                        self.error(
                            stmt.line,
                            stmt.col,
                            format!(
                                "loop variable '{}' has type {}, but range() yields int",
                                f.locals[*var as usize].name,
                                got.name()
                            ),
                        );
                    }
                }
                self.check_range_arg("start", start, f);
                self.check_range_arg("stop", stop, f);
                if let Some(step) = step {
                    self.check_range_arg("step", step, f);
                }
                self.check_stmts(body, f, has_value, has_bare);
                self.check_stmts(else_body, f, has_value, has_bare);
            }
            HStmtKind::ForEach {
                var,
                iterable,
                body,
                else_body,
            } => {
                self.check_expr(iterable, f);
                if self.inf.is_gradual(iterable.ty) {
                    self.force_any(iterable.ty);
                }
                let iterable_ty = self.inf.resolve(iterable.ty);
                match iterable_ty {
                    Ty::Concrete(Type::List)
                    | Ty::Concrete(Type::Str)
                    | Ty::Concrete(Type::Bytes)
                    | Ty::Concrete(Type::Dict)
                    | Ty::Concrete(Type::Tuple)
                    | Ty::Concrete(Type::Set)
                    | Ty::Concrete(Type::Range)
                    | Ty::Concrete(Type::Any) => {}
                    Ty::Var(_) => {
                        let _ = self.inf.unify(iterable.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(got) => {
                        self.error(
                            iterable.line,
                            iterable.col,
                            format!("'{}' object is not iterable", got.name()),
                        );
                    }
                }
                let var_ty = f.locals[*var as usize].ty;
                // `bytes` yields integer byte values; every other iterable
                // keeps its elements dynamically typed.
                let element = if iterable_ty == Ty::Concrete(Type::Bytes) {
                    Type::Int
                } else {
                    Type::Any
                };
                let _ = self.inf.unify(var_ty, Ty::Concrete(element));
                self.check_stmts(body, f, has_value, has_bare);
                self.check_stmts(else_body, f, has_value, has_bare);
            }
            HStmtKind::Return(value) => match value {
                Some(value) => {
                    *has_value = true;
                    self.check_expr(value, f);
                    if !f.is_generator {
                        let expected = self.inf.resolve(f.ret_ty);
                        let got = self.inf.resolve(value.ty);
                        if (matches!(expected, Ty::Concrete(Type::None)))
                            != (matches!(got, Ty::Concrete(Type::None)))
                        {
                            // Mixed `return value` / `return None` functions
                            // are dynamic, regardless of which branch was
                            // checked first.
                            self.force_any(f.ret_ty);
                            self.force_any(value.ty);
                        }
                        match f.ret_ty {
                            Ty::Concrete(expected) => match self.inf.resolve(value.ty) {
                                Ty::Concrete(got) => {
                                    if !annotation_accepts(expected, got) {
                                        if self.inf.is_gradual(value.ty) {
                                            // The returned value comes from an
                                            // unannotated parameter, so its
                                            // runtime type is unknown.
                                            self.force_any(value.ty);
                                        } else if expected == Type::None || got == Type::None {
                                            // A function that returns both a
                                            // value and `None` is dynamic; the
                                            // implicit return typing must not
                                            // reject the second branch.
                                            let _ =
                                                self.inf.unify(f.ret_ty, Ty::Concrete(Type::Any));
                                        } else {
                                            self.error(
                                                value.line,
                                                value.col,
                                                format!(
                                                    "return type {} does not match declared return type {}",
                                                    got.name(),
                                                    expected.name()
                                                ),
                                            );
                                        }
                                    }
                                }
                                Ty::Var(_) => {
                                    let _ = self.inf.unify(value.ty, Ty::Concrete(expected));
                                }
                            },
                            Ty::Var(_) => {
                                let _ = self.inf.unify(f.ret_ty, value.ty);
                            }
                        }
                    }
                }
                None => *has_bare = true,
            },
            HStmtKind::Yield { value } => {
                if let Some(value) = value {
                    self.check_expr(value, f);
                    self.widen_yielded_tuple(value);
                }
            }
            HStmtKind::Break | HStmtKind::Continue | HStmtKind::Pass => {}
            HStmtKind::Raise { msg, cause, .. } => {
                if let Some(msg) = msg {
                    self.check_expr(msg, f);
                    self.checks.push(Check::RaiseMsg {
                        ty: msg.ty,
                        line: msg.line,
                        col: msg.col,
                    });
                }
                // The explicit cause is a runtime exception object; its static
                // type is dynamic, so only walk it for inner expression checks.
                if let Some(cause) = cause {
                    self.check_expr(cause, f);
                }
            }
            HStmtKind::Assert { test, msg } => {
                self.check_expr(test, f);
                // The message may be any value: `assert x, [1, 2]` is legal
                // Python, so unlike `raise` it is not restricted to `str`.
                if let Some(msg) = msg {
                    self.check_expr(msg, f);
                }
            }
            HStmtKind::Delete { targets } => {
                // Receivers, indices and slice bounds are ordinary
                // expressions. Deleting from an immutable receiver or a class
                // namespace raises at runtime, so it is not a compile error.
                for target in targets {
                    match target {
                        HDeleteTarget::Name(_)
                        | HDeleteTarget::Cell(_)
                        | HDeleteTarget::Global { .. } => {}
                        HDeleteTarget::Attr { object, .. } => self.check_expr(object, f),
                        HDeleteTarget::Subscript { object, index } => {
                            self.check_expr(object, f);
                            self.check_expr(index, f);
                            self.check_subscript_assignment(object, index);
                        }
                        HDeleteTarget::Slice {
                            object,
                            start,
                            stop,
                            step,
                        } => {
                            self.check_expr(object, f);
                            self.check_slice_bound(start, f);
                            self.check_slice_bound(stop, f);
                            self.check_slice_bound(step, f);
                            self.check_slice_assignment_target(object);
                        }
                    }
                }
            }
            HStmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => {
                self.check_stmts(body, f, has_value, has_bare);
                for handler in handlers {
                    if let Some(binding) = handler.binding {
                        let ty = f.locals[binding as usize].ty;
                        // Exception bindings are runtime objects; keep them
                        // dynamic instead of guessing a static type.
                        let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
                    }
                    self.check_stmts(&handler.body, f, has_value, has_bare);
                }
                self.check_stmts(else_body, f, has_value, has_bare);
                self.check_stmts(finally_body, f, has_value, has_bare);
            }
        }
    }

    /// Validate the receiver and index of an assignment-style subscript after
    /// both expressions were checked.
    fn check_subscript_assignment(&mut self, object: &HExpr, index: &HExpr) {
        if self.inf.is_gradual(object.ty) {
            self.force_any(object.ty);
            return;
        }
        match self.inf.resolve(object.ty) {
            Ty::Concrete(Type::List) => {
                // A slice-carrying tuple key is a dynamic key: the runtime
                // `__setitem__`/native check reports the CPython error.
                if !Self::index_has_slice(index) {
                    self.check_int_index("list", index.ty, index.line, index.col);
                }
            }
            Ty::Concrete(Type::Dict) | Ty::Concrete(Type::Any) => {}
            Ty::Var(_) => {
                let _ = self.inf.unify(object.ty, Ty::Concrete(Type::Any));
            }
            Ty::Concrete(got) => {
                self.error_code(
                    object.line,
                    object.col,
                    codes::INDEX,
                    format!("'{}' object does not support item assignment", got.name()),
                );
            }
        }
    }

    /// Validate the receiver of a slice assignment (`a[i:j] = v`). Only lists
    /// support item assignment through a slice; a statically known dict fails
    /// like a dict slice read (a `slice` is unhashable) and every other known
    /// type rejects item assignment, matching the runtime errors.
    fn check_slice_assignment_target(&mut self, object: &HExpr) {
        if self.inf.is_gradual(object.ty) {
            self.force_any(object.ty);
            return;
        }
        match self.inf.resolve(object.ty) {
            Ty::Concrete(Type::List) | Ty::Concrete(Type::Any) => {}
            Ty::Var(_) => {
                let _ = self.inf.unify(object.ty, Ty::Concrete(Type::Any));
            }
            Ty::Concrete(Type::Dict) => {
                self.error_code(
                    object.line,
                    object.col,
                    codes::INDEX,
                    "unhashable type: 'slice'".to_string(),
                );
            }
            Ty::Concrete(got) => {
                self.error_code(
                    object.line,
                    object.col,
                    codes::INDEX,
                    format!("'{}' object does not support item assignment", got.name()),
                );
            }
        }
    }

    /// Validate one binding target of a sequence assignment. Names become
    /// dynamically typed; attribute and subscript targets check their receiver
    /// expressions; nested targets recurse.
    fn check_unpack_target(&mut self, target: &HUnpackTarget, f: &HFunction) {
        match target {
            HUnpackTarget::Name(id) | HUnpackTarget::Cell(id) => {
                let ty = f.locals[*id as usize].ty;
                let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
            }
            HUnpackTarget::Attr { object, .. } => self.check_expr(object, f),
            // A `global` target stores a dynamically typed value into the
            // runtime global table: there is no local slot to type.
            HUnpackTarget::Global { .. } => {}
            HUnpackTarget::Subscript { object, index } => {
                self.check_expr(object, f);
                self.check_expr(index, f);
                self.check_subscript_assignment(object, index);
            }
            HUnpackTarget::Star(inner) => self.check_unpack_target(inner, f),
            HUnpackTarget::Nested(targets) => {
                for target in targets {
                    self.check_unpack_target(target, f);
                }
            }
        }
    }

    /// Reject dict-literal keys whose elements are statically unhashable.
    /// Plain tuple keys are fine; a tuple is walked recursively so nested
    /// lists/dicts are reported with the offending element's position.
    fn check_hashable(&mut self, key: &HExpr) {
        if let HExprKind::Tuple(elems) = &key.kind {
            for elem in elems {
                self.check_hashable(elem);
            }
            return;
        }
        if let Ty::Concrete(t) = self.inf.resolve(key.ty) {
            if t == Type::List || t == Type::Dict || t == Type::Set {
                if self.inf.is_gradual(key.ty) {
                    self.force_any(key.ty);
                } else {
                    self.error(
                        key.line,
                        key.col,
                        format!("unhashable type: '{}'", t.name()),
                    );
                }
            }
        }
    }

    /// Validate a comprehension's clauses outward-in. Each iterable must be
    /// iterable; targets are dynamic; conditions accept any truthy value.
    fn check_comp_clauses(&mut self, clauses: &[HCompClause], f: &HFunction) {
        for clause in clauses {
            self.check_expr(&clause.iterable, f);
            if self.inf.is_gradual(clause.iterable.ty) {
                self.force_any(clause.iterable.ty);
            }
            if clause.is_async {
                // An asynchronous iterable only needs `__aiter__`; library and
                // user objects are dynamically typed, so only a concrete
                // builtin that has no such protocol is statically impossible.
                match self.inf.resolve(clause.iterable.ty) {
                    Ty::Var(_) => {
                        let _ = self.inf.unify(clause.iterable.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(Type::Any) => {}
                    Ty::Concrete(got) => self.error(
                        clause.iterable.line,
                        clause.iterable.col,
                        format!("'{}' object is not async iterable", got.name()),
                    ),
                }
            } else {
                match self.inf.resolve(clause.iterable.ty) {
                    Ty::Concrete(Type::List)
                    | Ty::Concrete(Type::Tuple)
                    | Ty::Concrete(Type::Set)
                    | Ty::Concrete(Type::Range)
                    | Ty::Concrete(Type::Str)
                    | Ty::Concrete(Type::Bytes)
                    | Ty::Concrete(Type::Dict)
                    | Ty::Concrete(Type::Any) => {}
                    Ty::Var(_) => {
                        let _ = self.inf.unify(clause.iterable.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(got) => {
                        self.error(
                            clause.iterable.line,
                            clause.iterable.col,
                            format!("'{}' object is not iterable", got.name()),
                        );
                    }
                }
            }
            match &clause.target {
                HCompTarget::Name(id) => {
                    let ty = f.locals[*id as usize].ty;
                    let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
                }
                HCompTarget::Unpack(targets) => {
                    for target in targets {
                        self.check_unpack_target(target, f);
                    }
                }
            }
            for condition in &clause.conditions {
                self.check_expr(condition, f);
            }
        }
    }

    fn check_range_arg(&mut self, name: &str, expr: &HExpr, f: &HFunction) {
        self.check_expr(expr, f);
        match self.inf.resolve(expr.ty) {
            Ty::Concrete(Type::Int)
            | Ty::Concrete(Type::BigInt)
            | Ty::Concrete(Type::Bool)
            | Ty::Concrete(Type::Any) => {}
            Ty::Var(_) => {
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Int));
            }
            Ty::Concrete(got) => {
                if self.inf.is_gradual(expr.ty) {
                    self.force_any(expr.ty);
                } else {
                    self.error(
                        expr.line,
                        expr.col,
                        format!(
                            "range() argument '{}' has type {}, expected int",
                            name,
                            got.name()
                        ),
                    );
                }
            }
        }
    }

    /// True when a subscript index is a tuple or list literal carrying at
    /// least one `slice` value. The lowering builds those elements by calling
    /// the cached `builtins.slice` class (`a[1:2, :]`), never by a standalone
    /// slice expression, so the callee shape identifies them.
    fn index_has_slice(index: &HExpr) -> bool {
        let elements = match &index.kind {
            HExprKind::Tuple(elements) | HExprKind::List(elements) => elements,
            _ => return false,
        };
        elements.iter().any(|element| match &element.kind {
            HExprKind::IndirectCall { callee, .. } => matches!(
                &callee.kind,
                HExprKind::ModuleAttr { module, name }
                    if module.as_str() == "builtins" && name.as_str() == "slice"
            ),
            _ => false,
        })
    }

    fn check_int_index(&mut self, container: &str, index: Ty, line: u32, col: u32) {
        match self.inf.resolve(index) {
            Ty::Concrete(Type::Int)
            | Ty::Concrete(Type::BigInt)
            | Ty::Concrete(Type::Bool)
            | Ty::Concrete(Type::Any) => {}
            Ty::Var(_) => {
                let _ = self.inf.unify(index, Ty::Concrete(Type::Int));
            }
            Ty::Concrete(got) => {
                if self.inf.is_gradual(index) {
                    self.force_any(index);
                } else {
                    self.error_code(
                        line,
                        col,
                        codes::INDEX,
                        format!("{} index must be int, not {}", container, got.name()),
                    );
                }
            }
        }
    }

    fn check_slice_bound(&mut self, bound: &Option<Box<HExpr>>, f: &HFunction) {
        if let Some(bound) = bound {
            self.check_expr(bound, f);
            self.check_slice_index(bound);
        }
    }

    /// Slice bounds and step follow Python's `slice.indices`: integers, booleans
    /// (as `int`), or `None`. Unknown types bind to `int`.
    fn check_slice_index(&mut self, expr: &HExpr) {
        match self.inf.resolve(expr.ty) {
            Ty::Concrete(Type::Int)
            | Ty::Concrete(Type::BigInt)
            | Ty::Concrete(Type::Bool)
            | Ty::Concrete(Type::None)
            | Ty::Concrete(Type::Any) => {}
            Ty::Var(_) => {
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Int));
            }
            Ty::Concrete(_) => {
                if self.inf.is_gradual(expr.ty) {
                    self.force_any(expr.ty);
                } else {
                    self.error_code(
                        expr.line,
                        expr.col,
                        codes::INDEX,
                        "slice indices must be integers or None".to_string(),
                    );
                }
            }
        }
    }

    fn check_expr(&mut self, expr: &HExpr, f: &HFunction) {
        if let Some(end_col) = simple_expr_end(expr, f) {
            self.spans.entry((expr.line, expr.col)).or_insert(end_col);
        }
        match &expr.kind {
            HExprKind::Const(_) => {}
            HExprKind::Local(id) => {
                let local_ty = f.locals[*id as usize].ty;
                let _ = self.inf.unify(local_ty, expr.ty);
            }
            HExprKind::CellGet(id) => {
                let local_ty = f.locals[*id as usize].ty;
                let _ = self.inf.unify(local_ty, expr.ty);
            }
            HExprKind::CellSet { target, value } => {
                self.check_expr(value, f);
                let slot = f.locals[*target as usize].ty;
                if matches!(self.inf.resolve(slot), Ty::Var(_)) {
                    let _ = self.inf.unify(slot, Ty::Concrete(Type::Any));
                }
                let _ = self.inf.unify(expr.ty, value.ty);
            }
            HExprKind::NamedExpr { target, value } => {
                self.check_expr(value, f);
                // The slot may receive any value; only a still-open slot is
                // widened to `Any`, so an annotated local keeps its type.
                let slot = f.locals[*target as usize].ty;
                if matches!(self.inf.resolve(slot), Ty::Var(_)) {
                    let _ = self.inf.unify(slot, Ty::Concrete(Type::Any));
                }
                // The expression evaluates to the assigned value.
                let _ = self.inf.unify(expr.ty, value.ty);
            }
            HExprKind::GlobalSet { value, .. } => {
                // A module global is dynamically typed; the expression
                // evaluates to the value it stored.
                self.check_expr(value, f);
                let _ = self.inf.unify(expr.ty, value.ty);
            }
            HExprKind::Bin { op, lhs, rhs } => {
                self.check_expr(lhs, f);
                self.check_expr(rhs, f);
                // A tuple on the left of dict/set membership must be hashed;
                // walk literal tuples so nested containers are reported with
                // the offending element's position.
                if matches!(op, BinOp::In | BinOp::NotIn)
                    && matches!(
                        self.inf.resolve(rhs.ty),
                        Ty::Concrete(Type::Dict) | Ty::Concrete(Type::Set)
                    )
                    && matches!(&lhs.kind, HExprKind::Tuple(_))
                {
                    self.check_hashable(lhs);
                }
                self.ops.push(Op {
                    kind: OpKind::Bin(*op),
                    a: lhs.ty,
                    b: rhs.ty,
                    out: expr.ty,
                    line: expr.line,
                    col: expr.col,
                    done: false,
                });
            }
            HExprKind::Un { op, operand } => {
                self.check_expr(operand, f);
                self.ops.push(Op {
                    kind: OpKind::Un(*op),
                    a: operand.ty,
                    b: Ty::Concrete(Type::None),
                    out: expr.ty,
                    line: expr.line,
                    col: expr.col,
                    done: false,
                });
            }
            HExprKind::BoolOp { lhs, rhs, .. } => {
                self.check_expr(lhs, f);
                self.check_expr(rhs, f);
                // A gradual operand (an unannotated parameter) may hold either
                // branch's value; unifying it with the other operand would pin
                // it to that type. CPython's `key or None` in `configparser`
                // is a dynamic `str | None` expression, not a constant `None`,
                // so widen it and keep the result dynamic instead of reporting
                // a bogus `str + None`.
                if self.inf.is_gradual(lhs.ty) || self.inf.is_gradual(rhs.ty) {
                    self.widen_gradual(lhs.ty);
                    self.widen_gradual(rhs.ty);
                    self.force_any(expr.ty);
                    return;
                }
                // `and`/`or` return one of their operands; a shared type
                // narrows the result, mixed operands stay dynamic.
                match self.inf.unify(lhs.ty, rhs.ty) {
                    Ok(ty) => {
                        let _ = self.inf.unify(expr.ty, ty);
                    }
                    Err(_) => {
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    }
                }
            }
            HExprKind::IfExp { test, then, orelse } => {
                self.check_expr(test, f);
                self.check_expr(then, f);
                self.check_expr(orelse, f);
                // Python's conditional expression may mix branch types (the
                // static result is then dynamic); only a shared branch type
                // narrows the result.
                match self.inf.unify(then.ty, orelse.ty) {
                    Ok(ty) => {
                        let _ = self.inf.unify(expr.ty, ty);
                    }
                    Err(_) => self.force_any(expr.ty),
                }
            }
            HExprKind::List(elems) => {
                for elem in elems {
                    self.check_expr(elem, f);
                }
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::List));
            }
            HExprKind::Tuple(elems) => {
                for elem in elems {
                    self.check_expr(elem, f);
                }
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Tuple));
            }
            HExprKind::Set(elems) => {
                for elem in elems {
                    self.check_expr(elem, f);
                    self.check_hashable(elem);
                }
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Set));
            }
            HExprKind::Dict(pairs) => {
                for (key, value) in pairs {
                    self.check_expr(key, f);
                    self.check_expr(value, f);
                    self.check_hashable(key);
                }
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Dict));
            }
            HExprKind::ListComp { element, clauses } => {
                self.check_comp_clauses(clauses, f);
                self.check_expr(element, f);
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::List));
            }
            HExprKind::SetComp { element, clauses } => {
                self.check_comp_clauses(clauses, f);
                self.check_expr(element, f);
                self.check_hashable(element);
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Set));
            }
            HExprKind::DictComp {
                key,
                value,
                clauses,
            } => {
                self.check_comp_clauses(clauses, f);
                self.check_expr(key, f);
                self.check_expr(value, f);
                self.check_hashable(key);
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Dict));
            }
            HExprKind::Subscript { object, index } => {
                self.check_expr(object, f);
                self.check_expr(index, f);
                // A tuple index carrying a slice (`a[:, 1]`, `a[1, :]`) is a
                // NumPy-style key: the receiver's `__getitem__` (or the native
                // sequence check for list/str/bytes/tuple) owns the verdict, so
                // the static integer-index check is skipped and the result is
                // dynamic.
                let slice_index = Self::index_has_slice(index);
                // A builtin class value is a function at runtime, but the
                // generic ones accept type parameters in annotations
                // (`tuple[int, ...]`, `list[str]`). Hydro has no generic
                // substitution: the runtime builds an alias value, so the
                // result is dynamic and the index operands are unconstrained.
                if matches!(
                    &object.kind,
                    HExprKind::BuiltinRef(builtin) if is_generic_builtin(*builtin)
                ) {
                    let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    return;
                }
                // An unannotated parameter is gradual: a call site may have
                // narrowed it (a marker passed as `bool`), but a subscript in
                // the body is still valid Python and the runtime decides.
                if self.inf.is_gradual(object.ty) {
                    self.force_any(object.ty);
                    let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    return;
                }
                match self.inf.resolve(object.ty) {
                    Ty::Concrete(Type::List) => {
                        if !slice_index {
                            self.check_int_index("list", index.ty, index.line, index.col);
                        }
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(Type::Str) => {
                        let _ = if slice_index {
                            self.inf.unify(expr.ty, Ty::Concrete(Type::Any))
                        } else {
                            self.check_int_index("str", index.ty, index.line, index.col);
                            self.inf.unify(expr.ty, Ty::Concrete(Type::Str))
                        };
                    }
                    Ty::Concrete(Type::Bytes) => {
                        let _ = if slice_index {
                            self.inf.unify(expr.ty, Ty::Concrete(Type::Any))
                        } else {
                            self.check_int_index("bytes", index.ty, index.line, index.col);
                            // Indexing a byte string yields an `int` byte value.
                            self.inf.unify(expr.ty, Ty::Concrete(Type::Int))
                        };
                    }
                    Ty::Concrete(Type::Tuple) => {
                        if !slice_index {
                            self.check_int_index("tuple", index.ty, index.line, index.col);
                        }
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    }
                    // A range is a subscriptable sequence of its arithmetic
                    // progression: `r[i]` is an `int`. A tuple key carrying a
                    // slice (`r[:, 1]`) is a runtime `TypeError`, so it stays
                    // dynamic like the other sequence receivers.
                    Ty::Concrete(Type::Range) => {
                        let _ = if slice_index {
                            self.inf.unify(expr.ty, Ty::Concrete(Type::Any))
                        } else {
                            self.check_int_index("range", index.ty, index.line, index.col);
                            self.inf.unify(expr.ty, Ty::Concrete(Type::Int))
                        };
                    }
                    Ty::Concrete(Type::Dict) | Ty::Concrete(Type::Any) => {
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Var(_) => {
                        // Object type is not known yet: execute dynamically.
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(got) => {
                        self.error_code(
                            expr.line,
                            expr.col,
                            codes::INDEX,
                            format!("'{}' object is not subscriptable", got.name()),
                        );
                        self.force_any(expr.ty);
                    }
                }
            }
            HExprKind::Slice {
                object,
                start,
                stop,
                step,
            } => {
                self.check_expr(object, f);
                self.check_slice_bound(start, f);
                self.check_slice_bound(stop, f);
                self.check_slice_bound(step, f);
                if self.inf.is_gradual(object.ty) {
                    self.force_any(object.ty);
                    let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    return;
                }
                match self.inf.resolve(object.ty) {
                    Ty::Concrete(Type::Str) => {
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Str));
                    }
                    Ty::Concrete(Type::Bytes) => {
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Bytes));
                    }
                    Ty::Concrete(Type::List) => {
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::List));
                    }
                    Ty::Concrete(Type::Tuple) => {
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Tuple));
                    }
                    // Slicing a range yields a range (CPython semantics).
                    Ty::Concrete(Type::Range) => {
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Range));
                    }
                    Ty::Concrete(Type::Any) => {
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Var(_) => {
                        // Object type is not known yet: execute dynamically.
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(Type::Dict) => {
                        self.error_code(
                            expr.line,
                            expr.col,
                            codes::INDEX,
                            "unhashable type: 'slice'",
                        );
                        self.force_any(expr.ty);
                    }
                    Ty::Concrete(got) => {
                        self.error_code(
                            expr.line,
                            expr.col,
                            codes::INDEX,
                            format!("'{}' object is not subscriptable", got.name()),
                        );
                        self.force_any(expr.ty);
                    }
                }
            }
            HExprKind::MethodCall {
                object,
                method,
                args,
                star,
                keywords,
                double_star,
            } => self.check_method(
                expr,
                object,
                method,
                args,
                star.as_deref(),
                keywords,
                double_star.as_deref(),
                f,
            ),
            HExprKind::FunctionRef(_) => {
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Function));
            }
            HExprKind::BuiltinRef(_) => {
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Function));
            }
            // A class object is a runtime value without a static type.
            HExprKind::ExceptionClass(_) => {
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
            }
            HExprKind::ModuleCall {
                module,
                function,
                args,
                keywords,
            } => {
                for arg in args {
                    self.check_expr(arg, f);
                }
                for (_, value) in keywords {
                    self.check_expr(value, f);
                }
                match stdlib::function(module, function) {
                    Some(spec) => {
                        let supplied = args.len()
                            + if spec.dynamic_keywords {
                                0
                            } else {
                                keywords.len()
                            };
                        if args.len() < spec.min_args || supplied > spec.max_args {
                            let expected = if spec.min_args == spec.max_args {
                                spec.min_args.to_string()
                            } else if spec.max_args == usize::MAX {
                                format!("at least {}", spec.min_args)
                            } else {
                                format!("{} to {}", spec.min_args, spec.max_args)
                            };
                            self.error_code(
                                expr.line,
                                expr.col,
                                codes::CALL,
                                format!(
                                    "{}.{}() takes {} argument(s) ({} given)",
                                    module, function, expected, supplied
                                ),
                            );
                        }
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(spec.ret));
                    }
                    None => self.force_any(expr.ty),
                }
            }
            HExprKind::ModuleAttr { module, name } => {
                if stdlib::function(module, name).is_some() {
                    let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Function));
                } else if let Some(constant) = stdlib::constant(module, name) {
                    let _ = self.inf.unify(expr.ty, Ty::Concrete(constant.ty));
                } else {
                    self.force_any(expr.ty);
                }
            }
            HExprKind::ModuleRef { .. } | HExprKind::ExtensionModuleRef { .. } => {
                // A module value has no static type; attributes read from it
                // are resolved at runtime.
                self.force_any(expr.ty);
            }
            HExprKind::GlobalGet { .. } => {
                // Module-level values are dynamically typed at the read site.
                self.force_any(expr.ty);
            }
            HExprKind::DeferredImport { .. } => {
                // The value only exists to raise at runtime; it is dynamic and
                // may be used as a callable or as a value.
                self.force_any(expr.ty);
            }
            HExprKind::ExtensionAttr { .. } => self.force_any(expr.ty),
            HExprKind::GetAttr { object, .. } => {
                // Attributes are resolved at runtime by receiver kind, so the
                // result is always dynamic. The receiver still gets checked.
                self.check_expr(object, f);
                self.force_any(expr.ty);
            }
            HExprKind::IndirectCall { callee, args, star } => {
                self.check_expr(callee, f);
                for arg in args {
                    self.check_expr(arg, f);
                }
                if let Some(star) = star {
                    self.check_expr(star, f);
                }
                if self.inf.is_gradual(callee.ty) {
                    self.force_any(callee.ty);
                }
                match self.inf.resolve(callee.ty) {
                    Ty::Concrete(Type::Function) | Ty::Concrete(Type::Any) => {}
                    Ty::Var(_) => {
                        // The value is only known at runtime: execute
                        // dynamically instead of guessing a signature.
                        let _ = self.inf.unify(callee.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(got) => self.error_code(
                        expr.line,
                        expr.col,
                        codes::CALL,
                        format!("'{}' object is not callable", got.name()),
                    ),
                }
                self.force_any(expr.ty);
            }
            HExprKind::Call { callee, args } => self.check_call(expr, callee, args, f),
            HExprKind::CallKw {
                callee,
                args,
                keywords,
            } => {
                self.check_expr(callee, f);
                for arg in args {
                    self.check_expr(arg, f);
                }
                for (_, value) in keywords {
                    self.check_expr(value, f);
                }
                // A call through the function value of a known function still
                // binds the same signature: a required parameter is satisfied
                // by a positional argument or by a keyword of its name.
                let arity = match &callee.kind {
                    HExprKind::FunctionRef(id) => {
                        self.prog.functions.get(*id as usize).map(CallArity::of)
                    }
                    _ => None,
                };
                if let Some(arity) = arity {
                    let names = keywords
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>();
                    if let Some(message) = arity.keyword_error(args.len(), &names) {
                        self.error_code(expr.line, expr.col, codes::CALL, message);
                    }
                }
                self.force_any(expr.ty);
            }
            HExprKind::CallFull {
                callee,
                args,
                star,
                keywords,
                double_star,
            } => {
                self.check_expr(callee, f);
                for arg in args {
                    self.check_expr(arg, f);
                }
                if let Some(star) = star {
                    self.check_expr(star, f);
                }
                for (_, value) in keywords {
                    self.check_expr(value, f);
                }
                if let Some(mapping) = double_star {
                    self.check_expr(mapping, f);
                    // `**mapping` must be a dict; a runtime value that cannot
                    // be a mapping is rejected here when its type is known.
                    if self.inf.is_gradual(mapping.ty) {
                        self.force_any(mapping.ty);
                    } else if let Ty::Concrete(got) = self.inf.resolve(mapping.ty) {
                        if !matches!(got, Type::Dict | Type::Any) {
                            self.error_code(
                                mapping.line,
                                mapping.col,
                                codes::CALL,
                                format!(
                                    "argument after ** must be a mapping, not '{}'",
                                    got.name()
                                ),
                            );
                        }
                    }
                }
                if self.inf.is_gradual(callee.ty) {
                    self.force_any(callee.ty);
                }
                match self.inf.resolve(callee.ty) {
                    Ty::Concrete(Type::Function) | Ty::Concrete(Type::Any) => {}
                    Ty::Var(_) => {
                        let _ = self.inf.unify(callee.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(got) => self.error_code(
                        expr.line,
                        expr.col,
                        codes::CALL,
                        format!("'{}' object is not callable", got.name()),
                    ),
                }
                self.force_any(expr.ty);
            }
            HExprKind::ClassCall { class, args, .. } => {
                self.check_expr(class, f);
                for arg in args {
                    self.check_expr(arg, f);
                }
                // The class constructor's result has no static type.
                self.force_any(expr.ty);
            }
            HExprKind::DeferredCall { callee, args } => {
                // Operands are still type-checked; the call itself only fails
                // when it executes.
                if let Some(callee) = callee {
                    self.check_expr(callee, f);
                }
                for arg in args {
                    self.check_expr(arg, f);
                }
                self.force_any(expr.ty);
            }
            HExprKind::Await { value } => {
                self.check_expr(value, f);
                // The awaited result is only known at runtime.
                self.force_any(expr.ty);
            }
            HExprKind::Yield { value } => {
                if let Some(value) = value {
                    self.check_expr(value, f);
                }
                // The value sent on the next resume is only known at runtime.
                self.force_any(expr.ty);
            }
            HExprKind::YieldFrom { value } => {
                self.check_expr(value, f);
                // The delegate's return value is only known at runtime.
                self.force_any(expr.ty);
            }
            HExprKind::Template { interpolations, .. } => {
                // A template is not a `str`; its static type is dynamic.
                // Interpolation values and format specs are ordinary
                // expressions and keep their own checks.
                for interpolation in interpolations {
                    self.check_expr(&interpolation.value, f);
                    if let Some(spec) = &interpolation.format_spec {
                        self.check_expr(spec, f);
                    }
                }
                self.force_any(expr.ty);
            }
        }
    }

    fn check_call(&mut self, expr: &HExpr, callee: &Callee, args: &[HExpr], f: &HFunction) {
        for arg in args {
            self.check_expr(arg, f);
        }
        match callee {
            Callee::Function(id) => {
                let id = *id;
                let (arity, params, ret_ty, is_coroutine) = {
                    let function = &self.prog.functions[id as usize];
                    let captures = function.capture_count.min(function.params.len());
                    (
                        CallArity::of(function),
                        function.params[captures..]
                            .iter()
                            .map(|p| (p.ty, p.declared))
                            .collect::<Vec<_>>(),
                        function.ret_ty,
                        function.is_async || function.is_generator,
                    )
                };
                let name = &arity.name;
                if let Some(message) = arity.positional_error(args.len()) {
                    self.error_code(expr.line, expr.col, codes::CALL, message);
                }
                for (index, arg) in args.iter().enumerate() {
                    if let Some(&(param_ty, declared)) = params.get(index) {
                        match param_ty {
                            Ty::Concrete(expected) => match self.inf.resolve(arg.ty) {
                                Ty::Concrete(got) => {
                                    if !annotation_accepts(expected, got) {
                                        if declared {
                                            self.error(
                                                arg.line,
                                                arg.col,
                                                format!(
                                                    "argument {} of {} has type {}, expected {}",
                                                    index + 1,
                                                    name,
                                                    got.name(),
                                                    expected.name()
                                                ),
                                            );
                                        } else {
                                            // An unannotated parameter has no
                                            // declared contract: the argument
                                            // and any conflicting body use widen
                                            // it to `Any`, and the runtime
                                            // decides.
                                            self.force_any(param_ty);
                                        }
                                    }
                                }
                                Ty::Var(_) => {
                                    if declared {
                                        let _ = self.inf.unify(arg.ty, Ty::Concrete(expected));
                                    } else if expected != Type::Any {
                                        // An argument that is still unresolved
                                        // must not be pinned to the argument
                                        // type of another call site.
                                        self.force_any(param_ty);
                                    }
                                }
                            },
                            Ty::Var(_) => {
                                match (self.inf.resolve(param_ty), self.inf.resolve(arg.ty)) {
                                    (Ty::Concrete(_), Ty::Concrete(_)) => {
                                        if self.inf.unify(param_ty, arg.ty).is_err() {
                                            // Call sites supply different types for
                                            // one unannotated parameter: the
                                            // parameter is dynamic.
                                            self.force_any(param_ty);
                                        }
                                    }
                                    (Ty::Concrete(expected), Ty::Var(_)) => {
                                        if expected != Type::Any {
                                            // The argument is not resolved yet.
                                            // Do not pin it to a type another call
                                            // site supplied (e.g. a `None` from an
                                            // omitted default); the parameter is
                                            // dynamic instead.
                                            self.force_any(param_ty);
                                        } else {
                                            let _ = self.inf.unify(param_ty, arg.ty);
                                        }
                                    }
                                    (Ty::Var(_), Ty::Concrete(Type::None)) => {
                                        // A `None` (often an omitted default)
                                        // does not type an unannotated
                                        // parameter; its own uses still infer it
                                        // (`depth = depth - 1` makes it an int).
                                    }
                                    _ => {
                                        let _ = self.inf.unify(param_ty, arg.ty);
                                    }
                                }
                            }
                        }
                    }
                }
                // Calling an async or generator function returns a coroutine or
                // generator object whose element/result type is only known at
                // runtime.
                let result_ty = if is_coroutine {
                    Ty::Concrete(Type::Any)
                } else {
                    ret_ty
                };
                let _ = self.inf.unify(expr.ty, result_ty);
            }
            Callee::Builtin(builtin) => {
                let builtin = *builtin;
                // `range(...)` has a concrete result type, so callers can
                // type-check against it eagerly; argument validation stays
                // deferred.
                if builtin == Builtin::Range {
                    let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Range));
                }
                self.checks.push(Check::Builtin {
                    builtin,
                    args: args.iter().map(|a| (a.ty, a.line, a.col)).collect(),
                    out: expr.ty,
                    line: expr.line,
                    col: expr.col,
                });
            }
            // A limited-API extension exposes no static types: the call is
            // checked at runtime and returns a dynamically typed value.
            Callee::Extension { .. } => self.force_any(expr.ty),
        }
    }

    /// Resolve `object.method(args)` against the known method matrix.
    ///
    /// Receivers whose type is not statically known (or `Any`) are executed
    /// dynamically: every method name and arity is accepted and the result is
    /// `Any`, leaving the checks to the runtime.
    ///
    /// A receiver with a concrete builtin type only reports a missing method
    /// when it is written out literally. Values held in locals, parameters,
    /// closure cells, globals or any derived expression keep the runtime's
    /// class resolution (see [`is_gradual_receiver`]), so their result type is
    /// forced to `Any` and the runtime reports a genuine `AttributeError`.
    #[allow(clippy::too_many_arguments)]
    fn check_method(
        &mut self,
        expr: &HExpr,
        object: &HExpr,
        method: &str,
        args: &[HExpr],
        star: Option<&HExpr>,
        keywords: &[(String, HExpr)],
        double_star: Option<&HExpr>,
        f: &HFunction,
    ) {
        self.check_expr(object, f);
        for arg in args {
            self.check_expr(arg, f);
        }
        if let Some(star) = star {
            self.check_expr(star, f);
        }
        for (_, value) in keywords {
            self.check_expr(value, f);
        }
        let mut dynamic_keywords = false;
        if let Some(mapping) = double_star {
            self.check_expr(mapping, f);
            if self.inf.is_gradual(mapping.ty) {
                self.force_any(mapping.ty);
            } else if let Ty::Concrete(got) = self.inf.resolve(mapping.ty) {
                if !matches!(got, Type::Dict | Type::Any) {
                    self.error_code(
                        mapping.line,
                        mapping.col,
                        codes::CALL,
                        format!("argument after ** must be a mapping, not '{}'", got.name()),
                    );
                }
            }
            // A dynamic mapping supplies keywords with names unknown here.
            dynamic_keywords = true;
        }

        // A builtin class value is a function at runtime, but
        // `int.from_bytes(...)` and friends are class-level methods with
        // their own shapes. Recognised methods keep a concrete result type;
        // unknown class-level methods dispatch dynamically.
        if let HExprKind::BuiltinRef(builtin) = &object.kind {
            if is_builtin_class_value(*builtin) {
                match builtin_class_method_result(*builtin, method) {
                    Some(result) => {
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(result));
                    }
                    None => self.force_any(expr.ty),
                }
                return;
            }
        }

        // An unannotated parameter is gradual: a call site may have narrowed
        // it to a builtin type, but the body's method call is valid Python
        // (the argument may be a subclass or another value at runtime).
        if self.inf.is_gradual(object.ty) {
            self.force_any(object.ty);
            self.force_any(expr.ty);
            return;
        }
        let result = match self.inf.resolve(object.ty) {
            Ty::Var(_) | Ty::Concrete(Type::Any) => Type::Any,
            Ty::Concrete(receiver) => {
                let table = match receiver {
                    Type::Str => STR_METHODS,
                    Type::Bytes => BYTES_METHODS,
                    Type::List => LIST_METHODS,
                    Type::Dict => DICT_METHODS,
                    Type::Tuple => TUPLE_METHODS,
                    Type::Range => RANGE_METHODS,
                    Type::Set => SET_METHODS,
                    _ => {
                        self.error_no_method(expr, receiver, method, None);
                        self.force_any(expr.ty);
                        return;
                    }
                };
                let Some(entry) = table.iter().find(|entry| entry.0 == method) else {
                    if is_gradual_receiver(&object.kind) {
                        self.force_any(expr.ty);
                        return;
                    }
                    self.error_no_method(expr, receiver, method, Some(table));
                    self.force_any(expr.ty);
                    return;
                };
                let signature = &entry.1;

                // A starred operand expands to an unknown number of
                // positional arguments at runtime, so only the static
                // positional count is checked here. Keywords fill the
                // remaining slots at runtime; the runtime binder diagnoses
                // unknown and duplicate keywords.
                let arity_ok = if star.is_some() || dynamic_keywords {
                    args.len() <= signature.max_args
                } else if keywords.is_empty() {
                    args.len() >= signature.min_args && args.len() <= signature.max_args
                } else {
                    args.len() <= signature.max_args
                };
                if !arity_ok {
                    let expected = if signature.min_args == signature.max_args {
                        signature.min_args.to_string()
                    } else if signature.max_args == usize::MAX {
                        format!("at least {}", signature.min_args)
                    } else {
                        format!("{} to {}", signature.min_args, signature.max_args)
                    };
                    self.error_code(
                        expr.line,
                        expr.col,
                        codes::CALL,
                        format!(
                            "{}.{}() takes {} positional argument(s) ({} given)",
                            receiver.name(),
                            method,
                            expected,
                            args.len()
                        ),
                    );
                } else {
                    for (index, arg) in args.iter().enumerate() {
                        let Some(kind) = signature.arg_kinds.get(index) else {
                            continue;
                        };
                        self.check_method_arg(receiver, method, index, *kind, arg);
                    }
                }
                signature.result
            }
        };

        if result == Type::Any {
            self.force_any(expr.ty);
        } else {
            let _ = self.inf.unify(expr.ty, Ty::Concrete(result));
        }
    }

    fn check_method_arg(
        &mut self,
        receiver: Type,
        method: &str,
        index: usize,
        kind: ArgKind,
        arg: &HExpr,
    ) {
        match kind {
            ArgKind::Any => return,
            ArgKind::Hashable => {
                // Sets/dicts hash every element; tuples are walked recursively
                // so nested containers get the precise diagnostic.
                self.check_hashable(arg);
                return;
            }
            _ => {}
        }
        let expected = kind.expected_name();
        match self.inf.resolve(arg.ty) {
            Ty::Var(_) => {
                if let Some(bound) = kind.bound_type() {
                    let _ = self.inf.unify(arg.ty, Ty::Concrete(bound));
                }
            }
            Ty::Concrete(got) => {
                if !kind.accepts(got) {
                    if self.inf.is_gradual(arg.ty) {
                        self.force_any(arg.ty);
                    } else {
                        self.error(
                            arg.line,
                            arg.col,
                            format!(
                                "{}.{}() argument {} has wrong type: expected {}, got {}",
                                receiver.name(),
                                method,
                                index + 1,
                                expected,
                                got.name()
                            ),
                        );
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Constraint solving
    // ------------------------------------------------------------------

    fn bind_type(&mut self, out: Ty, ty: Type, line: u32, col: u32) {
        if self.inf.is_gradual(out) {
            // An unannotated parameter used as an augmented-assignment target
            // stays dynamic instead of pinning a result type.
            self.force_any(out);
            return;
        }
        if self.inf.unify(out, Ty::Concrete(ty)).is_err() {
            let existing = self.inf.name_of(out);
            if existing == "None" {
                // An output slot previously constrained to `None` (usually
                // through an implicit/explicit return) stays dynamic.
                self.force_any(out);
                return;
            }
            // Boolean operands are routinely used as truth tests for dynamic
            // or container-valued expressions; do not pin those slots to the
            // first branch's static type.
            if ty == Type::Bool
                && matches!(existing.as_str(), "dict" | "list" | "tuple" | "set" | "str")
            {
                self.force_any(out);
                return;
            }
            self.error(
                line,
                col,
                format!(
                    "type mismatch: {} result used where {} is expected",
                    ty.name(),
                    existing
                ),
            );
        }
    }

    fn force_any(&mut self, ty: Ty) {
        let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
    }

    /// Widen a gradual (unannotated-parameter) slot to `Any`, returning
    /// whether it was gradual.
    fn widen_gradual(&mut self, ty: Ty) -> bool {
        if self.inf.is_gradual(ty) {
            self.force_any(ty);
            true
        } else {
            false
        }
    }

    /// Validate `left in right` for concrete operand types, reporting the
    /// Python-style diagnostic when the right operand cannot be iterated or
    /// the left operand is not hashable/str where required.
    fn membership_result(&mut self, left: Type, right: Type, line: u32, col: u32) -> Option<Type> {
        match right {
            Type::Any | Type::List | Type::Tuple => Some(Type::Bool),
            // Python returns False for non-int values and True for integral
            // floats; the runtime implements that, so any left type is valid.
            Type::Range => Some(Type::Bool),
            Type::Str => {
                if matches!(left, Type::Str | Type::Any | Type::Never) {
                    Some(Type::Bool)
                } else {
                    self.error(
                        line,
                        col,
                        format!(
                            "'in <str>' requires string left operand, not {}",
                            left.name()
                        ),
                    );
                    None
                }
            }
            Type::Bytes => {
                if matches!(
                    left,
                    Type::Bytes | Type::Int | Type::Bool | Type::Any | Type::Never
                ) {
                    Some(Type::Bool)
                } else {
                    self.error(
                        line,
                        col,
                        format!(
                            "'in <bytes>' requires bytes or int left operand, not {}",
                            left.name()
                        ),
                    );
                    None
                }
            }
            Type::Dict | Type::Set => {
                if matches!(left, Type::List | Type::Dict | Type::Set) {
                    self.error(line, col, format!("unhashable type: '{}'", left.name()));
                    None
                } else {
                    Some(Type::Bool)
                }
            }
            other => {
                self.error(
                    line,
                    col,
                    format!("'{}' object is not iterable", other.name()),
                );
                None
            }
        }
    }

    fn unsupported(&mut self, op: BinOp, a: Type, b: Type, line: u32, col: u32) {
        self.error(
            line,
            col,
            format!(
                "unsupported operand type(s) for {}: '{}' and '{}'",
                op.symbol(),
                a.name(),
                b.name()
            ),
        );
    }

    fn apply_builtin_checks(&mut self) {
        let checks = std::mem::take(&mut self.checks);
        let mut keep = Vec::new();
        for check in checks {
            match check {
                Check::Builtin {
                    builtin,
                    args,
                    out,
                    line,
                    col,
                } => self.apply_builtin(builtin, args, out, line, col),
                other @ Check::RaiseMsg { .. } => keep.push(other),
            }
        }
        self.checks = keep;
    }

    fn apply_builtin(
        &mut self,
        builtin: Builtin,
        args: Vec<(Ty, u32, u32)>,
        out: Ty,
        line: u32,
        col: u32,
    ) {
        // An unannotated parameter is gradual. A builtin argument that
        // happens to be constrained by a call site (e.g. `len(parameters)`
        // where a caller passed a `bool`) is not a static error: the slot
        // widens to `Any` and the runtime validates the actual value.
        let mut args = args;
        for (ty, _, _) in &mut args {
            if self.inf.is_gradual(*ty) {
                self.force_any(*ty);
                *ty = Ty::Concrete(Type::Any);
            }
        }
        let count = args.len();
        match builtin {
            Builtin::Print => {
                let _ = self.inf.unify(out, Ty::Concrete(Type::None));
            }
            Builtin::Len => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("len() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                match self.inf.resolve(ty) {
                    // An unresolved argument may still be constrained later
                    // (e.g. `x = a - b; len(x)`). Unifying with `Any` keeps
                    // the constraint open instead of forcing `str`.
                    Ty::Var(_) => {
                        let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(Type::Str)
                    | Ty::Concrete(Type::Bytes)
                    | Ty::Concrete(Type::List)
                    | Ty::Concrete(Type::Dict)
                    | Ty::Concrete(Type::Tuple)
                    | Ty::Concrete(Type::Set)
                    | Ty::Concrete(Type::Range)
                    | Ty::Concrete(Type::Any) => {}
                    Ty::Concrete(t) => self.error(
                        arg_line,
                        arg_col,
                        format!(
                            "len() argument must be str, bytes, list, dict, tuple, set or range, not {}",
                            t.name()
                        ),
                    ),
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Int));
            }
            Builtin::Str => {
                // `str()`, `str(object)` and
                // `str(bytes_like, encoding[, errors])`.
                if count > 3 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("str() takes at most 3 arguments ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Str));
            }
            Builtin::Repr => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("repr() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Str));
            }
            Builtin::Ascii => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("ascii() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Str));
            }
            Builtin::Int | Builtin::Float => {
                let target = if builtin == Builtin::Int {
                    Type::Int
                } else {
                    Type::Float
                };
                let allowed = if builtin == Builtin::Int {
                    0..=2
                } else {
                    1..=1
                };
                if !allowed.contains(&count) {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!(
                            "{}() takes {} argument(s) ({} given)",
                            builtin.name(),
                            if builtin == Builtin::Int {
                                "0 to 2"
                            } else {
                                "exactly 1"
                            },
                            count
                        ),
                    );
                    return;
                }
                if args.is_empty() {
                    let _ = self.inf.unify(out, Ty::Concrete(target));
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                // `int(x)` and `float(x)` accept any object: CPython dispatches
                // to `__int__`/`__index__`/`__float__`, which user classes (and
                // subclasses of builtins) may define, so a concrete argument
                // type is never a static rejection. Non-convertible values fail
                // at runtime with a TypeError.
                //
                // The two-argument form `int(string, base)` is the exception:
                // CPython only accepts string-like values there, so a
                // statically known non-string argument is an error.
                if builtin == Builtin::Int && count == 2 {
                    if let Ty::Concrete(t) = self.inf.resolve(ty) {
                        // `Never` is uninhabited, so it cannot fail at runtime.
                        let string_like =
                            matches!(t, Type::Any | Type::Str | Type::Bytes | Type::Never);
                        if !string_like {
                            self.error(
                                arg_line,
                                arg_col,
                                "int() can't convert non-string with explicit base".to_string(),
                            );
                        }
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(target));
            }
            Builtin::Import => {
                if count == 0 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        " __import__() missing required argument 'name'".to_string(),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::Reversed => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("reversed() takes exactly 1 argument ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::List));
            }
            Builtin::Type => {
                // `type(x)` reports an object's type; `type(name, bases, ns)`
                // creates a class and may forward class-header keywords to the
                // metaclass (checked by the runtime).
                if count != 1 && count != 3 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("type() takes 1 or 3 arguments ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::Super => {
                // `super(type, object)` is the explicit proxy form; the
                // zero-argument form is lowered by the frontend before it
                // reaches this builtin. `super(type)` is the unbound proxy.
                if count > 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("super() takes at most 2 arguments ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::Bool => {
                if count > 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("bool() takes at most 1 argument ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Bool));
            }
            Builtin::Abs => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("abs() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                match self.inf.resolve(ty) {
                    Ty::Var(_) => {
                        let _ = self.inf.unify(out, ty);
                    }
                    Ty::Concrete(Type::Any) => self.force_any(out),
                    Ty::Concrete(Type::Int) | Ty::Concrete(Type::Bool) => {
                        let _ = self.inf.unify(out, Ty::Concrete(Type::Int));
                    }
                    Ty::Concrete(Type::BigInt) => {
                        let _ = self.inf.unify(out, Ty::Concrete(Type::BigInt));
                    }
                    Ty::Concrete(Type::Float) => {
                        let _ = self.inf.unify(out, Ty::Concrete(Type::Float));
                    }
                    Ty::Concrete(t) => self.error(
                        arg_line,
                        arg_col,
                        format!("abs() argument must be a number, not {}", t.name()),
                    ),
                }
            }
            Builtin::Set => {
                if count > 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("set() takes 0 to 1 positional argument(s) ({count} given)"),
                    );
                    return;
                }
                if count == 1 {
                    let (ty, arg_line, arg_col) = args[0];
                    // Any iterable is accepted; the element type is not
                    // tracked, so only a statically known non-iterable
                    // (`int`) is rejected.
                    self.check_builtin_iterable(Builtin::Set, ty, arg_line, arg_col);
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Set));
            }
            Builtin::Format => {
                // Source-level `format(value, spec)` and the internal f-string
                // field (value, spec, conversion flag) share this builtin.
                // The spec is a `str` in either case: a static literal for the
                // common path, a runtime expression when the f-string spec has
                // nested replacement fields.
                if !(2..=3).contains(&count) {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("format() takes 2 arguments ({count} given)"),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[1];
                match self.inf.resolve(ty) {
                    Ty::Var(_) => {
                        let _ = self.inf.unify(ty, Ty::Concrete(Type::Str));
                    }
                    Ty::Concrete(Type::Str)
                    | Ty::Concrete(Type::Any)
                    | Ty::Concrete(Type::Never) => {}
                    Ty::Concrete(t) => self.error(
                        arg_line,
                        arg_col,
                        format!("format() argument 2 must be str, not {}", t.name()),
                    ),
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Str));
            }
            Builtin::Max | Builtin::Min => {
                if count == 0 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("{}() expects at least 1 argument (0 given)", builtin.name()),
                    );
                    return;
                }
                // The extremum keeps the element type only at runtime.
                self.force_any(out);
            }
            Builtin::Sorted => {
                // The frontend materialises the `key` and `reverse` slots, so
                // a direct call carries two or three arguments; hand-built IR
                // may pass the iterable alone.
                if count == 0 || count > 3 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("sorted() takes 1 to 3 positional arguments ({count} given)"),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                self.check_builtin_iterable(Builtin::Sorted, ty, arg_line, arg_col);
                if let Some((ty, arg_line, arg_col)) = args.get(1) {
                    match self.inf.resolve(*ty) {
                        Ty::Var(_) => {
                            let _ = self.inf.unify(*ty, Ty::Concrete(Type::Any));
                        }
                        Ty::Concrete(Type::Function)
                        | Ty::Concrete(Type::None)
                        | Ty::Concrete(Type::Any)
                        | Ty::Concrete(Type::Never) => {}
                        Ty::Concrete(t) => self.error(
                            *arg_line,
                            *arg_col,
                            format!("sorted() key must be callable or None, not {}", t.name()),
                        ),
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::List));
            }
            Builtin::Round => {
                if count == 0 || count > 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("round() takes 1 or 2 arguments ({count} given)"),
                    );
                    return;
                }
                if let Some((ty, arg_line, arg_col)) = args.get(1) {
                    match self.inf.resolve(*ty) {
                        Ty::Var(_) => {
                            let _ = self.inf.unify(*ty, Ty::Concrete(Type::Int));
                        }
                        Ty::Concrete(Type::Int)
                        | Ty::Concrete(Type::BigInt)
                        | Ty::Concrete(Type::Bool)
                        | Ty::Concrete(Type::Any)
                        | Ty::Concrete(Type::Never) => {}
                        Ty::Concrete(Type::None) => {}
                        Ty::Concrete(t) => self.error(
                            *arg_line,
                            *arg_col,
                            format!("round() ndigits must be int, not {}", t.name()),
                        ),
                    }
                }
                // `round(x)` returns int, `round(x, n)` a float; the static
                // type depends on the argument count and operand type.
                self.force_any(out);
            }
            Builtin::Isinstance => {
                if count != 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("isinstance() takes exactly 2 arguments ({count} given)"),
                    );
                    return;
                }
                // The target type is validated at runtime.
                let _ = self.inf.unify(out, Ty::Concrete(Type::Bool));
            }
            Builtin::Issubclass => {
                if count != 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("issubclass() takes exactly 2 arguments ({count} given)"),
                    );
                    return;
                }
                // Both arguments are classes (or the second a tuple of
                // classes); the runtime validates them and reports
                // `TypeError` for anything else.
                let _ = self.inf.unify(out, Ty::Concrete(Type::Bool));
            }
            Builtin::Bytes => {
                if count > 3 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("bytes() takes 0 to 3 positional arguments ({count} given)"),
                    );
                    return;
                }
                if count == 1 {
                    let (ty, arg_line, arg_col) = args[0];
                    match self.inf.resolve(ty) {
                        Ty::Var(_) => {
                            let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
                        }
                        Ty::Concrete(Type::Int)
                        | Ty::Concrete(Type::BigInt)
                        | Ty::Concrete(Type::Bool)
                        | Ty::Concrete(Type::Str)
                        | Ty::Concrete(Type::List)
                        | Ty::Concrete(Type::Tuple)
                        | Ty::Concrete(Type::Set)
                        | Ty::Concrete(Type::Dict)
                        | Ty::Concrete(Type::Range)
                        | Ty::Concrete(Type::Any)
                        | Ty::Concrete(Type::Never) => {}
                        Ty::Concrete(t) => self.error(
                            arg_line,
                            arg_col,
                            format!(
                                "bytes() argument must be an int, str or iterable of ints, not {}",
                                t.name()
                            ),
                        ),
                    }
                } else if count >= 2 {
                    // `bytes(source, encoding[, errors])`: the source, the
                    // encoding and the error-handler name are all strings.
                    for index in 0..count {
                        let (ty, arg_line, arg_col) = args[index];
                        match self.inf.resolve(ty) {
                            Ty::Var(_) | Ty::Concrete(Type::Any) | Ty::Concrete(Type::Never) => {}
                            Ty::Concrete(Type::Str) => {}
                            Ty::Concrete(t) => self.error(
                                arg_line,
                                arg_col,
                                format!(
                                    "bytes() argument {} must be str, not {}",
                                    index + 1,
                                    t.name()
                                ),
                            ),
                        }
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Bytes));
            }
            Builtin::Iter => {
                if count == 0 || count > 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("iter() takes 1 or 2 positional arguments ({count} given)"),
                    );
                    return;
                }
                if count == 1 {
                    let (ty, arg_line, arg_col) = args[0];
                    self.check_builtin_iterable(Builtin::Iter, ty, arg_line, arg_col);
                }
                // The element type is only known at runtime. With the
                // two-argument form the callable's return type is not
                // statically constrained either.
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::Next => {
                if count == 0 || count > 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("next() takes 1 or 2 positional arguments ({count} given)"),
                    );
                    return;
                }
                // Any object is accepted statically: whether it is an
                // iterator is only known at runtime (`next([])` is a
                // runtime `TypeError`, mirroring CPython).
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::Frozenset => {
                if count > 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("frozenset() takes 0 to 1 positional argument(s) ({count} given)"),
                    );
                    return;
                }
                if count == 1 {
                    let (ty, arg_line, arg_col) = args[0];
                    // Same iterable rule as `set()`; a `range` is accepted.
                    self.check_builtin_iterable(Builtin::Frozenset, ty, arg_line, arg_col);
                }
                // Hydro has one immutable set representation.
                let _ = self.inf.unify(out, Ty::Concrete(Type::Set));
            }
            Builtin::Getattr => {
                if count != 2 && count != 3 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("getattr() takes 2 or 3 arguments ({count} given)"),
                    );
                    return;
                }
                // The attribute name is resolved at runtime; a missing
                // attribute raises `AttributeError`.
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::Hasattr => {
                if count != 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("hasattr() takes exactly 2 arguments ({count} given)"),
                    );
                    return;
                }
                let (name_ty, name_line, name_col) = args[1];
                match self.inf.resolve(name_ty) {
                    Ty::Var(_) => {
                        let _ = self.inf.unify(name_ty, Ty::Concrete(Type::Str));
                    }
                    Ty::Concrete(Type::Str) | Ty::Concrete(Type::Any) => {}
                    Ty::Concrete(got) => self.error(
                        name_line,
                        name_col,
                        format!("attribute name must be string, not '{}'", got.name()),
                    ),
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Bool));
            }
            Builtin::Range => {
                if count == 0 || count > 3 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("range() takes 1 to 3 positional arguments ({count} given)"),
                    );
                    return;
                }
                let names: &[&str] = if count == 1 {
                    &["stop"]
                } else if count == 2 {
                    &["start", "stop"]
                } else {
                    &["start", "stop", "step"]
                };
                for (name, (ty, arg_line, arg_col)) in names.iter().zip(args.iter()) {
                    match self.inf.resolve(*ty) {
                        Ty::Concrete(Type::Int)
                        | Ty::Concrete(Type::BigInt)
                        | Ty::Concrete(Type::Bool)
                        | Ty::Concrete(Type::Any) => {}
                        Ty::Var(_) => {
                            let _ = self.inf.unify(*ty, Ty::Concrete(Type::Int));
                        }
                        Ty::Concrete(got) => self.error(
                            *arg_line,
                            *arg_col,
                            format!(
                                "range() argument '{}' has type {}, expected int",
                                name,
                                got.name()
                            ),
                        ),
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Range));
            }
            Builtin::List => {
                if count > 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("list() takes 0 to 1 positional argument(s) ({count} given)"),
                    );
                    return;
                }
                if count == 1 {
                    let (ty, arg_line, arg_col) = args[0];
                    self.check_builtin_iterable(Builtin::List, ty, arg_line, arg_col);
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::List));
            }
            Builtin::Tuple => {
                if count > 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("tuple() takes 0 to 1 positional argument(s) ({count} given)"),
                    );
                    return;
                }
                if count == 1 {
                    let (ty, arg_line, arg_col) = args[0];
                    self.check_builtin_iterable(Builtin::Tuple, ty, arg_line, arg_col);
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Tuple));
            }
            Builtin::Dict => {
                if count > 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("dict() takes 0 to 1 positional argument(s) ({count} given)"),
                    );
                    return;
                }
                if count == 1 {
                    let (ty, arg_line, arg_col) = args[0];
                    match self.inf.resolve(ty) {
                        Ty::Var(_) => {
                            let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
                        }
                        Ty::Concrete(Type::Dict)
                        | Ty::Concrete(Type::List)
                        | Ty::Concrete(Type::Tuple)
                        | Ty::Concrete(Type::Bytes)
                        | Ty::Concrete(Type::Any)
                        | Ty::Concrete(Type::Never) => {}
                        Ty::Concrete(_) => {
                            self.error(arg_line, arg_col, "dict() argument must be iterable")
                        }
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Dict));
            }
            Builtin::Sum => {
                if count == 0 || count > 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("sum() takes 1 or 2 positional arguments ({count} given)"),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                let mut range_input = false;
                match self.inf.resolve(ty) {
                    Ty::Var(_) => {
                        let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(Type::List)
                    | Ty::Concrete(Type::Tuple)
                    | Ty::Concrete(Type::Set)
                    | Ty::Concrete(Type::Bytes)
                    | Ty::Concrete(Type::Any)
                    | Ty::Concrete(Type::Never) => {}
                    Ty::Concrete(Type::Range) => range_input = true,
                    Ty::Concrete(_) => {
                        self.error(arg_line, arg_col, "sum() argument must be iterable")
                    }
                }
                if let Some((ty, arg_line, arg_col)) = args.get(1) {
                    match self.inf.resolve(*ty) {
                        Ty::Concrete(t) if t.is_numeric() || t == Type::Any || t == Type::Never => {
                        }
                        Ty::Var(_) => {
                            let _ = self.inf.unify(*ty, Ty::Concrete(Type::Int));
                        }
                        Ty::Concrete(t) => self.error(
                            *arg_line,
                            *arg_col,
                            format!("sum() start argument must be a number, not {}", t.name()),
                        ),
                    }
                }
                let result = if range_input { Type::Int } else { Type::Any };
                let _ = self.inf.unify(out, Ty::Concrete(result));
            }
            Builtin::Enumerate => {
                if count == 0 || count > 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("enumerate() takes 1 or 2 positional arguments ({count} given)"),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                self.check_builtin_iterable(Builtin::Enumerate, ty, arg_line, arg_col);
                if let Some((ty, arg_line, arg_col)) = args.get(1) {
                    match self.inf.resolve(*ty) {
                        Ty::Concrete(Type::Int)
                        | Ty::Concrete(Type::BigInt)
                        | Ty::Concrete(Type::Bool)
                        | Ty::Concrete(Type::Any) => {}
                        Ty::Var(_) => {
                            let _ = self.inf.unify(*ty, Ty::Concrete(Type::Int));
                        }
                        Ty::Concrete(t) => self.error(
                            *arg_line,
                            *arg_col,
                            format!("enumerate() start argument must be int, not {}", t.name()),
                        ),
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::List));
            }
            Builtin::Zip => {
                for (ty, arg_line, arg_col) in args.iter() {
                    self.check_builtin_iterable(Builtin::Zip, *ty, *arg_line, *arg_col);
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::List));
            }
            Builtin::Map | Builtin::Filter => {
                let invalid = if builtin == Builtin::Map {
                    count < 2
                } else {
                    count != 2
                };
                if invalid {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!(
                            "{}() takes {} positional arguments ({count} given)",
                            builtin.name(),
                            if builtin == Builtin::Map {
                                "at least 2"
                            } else {
                                "exactly 2"
                            }
                        ),
                    );
                    let _ = self.inf.unify(out, Ty::Concrete(Type::List));
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                match self.inf.resolve(ty) {
                    Ty::Var(_) => {
                        let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(Type::Function)
                    | Ty::Concrete(Type::Any)
                    | Ty::Concrete(Type::Never) => {}
                    Ty::Concrete(Type::None) if builtin == Builtin::Filter => {}
                    Ty::Concrete(_) => {
                        let message = if builtin == Builtin::Map {
                            "map() argument 1 must be callable"
                        } else {
                            "filter() argument 1 must be callable or None"
                        };
                        self.error(arg_line, arg_col, message);
                    }
                }
                let (ty, arg_line, arg_col) = args[1];
                match self.inf.resolve(ty) {
                    Ty::Var(_) => {
                        let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(Type::List)
                    | Ty::Concrete(Type::Tuple)
                    | Ty::Concrete(Type::Set)
                    | Ty::Concrete(Type::Range)
                    | Ty::Concrete(Type::Str)
                    | Ty::Concrete(Type::Bytes)
                    | Ty::Concrete(Type::Dict)
                    | Ty::Concrete(Type::Any)
                    | Ty::Concrete(Type::Never) => {}
                    Ty::Concrete(_) => self.error(
                        arg_line,
                        arg_col,
                        format!("{}() argument 2 must be iterable", builtin.name()),
                    ),
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::List));
            }
            Builtin::AnyValue => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("any() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Bool));
            }
            Builtin::All => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("all() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                self.check_builtin_iterable(builtin, ty, arg_line, arg_col);
                let _ = self.inf.unify(out, Ty::Concrete(Type::Bool));
            }
            Builtin::Ord => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("ord() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                match self.inf.resolve(ty) {
                    Ty::Var(_) => {
                        let _ = self.inf.unify(ty, Ty::Concrete(Type::Str));
                    }
                    Ty::Concrete(Type::Str)
                    | Ty::Concrete(Type::Any)
                    | Ty::Concrete(Type::Never) => {}
                    Ty::Concrete(t) => self.error(
                        arg_line,
                        arg_col,
                        format!("ord() argument must be str, not {}", t.name()),
                    ),
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Int));
            }
            Builtin::Chr => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("chr() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                match self.inf.resolve(ty) {
                    Ty::Var(_) => {
                        let _ = self.inf.unify(ty, Ty::Concrete(Type::Int));
                    }
                    Ty::Concrete(Type::Int)
                    | Ty::Concrete(Type::BigInt)
                    | Ty::Concrete(Type::Bool)
                    | Ty::Concrete(Type::Any)
                    | Ty::Concrete(Type::Never) => {}
                    Ty::Concrete(t) => self.error(
                        arg_line,
                        arg_col,
                        format!("chr() argument must be an int, not {}", t.name()),
                    ),
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Str));
            }
            Builtin::Hex | Builtin::Bin | Builtin::Oct => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!(
                            "{}() takes exactly one argument ({count} given)",
                            builtin.name()
                        ),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                match self.inf.resolve(ty) {
                    Ty::Var(_) => {
                        let _ = self.inf.unify(ty, Ty::Concrete(Type::Int));
                    }
                    Ty::Concrete(Type::Int)
                    | Ty::Concrete(Type::BigInt)
                    | Ty::Concrete(Type::Bool)
                    | Ty::Concrete(Type::Any)
                    | Ty::Concrete(Type::Never) => {}
                    Ty::Concrete(t) => self.error(
                        arg_line,
                        arg_col,
                        format!(
                            "{}() argument must be an int, not {}",
                            builtin.name(),
                            t.name()
                        ),
                    ),
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Str));
            }
            Builtin::Divmod => {
                if count != 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("divmod() takes exactly two arguments ({count} given)"),
                    );
                    return;
                }
                // Both operands are checked at runtime: the numeric types
                // flow through the same dynamic tags as `//` and `%`.
                let _ = self.inf.unify(out, Ty::Concrete(Type::Tuple));
            }
            Builtin::Callable => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("callable() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Bool));
            }
            Builtin::Hash => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("hash() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                // Hashability is checked at runtime so values whose static type
                // is dynamic (the common case) are never rejected early.
                let _ = self.inf.unify(out, Ty::Concrete(Type::Int));
            }
            Builtin::Id => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("id() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                // Every value has an identity; the runtime encodes it as an
                // integer.
                let _ = self.inf.unify(out, Ty::Concrete(Type::Int));
            }
            Builtin::DictMerge => {
                // Internal node produced by a dict display with `**`; both
                // operands must be mappings.
                if count != 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("internal dict merge expects 2 arguments ({count} given)"),
                    );
                    return;
                }
                for (ty, arg_line, arg_col) in &args {
                    match self.inf.resolve(*ty) {
                        Ty::Var(_) => {
                            let _ = self.inf.unify(*ty, Ty::Concrete(Type::Dict));
                        }
                        Ty::Concrete(Type::Dict)
                        | Ty::Concrete(Type::Any)
                        | Ty::Concrete(Type::Never) => {}
                        Ty::Concrete(t) => self.error(
                            *arg_line,
                            *arg_col,
                            format!("dict unpacking requires a mapping, not {}", t.name()),
                        ),
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Dict));
            }
            Builtin::Input => {
                if count > 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("input() takes at most one argument ({count} given)"),
                    );
                    return;
                }
                if let Some((ty, arg_line, arg_col)) = args.first() {
                    match self.inf.resolve(*ty) {
                        Ty::Var(_) => {
                            let _ = self.inf.unify(*ty, Ty::Concrete(Type::Str));
                        }
                        Ty::Concrete(Type::Str) | Ty::Concrete(Type::Any) => {}
                        Ty::Concrete(t) => self.error(
                            *arg_line,
                            *arg_col,
                            format!("input() argument must be str, not {}", t.name()),
                        ),
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Str));
            }
            Builtin::GetIpython => {
                if count != 0 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("get_ipython() takes no arguments ({count} given)"),
                    );
                    return;
                }
                // The result is an opaque object inspected through dynamic
                // attribute access (`.__class__.__name__`).
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::Classmethod | Builtin::Staticmethod => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!(
                            "{}() takes exactly 1 argument ({count} given)",
                            builtin.name()
                        ),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::Property => {
                // `property()` with no arguments is a valid blank descriptor
                // (`fget`/`fset`/`fdel` are None) that `enum` fills in later.
                if count > 4 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("property() takes 0 to 4 arguments ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::ByteArray | Builtin::MemoryView => {
                // `bytearray()` takes 0 to 3 arguments; `memoryview()` takes
                // exactly one. Values are dynamically typed byte containers.
                let max = if builtin == Builtin::ByteArray { 3 } else { 1 };
                if count > max {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!(
                            "{}() takes at most {max} argument(s) ({count} given)",
                            builtin.name()
                        ),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::Globals => {
                // The frontend appends the owning module's name as a hidden
                // trailing argument, so the lowered call always has one.
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        "globals() takes no arguments".to_string(),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Dict));
            }
            Builtin::Locals => {
                if count != 0 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("locals() takes no arguments ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Dict));
            }
            Builtin::Dir => {
                if count > 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("dir() takes at most 1 argument ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::List));
            }
            Builtin::Vars => {
                if count > 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("vars() takes at most 1 argument ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Dict));
            }
            Builtin::Eval | Builtin::Exec => {
                if count == 0 || count > 3 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!(
                            "{}() takes 1 to 3 arguments ({count} given)",
                            builtin.name()
                        ),
                    );
                    return;
                }
                // The result of `eval` depends on the source text; `exec`
                // returns `None` at runtime but the failure path is dynamic.
                if builtin == Builtin::Exec {
                    let _ = self.inf.unify(out, Ty::Concrete(Type::None));
                } else {
                    let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
                }
            }
            Builtin::Compile => {
                if !(3..=6).contains(&count) {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("compile() takes 3 to 6 arguments ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::ClassFromNamespace => {
                if count != 4 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        "internal class-from-namespace expects 4 arguments".to_string(),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Any));
            }
            Builtin::GlobalSetDynamic => {
                if count != 3 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        "internal dynamic global store expects 3 arguments".to_string(),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::None));
            }
        }
    }

    /// Reject a statically known non-iterable argument for a builtin.
    fn check_builtin_iterable(&mut self, builtin: Builtin, ty: Ty, line: u32, col: u32) {
        if self.inf.is_gradual(ty) {
            self.force_any(ty);
            return;
        }
        match self.inf.resolve(ty) {
            Ty::Var(_) => {
                let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
            }
            Ty::Concrete(Type::List)
            | Ty::Concrete(Type::Tuple)
            | Ty::Concrete(Type::Set)
            | Ty::Concrete(Type::Str)
            | Ty::Concrete(Type::Bytes)
            | Ty::Concrete(Type::Dict)
            | Ty::Concrete(Type::Range)
            | Ty::Concrete(Type::Any)
            | Ty::Concrete(Type::Never) => {}
            Ty::Concrete(_) => self.error(
                line,
                col,
                format!("{}() argument must be iterable", builtin.name()),
            ),
        }
    }

    fn apply_raise_checks(&mut self) {
        let checks = std::mem::take(&mut self.checks);
        for check in checks {
            let Check::RaiseMsg { ty, line, col } = check else {
                continue;
            };
            // Python exceptions accept any arguments; the runtime renders
            // non-strings for the traceback.
            let _ = (ty, line, col);
        }
    }

    #[allow(clippy::needless_range_loop)]
    fn solve_ops(&mut self) {
        loop {
            let mut progress = false;
            for index in 0..self.ops.len() {
                if self.ops[index].done {
                    continue;
                }
                if self.try_resolve_op(index) {
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }
        for index in 0..self.ops.len() {
            if !self.ops[index].done {
                self.finalize_op(index);
            }
        }
    }

    fn finalize_op(&mut self, index: usize) {
        let op = self.ops[index];
        self.force_any(op.out);
        self.ops[index].done = true;
    }

    fn try_resolve_op(&mut self, index: usize) -> bool {
        let op = self.ops[index];
        match op.kind {
            OpKind::Un(unary) => self.resolve_unary(index, op, unary),
            OpKind::Bin(binary) => self.resolve_binary(index, op, binary),
        }
    }

    fn resolve_unary(&mut self, index: usize, op: Op, unary: UnOp) -> bool {
        // An `Any` operand is dynamic: the runtime applies the operator and
        // raises `TypeError` for a value that does not support it, so only a
        // concrete operand type is reported here. An unresolved variable waits
        // for a constraint and otherwise falls back to `Any` in
        // `finalize_op`, also without a diagnostic.
        let operand = self.inf.resolve(op.a);
        match unary {
            UnOp::Not => self.bind_type(op.out, Type::Bool, op.line, op.col),
            // `~` is defined for ints only (`bool` is numerically an int);
            // floats and every other type raise `TypeError` at runtime.
            UnOp::Invert => match operand {
                Ty::Var(_) => return false,
                Ty::Concrete(Type::Any) => self.force_any(op.out),
                Ty::Concrete(Type::Int) | Ty::Concrete(Type::Bool) => {
                    self.bind_type(op.out, Type::Int, op.line, op.col);
                }
                Ty::Concrete(Type::BigInt) => {
                    self.bind_type(op.out, Type::BigInt, op.line, op.col);
                }
                Ty::Concrete(t) => {
                    if self.inf.is_gradual(op.a) {
                        // An unannotated parameter is gradual; a callsite
                        // narrowing it to a non-int is not a static contract.
                        self.force_any(op.a);
                    } else {
                        self.error(
                            op.line,
                            op.col,
                            format!("bad operand type for unary ~: '{}'", t.name()),
                        );
                    }
                    self.force_any(op.out);
                }
            },
            _ => match operand {
                Ty::Var(_) => return false,
                Ty::Concrete(Type::Any) => self.force_any(op.out),
                Ty::Concrete(Type::Complex) => {
                    self.bind_type(op.out, Type::Complex, op.line, op.col);
                }
                Ty::Concrete(t) if t.is_numeric() => {
                    let result = if t == Type::Float {
                        Type::Float
                    } else if t == Type::BigInt {
                        Type::BigInt
                    } else {
                        Type::Int
                    };
                    self.bind_type(op.out, result, op.line, op.col);
                }
                Ty::Concrete(t) => {
                    if self.inf.is_gradual(op.a) {
                        self.force_any(op.a);
                    } else {
                        self.error(
                            op.line,
                            op.col,
                            format!(
                                "bad operand type for unary {}: '{}'",
                                unary.symbol(),
                                t.name()
                            ),
                        );
                    }
                    self.force_any(op.out);
                }
            },
        }
        self.ops[index].done = true;
        true
    }

    fn resolve_binary(&mut self, index: usize, op: Op, binary: BinOp) -> bool {
        let lhs = self.inf.resolve(op.a);
        let rhs = self.inf.resolve(op.b);
        let resolved = match (lhs, rhs) {
            (Ty::Var(_), Ty::Var(_)) => {
                if binary.is_comparison() {
                    self.bind_type(op.out, Type::Bool, op.line, op.col);
                    true
                } else {
                    // Wait for another constraint to give the operands a type.
                    false
                }
            }
            (Ty::Var(_), Ty::Concrete(t)) => {
                self.resolve_one_var(op, binary, true, t);
                true
            }
            (Ty::Concrete(t), Ty::Var(_)) => {
                self.resolve_one_var(op, binary, false, t);
                true
            }
            (Ty::Concrete(x), Ty::Concrete(y)) => {
                // An `Any` operand makes the operation dynamic: the value it
                // holds at runtime decides whether the operator applies (and
                // a user type may supply the dunder), so an operand pair that
                // only fails because one side is `Any` is never an
                // unsupported-operand error. Comparisons are still boolean;
                // other operators keep their precise result when the pair has
                // one and are `Any` otherwise.
                //
                // Membership keeps its own diagnostics: an `Any` left operand
                // is fine, but a concrete right operand that cannot be
                // iterated is invalid whatever the left side holds.
                if (x == Type::Any || y == Type::Any) && !matches!(binary, BinOp::In | BinOp::NotIn)
                {
                    // Keep the precise result when the operator has one for
                    // this pair (`str % Any` is still `str`); otherwise the
                    // result is dynamic.
                    let result = if binary.is_comparison() {
                        Type::Bool
                    } else {
                        op_result(binary, x, y).unwrap_or(Type::Any)
                    };
                    self.bind_type(op.out, result, op.line, op.col);
                } else if matches!(binary, BinOp::In | BinOp::NotIn) {
                    // An unannotated parameter may hold an iterable (or a
                    // string) at runtime; only a fully concrete pair is a
                    // static error.
                    let gradual_a = self.widen_gradual(op.a);
                    let gradual_b = self.widen_gradual(op.b);
                    if gradual_a || gradual_b {
                        self.bind_type(op.out, Type::Bool, op.line, op.col);
                    } else {
                        match self.membership_result(x, y, op.line, op.col) {
                            Some(result) => self.bind_type(op.out, result, op.line, op.col),
                            None => self.force_any(op.out),
                        }
                    }
                } else {
                    match op_result(binary, x, y) {
                        Some(result) => self.bind_type(op.out, result, op.line, op.col),
                        None => {
                            let gradual_a = self.widen_gradual(op.a);
                            let gradual_b = self.widen_gradual(op.b);
                            if gradual_a || gradual_b {
                                // The pair only fails because a call site
                                // narrowed an unannotated parameter; keep the
                                // operation dynamic.
                                self.force_any(op.out);
                            } else {
                                self.unsupported(binary, x, y, op.line, op.col);
                                self.force_any(op.out);
                            }
                        }
                    }
                }
                true
            }
        };
        if resolved {
            self.ops[index].done = true;
        }
        resolved
    }

    fn resolve_one_var(&mut self, op: Op, binary: BinOp, var_is_lhs: bool, known: Type) {
        let var = if var_is_lhs { op.a } else { op.b };
        let comparison = binary.is_comparison();

        if known == Type::Any {
            let result = if comparison { Type::Bool } else { Type::Any };
            self.bind_type(op.out, result, op.line, op.col);
            return;
        }

        if matches!(binary, BinOp::In | BinOp::NotIn) {
            if var_is_lhs {
                match known {
                    // String membership requires a string on the left.
                    Type::Str => {
                        let _ = self.inf.unify(var, Ty::Concrete(Type::Str));
                    }
                    // `bytes` membership accepts a `bytes` sub-sequence or an
                    // `int` byte value.
                    Type::Bytes => {}
                    // Any value is accepted; the runtime decides membership.
                    Type::List | Type::Tuple | Type::Dict | Type::Set | Type::Range => {}
                    _ => {
                        let known_slot = if var_is_lhs { op.b } else { op.a };
                        if self.inf.is_gradual(known_slot) {
                            // A call site narrowed an unannotated parameter to
                            // a non-iterable; its runtime value may differ.
                            self.force_any(known_slot);
                            self.force_any(op.out);
                            return;
                        }
                        self.error(
                            op.line,
                            op.col,
                            format!("'{}' object is not iterable", known.name()),
                        );
                        self.force_any(op.out);
                        return;
                    }
                }
            }
            // The right side may be any iterable; the result is always bool.
            self.bind_type(op.out, Type::Bool, op.line, op.col);
            return;
        }

        // Identity accepts any pair of values and must not constrain the
        // unknown operand: `x is None` says nothing about `x`'s type.
        if matches!(binary, BinOp::Is | BinOp::IsNot) {
            self.bind_type(op.out, Type::Bool, op.line, op.col);
            return;
        }

        // `@` dispatches through user dunders at runtime, so a statically
        // known operand must not constrain the other side: `1 @ obj` may be
        // handled by `obj.__rmatmul__`. Its result is dynamic.
        if matches!(binary, BinOp::MatMul | BinOp::MatMulInPlace) {
            self.force_any(op.out);
            return;
        }

        // `%` with a string on either side is printf-style formatting. A
        // `str`/`bytes` right operand is the format string, and a `str`/`bytes`
        // left operand accepts any single value or tuple on the right. The
        // runtime parses the format, so neither operand constrains the other.
        if binary == BinOp::Mod && matches!(known, Type::Str | Type::Bytes) {
            if var_is_lhs {
                // The unknown receiver decides whether `%` formats at all: a
                // number modulo, or a user type's `__mod__`, yields another
                // type.
                self.force_any(op.out);
            } else {
                // `known` is the format string; a successful `%` returns the
                // same string type.
                self.bind_type(op.out, known, op.line, op.col);
            }
            return;
        }

        let compatible = match binary {
            BinOp::Eq | BinOp::Ne | BinOp::Is | BinOp::IsNot => true,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                known.is_numeric()
                    || known == Type::Str
                    || known == Type::Bytes
                    || known == Type::Tuple
                    || known == Type::List
                    || known == Type::Set
            }
            BinOp::Add => {
                known.is_numeric()
                    || known == Type::Complex
                    || known == Type::Str
                    || known == Type::Bytes
                    || known == Type::List
                    || known == Type::Tuple
            }
            BinOp::Mul => {
                known.is_numeric()
                    || known == Type::Complex
                    || known == Type::Str
                    || known == Type::Bytes
                    || known == Type::Tuple
                    || known == Type::List
            }
            BinOp::Sub | BinOp::Div | BinOp::FloorDiv | BinOp::Mod | BinOp::Pow => {
                known.is_numeric() || known == Type::Complex
            }
            // Bitwise operators accept integers; any other known operand is
            // rejected by the runtime with `TypeError`, so the unknown side
            // does not constrain the type.
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => true,
            // `@` dispatches through user dunders at runtime, so an unknown
            // side never constrains the other operand's type.
            BinOp::MatMul | BinOp::MatMulInPlace => true,
            // Handled above; membership accepts any unknown side.
            BinOp::In | BinOp::NotIn => true,
        };

        if !compatible {
            // The unknown operand makes the operation dynamic: it may hold a
            // value the operator accepts at runtime, or a user type with the
            // matching dunder. Reporting an unsupported operand pair here
            // would be a false positive (gradual `Any` is not a wrong type),
            // so leave the unknown side open and let the runtime decide.
            self.force_any(op.out);
            return;
        }

        let result = if binary == BinOp::Mul && known == Type::Str {
            // `str * n`: the unknown operand is the repeat count, so it must
            // be an `int` (or the `bool` subtype), never another `str`.
            let _ = self.inf.unify(var, Ty::Concrete(Type::Int));
            Type::Str
        } else if binary == BinOp::Mul && known == Type::Bytes {
            // `bytes * n`: the unknown operand is the repeat count.
            let _ = self.inf.unify(var, Ty::Concrete(Type::Int));
            Type::Bytes
        } else if binary == BinOp::Mul && known == Type::Tuple {
            // `tuple * n`: same repeat-count rule as `str * n`.
            let _ = self.inf.unify(var, Ty::Concrete(Type::Int));
            Type::Tuple
        } else {
            let _ = self.inf.unify(var, Ty::Concrete(known));
            op_result(binary, known, known).unwrap_or(Type::Any)
        };
        self.bind_type(op.out, result, op.line, op.col);
    }
}

/// Run type inference over a lowered unit. Returns the collected diagnostics
/// if anything failed to type-check.
pub fn check(prog: &LHir, inf: &mut Infer, filename: &str) -> Result<(), Vec<Diagnostic>> {
    let mut checker = Checker {
        prog,
        inf,
        filename,
        diags: Vec::new(),
        ops: Vec::new(),
        checks: Vec::new(),
        spans: HashMap::new(),
    };

    let functions = prog.functions.clone();
    for function in &functions {
        checker.check_function(function);
    }

    checker.apply_builtin_checks();
    checker.solve_ops();
    checker.apply_raise_checks();

    // Most semantic errors carry only a start column; widen them to the
    // identifier or literal they point at.
    for diagnostic in &mut checker.diags {
        if diagnostic.end_col <= diagnostic.col.saturating_add(1) {
            if let Some(&end_col) = checker.spans.get(&(diagnostic.line, diagnostic.col)) {
                if end_col > diagnostic.col {
                    diagnostic.end_col = end_col;
                }
            }
        }
    }

    if checker.diags.is_empty() {
        Ok(())
    } else {
        Err(std::mem::take(&mut checker.diags))
    }
}

/// Convert the solved HIR into the typed IR contract.
pub fn emit(prog: &LHir, inf: &mut Infer) -> Program {
    let mut functions = Vec::with_capacity(prog.functions.len());
    for function in &prog.functions {
        functions.push(emit_function(function, inf));
    }
    let mut program = Program {
        source_name: prog.source_name.clone(),
        functions,
        main: 0,
        initializers: Vec::new(),
    };
    // The rewrite needs the resolved `str` types, so it runs on the typed IR.
    str_acc::apply(&mut program);
    program
}

fn emit_function(function: &HFunction, inf: &mut Infer) -> IrFunction {
    let params = function
        .params
        .iter()
        .map(|p| Param {
            local: p.local,
            name: p.name.clone(),
            ty: inf.type_of(p.ty),
            declared: p.declared,
            default: p.default.clone(),
            default_expr: p
                .default_expr
                .as_ref()
                .map(|value| Box::new(emit_expr(value, inf))),
            kind: p.kind,
        })
        .collect();
    let locals = function
        .locals
        .iter()
        .map(|l| LocalInfo {
            name: l.name.clone(),
            ty: inf.type_of(l.ty),
        })
        .collect();
    let ret_ty = if function.is_module {
        Type::None
    } else {
        inf.type_of(function.ret_ty)
    };
    let body = emit_stmts(&function.body, inf);
    IrFunction {
        name: function.name.clone(),
        params,
        capture_count: function.capture_count,
        ret_ty,
        locals,
        body,
        is_async: function.is_async,
        is_generator: function.is_generator,
    }
}

fn emit_stmts(stmts: &[HStmt], inf: &mut Infer) -> Vec<IrStmt> {
    stmts.iter().map(|s| emit_stmt(s, inf)).collect()
}

fn emit_stmt(stmt: &HStmt, inf: &mut Infer) -> IrStmt {
    let kind = match &stmt.kind {
        HStmtKind::Expr(e) => StmtKind::Expr(emit_expr(e, inf)),
        HStmtKind::Assign { target, value } => StmtKind::Assign {
            target: *target,
            value: emit_expr(value, inf),
        },
        HStmtKind::CellInit { target, value } => StmtKind::CellInit {
            target: *target,
            value: value.as_ref().map(|value| emit_expr(value, inf)),
        },
        HStmtKind::CellAssign { target, value } => StmtKind::CellAssign {
            target: *target,
            value: emit_expr(value, inf),
        },
        HStmtKind::GlobalSet {
            module,
            name,
            value,
        } => StmtKind::GlobalSet {
            module: module.clone(),
            name: name.clone(),
            value: emit_expr(value, inf),
        },
        HStmtKind::Unpack { targets, value } => StmtKind::Unpack {
            targets: targets
                .iter()
                .map(|target| emit_unpack_target(target, inf))
                .collect(),
            value: emit_expr(value, inf),
        },
        HStmtKind::AugAssign { target, op, value } => StmtKind::AugAssign {
            target: *target,
            op: *op,
            value: emit_expr(value, inf),
        },
        HStmtKind::Closure {
            target,
            function,
            captures,
        } => StmtKind::Closure {
            target: *target,
            function: *function,
            captures: captures.clone(),
        },
        HStmtKind::ClassDef {
            target,
            name,
            bases,
            metaclass,
            methods,
            attributes,
        } => StmtKind::ClassDef {
            target: *target,
            name: name.clone(),
            bases: bases.iter().map(|b| emit_expr(b, inf)).collect(),
            metaclass: metaclass.as_ref().map(|m| emit_expr(m, inf)),
            methods: methods
                .iter()
                .map(|(name, function, kind)| ClassMethod {
                    name: name.clone(),
                    function: *function,
                    kind: *kind,
                })
                .collect(),
            attributes: attributes
                .iter()
                .map(|(name, value)| ClassAttr {
                    name: name.clone(),
                    value: emit_expr(value, inf),
                })
                .collect(),
        },
        HStmtKind::SetAttr {
            object,
            name,
            value,
        } => StmtKind::SetAttr {
            object: emit_expr(object, inf),
            name: name.clone(),
            value: emit_expr(value, inf),
        },
        HStmtKind::If {
            cond,
            then_body,
            else_body,
        } => StmtKind::If {
            cond: emit_expr(cond, inf),
            then_body: emit_stmts(then_body, inf),
            else_body: emit_stmts(else_body, inf),
        },
        HStmtKind::While {
            cond,
            body,
            else_body,
        } => StmtKind::While {
            cond: emit_expr(cond, inf),
            body: emit_stmts(body, inf),
            else_body: emit_stmts(else_body, inf),
        },
        HStmtKind::ForRange {
            var,
            start,
            stop,
            step,
            body,
            else_body,
        } => StmtKind::ForRange {
            var: *var,
            start: emit_expr(start, inf),
            stop: emit_expr(stop, inf),
            step: step.as_ref().map(|s| emit_expr(s, inf)),
            body: emit_stmts(body, inf),
            else_body: emit_stmts(else_body, inf),
        },
        HStmtKind::ForEach {
            var,
            iterable,
            body,
            else_body,
        } => StmtKind::ForEach {
            var: *var,
            iterable: emit_expr(iterable, inf),
            body: emit_stmts(body, inf),
            else_body: emit_stmts(else_body, inf),
        },
        HStmtKind::SetSubscript {
            object,
            index,
            value,
        } => StmtKind::SetSubscript {
            object: emit_expr(object, inf),
            index: emit_expr(index, inf),
            value: emit_expr(value, inf),
        },
        HStmtKind::SetSlice {
            object,
            start,
            stop,
            step,
            value,
        } => {
            // Omitted bounds lower to the runtime `None` sentinel; the IR keeps
            // them as plain expressions.
            let target = emit_expr(object, inf);
            let start = emit_slice_bound(start, inf, stmt.line);
            let stop = emit_slice_bound(stop, inf, stmt.line);
            let step = emit_slice_bound(step, inf, stmt.line);
            StmtKind::SetSlice {
                target,
                start,
                stop,
                step,
                value: emit_expr(value, inf),
            }
        }
        HStmtKind::Return(value) => StmtKind::Return(value.as_ref().map(|e| emit_expr(e, inf))),
        HStmtKind::Yield { value } => StmtKind::Yield {
            value: value.as_ref().map(|e| emit_expr(e, inf)),
        },
        HStmtKind::Break => StmtKind::Break,
        HStmtKind::Continue => StmtKind::Continue,
        HStmtKind::Pass => StmtKind::Pass,
        HStmtKind::Raise { exc, msg, cause } => StmtKind::Raise {
            exc: *exc,
            msg: msg.as_ref().map(|e| emit_expr(e, inf)),
            cause: cause.as_ref().map(|e| emit_expr(e, inf)),
        },
        HStmtKind::Assert { test, msg } => StmtKind::Assert {
            test: emit_expr(test, inf),
            msg: msg.as_ref().map(|e| emit_expr(e, inf)),
        },
        HStmtKind::Delete { targets } => StmtKind::Delete {
            targets: targets
                .iter()
                .map(|target| match target {
                    HDeleteTarget::Name(local) => DeleteTarget::Name(*local),
                    HDeleteTarget::Cell(local) => DeleteTarget::Cell(*local),
                    HDeleteTarget::Global { module, name } => DeleteTarget::Global {
                        module: module.clone(),
                        name: name.clone(),
                    },
                    HDeleteTarget::Attr { object, name } => DeleteTarget::Attr {
                        object: emit_expr(object, inf),
                        name: name.clone(),
                    },
                    HDeleteTarget::Subscript { object, index } => DeleteTarget::Subscript {
                        object: emit_expr(object, inf),
                        index: emit_expr(index, inf),
                    },
                    HDeleteTarget::Slice {
                        object,
                        start,
                        stop,
                        step,
                    } => DeleteTarget::Slice {
                        object: Box::new(emit_expr(object, inf)),
                        start: Box::new(emit_slice_bound(start, inf, stmt.line)),
                        stop: Box::new(emit_slice_bound(stop, inf, stmt.line)),
                        step: Box::new(emit_slice_bound(step, inf, stmt.line)),
                    },
                })
                .collect(),
        },
        HStmtKind::Try {
            body,
            handlers,
            else_body,
            finally_body,
        } => StmtKind::Try {
            body: emit_stmts(body, inf),
            handlers: handlers
                .iter()
                .map(|h| IrHandler {
                    exc: h.exc,
                    binding: h.binding,
                    binding_cell: h.binding_cell,
                    body: emit_stmts(&h.body, inf),
                })
                .collect(),
            else_body: emit_stmts(else_body, inf),
            finally_body: emit_stmts(finally_body, inf),
        },
    };
    IrStmt::new(kind, stmt.line)
}

fn emit_unpack_target(target: &HUnpackTarget, inf: &mut Infer) -> UnpackTarget {
    match target {
        HUnpackTarget::Name(id) => UnpackTarget::Name(*id),
        HUnpackTarget::Cell(id) => UnpackTarget::Cell(*id),
        HUnpackTarget::Global { module, name } => UnpackTarget::Global {
            module: module.clone(),
            name: name.clone(),
        },
        HUnpackTarget::Attr { object, name } => UnpackTarget::Attr {
            object: emit_expr(object, inf),
            name: name.clone(),
        },
        HUnpackTarget::Subscript { object, index } => UnpackTarget::Subscript {
            object: emit_expr(object, inf),
            index: emit_expr(index, inf),
        },
        HUnpackTarget::Nested(targets) => UnpackTarget::Nested(
            targets
                .iter()
                .map(|target| emit_unpack_target(target, inf))
                .collect(),
        ),
        HUnpackTarget::Star(target) => {
            UnpackTarget::Star(Box::new(emit_unpack_target(target, inf)))
        }
    }
}

/// Emits one bound of a slice assignment: an omitted bound becomes the
/// runtime `None` sentinel (`Const::None`), as `ExprKind::Slice` does in
/// codegen.
fn emit_slice_bound(bound: &Option<Box<HExpr>>, inf: &mut Infer, line: u32) -> IrExpr {
    match bound {
        Some(bound) => emit_expr(bound, inf),
        None => IrExpr::new(ExprKind::Const(Const::None), Type::None, line),
    }
}

fn emit_expr(expr: &HExpr, inf: &mut Infer) -> IrExpr {
    let kind = match &expr.kind {
        HExprKind::Const(c) => ExprKind::Const(c.clone()),
        HExprKind::Local(id) => ExprKind::Local(*id),
        HExprKind::CellGet(id) => ExprKind::CellGet(*id),
        HExprKind::CellSet { target, value } => ExprKind::CellSet {
            target: *target,
            value: Box::new(emit_expr(value, inf)),
        },
        HExprKind::NamedExpr { target, value } => ExprKind::NamedExpr {
            target: *target,
            value: Box::new(emit_expr(value, inf)),
        },
        HExprKind::GlobalSet {
            module,
            name,
            value,
        } => ExprKind::GlobalSet {
            module: module.clone(),
            name: name.clone(),
            value: Box::new(emit_expr(value, inf)),
        },
        HExprKind::Bin { op, lhs, rhs } => ExprKind::Bin {
            op: *op,
            lhs: Box::new(emit_expr(lhs, inf)),
            rhs: Box::new(emit_expr(rhs, inf)),
        },
        HExprKind::Un { op, operand } => ExprKind::Un {
            op: *op,
            operand: Box::new(emit_expr(operand, inf)),
        },
        HExprKind::BoolOp { op, lhs, rhs } => ExprKind::BoolOp {
            op: *op,
            lhs: Box::new(emit_expr(lhs, inf)),
            rhs: Box::new(emit_expr(rhs, inf)),
        },
        HExprKind::IfExp { test, then, orelse } => ExprKind::IfExp {
            test: Box::new(emit_expr(test, inf)),
            then: Box::new(emit_expr(then, inf)),
            orelse: Box::new(emit_expr(orelse, inf)),
        },
        HExprKind::List(elems) => ExprKind::List(elems.iter().map(|e| emit_expr(e, inf)).collect()),
        HExprKind::Tuple(elems) => {
            ExprKind::Tuple(elems.iter().map(|e| emit_expr(e, inf)).collect())
        }
        HExprKind::Set(elems) => ExprKind::Set(elems.iter().map(|e| emit_expr(e, inf)).collect()),
        HExprKind::Dict(pairs) => {
            let mut out = Vec::with_capacity(pairs.len());
            for (key, value) in pairs {
                out.push((emit_expr(key, inf), emit_expr(value, inf)));
            }
            ExprKind::Dict(out)
        }
        HExprKind::ListComp { element, clauses } => ExprKind::ListComp {
            element: Box::new(emit_expr(element, inf)),
            clauses: emit_comp_clauses(clauses, inf),
        },
        HExprKind::SetComp { element, clauses } => ExprKind::SetComp {
            element: Box::new(emit_expr(element, inf)),
            clauses: emit_comp_clauses(clauses, inf),
        },
        HExprKind::DictComp {
            key,
            value,
            clauses,
        } => ExprKind::DictComp {
            key: Box::new(emit_expr(key, inf)),
            value: Box::new(emit_expr(value, inf)),
            clauses: emit_comp_clauses(clauses, inf),
        },
        HExprKind::Subscript { object, index } => ExprKind::Subscript {
            object: Box::new(emit_expr(object, inf)),
            index: Box::new(emit_expr(index, inf)),
        },
        HExprKind::Slice {
            object,
            start,
            stop,
            step,
        } => ExprKind::Slice {
            object: Box::new(emit_expr(object, inf)),
            start: start.as_ref().map(|e| Box::new(emit_expr(e, inf))),
            stop: stop.as_ref().map(|e| Box::new(emit_expr(e, inf))),
            step: step.as_ref().map(|e| Box::new(emit_expr(e, inf))),
        },
        HExprKind::MethodCall {
            object,
            method,
            args,
            star,
            keywords,
            double_star,
        } => ExprKind::MethodCall {
            object: Box::new(emit_expr(object, inf)),
            method: method.clone(),
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
            star: star.as_ref().map(|star| Box::new(emit_expr(star, inf))),
            keywords: keywords
                .iter()
                .map(|(name, value)| (name.clone(), emit_expr(value, inf)))
                .collect(),
            double_star: double_star
                .as_ref()
                .map(|value| Box::new(emit_expr(value, inf))),
        },
        HExprKind::FunctionRef(id) => ExprKind::FunctionRef(*id),
        HExprKind::BuiltinRef(builtin) => ExprKind::BuiltinRef(*builtin),
        HExprKind::ExceptionClass(kind) => ExprKind::ExceptionClass(*kind),
        HExprKind::ModuleCall {
            module,
            function,
            args,
            keywords,
        } => ExprKind::ModuleCall {
            module: module.clone(),
            function: function.clone(),
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
            keywords: keywords
                .iter()
                .map(|(name, value)| (name.clone(), emit_expr(value, inf)))
                .collect(),
        },
        HExprKind::ModuleAttr { module, name } => ExprKind::ModuleAttr {
            module: module.clone(),
            name: name.clone(),
        },
        HExprKind::ModuleRef { module } => ExprKind::ModuleRef {
            module: module.clone(),
        },
        HExprKind::ExtensionModuleRef { module, path } => ExprKind::ExtensionModuleRef {
            module: module.clone(),
            path: path.clone(),
        },
        HExprKind::GlobalGet { module, name } => ExprKind::GlobalGet {
            module: module.clone(),
            name: name.clone(),
        },
        HExprKind::DeferredImport {
            module,
            member,
            path,
        } => ExprKind::DeferredImport {
            module: module.clone(),
            member: member.clone(),
            path: path.clone(),
        },
        HExprKind::ExtensionAttr { path, member } => ExprKind::ExtensionAttr {
            path: path.clone(),
            member: member.clone(),
        },
        HExprKind::GetAttr { object, name } => ExprKind::GetAttr {
            object: Box::new(emit_expr(object, inf)),
            name: name.clone(),
        },
        HExprKind::IndirectCall { callee, args, star } => ExprKind::IndirectCall {
            callee: Box::new(emit_expr(callee, inf)),
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
            star: star.as_ref().map(|star| Box::new(emit_expr(star, inf))),
        },
        HExprKind::Call { callee, args } => ExprKind::Call {
            callee: callee.clone(),
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
        },
        HExprKind::CallKw {
            callee,
            args,
            keywords,
        } => ExprKind::CallKw {
            callee: Box::new(emit_expr(callee, inf)),
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
            keywords: keywords
                .iter()
                .map(|(name, value)| (name.clone(), emit_expr(value, inf)))
                .collect(),
        },
        HExprKind::CallFull {
            callee,
            args,
            star,
            keywords,
            double_star,
        } => ExprKind::CallFull {
            callee: Box::new(emit_expr(callee, inf)),
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
            star: star.as_ref().map(|star| Box::new(emit_expr(star, inf))),
            keywords: keywords
                .iter()
                .map(|(name, value)| (name.clone(), emit_expr(value, inf)))
                .collect(),
            double_star: double_star
                .as_ref()
                .map(|value| Box::new(emit_expr(value, inf))),
        },
        HExprKind::ClassCall {
            class,
            init,
            args,
            dynamic_kwargs,
            param_names,
            kwarg_index,
            class_name,
        } => ExprKind::ClassCall {
            class: Box::new(emit_expr(class, inf)),
            init: *init,
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
            dynamic_kwargs: dynamic_kwargs
                .as_ref()
                .map(|value| Box::new(emit_expr(value, inf))),
            param_names: param_names.clone(),
            kwarg_index: *kwarg_index,
            class_name: class_name.clone(),
        },
        HExprKind::DeferredCall { callee, args } => ExprKind::DeferredCall {
            callee: callee
                .as_ref()
                .map(|callee| Box::new(emit_expr(callee, inf))),
            args: args.iter().map(|arg| emit_expr(arg, inf)).collect(),
        },
        HExprKind::Await { value } => ExprKind::Await {
            value: Box::new(emit_expr(value, inf)),
        },
        HExprKind::Yield { value } => ExprKind::Yield {
            value: value.as_ref().map(|value| Box::new(emit_expr(value, inf))),
        },
        HExprKind::YieldFrom { value } => ExprKind::YieldFrom {
            value: Box::new(emit_expr(value, inf)),
        },
        HExprKind::Template {
            strings,
            interpolations,
        } => ExprKind::Template {
            strings: strings.clone(),
            interpolations: interpolations
                .iter()
                .map(|interpolation| hydro_ir::TemplateInterpolation {
                    value: emit_expr(&interpolation.value, inf),
                    expression: interpolation.expression.clone(),
                    conversion: interpolation.conversion,
                    format_spec: interpolation
                        .format_spec
                        .as_ref()
                        .map(|spec| Box::new(emit_expr(spec, inf))),
                    self_documenting: interpolation.self_documenting,
                })
                .collect(),
        },
    };
    IrExpr::new(kind, inf.type_of(expr.ty), expr.line)
}

fn emit_comp_clauses(clauses: &[HCompClause], inf: &mut Infer) -> Vec<CompClause> {
    clauses
        .iter()
        .map(|clause| {
            // Asynchronous clauses are desugared into `await __anext__()`
            // loops that never reach the IR comprehension nodes.
            debug_assert!(
                !clause.is_async,
                "async comprehension clause reached the IR"
            );
            CompClause {
                target: match &clause.target {
                    HCompTarget::Name(id) => CompTarget::Name(*id),
                    HCompTarget::Unpack(targets) => CompTarget::Unpack(
                        targets
                            .iter()
                            .map(|target| emit_unpack_target(target, inf))
                            .collect(),
                    ),
                },
                iterable: emit_expr(&clause.iterable, inf),
                conditions: clause
                    .conditions
                    .iter()
                    .map(|condition| emit_expr(condition, inf))
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
