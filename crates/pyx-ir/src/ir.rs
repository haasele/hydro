//! Typed tree: expressions, statements, functions and the program itself.
//!
//! The frontend produces a [`Program`] with fully resolved local slots,
//! function ids and [`Type`]s. The codegen must not perform name resolution or
//! type inference; it only lowers this tree to machine code.
//!
//! Invariants:
//! - `functions[0]` is the module body, named `<module>`, with no parameters
//!   and return type [`Type::None`]. [`Program::main`] points at it.
//! - Every [`LocalId`] indexes the enclosing function's `locals` vector.
//!   Parameters are locals too and appear first in `locals`.
//! - Each [`FunctionId`] indexes `Program::functions`.
//! - `line` is 1-based and refers to the source file given by
//!   [`Program::source_name`].
//! - Nested functions are closures by value: [`StmtKind::Closure`] captures
//!   the current values of enclosing locals. Classes are limited to a single
//!   optional base and plain methods.

use crate::model::{
    BinOp, BoolOp, Builtin, Callee, Const, ExcKind, FunctionId, LocalId, Type, UnOp,
};

/// Target binding inside a comprehension clause.
#[derive(Clone, Debug)]
pub enum CompTarget {
    Name(LocalId),
    Unpack(Vec<LocalId>),
}

/// One `for target in iterable if cond ...` clause of a comprehension.
/// Conditions are evaluated in order after the target is bound.
#[derive(Clone, Debug)]
pub struct CompClause {
    pub target: CompTarget,
    pub iterable: Expr,
    pub conditions: Vec<Expr>,
}

/// Expression node with its inferred type and source line.
#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub ty: Type,
    pub line: u32,
}

impl Expr {
    pub fn new(kind: ExprKind, ty: Type, line: u32) -> Expr {
        Expr { kind, ty, line }
    }
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Const(Const),
    Local(LocalId),
    Bin {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Un {
        op: UnOp,
        operand: Box<Expr>,
    },
    BoolOp {
        op: BoolOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// List literal, e.g. `[1, 2, 3]`.
    List(Vec<Expr>),
    /// Dict literal preserving insertion order; duplicate keys keep the last
    /// value and the first position (Python semantics).
    Dict(Vec<(Expr, Expr)>),
    /// Tuple literal, e.g. `(1, 2)` or `(1,)`.
    Tuple(Vec<Expr>),
    /// Set literal, e.g. `{1, 2}`. Duplicate elements are collapsed.
    Set(Vec<Expr>),
    /// `object[index]`. Slices use [`ExprKind::Slice`].
    Subscript {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    /// `object.start:stop:step` with optional bounds. Missing bounds are
    /// `None` and lower to the runtime `None` sentinel.
    Slice {
        object: Box<Expr>,
        start: Option<Box<Expr>>,
        stop: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },
    /// Reference to a program function or lambda as a value.
    FunctionRef(FunctionId),
    /// Reference to a builtin function as a first-class value (`f = int`).
    BuiltinRef(Builtin),
    /// `module.function(args...)` resolved from a builtin module.
    ModuleCall {
        module: String,
        function: String,
        args: Vec<Expr>,
    },
    /// `module.name` (constant or callable value) from a builtin module.
    ModuleAttr {
        module: String,
        name: String,
    },
    /// `object.name` data attribute of a native value. Method attributes
    /// without a call raise `AttributeError` at runtime.
    GetAttr {
        object: Box<Expr>,
        name: String,
    },
    /// Indirect call through a function value.
    IndirectCall {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `[element for ...]`.
    ListComp {
        element: Box<Expr>,
        clauses: Vec<CompClause>,
    },
    /// `{element for ...}`.
    SetComp {
        element: Box<Expr>,
        clauses: Vec<CompClause>,
    },
    /// `{key: value for ...}`.
    DictComp {
        key: Box<Expr>,
        value: Box<Expr>,
        clauses: Vec<CompClause>,
    },
    /// `(element for ...)`. Phase 2.8 materialises this eagerly as a list
    /// (documented deviation until real generator objects exist).
    GeneratorExp {
        element: Box<Expr>,
        clauses: Vec<CompClause>,
    },
    /// `object.method(args...)`. A bare `object.method` without a call is a
    /// [`ExprKind::GetAttr`] and raises `AttributeError` at runtime.
    MethodCall {
        object: Box<Expr>,
        method: String,
        args: Vec<Expr>,
        /// Keyword arguments in source order; the runtime binds them.
        keywords: Vec<(String, Expr)>,
    },
    Call {
        callee: Callee,
        args: Vec<Expr>,
    },
    /// `await value` inside an `async def` body. The runtime drives the
    /// awaitable and returns its result.
    Await {
        value: Box<Expr>,
    },
}

/// Statement node with its source line.
#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub line: u32,
}

impl Stmt {
    pub fn new(kind: StmtKind, line: u32) -> Stmt {
        Stmt { kind, line }
    }
}

/// One `except [exc] [as <local>]:` arm. `exc == None` is a bare `except:`.
#[derive(Clone, Debug)]
pub struct Handler {
    pub exc: Option<ExcKind>,
    pub binding: Option<LocalId>,
    pub body: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    /// Evaluate an expression for its side effects (calls).
    Expr(Expr),
    Assign {
        target: LocalId,
        value: Expr,
    },
    AugAssign {
        target: LocalId,
        op: BinOp,
        value: Expr,
    },
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    /// `for var in range(start, stop[, step])`.
    ForRange {
        var: LocalId,
        start: Expr,
        stop: Expr,
        step: Option<Expr>,
        body: Vec<Stmt>,
    },
    /// `for var in <iterable>` over a runtime iterable (list, str, dict).
    ForEach {
        var: LocalId,
        iterable: Expr,
        body: Vec<Stmt>,
    },
    /// `a, b, c = value`. The value must be a list or tuple of exactly
    /// `targets.len()` elements; codegen checks the length once and then
    /// reads the elements by index.
    Unpack {
        targets: Vec<LocalId>,
        value: Expr,
    },
    /// `object[index] = value`. Python evaluates `value` first, then
    /// `object`, then `index`; codegen must preserve that order.
    SetSubscript {
        object: Expr,
        index: Expr,
        value: Expr,
    },
    /// `def inner(...)` inside a function: create a closure value from
    /// `function` and the current values of `captures` (in this order) and
    /// store it in `target`.
    Closure {
        target: LocalId,
        function: FunctionId,
        captures: Vec<LocalId>,
    },
    /// `class Name(Base):` with already-lowered method functions. The runtime
    /// class object is stored in `target`.
    ClassDef {
        target: LocalId,
        name: String,
        base: Option<Expr>,
        methods: Vec<ClassMethod>,
    },
    /// `object.name = value`.
    SetAttr {
        object: Expr,
        name: String,
        value: Expr,
    },
    Return(Option<Expr>),
    Break,
    Continue,
    Pass,
    /// `yield [value]` inside a generator function. Suspends the generator
    /// fiber until the iterator is advanced again and stores `value` (or
    /// `None` for a bare yield) as the currently yielded element.
    Yield {
        value: Option<Expr>,
    },
    /// `raise` when both fields are `None` (bare re-raise), otherwise
    /// `raise ExcKind(msg)` or `raise ExcKind`.
    Raise {
        exc: Option<ExcKind>,
        msg: Option<Expr>,
    },
    /// `try: ... except ...: ... else: ... finally: ...`.
    /// At least one of `handlers` and `finally_body` is non-empty. `else_body`
    /// runs only after a normal, exception-free body; exceptions raised there
    /// are not caught by this statement's handlers but do run `finally_body`.
    Try {
        body: Vec<Stmt>,
        handlers: Vec<Handler>,
        else_body: Vec<Stmt>,
        finally_body: Vec<Stmt>,
    },
}

/// One method entry of a class definition.
#[derive(Clone, Debug)]
pub struct ClassMethod {
    pub name: String,
    pub function: FunctionId,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub local: LocalId,
    pub name: String,
    pub ty: Type,
}

#[derive(Clone, Debug)]
pub struct LocalInfo {
    pub name: String,
    pub ty: Type,
}

/// A function. The module body is a function too (see [`Program`]).
#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub params: Vec<Param>,
    /// Number of leading `params` that are closure captures. Those parameters
    /// are supplied by the runtime from the function value's captured values;
    /// the user-visible arity is `params.len() - capture_count`.
    pub capture_count: usize,
    pub ret_ty: Type,
    pub locals: Vec<LocalInfo>,
    pub body: Vec<Stmt>,
    /// `async def`: calling the function creates a coroutine object instead
    /// of executing the body.
    pub is_async: bool,
    /// The body contains a `yield`: calling the function creates a generator
    /// object instead of executing the body.
    pub is_generator: bool,
}

impl Function {
    pub fn local(&self, id: LocalId) -> &LocalInfo {
        &self.locals[id as usize]
    }
}

/// A fully typed compilation unit.
#[derive(Clone, Debug)]
pub struct Program {
    pub source_name: String,
    pub functions: Vec<Function>,
    /// Always `0` in Phase 1; kept explicit for later module splitting.
    pub main: FunctionId,
}

impl Program {
    pub fn main_function(&self) -> &Function {
        &self.functions[self.main as usize]
    }

    pub fn function(&self, id: FunctionId) -> &Function {
        &self.functions[id as usize]
    }
}
