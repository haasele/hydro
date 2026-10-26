//! Typed tree: expressions, statements, functions and the program itself.
//!
//! The frontend produces a [`Program`] with fully resolved local slots,
//! function ids and [`Type`]s. The codegen must not perform name resolution or
//! type inference; it only lowers this tree to machine code.
//!
//! Invariants:
//! - [`Program::main`] points at the entry module body: a function named
//!   `<module>` with no parameters and return type [`Type::None`]. Single-
//!   source programs always have it at index `0`; multi-module programs emit
//!   imported module bodies first and list them in
//!   [`Program::initializers`].
//! - Every [`LocalId`] indexes the enclosing function's `locals` vector.
//!   Parameters are locals too and appear first in `locals`.
//! - Each [`FunctionId`] indexes `Program::functions`.
//! - `line` is 1-based and refers to the source file given by
//!   [`Program::source_name`].
//! - Nested functions are closures: [`StmtKind::Closure`] captures the
//!   current values of enclosing locals, or the cell (`StmtKind::CellInit`)
//!   for bindings that are mutated through `nonlocal`. Classes are limited to a single
//!   optional base, methods (plain, property, classmethod or staticmethod) and
//!   attributes frozen at class creation.

use crate::model::{
    BinOp, BoolOp, Builtin, Callee, Const, ExcKind, FunctionId, LocalId, Type, UnOp,
};

/// One binding target of a sequence assignment.
///
/// Nested targets and attribute/subscript targets are bound left to right in
/// source order. Receiver expressions are evaluated when that target binds
/// (never before the source), so codegen must keep the extracted element alive
/// while the receiver expression runs.
#[derive(Clone, Debug)]
pub enum UnpackTarget {
    /// Bind a local slot.
    Name(LocalId),
    /// Bind through the closure cell stored in a local slot.
    Cell(LocalId),
    /// `*name = element`: `element` is a fresh list holding the elements the
    /// plain targets around it did not consume (`a, *rest, b = value`). At
    /// most one starred target is allowed, and it never nests inside another
    /// container target.
    Star(Box<UnpackTarget>),
    /// `object.name = element`.
    Attr { object: Expr, name: String },
    /// `object[index] = element`.
    Subscript { object: Expr, index: Expr },
    /// `a, (b, c) = ...` inside an outer sequence target.
    Nested(Vec<UnpackTarget>),
}

/// Target binding inside a comprehension clause.
#[derive(Clone, Debug)]
pub enum CompTarget {
    Name(LocalId),
    /// `for a, b in ...` (nested and attribute/subscript elements included).
    Unpack(Vec<UnpackTarget>),
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
    /// Read through a closure cell stored in `local` (`hydro_cell_get`).
    CellGet(LocalId),
    /// Store `value` into the closure cell held by `local` and yield the
    /// stored value (`x := v` where `x` is a captured mutable local).
    CellSet {
        target: LocalId,
        value: Box<Expr>,
    },
    /// `(target := value)` (an assignment expression). Evaluates `value`,
    /// stores it in the local `target` and yields the same value. The local is
    /// the enclosing function's slot, so later reads of the name in that scope
    /// observe it. Chained comparisons use this node to evaluate each middle
    /// operand exactly once.
    NamedExpr {
        target: LocalId,
        value: Box<Expr>,
    },
    /// Start of a proved string accumulator: allocates a mutable builder
    /// seeded with `init`, which must be a `str`. Emitted only by the
    /// frontend's string-accumulator pass for a hidden local the program
    /// never observes directly; every other `str` operation keeps using
    /// [`ExprKind::Bin`].
    StrBuilderNew {
        init: Box<Expr>,
    },
    /// Appends `value` to the builder and yields the builder again: the
    /// exact semantics of `accumulator = accumulator + value`, but without
    /// copying the accumulated text. The runtime preserves `+` semantics
    /// (including the raised `TypeError`) when `value` is not a `str`.
    StrBuilderAppend {
        builder: Box<Expr>,
        value: Box<Expr>,
    },
    /// Ends a builder and yields the accumulated text as a normal `str`.
    /// Generated code calls this exactly once, right after the loop; a repeated
    /// call on an already emptied builder yields the empty string. A builder
    /// that never received an append yields its seed.
    StrBuilderFinish {
        builder: Box<Expr>,
    },
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
    /// `then if test else orelse`. Only the selected branch is evaluated.
    IfExp {
        test: Box<Expr>,
        then: Box<Expr>,
        orelse: Box<Expr>,
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
    /// Reference to a built-in exception class as a first-class value
    /// (`x = ValueError`, `class E(Exception):`). Codegen materialises the
    /// runtime's cached native class object for `kind` through the hidden
    /// `hydro_exception_class` ABI. `raise`/`except` keep their tag-based
    /// lowering and never produce this node.
    ExceptionClass(ExcKind),
    /// `module.function(args...)` resolved from a builtin module.
    ///
    /// `keywords` holds keyword arguments kept by name because the spec
    /// accepts a dynamic keyword set (`functools.partial`). Calls whose spec
    /// only lists fixed parameter names bind those keywords into `args` and
    /// leave this empty.
    ModuleCall {
        module: String,
        function: String,
        args: Vec<Expr>,
        keywords: Vec<(String, Expr)>,
    },
    /// Reference to a builtin module as a value (`m = errno`, `getattr(errno,
    /// ...)`). Codegen materializes the runtime module object.
    ModuleRef {
        module: String,
    },
    /// Name bound by an import nested inside a function/class/compound body
    /// that was not linked at compile time. Evaluating it calls the runtime
    /// `hydro_deferred_import` helper, which raises `RuntimeError`: Hydro cannot
    /// execute a dynamic import. `member` is the imported name (`None` for
    /// `import x as y`, where the whole module is the binding).
    DeferredImport {
        module: String,
        member: Option<String>,
    },
    /// `module.name` (constant or callable value) from a builtin module.
    ModuleAttr {
        module: String,
        name: String,
    },
    /// Read of a module-level name from inside a function. `module` is the
    /// compilation unit's module name (`__main__` for the entry unit).
    GlobalGet {
        module: String,
        name: String,
    },
    /// `ext.name` value from a compiled CPython extension module identified
    /// by the shared-object path resolved at compile time.
    ExtensionAttr {
        path: String,
        member: String,
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
        /// `callee(args..., *star)`: the single starred positional operand.
        /// It is evaluated after `args` (which hold the operands that appear
        /// before it) and the runtime flattens a list/tuple into positional
        /// arguments, raising `TypeError` for any other value.
        star: Option<Box<Expr>>,
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
    /// `object.method(args...)`. A bare `object.method` without a call is a
    /// [`ExprKind::GetAttr`] and raises `AttributeError` at runtime.
    MethodCall {
        object: Box<Expr>,
        method: String,
        args: Vec<Expr>,
        /// `object.method(*star, ...)`: the single starred positional operand.
        /// It is evaluated after `args` (which must then be empty, see the
        /// frontend restriction) and before `keywords`; the runtime flattens a
        /// list/tuple into positional arguments.
        star: Option<Box<Expr>>,
        /// Keyword arguments in source order; the runtime binds them.
        keywords: Vec<(String, Expr)>,
    },
    Call {
        callee: Callee,
        args: Vec<Expr>,
    },
    /// Call through a function value with keyword arguments:
    /// `f(args..., name=value, ...)`. Codegen forwards the argument/keyword
    /// buffers to the runtime, which binds them through the function's
    /// metadata and full-argv trampoline.
    CallKw {
        callee: Box<Expr>,
        args: Vec<Expr>,
        keywords: Vec<(String, Expr)>,
    },
    /// `Class(args..., name=value, **kwargs)` for a class declared in the
    /// unit whose `__init__` has a dynamic signature. The frontend binds the
    /// call into the concrete `__init__` parameter slots (`self` is supplied
    /// by codegen); `init` is that function. `class` is evaluated first, then
    /// the arguments in source order.
    ClassCall {
        class: Box<Expr>,
        init: FunctionId,
        /// Values for every `__init__` parameter after `self`, in parameter
        /// order (defaults already filled).
        args: Vec<Expr>,
        /// `**mapping` that must be routed to named keyword-only/positional
        /// slots at runtime (its keys are not known statically).
        dynamic_kwargs: Option<Box<Expr>>,
        /// Bindable parameter names after `self`, in `args` order. Variadic
        /// `*args`/`**kwargs` slots carry their names but never receive a
        /// mapping key.
        param_names: Vec<String>,
        /// Index in `args` of the `**kwargs` slot, if any.
        kwarg_index: Option<usize>,
        /// Class name for runtime error messages.
        class_name: String,
    },
    /// A call the frontend could not bind statically (`f(*args, **kwargs)` on
    /// a callee with a dynamic signature). Every operand is evaluated in
    /// source order, then the runtime raises
    /// `RuntimeError("dynamic call ... is not supported")`.
    DeferredCall {
        /// Callee expression when it has side effects worth preserving
        /// (`obj.method(**kw)` evaluates `obj`). `None` when the callee is a
        /// statically known name that evaluates to nothing.
        callee: Option<Box<Expr>>,
        /// Positional operands, the elements of a starred operand, and the
        /// values of keyword arguments, in source order.
        args: Vec<Expr>,
    },
    /// `await value` inside an `async def` body. The runtime drives the
    /// awaitable and returns its result.
    Await {
        value: Box<Expr>,
    },
    /// `yield [value]` used as an expression. Its value is the value sent by
    /// the resumer (`None` for plain iteration); a statement-level `yield`
    /// keeps using [`StmtKind::Yield`].
    Yield {
        value: Option<Box<Expr>>,
    },
    /// `yield from iterable`: delegates iteration to `iterable`; the
    /// expression evaluates to the delegate's return value.
    YieldFrom {
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

/// One target of a `del` statement.
///
/// Python evaluates each target's receiver (and index or slice bounds) in
/// source order and performs the deletion immediately, before the next target
/// is evaluated. Codegen must preserve that order and stop at the first
/// failing target. Missing attributes, keys, indices and out-of-range list
/// positions raise at runtime; immutable receivers (`str`, `bytes`, `tuple`)
/// and class namespaces reject deletion (`TypeError`).
#[derive(Clone, Debug)]
pub enum DeleteTarget {
    /// Delete a local slot binding. Deleting a name with no slot is a
    /// compile-time diagnostic; at runtime the slot is cleared to `None`
    /// (documented limitation: Hydro does not track unbound locals, so a later
    /// read yields `None` instead of raising `NameError`).
    Name(LocalId),
    /// Clear the closure cell held by a local slot to `None`.
    Cell(LocalId),
    /// Delete a module-level binding from the runtime global table
    /// (`global name; del name`, or `del name` in a module body). A missing
    /// entry raises `NameError`.
    Global { module: String, name: String },
    /// `del object.name`: evaluates `object`, then removes the instance
    /// attribute.
    Attr { object: Expr, name: String },
    /// `del object[index]`: evaluates `object`, then `index`, then removes the
    /// list element or dict entry.
    Subscript { object: Expr, index: Expr },
    /// `del object[start:stop:step]`: evaluates `object` and then the bounds
    /// in source order, then removes the list slice. An omitted bound is a
    /// `Const::None` expression, as in [`StmtKind::SetSlice`].
    Slice {
        object: Box<Expr>,
        start: Box<Expr>,
        stop: Box<Expr>,
        step: Box<Expr>,
    },
}

/// One `except [exc] [as <local>]:` arm. `exc == None` is a bare `except:`.
///
/// `exc` is always a runtime tag. The frontend approximates handlers that name
/// a user exception class (declared in the unit, imported, or dotted such as
/// `errors.StyleSyntaxError`) with [`ExcKind::RuntimeError`], because user
/// classes have no runtime tag; builtin names keep their exact tag.
#[derive(Clone, Debug)]
pub struct Handler {
    pub exc: Option<ExcKind>,
    pub binding: Option<LocalId>,
    /// The binding local holds a closure cell; store the exception through it.
    pub binding_cell: bool,
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
    /// Allocate a closure cell holding `value` (or `None`) and store it in
    /// `target`. Emitted at function entry for every local that is captured
    /// and mutated, and again at the top of a loop body for per-iteration
    /// cells.
    CellInit {
        target: LocalId,
        value: Option<Expr>,
    },
    /// Store `value` through the closure cell held by `target`.
    CellAssign {
        target: LocalId,
        value: Expr,
    },
    /// Store a module-level name so functions of the same unit can read it.
    /// Emitted right after the module-body assignment; `value` is normally a
    /// read of the just-assigned local, so the right-hand side is evaluated
    /// once.
    GlobalSet {
        module: String,
        name: String,
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
    /// `while cond:` with an optional `else:` clause.
    ///
    /// `else_body` runs only when the loop finishes by evaluating `cond` as
    /// false: a `break`, `return`, `raise` (or any other exception) in `body`
    /// skips it. `continue` never skips it, because the loop may still
    /// exhaust normally afterwards.
    While {
        cond: Expr,
        body: Vec<Stmt>,
        else_body: Vec<Stmt>,
    },
    /// `for var in range(start, stop[, step])`, with an optional `else:`.
    ///
    /// `else_body` runs only when the iterator is exhausted: `break`,
    /// `return` and exceptions in `body` skip it.
    ForRange {
        var: LocalId,
        start: Expr,
        stop: Expr,
        step: Option<Expr>,
        body: Vec<Stmt>,
        else_body: Vec<Stmt>,
    },
    /// `for var in <iterable>` over a runtime iterable (list, str, dict), with
    /// an optional `else:`.
    ///
    /// `else_body` runs only when the iterator is exhausted: `break`,
    /// `return` and exceptions in `body` skip it.
    ForEach {
        var: LocalId,
        iterable: Expr,
        body: Vec<Stmt>,
        else_body: Vec<Stmt>,
    },
    /// `a, b, c = value`. `value` may be any iterable; codegen materialises it
    /// (lists and tuples are used as-is) and checks that it has exactly
    /// `targets.len()` elements before binding the targets left to right.
    /// Nested targets check their own element count when they bind.
    Unpack {
        targets: Vec<UnpackTarget>,
        value: Expr,
    },
    /// `object[index] = value`. Python evaluates `value` first, then
    /// `object`, then `index`; codegen must preserve that order.
    SetSubscript {
        object: Expr,
        index: Expr,
        value: Expr,
    },
    /// `target[start:stop:step] = value`. As with [`StmtKind::SetSubscript`],
    /// Python evaluates `value` first, then the target expression, then the
    /// bounds in source order (start, stop, step); codegen must preserve that
    /// order. An omitted bound is represented by a `Const::None` expression,
    /// which lowers to the runtime `None` sentinel.
    SetSlice {
        target: Expr,
        start: Expr,
        stop: Expr,
        step: Expr,
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
    /// `class Name(Base):` with already-lowered method functions and class
    /// attribute values. The runtime class object is stored in `target`.
    ///
    /// Attribute values are evaluated in declaration order when the class
    /// statement runs; the resulting class namespace is immutable.
    ClassDef {
        target: LocalId,
        name: String,
        base: Option<Expr>,
        methods: Vec<ClassMethod>,
        attributes: Vec<ClassAttr>,
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
        /// `raise ... from cause` (explicit exception chaining). `None` means
        /// no explicit cause; `Some(None)` is represented by a `None`
        /// constant expression, which suppresses chaining.
        cause: Option<Expr>,
    },
    /// `assert test[, msg]`: when `test` is falsy, `AssertionError` is raised
    /// with `msg` as its message. Python evaluates `test` first and `msg` only
    /// when the test fails, so codegen must keep `msg` inside the failure
    /// branch.
    Assert {
        test: Expr,
        msg: Option<Expr>,
    },
    /// `del a, obj.name, obj[key], obj[start:stop:step]`: evaluates and
    /// deletes each target left to right. See [`DeleteTarget`] for the exact
    /// evaluation order and the runtime error contract.
    Delete {
        targets: Vec<DeleteTarget>,
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

/// How a class-body method binds its receiver when it is called.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MethodKind {
    /// `def method(self, ...)`: `instance.method(...)` binds the instance.
    #[default]
    Normal,
    /// `@property`: `instance.name` calls the getter with the instance.
    Property,
    /// `@classmethod`: the class object is bound as the first argument.
    Classmethod,
    /// `@staticmethod`: no receiver is bound.
    Staticmethod,
}

impl MethodKind {
    /// Numeric tag used by the `hydro_class_new` ABI.
    pub fn abi_tag(self) -> i64 {
        match self {
            MethodKind::Normal => 0,
            MethodKind::Property => 1,
            MethodKind::Classmethod => 2,
            MethodKind::Staticmethod => 3,
        }
    }

    /// Decodes a `hydro_class_new` method-kind tag; `None` for an unknown tag.
    pub fn from_abi_tag(tag: i64) -> Option<MethodKind> {
        match tag {
            0 => Some(MethodKind::Normal),
            1 => Some(MethodKind::Property),
            2 => Some(MethodKind::Classmethod),
            3 => Some(MethodKind::Staticmethod),
            _ => None,
        }
    }
}

/// One method entry of a class definition.
#[derive(Clone, Debug)]
pub struct ClassMethod {
    pub name: String,
    pub function: FunctionId,
    pub kind: MethodKind,
}

/// One class-level attribute frozen into the class namespace when the class
/// statement runs.
#[derive(Clone, Debug)]
pub struct ClassAttr {
    pub name: String,
    pub value: Expr,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub local: LocalId,
    pub name: String,
    pub ty: Type,
    /// Literal default (`def f(x=1)`). Direct calls fill omitted parameters
    /// with it; the runtime binder of a dynamic signature uses it for
    /// calls through a function value.
    pub default: Option<Const>,
    /// Non-literal source default, evaluated where the function value is
    /// created (and at direct call sites).
    pub default_expr: Option<Box<Expr>>,
    /// Position of the parameter in the signature. Parameters are ordered
    /// captures, positional, `*args`, keyword-only, `**kwargs`.
    pub kind: ParamKind,
}

/// Binding kind of a [`Param`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ParamKind {
    /// Ordinary positional parameter (or a closure capture).
    #[default]
    Positional,
    /// `*args`: the slot always receives a list.
    VarArg,
    /// Keyword-only parameter: its slot receives the keyword value or the
    /// literal default.
    KwOnly,
    /// `**kwargs`: the slot always receives a dict.
    KwArg,
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

    /// Absolute index of the first parameter that is not positional (the
    /// `*args` slot, a keyword-only parameter, or the `**kwargs` slot).
    pub fn positional_end(&self) -> usize {
        self.params[self.capture_count.min(self.params.len())..]
            .iter()
            .position(|param| param.kind != ParamKind::Positional)
            .map(|offset| self.capture_count + offset)
            .unwrap_or(self.params.len())
    }

    /// Absolute index of the keyword-only range end (the `**kwargs` slot or
    /// the parameter count).
    pub fn kwonly_end(&self) -> usize {
        self.params
            .iter()
            .position(|param| param.kind == ParamKind::KwArg)
            .unwrap_or(self.params.len())
    }

    /// Absolute index of the `*args` slot.
    pub fn vararg(&self) -> Option<usize> {
        self.params
            .iter()
            .position(|param| param.kind == ParamKind::VarArg)
    }

    /// Absolute index of the `**kwargs` slot.
    pub fn kwarg(&self) -> Option<usize> {
        self.params
            .iter()
            .position(|param| param.kind == ParamKind::KwArg)
    }

    /// True when the signature cannot be bound by position alone: it has
    /// `*args`, `**kwargs` or keyword-only parameters. Such functions are
    /// called through the runtime binder when the caller only has a function
    /// value.
    pub fn has_dynamic_signature(&self) -> bool {
        self.params
            .iter()
            .any(|param| param.kind != ParamKind::Positional)
    }

    /// Keyword-only parameter range.
    pub fn kwonly_params(&self) -> impl Iterator<Item = &Param> {
        let end = self.kwonly_end();
        let start = self.params[..end]
            .iter()
            .position(|param| param.kind == ParamKind::KwOnly)
            .unwrap_or(end);
        self.params[start..end]
            .iter()
            .filter(|param| param.kind == ParamKind::KwOnly)
    }

    /// User-visible arity recorded in a function value. For a dynamic
    /// signature this is the minimum number of positional arguments (the
    /// runtime binder accepts more); otherwise it is the exact count.
    pub fn call_arity(&self) -> usize {
        if !self.has_dynamic_signature() {
            return self.params.len().saturating_sub(self.capture_count);
        }
        let start = self.capture_count.min(self.positional_end());
        self.params[start..self.positional_end()]
            .iter()
            .filter(|param| param.default.is_none())
            .count()
    }
}

/// A fully typed compilation unit.
#[derive(Clone, Debug, Default)]
pub struct Program {
    pub source_name: String,
    pub functions: Vec<Function>,
    /// The entry unit's module body. Always `0` in single-source programs;
    /// multi-module programs use the entry unit's position.
    pub main: FunctionId,
    /// Module bodies of every imported user module, in dependency order
    /// (imports before importers, entry excluded). The runtime runs them once
    /// before [`Program::main`].
    pub initializers: Vec<FunctionId>,
}

impl Program {
    pub fn main_function(&self) -> &Function {
        &self.functions[self.main as usize]
    }

    pub fn function(&self, id: FunctionId) -> &Function {
        &self.functions[id as usize]
    }
}
