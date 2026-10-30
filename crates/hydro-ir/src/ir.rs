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
//!   for bindings that are mutated through `nonlocal` or that a nested
//!   closure reads as a `def` name (recursion and mutual recursion), where
//!   `StmtKind::CellAssign` stores the closure value into the cell. Classes
//!   are limited to a single
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
    /// Bind a module-level name in the runtime global table (`global name`
    /// inside a function). The target has no local slot, so the element is
    /// stored through `hydro_global_set` when the target binds.
    Global { module: String, name: String },
    /// `*name = element`: `element` is a fresh list holding the elements the
    /// plain targets around it did not consume (`a, *rest, b = value`). At
    /// most one starred target is allowed per sequence level; a nested
    /// container target may own its own star.
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

/// One `{...}` interpolation of a PEP 750 template string.
///
/// The parser reports template literals out of band (a reserved `Name`
/// placeholder plus a side table), so the IR carries the resolved structure
/// instead of pretending the literal is an f-string. Codegen evaluates
/// `value` and `format_spec` in source order and materialises an
/// `Interpolation` runtime value for each entry.
#[derive(Clone, Debug)]
pub struct TemplateInterpolation {
    /// Expression inside the braces. Its value is the interpolation's
    /// `value` attribute.
    pub value: Expr,
    /// Source text of the expression exactly as written (CPython's
    /// `Interpolation.expression`), used by the runtime attribute and repr.
    pub expression: String,
    /// `!s`/`!r`/`!a` conversion: `0` none, `1` `str`, `2` `repr`,
    /// `3` `ascii`.
    pub conversion: u8,
    /// Expression producing the format-spec text, or `None` when no `:`
    /// spec was written. The expression always evaluates to a `str`.
    pub format_spec: Option<Box<Expr>>,
    /// True when the source used the `=` debug specifier (`t"{x=}"`).
    pub self_documenting: bool,
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
    /// `(name := value)` where `name` is declared `global`. Evaluates `value`,
    /// stores it in the runtime global table of `module` and yields the same
    /// value. The name has no local slot.
    GlobalSet {
        module: String,
        name: String,
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
    /// Reference to a compiled CPython extension module as a value
    /// (`import ext as e`, then `getattr(e, ...)` or `m = e`).
    ///
    /// `module` is the import name and `path` the shared object resolved by
    /// the frontend. Codegen materializes a runtime module object that keeps
    /// the path, so attributes are resolved through the extension ABI.
    ExtensionModuleRef {
        module: String,
        path: String,
    },
    /// Name bound by an import statement that was not linked at compile time:
    /// an import nested inside a function/class/compound body, or an import
    /// inside a `try` whose handlers catch `ImportError`, which must fail at
    /// runtime so the handler runs. Evaluating it calls the runtime
    /// `hydro_deferred_import` helper, which resolves the name from the
    /// runtime module registry and raises `ModuleNotFoundError` (no such
    /// module) or `ImportError` (`cannot import name ...`): Hydro cannot
    /// execute a dynamic import.
    ///
    /// `member` is the imported name (`None` for `import x as y`, where the
    /// whole module is the binding). `path` is the shared object of a compiled
    /// extension when the import names one, so the runtime can materialise the
    /// extension module value and report a missing attribute as `ImportError`.
    DeferredImport {
        module: String,
        member: Option<String>,
        path: Option<String>,
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
    /// `object.method(args..., *star, kw=value..., **double_star)`.
    MethodCall {
        object: Box<Expr>,
        method: String,
        args: Vec<Expr>,
        /// `object.method(*star, ...)`: the single starred positional
        /// operand. It is evaluated after `args` and before `keywords`; the
        /// runtime flattens a list/tuple into positional arguments.
        star: Option<Box<Expr>>,
        /// Keyword arguments in source order; the runtime binds them.
        keywords: Vec<(String, Expr)>,
        /// A single `**mapping` operand, evaluated after the named keywords.
        /// Present only together with `star` or a non-empty keyword list; the
        /// runtime merges it into the keyword set and raises `TypeError` for a
        /// non-mapping, a non-`str` key or a duplicate key.
        double_star: Option<Box<Expr>>,
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
    /// `f(args..., *star, kw=value..., **double_star)` through a function
    /// value. Used whenever an operand shape needs runtime flattening that
    /// [`ExprKind::IndirectCall`] (no keywords) or [`ExprKind::CallKw`] (no
    /// starred operand) cannot express.
    ///
    /// Evaluation order is Python's: `callee`, then every positional operand
    /// in source order (positional operands written after the starred one are
    /// folded into the star operand by the frontend), then each named keyword
    /// value, then the `**mapping`. Codegen forwards
    /// the buffers to `hydro_call_star_kw`, which flattens `star`, merges the
    /// keywords and raises `TypeError` for a non-mapping `double_star`, a
    /// non-`str` key or a key bound twice. A class, function, builtin or
    /// native function value is dispatched by the shared call protocol.
    CallFull {
        callee: Box<Expr>,
        args: Vec<Expr>,
        star: Option<Box<Expr>>,
        keywords: Vec<(String, Expr)>,
        double_star: Option<Box<Expr>>,
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
    /// PEP 750 template string literal (`t"..."`, `rt"..."`).
    ///
    /// `strings.len() == interpolations.len() + 1`, matching CPython's
    /// `Template.strings` invariant. This is a distinct expression on
    /// purpose: a template is not a `str` and must never be lowered to an
    /// f-string, a normal variable or a string concatenation. Codegen builds
    /// a runtime `Template` through `hydro_template_new` and evaluates every
    /// interpolation (value, then format spec) in source order.
    Template {
        /// Literal parts in source order, decoded exactly like an f-string
        /// constant part.
        strings: Vec<String>,
        /// `{...}` fields in source order.
        interpolations: Vec<TemplateInterpolation>,
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
    ///
    /// `target` never holds a cell: a definition whose binding is a cell
    /// (recursion or mutual recursion, where the closure captures the cell)
    /// creates the value in a hidden local and stores it with
    /// [`StmtKind::CellAssign`], so the cell keeps holding the cell object.
    Closure {
        target: LocalId,
        function: FunctionId,
        captures: Vec<LocalId>,
    },
    /// `class Name(bases...):` with already-lowered method functions and
    /// class attribute values. The runtime class object is stored in
    /// `target`.
    ///
    /// Attribute values are evaluated in declaration order when the class
    /// statement runs; the resulting class namespace is immutable.
    ClassDef {
        target: LocalId,
        name: String,
        /// Base expressions in source order (at most one for the single-base
        /// `hydro_class_new` path; more use `hydro_class_new_multi`).
        bases: Vec<Expr>,
        /// `metaclass=value` keyword, evaluated after the bases and before
        /// the class body (CPython order). `None` selects the metatype of the
        /// bases at runtime.
        metaclass: Option<Expr>,
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
    ///
    /// `exc: None, msg: Some(value)` is `raise value`: the expression
    /// evaluates to an exception object that is raised as-is. Keyword
    /// exception constructors (`raise AttributeError(msg, name=None)`) lower
    /// through this form so the runtime constructor validates the keywords.
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
    /// The source wrote a type annotation for this parameter. Unannotated
    /// parameters are gradual: the frontend widens them to `Any` instead of
    /// treating a call argument as their declared type.
    pub declared: bool,
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
    /// `def f(x, /)`: a positional-only parameter. It binds from the
    /// positional argument stream exactly like [`ParamKind::Positional`], but
    /// a keyword argument with the same name never binds it: the keyword goes
    /// to `**kwargs` when the signature has one, otherwise the call raises
    /// `TypeError: ... got an unexpected keyword argument 'x'`. PosOnly
    /// parameters are always the leading run of positional parameters.
    PosOnly,
    /// `*args`: the slot always receives a list.
    VarArg,
    /// Keyword-only parameter: its slot receives the keyword value or the
    /// literal default.
    KwOnly,
    /// `**kwargs`: the slot always receives a dict.
    KwArg,
}

impl ParamKind {
    /// True for parameters filled from the positional argument stream
    /// (`Positional` and `PosOnly`).
    pub fn is_positional(self) -> bool {
        matches!(self, ParamKind::Positional | ParamKind::PosOnly)
    }

    /// Stable encoding used by the `hydro_function_meta` ABI.
    ///
    /// `4` is positional-only; the remaining tags keep their historical
    /// values so existing metadata stays decodable.
    pub fn abi_tag(self) -> i64 {
        match self {
            ParamKind::Positional => 0,
            ParamKind::VarArg => 1,
            ParamKind::KwOnly => 2,
            ParamKind::KwArg => 3,
            ParamKind::PosOnly => 4,
        }
    }
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
            .position(|param| !param.kind.is_positional())
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
        self.params.iter().any(|param| !param.kind.is_positional())
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

    /// Module key owning `function`, matching the key the frontend uses for
    /// its `GlobalGet`/`GlobalSet` operations.
    ///
    /// The frontend emits every imported unit's functions as one contiguous
    /// block that starts at that unit's module body (an entry of
    /// [`Program::initializers`]) and prefixes their names with the dotted
    /// module name (`pkg.mod.<module>`, `pkg.mod.some_function`). The entry
    /// unit's block starts at [`Program::main`] and uses `"__main__"`.
    /// Frame introspection (`sys._getframe().f_globals`) uses this to resolve
    /// the module dictionary a frame's code belongs to.
    pub fn module_of(&self, function: FunctionId) -> &str {
        let index = function as usize;
        if index >= self.functions.len() {
            return "__main__";
        }
        if index >= self.main as usize {
            return "__main__";
        }
        let start = self
            .initializers
            .iter()
            .map(|&id| id as usize)
            .filter(|&id| id <= index && id < self.functions.len())
            .max();
        let Some(start) = start else {
            return "__main__";
        };
        let name = self.functions[start].name.as_str();
        match name.rsplit_once(".<module>") {
            Some((module, _)) if !module.is_empty() => module,
            _ => "__main__",
        }
    }
}
