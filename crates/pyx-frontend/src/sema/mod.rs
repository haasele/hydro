//! Type inference (unification over type variables) and typed IR emission.
//!
//! Inference is deliberately simple: every expression owns a fresh type
//! variable, assignments/reads/calls/returns add equality constraints, and
//! operators are resolved against the numeric lattice (`bool` < `int` <
//! `float`). `str` supports `+` and repetition with an `int` count on either
//! side of `*`. Variables that never resolve become [`Type::Any`], which makes
//! the codegen use the dynamic runtime operators.

use pyx_ir::stdlib;
use pyx_ir::{
    BinOp, Builtin, Callee, ClassMethod, CompClause, CompTarget, Expr as IrExpr, ExprKind,
    Function as IrFunction, Handler as IrHandler, LocalInfo, Param, Program, Stmt as IrStmt,
    StmtKind, Type, UnOp,
};

use crate::diag::{self, codes, Diagnostic};
use crate::lower::{HCompClause, HCompTarget, HExpr, HExprKind, HFunction, HStmt, HStmtKind, LHir};

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
}

impl Infer {
    pub fn new() -> Infer {
        Infer {
            parent: Vec::new(),
            binding: Vec::new(),
        }
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
        return Some(if a == Type::Float || b == Type::Float {
            Type::Float
        } else {
            Type::Int
        });
    }
    None
}

fn numeric_result(a: Type, b: Type) -> Type {
    if a == Type::Float || b == Type::Float {
        Type::Float
    } else {
        Type::Int
    }
}

/// Whether a value of type `got` satisfies an explicit annotation `expected`.
/// Annotations are checked strictly, except for the documented numeric
/// subtyping of `bool` and `int`.
fn annotation_accepts(expected: Type, got: Type) -> bool {
    if expected == got || expected == Type::Any || got == Type::Any || got == Type::Never {
        return true;
    }
    matches!(
        (expected, got),
        (Type::Int, Type::Bool) | (Type::Float, Type::Int) | (Type::Float, Type::Bool)
    )
}

/// Expected static kind of an argument passed to a built-in method.
#[derive(Clone, Copy)]
enum ArgKind {
    Any,
    Str,
    Bytes,
    Int,
    List,
    Dict,
    /// Any value Python can hash: containers are rejected recursively.
    Hashable,
    /// `set.union` accepts a set, list or tuple.
    SetIterable,
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
            ArgKind::Bytes => got == Type::Bytes,
            ArgKind::Int => got == Type::Int || got == Type::Bool,
            ArgKind::List => got == Type::List,
            ArgKind::Dict => got == Type::Dict,
            // Detailed hashability is checked on the expression itself so
            // tuples with unhashable elements are rejected recursively.
            ArgKind::Hashable => true,
            ArgKind::SetIterable => matches!(got, Type::Set | Type::List | Type::Tuple),
        }
    }

    fn expected_name(self) -> &'static str {
        match self {
            ArgKind::Any => "Any",
            ArgKind::Str => "str",
            ArgKind::Bytes => "bytes",
            ArgKind::Int => "int",
            ArgKind::List => "list",
            ArgKind::Dict => "dict",
            ArgKind::Hashable => "hashable",
            ArgKind::SetIterable => "set, list or tuple",
        }
    }

    /// Type an unresolved argument is bound to, when narrowing is safe.
    fn bound_type(self) -> Option<Type> {
        match self {
            ArgKind::Any | ArgKind::Hashable => None,
            ArgKind::Str => Some(Type::Str),
            ArgKind::Bytes => Some(Type::Bytes),
            ArgKind::Int => Some(Type::Int),
            ArgKind::List => Some(Type::List),
            ArgKind::Dict => Some(Type::Dict),
            ArgKind::SetIterable => Some(Type::Set),
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
    ("strip", MethodSig::new(0, 0, &[], Type::Str)),
    ("lstrip", MethodSig::new(0, 0, &[], Type::Str)),
    ("rstrip", MethodSig::new(0, 0, &[], Type::Str)),
    ("isdigit", MethodSig::new(0, 0, &[], Type::Bool)),
    ("isalpha", MethodSig::new(0, 0, &[], Type::Bool)),
    ("isalnum", MethodSig::new(0, 0, &[], Type::Bool)),
    ("isspace", MethodSig::new(0, 0, &[], Type::Bool)),
    (
        "split",
        MethodSig::new(0, 2, &[ArgKind::Str, ArgKind::Int], Type::List),
    ),
    ("join", MethodSig::new(1, 1, &[ArgKind::List], Type::Str)),
    (
        "replace",
        MethodSig::new(2, 3, &[ArgKind::Str, ArgKind::Str, ArgKind::Int], Type::Str),
    ),
    (
        "startswith",
        MethodSig::new(1, 1, &[ArgKind::Str], Type::Bool),
    ),
    (
        "endswith",
        MethodSig::new(1, 1, &[ArgKind::Str], Type::Bool),
    ),
    ("find", MethodSig::new(1, 1, &[ArgKind::Str], Type::Int)),
    ("count", MethodSig::new(1, 1, &[ArgKind::Str], Type::Int)),
    ("encode", MethodSig::new(0, 1, &[ArgKind::Str], Type::Bytes)),
    // `str.format(*args)` accepts any number of positional arguments; field
    // count and types are validated at runtime by the format parser.
    ("format", MethodSig::new(0, usize::MAX, &[], Type::Str)),
];

/// Static signatures of `bytes` methods. Byte-oriented arguments are byte
/// strings; `decode` takes an optional string encoding.
const BYTES_METHODS: &[(&str, MethodSig)] = &[
    ("decode", MethodSig::new(0, 1, &[ArgKind::Str], Type::Str)),
    ("hex", MethodSig::new(0, 0, &[], Type::Str)),
    ("count", MethodSig::new(1, 1, &[ArgKind::Bytes], Type::Int)),
    ("find", MethodSig::new(1, 1, &[ArgKind::Bytes], Type::Int)),
    (
        "startswith",
        MethodSig::new(1, 1, &[ArgKind::Bytes], Type::Bool),
    ),
    (
        "endswith",
        MethodSig::new(1, 1, &[ArgKind::Bytes], Type::Bool),
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
    ("join", MethodSig::new(1, 1, &[ArgKind::List], Type::Bytes)),
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
    ("extend", MethodSig::new(1, 1, &[ArgKind::List], Type::None)),
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
    ("keys", MethodSig::new(0, 0, &[], Type::List)),
    ("values", MethodSig::new(0, 0, &[], Type::List)),
    ("items", MethodSig::new(0, 0, &[], Type::List)),
    (
        "pop",
        MethodSig::new(1, 2, &[ArgKind::Any, ArgKind::Any], Type::Any),
    ),
    ("update", MethodSig::new(1, 1, &[ArgKind::Dict], Type::None)),
    ("clear", MethodSig::new(0, 0, &[], Type::None)),
];

const TUPLE_METHODS: &[(&str, MethodSig)] = &[
    ("count", MethodSig::new(1, 1, &[ArgKind::Any], Type::Int)),
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
    ("clear", MethodSig::new(0, 0, &[], Type::None)),
    ("copy", MethodSig::new(0, 0, &[], Type::Set)),
    (
        "union",
        MethodSig::new(1, 1, &[ArgKind::SetIterable], Type::Set),
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
            {
                Some(Type::Bool)
            } else {
                None
            }
        }
        BinOp::Add => {
            if any {
                Some(Type::Any)
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
            } else if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else if (a == Type::Str && matches!(b, Type::Int | Type::Bool))
                || (b == Type::Str && matches!(a, Type::Int | Type::Bool))
            {
                // `str * int` repeats the string. `bool` is an `int` subtype.
                Some(Type::Str)
            } else if (a == Type::Bytes && matches!(b, Type::Int | Type::Bool))
                || (b == Type::Bytes && matches!(a, Type::Int | Type::Bool))
            {
                // `bytes * int` repeats the byte string.
                Some(Type::Bytes)
            } else if (a == Type::Tuple && matches!(b, Type::Int | Type::Bool))
                || (b == Type::Tuple && matches!(a, Type::Int | Type::Bool))
            {
                Some(Type::Tuple)
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
                    if matches!(a, Type::Int | Type::Bool | Type::Any | Type::Never) {
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
            } else if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else if a == Type::Set && b == Type::Set {
                Some(Type::Set)
            } else {
                None
            }
        }
        BinOp::Mod => {
            if a == Type::Str {
                // `str % value_or_tuple` is printf-style formatting; the
                // runtime validates the format string and argument count.
                Some(Type::Str)
            } else if any {
                Some(Type::Any)
            } else if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else {
                None
            }
        }
        BinOp::FloorDiv | BinOp::Pow => {
            if any {
                Some(Type::Any)
            } else if a.is_numeric() && b.is_numeric() {
                Some(numeric_result(a, b))
            } else {
                None
            }
        }
        BinOp::Div => {
            if any {
                Some(Type::Any)
            } else if a.is_numeric() && b.is_numeric() {
                Some(Type::Float)
            } else {
                None
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

struct Checker<'a> {
    prog: &'a LHir,
    inf: &'a mut Infer,
    filename: &'a str,
    diags: Vec<Diagnostic>,
    ops: Vec<Op>,
    checks: Vec<Check>,
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
            if has_bare {
                self.error_code(
                    f.line,
                    f.col,
                    codes::CONTROL,
                    format!(
                        "inconsistent return in function '{}': mix of value and bare returns",
                        f.name
                    ),
                );
            }
        } else {
            match f.ret_ty {
                Ty::Concrete(declared) => {
                    if self.inf.unify(f.ret_ty, Ty::Concrete(Type::None)).is_err() {
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
                }
                Ty::Var(_) => {
                    if self.inf.unify(f.ret_ty, Ty::Concrete(Type::None)).is_err() {
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
                let local_ty = f.locals[*target as usize].ty;
                if let Err((expected, got)) = self.inf.unify(local_ty, value.ty) {
                    let name = &f.locals[*target as usize].name;
                    self.error(
                        stmt.line,
                        stmt.col,
                        format!(
                            "cannot assign type {} to variable '{}' of type {}",
                            got.name(),
                            name,
                            expected.name()
                        ),
                    );
                }
            }
            HStmtKind::Unpack { targets, value } => {
                self.check_expr(value, f);
                match self.inf.resolve(value.ty) {
                    Ty::Concrete(Type::List)
                    | Ty::Concrete(Type::Tuple)
                    | Ty::Concrete(Type::Any) => {}
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
                    let ty = f.locals[*target as usize].ty;
                    let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
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
                match self.inf.resolve(object.ty) {
                    Ty::Concrete(Type::List) => {
                        self.check_int_index("list", index.ty, index.line, index.col);
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
            HStmtKind::Closure { target, .. } => {
                // The closure value is a runtime function object without a
                // static type.
                let local_ty = f.locals[*target as usize].ty;
                let _ = self.inf.unify(local_ty, Ty::Concrete(Type::Any));
            }
            HStmtKind::ClassDef { target, base, .. } => {
                if let Some(base) = base {
                    self.check_expr(base, f);
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
            HStmtKind::While { cond, body } => {
                self.check_expr(cond, f);
                self.check_stmts(body, f, has_value, has_bare);
            }
            HStmtKind::ForRange {
                var,
                start,
                stop,
                step,
                body,
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
            }
            HStmtKind::ForEach {
                var,
                iterable,
                body,
            } => {
                self.check_expr(iterable, f);
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
                // keeps its elements dynamically typed in this phase.
                let element = if iterable_ty == Ty::Concrete(Type::Bytes) {
                    Type::Int
                } else {
                    Type::Any
                };
                let _ = self.inf.unify(var_ty, Ty::Concrete(element));
                self.check_stmts(body, f, has_value, has_bare);
            }
            HStmtKind::Return(value) => match value {
                Some(value) => {
                    *has_value = true;
                    self.check_expr(value, f);
                    if !f.is_generator {
                        match f.ret_ty {
                            Ty::Concrete(expected) => match self.inf.resolve(value.ty) {
                                Ty::Concrete(got) => {
                                    if !annotation_accepts(expected, got) {
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
                }
            }
            HStmtKind::Break | HStmtKind::Continue | HStmtKind::Pass => {}
            HStmtKind::Raise { exc: _, msg } => {
                if let Some(msg) = msg {
                    self.check_expr(msg, f);
                    self.checks.push(Check::RaiseMsg {
                        ty: msg.ty,
                        line: msg.line,
                        col: msg.col,
                    });
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
                self.error(
                    key.line,
                    key.col,
                    format!("unhashable type: '{}'", t.name()),
                );
            }
        }
    }

    /// Validate a comprehension's clauses outward-in. Each iterable must be
    /// iterable; targets are dynamic; conditions accept any truthy value.
    fn check_comp_clauses(&mut self, clauses: &[HCompClause], f: &HFunction) {
        for clause in clauses {
            self.check_expr(&clause.iterable, f);
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
            match &clause.target {
                HCompTarget::Name(id) => {
                    let ty = f.locals[*id as usize].ty;
                    let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
                }
                HCompTarget::Unpack(ids) => {
                    for id in ids {
                        let ty = f.locals[*id as usize].ty;
                        let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
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
            Ty::Concrete(Type::Int) | Ty::Concrete(Type::Bool) | Ty::Concrete(Type::Any) => {}
            Ty::Var(_) => {
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Int));
            }
            Ty::Concrete(got) => {
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

    fn check_int_index(&mut self, container: &str, index: Ty, line: u32, col: u32) {
        match self.inf.resolve(index) {
            Ty::Concrete(Type::Int) | Ty::Concrete(Type::Bool) | Ty::Concrete(Type::Any) => {}
            Ty::Var(_) => {
                let _ = self.inf.unify(index, Ty::Concrete(Type::Int));
            }
            Ty::Concrete(got) => self.error_code(
                line,
                col,
                codes::INDEX,
                format!("{} index must be int, not {}", container, got.name()),
            ),
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
            | Ty::Concrete(Type::Bool)
            | Ty::Concrete(Type::None)
            | Ty::Concrete(Type::Any) => {}
            Ty::Var(_) => {
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Int));
            }
            Ty::Concrete(_) => self.error_code(
                expr.line,
                expr.col,
                codes::INDEX,
                "slice indices must be integers or None".to_string(),
            ),
        }
    }

    fn check_expr(&mut self, expr: &HExpr, f: &HFunction) {
        match &expr.kind {
            HExprKind::Const(_) => {}
            HExprKind::Local(id) => {
                let local_ty = f.locals[*id as usize].ty;
                let _ = self.inf.unify(local_ty, expr.ty);
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
                match self.inf.unify(lhs.ty, rhs.ty) {
                    Ok(ty) => {
                        let _ = self.inf.unify(expr.ty, ty);
                    }
                    Err(_) => {
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    }
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
            HExprKind::GeneratorExp { element, clauses } => {
                // Generator expressions are materialised as lists.
                self.check_comp_clauses(clauses, f);
                self.check_expr(element, f);
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::List));
            }
            HExprKind::Subscript { object, index } => {
                self.check_expr(object, f);
                self.check_expr(index, f);
                match self.inf.resolve(object.ty) {
                    Ty::Concrete(Type::List) => {
                        self.check_int_index("list", index.ty, index.line, index.col);
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
                    }
                    Ty::Concrete(Type::Str) => {
                        self.check_int_index("str", index.ty, index.line, index.col);
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Str));
                    }
                    Ty::Concrete(Type::Bytes) => {
                        self.check_int_index("bytes", index.ty, index.line, index.col);
                        // Indexing a byte string yields an `int` byte value.
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Int));
                    }
                    Ty::Concrete(Type::Tuple) => {
                        self.check_int_index("tuple", index.ty, index.line, index.col);
                        let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Any));
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
                keywords,
            } => self.check_method(expr, object, method, args, keywords, f),
            HExprKind::FunctionRef(_) => {
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Function));
            }
            HExprKind::BuiltinRef(_) => {
                let _ = self.inf.unify(expr.ty, Ty::Concrete(Type::Function));
            }
            HExprKind::ModuleCall {
                module,
                function,
                args,
            } => {
                for arg in args {
                    self.check_expr(arg, f);
                }
                match stdlib::function(module, function) {
                    Some(spec) => {
                        if args.len() < spec.min_args || args.len() > spec.max_args {
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
                                    "{}.{}() takes {} positional argument(s) ({} given)",
                                    module,
                                    function,
                                    expected,
                                    args.len()
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
            HExprKind::GetAttr { object, .. } => {
                // Attributes are resolved at runtime by receiver kind, so the
                // result is always dynamic. The receiver still gets checked.
                self.check_expr(object, f);
                self.force_any(expr.ty);
            }
            HExprKind::IndirectCall { callee, args } => {
                self.check_expr(callee, f);
                for arg in args {
                    self.check_expr(arg, f);
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
            HExprKind::Call { callee, args } => self.check_call(expr, *callee, args, f),
            HExprKind::Await { value } => {
                self.check_expr(value, f);
                // The awaited result is only known at runtime.
                self.force_any(expr.ty);
            }
        }
    }

    fn check_call(&mut self, expr: &HExpr, callee: Callee, args: &[HExpr], f: &HFunction) {
        for arg in args {
            self.check_expr(arg, f);
        }
        match callee {
            Callee::Function(id) => {
                let (name, params, ret_ty, is_coroutine) = {
                    let function = &self.prog.functions[id as usize];
                    (
                        function.name.clone(),
                        function.params.iter().map(|p| p.ty).collect::<Vec<_>>(),
                        function.ret_ty,
                        function.is_async || function.is_generator,
                    )
                };
                if args.len() != params.len() {
                    self.error_code(
                        expr.line,
                        expr.col,
                        codes::CALL,
                        format!(
                            "{}() takes {} positional argument(s) but {} were given",
                            name,
                            params.len(),
                            args.len()
                        ),
                    );
                }
                for (index, arg) in args.iter().enumerate() {
                    if let Some(param_ty) = params.get(index) {
                        match *param_ty {
                            Ty::Concrete(expected) => match self.inf.resolve(arg.ty) {
                                Ty::Concrete(got) => {
                                    if !annotation_accepts(expected, got) {
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
                                    }
                                }
                                Ty::Var(_) => {
                                    let _ = self.inf.unify(arg.ty, Ty::Concrete(expected));
                                }
                            },
                            Ty::Var(_) => {
                                let _ = self.inf.unify(*param_ty, arg.ty);
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
                // `range(...)` has a concrete result type, and callers check
                // it eagerly (e.g. subscripting rejects it before the deferred
                // builtin checks run). Argument validation stays deferred.
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
        }
    }

    /// Resolve `object.method(args)` against the Phase 2.4 method matrix.
    ///
    /// Receivers whose type is not statically known (or `Any`) are executed
    /// dynamically: every method name and arity is accepted and the result is
    /// `Any`, leaving the checks to the runtime.
    fn check_method(
        &mut self,
        expr: &HExpr,
        object: &HExpr,
        method: &str,
        args: &[HExpr],
        keywords: &[(String, HExpr)],
        f: &HFunction,
    ) {
        self.check_expr(object, f);
        for arg in args {
            self.check_expr(arg, f);
        }
        for (_, value) in keywords {
            self.check_expr(value, f);
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
                    Type::Set => SET_METHODS,
                    _ => {
                        self.error_no_method(expr, receiver, method, None);
                        self.force_any(expr.ty);
                        return;
                    }
                };
                let Some(entry) = table.iter().find(|entry| entry.0 == method) else {
                    self.error_no_method(expr, receiver, method, Some(table));
                    self.force_any(expr.ty);
                    return;
                };
                let signature = &entry.1;

                // Keywords fill the remaining slots at runtime, so only the
                // positional upper bound can be checked here when any keyword
                // is present. Unknown and duplicate keywords are diagnosed by
                // the runtime binder.
                let arity_ok = if keywords.is_empty() {
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

    // ------------------------------------------------------------------
    // Constraint solving
    // ------------------------------------------------------------------

    fn bind_type(&mut self, out: Ty, ty: Type, line: u32, col: u32) {
        if self.inf.unify(out, Ty::Concrete(ty)).is_err() {
            let existing = self.inf.name_of(out);
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
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("str() takes exactly one argument ({count} given)"),
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
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!(
                            "{}() takes exactly one argument ({} given)",
                            builtin.name(),
                            count
                        ),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                if let Ty::Concrete(t) = self.inf.resolve(ty) {
                    let ok = t == Type::Any
                        || t == Type::Int
                        || t == Type::Float
                        || t == Type::Bool
                        || t == Type::Str;
                    if !ok {
                        self.error(
                            arg_line,
                            arg_col,
                            format!(
                                "{}() argument must be a string or a number, not {}",
                                builtin.name(),
                                t.name()
                            ),
                        );
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(target));
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
                    match self.inf.resolve(ty) {
                        Ty::Var(_) => {
                            // No static element information: iterate dynamically.
                            let _ = self.inf.unify(ty, Ty::Concrete(Type::Any));
                        }
                        Ty::Concrete(Type::List)
                        | Ty::Concrete(Type::Tuple)
                        | Ty::Concrete(Type::Set)
                        | Ty::Concrete(Type::Str)
                        | Ty::Concrete(Type::Bytes)
                        | Ty::Concrete(Type::Dict)
                        | Ty::Concrete(Type::Any)
                        | Ty::Concrete(Type::Never) => {}
                        Ty::Concrete(_) => {
                            self.error(arg_line, arg_col, "set() argument must be iterable")
                        }
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Set));
            }
            Builtin::Format => {
                // Internal f-string field: value, constant spec, constant
                // conversion flag. The constants are validated by codegen.
                if count != 3 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("internal format field expects 3 arguments ({count} given)"),
                    );
                    return;
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Str));
            }
            Builtin::Bytes => {
                if count > 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("bytes() takes 0 to 2 positional arguments ({count} given)"),
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
                } else if count == 2 {
                    // `bytes(source, encoding)`: the source must be a string
                    // and the encoding a string.
                    for (index, expected) in [(0usize, "str"), (1usize, "str")] {
                        let (ty, arg_line, arg_col) = args[index];
                        match self.inf.resolve(ty) {
                            Ty::Var(_) | Ty::Concrete(Type::Any) | Ty::Concrete(Type::Never) => {}
                            Ty::Concrete(Type::Str) => {}
                            Ty::Concrete(t) => self.error(
                                arg_line,
                                arg_col,
                                format!(
                                    "bytes() argument {} must be {}, not {}",
                                    index + 1,
                                    expected,
                                    t.name()
                                ),
                            ),
                        }
                    }
                }
                let _ = self.inf.unify(out, Ty::Concrete(Type::Bytes));
            }
            Builtin::Iter => {
                if count != 1 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!("iter() takes exactly one argument ({count} given)"),
                    );
                    return;
                }
                let (ty, arg_line, arg_col) = args[0];
                self.check_builtin_iterable(Builtin::Iter, ty, arg_line, arg_col);
                // The element type is only known at runtime.
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
                if count != 2 {
                    self.error_code(
                        line,
                        col,
                        codes::CALL,
                        format!(
                            "{}() takes exactly 2 positional arguments ({count} given)",
                            builtin.name()
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
        }
    }

    /// Reject a statically known non-iterable argument for a builtin.
    fn check_builtin_iterable(&mut self, builtin: Builtin, ty: Ty, line: u32, col: u32) {
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
            if let Ty::Concrete(t) = self.inf.resolve(ty) {
                if t != Type::Str && t != Type::Any {
                    self.error(line, col, "exception message must be str");
                }
            }
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
        let operand = self.inf.resolve(op.a);
        match unary {
            UnOp::Not => self.bind_type(op.out, Type::Bool, op.line, op.col),
            _ => match operand {
                Ty::Var(_) => return false,
                Ty::Concrete(Type::Any) => self.force_any(op.out),
                Ty::Concrete(t) if t.is_numeric() => {
                    let result = if t == Type::Float {
                        Type::Float
                    } else {
                        Type::Int
                    };
                    self.bind_type(op.out, result, op.line, op.col);
                }
                Ty::Concrete(t) => {
                    self.error(
                        op.line,
                        op.col,
                        format!(
                            "bad operand type for unary {}: '{}'",
                            unary.symbol(),
                            t.name()
                        ),
                    );
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
                if matches!(binary, BinOp::In | BinOp::NotIn) {
                    match self.membership_result(x, y, op.line, op.col) {
                        Some(result) => self.bind_type(op.out, result, op.line, op.col),
                        None => self.force_any(op.out),
                    }
                } else {
                    match op_result(binary, x, y) {
                        Some(result) => self.bind_type(op.out, result, op.line, op.col),
                        None => {
                            self.unsupported(binary, x, y, op.line, op.col);
                            self.force_any(op.out);
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

        let compatible = match binary {
            BinOp::Eq | BinOp::Ne => true,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                known.is_numeric() || known == Type::Str || known == Type::Bytes
            }
            BinOp::Add => {
                known.is_numeric()
                    || known == Type::Str
                    || known == Type::Bytes
                    || known == Type::Tuple
            }
            BinOp::Mul => {
                known.is_numeric()
                    || known == Type::Str
                    || known == Type::Bytes
                    || known == Type::Tuple
            }
            BinOp::Sub | BinOp::Div | BinOp::FloorDiv | BinOp::Mod | BinOp::Pow => {
                known.is_numeric()
            }
            // Handled above; membership accepts any unknown side.
            BinOp::In | BinOp::NotIn => true,
        };

        if !compatible {
            // No possible type for the variable makes this operation valid.
            let (lhs_name, rhs_name) = if var_is_lhs {
                ("Any", known.name())
            } else {
                (known.name(), "Any")
            };
            self.error(
                op.line,
                op.col,
                format!(
                    "unsupported operand type(s) for {}: '{}' and '{}'",
                    binary.symbol(),
                    lhs_name,
                    rhs_name
                ),
            );
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
    };

    let functions = prog.functions.clone();
    for function in &functions {
        checker.check_function(function);
    }

    checker.apply_builtin_checks();
    checker.solve_ops();
    checker.apply_raise_checks();

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
    Program {
        source_name: prog.source_name.clone(),
        functions,
        main: 0,
    }
}

fn emit_function(function: &HFunction, inf: &mut Infer) -> IrFunction {
    let params = function
        .params
        .iter()
        .map(|p| Param {
            local: p.local,
            name: p.name.clone(),
            ty: inf.type_of(p.ty),
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
        HStmtKind::Unpack { targets, value } => StmtKind::Unpack {
            targets: targets.clone(),
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
            base,
            methods,
        } => StmtKind::ClassDef {
            target: *target,
            name: name.clone(),
            base: base.as_ref().map(|b| emit_expr(b, inf)),
            methods: methods
                .iter()
                .map(|(name, function)| ClassMethod {
                    name: name.clone(),
                    function: *function,
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
        HStmtKind::While { cond, body } => StmtKind::While {
            cond: emit_expr(cond, inf),
            body: emit_stmts(body, inf),
        },
        HStmtKind::ForRange {
            var,
            start,
            stop,
            step,
            body,
        } => StmtKind::ForRange {
            var: *var,
            start: emit_expr(start, inf),
            stop: emit_expr(stop, inf),
            step: step.as_ref().map(|s| emit_expr(s, inf)),
            body: emit_stmts(body, inf),
        },
        HStmtKind::ForEach {
            var,
            iterable,
            body,
        } => StmtKind::ForEach {
            var: *var,
            iterable: emit_expr(iterable, inf),
            body: emit_stmts(body, inf),
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
        HStmtKind::Return(value) => StmtKind::Return(value.as_ref().map(|e| emit_expr(e, inf))),
        HStmtKind::Yield { value } => StmtKind::Yield {
            value: value.as_ref().map(|e| emit_expr(e, inf)),
        },
        HStmtKind::Break => StmtKind::Break,
        HStmtKind::Continue => StmtKind::Continue,
        HStmtKind::Pass => StmtKind::Pass,
        HStmtKind::Raise { exc, msg } => StmtKind::Raise {
            exc: *exc,
            msg: msg.as_ref().map(|e| emit_expr(e, inf)),
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
                    body: emit_stmts(&h.body, inf),
                })
                .collect(),
            else_body: emit_stmts(else_body, inf),
            finally_body: emit_stmts(finally_body, inf),
        },
    };
    IrStmt::new(kind, stmt.line)
}

fn emit_expr(expr: &HExpr, inf: &mut Infer) -> IrExpr {
    let kind = match &expr.kind {
        HExprKind::Const(c) => ExprKind::Const(c.clone()),
        HExprKind::Local(id) => ExprKind::Local(*id),
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
        HExprKind::GeneratorExp { element, clauses } => ExprKind::GeneratorExp {
            element: Box::new(emit_expr(element, inf)),
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
            keywords,
        } => ExprKind::MethodCall {
            object: Box::new(emit_expr(object, inf)),
            method: method.clone(),
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
            keywords: keywords
                .iter()
                .map(|(name, value)| (name.clone(), emit_expr(value, inf)))
                .collect(),
        },
        HExprKind::FunctionRef(id) => ExprKind::FunctionRef(*id),
        HExprKind::BuiltinRef(builtin) => ExprKind::BuiltinRef(*builtin),
        HExprKind::ModuleCall {
            module,
            function,
            args,
        } => ExprKind::ModuleCall {
            module: module.clone(),
            function: function.clone(),
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
        },
        HExprKind::ModuleAttr { module, name } => ExprKind::ModuleAttr {
            module: module.clone(),
            name: name.clone(),
        },
        HExprKind::GetAttr { object, name } => ExprKind::GetAttr {
            object: Box::new(emit_expr(object, inf)),
            name: name.clone(),
        },
        HExprKind::IndirectCall { callee, args } => ExprKind::IndirectCall {
            callee: Box::new(emit_expr(callee, inf)),
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
        },
        HExprKind::Call { callee, args } => ExprKind::Call {
            callee: *callee,
            args: args.iter().map(|a| emit_expr(a, inf)).collect(),
        },
        HExprKind::Await { value } => ExprKind::Await {
            value: Box::new(emit_expr(value, inf)),
        },
    };
    IrExpr::new(kind, inf.type_of(expr.ty), expr.line)
}

fn emit_comp_clauses(clauses: &[HCompClause], inf: &mut Infer) -> Vec<CompClause> {
    clauses
        .iter()
        .map(|clause| CompClause {
            target: match &clause.target {
                HCompTarget::Name(id) => CompTarget::Name(*id),
                HCompTarget::Unpack(ids) => CompTarget::Unpack(ids.clone()),
            },
            iterable: emit_expr(&clause.iterable, inf),
            conditions: clause
                .conditions
                .iter()
                .map(|condition| emit_expr(condition, inf))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests;
