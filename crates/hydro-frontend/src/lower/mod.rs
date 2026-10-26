//! AST lowering: name resolution, local-slot assignment and unsupported
//! construct rejection.
//!
//! The untyped-but-resolved HIR keeps type slots ([`Ty`]) that the inference
//! pass in `sema` fills in. Lowering never panics on user input; every rejected
//! construct becomes a [`Diagnostic`].

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use hydro_ir::stdlib::{self, ConstantSpec, FunctionSpec, ModuleSpec};
use hydro_ir::{
    BinOp, Builtin, Callee, Const, ExcKind, FunctionId, LocalId, MethodKind, ParamKind, Type,
};
use hydro_ir::{PYTHON_ABI_TAG, PYTHON_VERSION_STRING};
use rustpython_parser::ast::{self, Ranged};
use rustpython_parser::template::{
    is_template_placeholder, template_placeholder_range, TemplateString,
};
use rustpython_parser::text_size::TextRange;

use crate::diag::{self, codes, line_col, Diagnostic};
use crate::sema::{Infer, Ty};

pub const MODULE_NAME: &str = "<module>";

const BUILTIN_NAMES: [&str; 29] = [
    "print",
    "len",
    "str",
    "int",
    "float",
    "abs",
    "set",
    "range",
    "list",
    "dict",
    "sum",
    "enumerate",
    "zip",
    "map",
    "filter",
    "bytes",
    "bytearray",
    "memoryview",
    "classmethod",
    "staticmethod",
    "property",
    "iter",
    "next",
    "all",
    "sorted",
    "ord",
    "chr",
    "input",
    "get_ipython",
];

/// A resolved source unit prior to type inference.
#[derive(Clone, Debug)]
pub struct LHir {
    pub source_name: String,
    pub functions: Vec<HFunction>,
}

#[derive(Clone, Debug)]
pub struct HFunction {
    pub name: String,
    pub params: Vec<HParam>,
    /// Leading parameters that receive captured values (nested functions).
    pub capture_count: usize,
    pub ret_ty: Ty,
    pub locals: Vec<HLocal>,
    pub body: Vec<HStmt>,
    /// `async def`: calling the function creates a coroutine object instead
    /// of running the body. Together with `is_generator` it marks an async
    /// generator.
    pub is_async: bool,
    /// The body contains a `yield`: calling the function creates a generator
    /// object instead of running the body. An async generator sets both
    /// flags.
    pub is_generator: bool,
    pub is_module: bool,
    pub line: u32,
    pub col: u32,
}

#[derive(Clone, Debug)]
pub struct HParam {
    pub local: LocalId,
    pub name: String,
    pub ty: Ty,
    /// Literal default value (`def f(x=1)`). Direct calls fill omitted
    /// trailing parameters with it; first-class calls still require the full
    /// positional arity.
    pub default: Option<Const>,
    /// Source default expression when it is not a literal. It is evaluated
    /// where the function/method value is created and used by the runtime
    /// keyword binder; direct calls evaluate it at the call site.
    pub default_expr: Option<HExpr>,
    /// Binding kind; parameters are ordered captures, positional, `*args`,
    /// keyword-only, `**kwargs`.
    pub kind: ParamKind,
}

#[derive(Clone, Debug)]
pub struct HLocal {
    pub name: String,
    pub ty: Ty,
}

/// One binding target of a sequence assignment.
///
/// Targets bind left to right in source order; the receivers of attribute and
/// subscript targets are evaluated at binding time, after the source value and
/// after every earlier target.
#[derive(Clone, Debug)]
pub enum HUnpackTarget {
    Name(LocalId),
    /// Bind through the closure cell stored in a local slot.
    Cell(LocalId),
    Attr {
        object: HExpr,
        name: String,
    },
    Subscript {
        object: HExpr,
        index: HExpr,
    },
    Nested(Vec<HUnpackTarget>),
    /// `*name = element`: `element` is the fresh list of the elements no
    /// plain target consumed. At most one starred target may appear in one
    /// sequence, and it never nests inside a container target.
    Star(Box<HUnpackTarget>),
}

#[derive(Clone, Debug)]
pub struct HStmt {
    pub kind: HStmtKind,
    pub line: u32,
    pub col: u32,
}

/// One target of a `del` statement: a local name, a module global, an
/// attribute, a subscript or a slice. Receivers and bounds are lowered
/// expressions, so they are evaluated in source order at runtime.
#[derive(Clone, Debug)]
pub enum HDeleteTarget {
    Name(LocalId),
    /// Clear the closure cell held by a local slot to `None`.
    Cell(LocalId),
    /// Module-level binding in the runtime global table (`global name` in a
    /// function, or the mirror of a module-body assignment).
    Global {
        module: String,
        name: String,
    },
    Attr {
        object: HExpr,
        name: String,
    },
    Subscript {
        object: HExpr,
        index: HExpr,
    },
    /// `del object[start:stop:step]`; omitted bounds stay `None` and become
    /// `Const::None` in the IR.
    Slice {
        object: HExpr,
        start: Option<Box<HExpr>>,
        stop: Option<Box<HExpr>>,
        step: Option<Box<HExpr>>,
    },
}

#[derive(Clone, Debug)]
pub enum HStmtKind {
    Expr(HExpr),
    Assign {
        target: LocalId,
        value: HExpr,
    },
    /// Allocate a closure cell holding `value` (or `None`) in `target`.
    CellInit {
        target: LocalId,
        value: Option<HExpr>,
    },
    /// Store `value` through the closure cell held by `target`.
    CellAssign {
        target: LocalId,
        value: HExpr,
    },
    /// `a, b, c = value`: `value` is any iterable with exactly
    /// `targets.len()` elements.
    Unpack {
        targets: Vec<HUnpackTarget>,
        value: HExpr,
    },
    /// Store a module-level name after its module-body assignment so
    /// functions of the same unit can read it back.
    GlobalSet {
        module: String,
        name: String,
        value: HExpr,
    },
    AugAssign {
        target: LocalId,
        op: BinOp,
        value: HExpr,
    },
    If {
        cond: HExpr,
        then_body: Vec<HStmt>,
        else_body: Vec<HStmt>,
    },
    /// `while cond:` with an optional `else:` clause that runs only when the
    /// condition becomes false (not after `break`, `return` or an exception).
    While {
        cond: HExpr,
        body: Vec<HStmt>,
        else_body: Vec<HStmt>,
    },
    /// `for var in range(...)` with an optional `else:` clause that runs only
    /// on normal exhaustion (not after `break`, `return` or an exception).
    ForRange {
        var: LocalId,
        start: HExpr,
        stop: HExpr,
        step: Option<HExpr>,
        body: Vec<HStmt>,
        else_body: Vec<HStmt>,
    },
    /// `for var in iterable` with an optional `else:` clause that runs only on
    /// normal exhaustion (not after `break`, `return` or an exception).
    ForEach {
        var: LocalId,
        iterable: HExpr,
        body: Vec<HStmt>,
        else_body: Vec<HStmt>,
    },
    SetSubscript {
        object: HExpr,
        index: HExpr,
        value: HExpr,
    },
    /// `object[start:stop:step] = value`. Omitted bounds stay `None`; the IR
    /// encodes them as `Const::None`. Python evaluates `value` first, then the
    /// object, then the bounds in source order.
    SetSlice {
        object: HExpr,
        start: Option<Box<HExpr>>,
        stop: Option<Box<HExpr>>,
        step: Option<Box<HExpr>>,
        value: HExpr,
    },
    /// `def inner(...)` inside a function: evaluate `captures` (current
    /// values) and create the closure value in `target`.
    Closure {
        target: LocalId,
        function: FunctionId,
        captures: Vec<LocalId>,
    },
    /// `class Name(bases...):` with already-lowered method functions. The
    /// runtime class object is stored in `target`.
    ClassDef {
        target: LocalId,
        name: String,
        /// Base expressions in source order; empty means `object`.
        bases: Vec<HExpr>,
        /// `metaclass=` keyword value, evaluated after the bases.
        metaclass: Option<HExpr>,
        methods: Vec<(String, FunctionId, MethodKind)>,
        /// Class attributes in declaration order, evaluated when the class
        /// statement runs and frozen into the class namespace.
        attributes: Vec<(String, HExpr)>,
    },
    /// `object.name = value`.
    SetAttr {
        object: HExpr,
        name: String,
        value: HExpr,
    },
    Return(Option<HExpr>),
    Break,
    Continue,
    Pass,
    /// `yield [value]` inside a generator body: suspend the fiber and yield
    /// `value` (or `None` for a bare yield).
    Yield {
        value: Option<HExpr>,
    },
    Raise {
        exc: Option<ExcKind>,
        msg: Option<HExpr>,
        /// `raise ... from cause`; `None` for no explicit cause.
        cause: Option<HExpr>,
    },
    /// `assert test[, msg]`: `test` is always evaluated; `msg` only when the
    /// test fails, where it becomes the `AssertionError` message.
    Assert {
        test: HExpr,
        msg: Option<HExpr>,
    },
    /// `del a, obj.name, obj[key], obj[start:stop:step]`: each target is
    /// evaluated and deleted left to right.
    Delete {
        targets: Vec<HDeleteTarget>,
    },
    Try {
        body: Vec<HStmt>,
        handlers: Vec<HHandler>,
        else_body: Vec<HStmt>,
        finally_body: Vec<HStmt>,
    },
}

#[derive(Clone, Debug)]
pub struct HHandler {
    /// `None` for a bare `except:`; tuple handlers are expanded into one
    /// handler per type by the lowering pass.
    pub exc: Option<ExcKind>,
    pub binding: Option<LocalId>,
    /// The binding local holds a closure cell; store the exception through it.
    pub binding_cell: bool,
    pub body: Vec<HStmt>,
}

/// Target binding inside a comprehension clause. Plain names always bind
/// synthetic `$compN` locals (user-visible names are remapped while the
/// comprehension is lowered and restored afterwards); unpack targets may also
/// contain nested containers and attribute/subscript receivers.
#[derive(Clone, Debug)]
pub enum HCompTarget {
    Name(LocalId),
    Unpack(Vec<HUnpackTarget>),
}

/// One `for target in iterable if cond ...` clause of a comprehension.
///
/// `is_async` marks an `async for` clause: the iterable is an asynchronous
/// iterable and each step goes through `await __anext__()`.
#[derive(Clone, Debug)]
pub struct HCompClause {
    pub target: HCompTarget,
    pub iterable: HExpr,
    pub conditions: Vec<HExpr>,
    pub is_async: bool,
}

/// Resolved target of a `for`/`async for` statement.
#[derive(Clone, Debug)]
struct LoopTarget {
    /// Plain-name target, already resolved to its local slot.
    name: Option<LocalId>,
    /// Sequence target, unpacked from the item the loop binds.
    unpack: Option<Vec<HUnpackTarget>>,
}

/// Result collection of an asynchronous comprehension.
enum AsyncCompKind<'a> {
    List(&'a ast::Expr),
    Set(&'a ast::Expr),
    Dict(&'a ast::Expr, &'a ast::Expr),
}

impl<'a> AsyncCompKind<'a> {
    /// Expressions evaluated in the innermost frame, in evaluation order.
    fn elements(&self) -> Vec<&'a ast::Expr> {
        match self {
            AsyncCompKind::List(element) | AsyncCompKind::Set(element) => vec![*element],
            AsyncCompKind::Dict(key, value) => vec![*key, *value],
        }
    }

    fn result_ty(&self) -> Type {
        match self {
            AsyncCompKind::List(_) => Type::List,
            AsyncCompKind::Set(_) => Type::Set,
            AsyncCompKind::Dict(..) => Type::Dict,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HExpr {
    pub kind: HExprKind,
    pub ty: Ty,
    pub line: u32,
    pub col: u32,
}

/// One `{...}` interpolation of a PEP 750 template literal in HIR form.
#[derive(Clone, Debug)]
pub struct HTemplateInterpolation {
    /// Expression inside the braces.
    pub value: HExpr,
    /// Source text of the expression exactly as written.
    pub expression: String,
    /// `!s`/`!r`/`!a` conversion: `0` none, `1` str, `2` repr, `3` ascii.
    pub conversion: u8,
    /// Expression producing the format-spec text, if any.
    pub format_spec: Option<Box<HExpr>>,
    /// True when the source used the `=` debug specifier.
    pub self_documenting: bool,
}

/// Positional arguments plus `(name, value)` keyword pairs of a lowered call.
type LoweredArguments = (Vec<HExpr>, Vec<(String, HExpr)>);

/// Positional arguments, an optional starred operand, keyword pairs and an
/// optional `**mapping` operand of a lowered call.
type LoweredCallOperands = (
    Vec<HExpr>,
    Option<HExpr>,
    Vec<(String, HExpr)>,
    Option<HExpr>,
);

#[derive(Clone, Debug)]
pub enum HExprKind {
    Const(Const),
    Local(LocalId),
    /// Read through the closure cell stored in a local slot.
    CellGet(LocalId),
    /// Store `value` through a closure cell and yield the stored value
    /// (assignment expressions targeting a captured mutable local).
    CellSet {
        target: LocalId,
        value: Box<HExpr>,
    },
    /// `(target := value)`: evaluate `value`, store it in the local `target`
    /// (a slot of the enclosing function, never a comprehension target) and
    /// yield the value.
    NamedExpr {
        target: LocalId,
        value: Box<HExpr>,
    },
    Bin {
        op: BinOp,
        lhs: Box<HExpr>,
        rhs: Box<HExpr>,
    },
    Un {
        op: hydro_ir::UnOp,
        operand: Box<HExpr>,
    },
    BoolOp {
        op: hydro_ir::BoolOp,
        lhs: Box<HExpr>,
        rhs: Box<HExpr>,
    },
    /// `then if test else orelse`. Only the selected branch is evaluated.
    IfExp {
        test: Box<HExpr>,
        then: Box<HExpr>,
        orelse: Box<HExpr>,
    },
    List(Vec<HExpr>),
    Tuple(Vec<HExpr>),
    Set(Vec<HExpr>),
    Dict(Vec<(HExpr, HExpr)>),
    /// `[element for ...]`.
    ListComp {
        element: Box<HExpr>,
        clauses: Vec<HCompClause>,
    },
    /// `{element for ...}`.
    SetComp {
        element: Box<HExpr>,
        clauses: Vec<HCompClause>,
    },
    /// `{key: value for ...}`.
    DictComp {
        key: Box<HExpr>,
        value: Box<HExpr>,
        clauses: Vec<HCompClause>,
    },
    Subscript {
        object: Box<HExpr>,
        index: Box<HExpr>,
    },
    Slice {
        object: Box<HExpr>,
        start: Option<Box<HExpr>>,
        stop: Option<Box<HExpr>>,
        step: Option<Box<HExpr>>,
    },
    MethodCall {
        object: Box<HExpr>,
        method: String,
        args: Vec<HExpr>,
        /// The single starred positional operand (`obj.m(*args)`). The runtime
        /// flattens a list/tuple into positional arguments.
        star: Option<Box<HExpr>>,
        /// Keyword arguments in source order; the runtime binds them.
        keywords: Vec<(String, HExpr)>,
        /// A single `**mapping` operand, evaluated after the named keywords.
        /// Only present in a call that needs the runtime flattener.
        double_star: Option<Box<HExpr>>,
    },
    /// Reference to a named function or lambda as a value.
    FunctionRef(FunctionId),
    /// Reference to a builtin function as a first-class value (`f = int`).
    BuiltinRef(Builtin),
    /// Reference to a built-in exception class as a first-class value
    /// (`x = ValueError`, `class E(Exception):`). `raise`/`except` resolve
    /// exception names through [`ExcKind`] directly and never produce this
    /// node.
    ExceptionClass(ExcKind),
    /// `module.function(args...)` resolved from `hydro_ir::stdlib`.
    ///
    /// `keywords` holds the keyword arguments a spec with
    /// [`FunctionSpec::dynamic_keywords`] forwards to the callee by name
    /// (`functools.partial`). Fixed-parameter keywords are folded into `args`.
    ModuleCall {
        module: String,
        function: String,
        args: Vec<HExpr>,
        keywords: Vec<(String, HExpr)>,
    },
    /// `module.name` (constant or callable value) resolved from `hydro_ir::stdlib`.
    ModuleAttr {
        module: String,
        name: String,
    },
    /// A builtin module used as a value (`m = errno`, `getattr(errno, ...)`).
    ModuleRef {
        module: String,
    },
    /// Name bound by a nested import that was not linked at compile time.
    /// Evaluating it raises `RuntimeError` from the runtime helper.
    DeferredImport {
        module: String,
        member: Option<String>,
    },
    /// Read of a module-level name from inside a function.
    GlobalGet {
        module: String,
        name: String,
    },
    /// `extension.member` / an imported extension member used as a value.
    ///
    /// `path` is the compiled shared object resolved by the frontend and
    /// `member` is the Python attribute name. The value type is dynamic:
    /// a limited-API extension exposes no static type information.
    ExtensionAttr {
        path: String,
        member: String,
    },
    /// `object.name` data attribute of a native value.
    GetAttr {
        object: Box<HExpr>,
        name: String,
    },
    /// Call through a function value; `callee` is evaluated before `args`.
    IndirectCall {
        callee: Box<HExpr>,
        args: Vec<HExpr>,
        /// `callee(args..., *star)`: the single starred positional operand.
        /// It is evaluated after `args`; the runtime flattens a list/tuple
        /// into positional arguments.
        star: Option<Box<HExpr>>,
    },
    Call {
        callee: Callee,
        args: Vec<HExpr>,
    },
    /// Call through a function value with keyword arguments:
    /// `f(args..., name=value, ...)`. Codegen forwards the argument/keyword
    /// buffers to the runtime, which binds them through the function's
    /// metadata and full-argv trampoline.
    CallKw {
        callee: Box<HExpr>,
        args: Vec<HExpr>,
        keywords: Vec<(String, HExpr)>,
    },
    /// `f(args..., *star, kw=value..., **double_star)` through a function
    /// value; see [`hydro_ir::ExprKind::CallFull`]. Emitted whenever a call
    /// needs runtime argument flattening: a starred operand next to keywords
    /// or a `**mapping` operand.
    CallFull {
        callee: Box<HExpr>,
        args: Vec<HExpr>,
        star: Option<Box<HExpr>>,
        keywords: Vec<(String, HExpr)>,
        double_star: Option<Box<HExpr>>,
    },
    /// `Class(args..., name=value, **kwargs)` for a class declared in the
    /// unit whose `__init__` has a dynamic signature. The frontend binds all
    /// `__init__` slots except `self`; codegen allocates the instance and
    /// calls `init` directly.
    ClassCall {
        class: Box<HExpr>,
        init: FunctionId,
        /// Values for every `__init__` parameter after `self`, in parameter
        /// order (defaults already filled).
        args: Vec<HExpr>,
        /// `**mapping` that must be routed to named keyword-only/positional
        /// slots at runtime (its keys are not known statically).
        dynamic_kwargs: Option<Box<HExpr>>,
        /// Bindable parameter names after `self`, in `args` order. Variadic
        /// `*args`/`**kwargs` slots carry their names but never receive a
        /// mapping key.
        param_names: Vec<String>,
        /// Index in `args` of the `**kwargs` slot, if any.
        kwarg_index: Option<usize>,
        /// Class name for runtime error messages.
        class_name: String,
    },
    /// A call the frontend could not bind statically (`f(*args, **kwargs)`,
    /// keyword arguments on a function value or imported signature). The
    /// operands are evaluated in source order; executing the call raises
    /// `RuntimeError`.
    DeferredCall {
        callee: Option<Box<HExpr>>,
        args: Vec<HExpr>,
    },
    /// `await value` inside an `async def` body.
    Await {
        value: Box<HExpr>,
    },
    /// `yield value` in an expression position (`x = yield v`). Suspends the
    /// generator and evaluates to the value sent on the next resume.
    Yield {
        value: Option<Box<HExpr>>,
    },
    /// `yield from iterable`: delegates iteration to `iterable`; the
    /// expression evaluates to the delegate's return value.
    YieldFrom {
        value: Box<HExpr>,
    },
    /// PEP 750 template string literal. See [`hydro_ir::ExprKind::Template`].
    Template {
        strings: Vec<String>,
        interpolations: Vec<HTemplateInterpolation>,
    },
}

/// Result of binding a dynamic-signature class constructor call.
struct ClassCallBind {
    /// Values for every `__init__` parameter after `self`.
    args: Vec<HExpr>,
    /// Routing mapping for a `**mapping` operand whose keys are dynamic.
    dynamic_kwargs: Option<HExpr>,
    /// Bindable parameter names in `args` order (variadic slots are empty).
    param_names: Vec<String>,
    /// Index in `args` of the `**kwargs` slot.
    kwarg_index: Option<usize>,
}

/// Signature-relevant fields shared by `def` and `async def`.
///
/// `is_async` marks a coroutine function; a body that also contains `yield`
/// makes it an async generator (`is_async && is_generator`).
struct FnDecl<'a> {
    range: TextRange,
    name: &'a str,
    is_async: bool,
    args: &'a ast::Arguments,
    body: &'a [ast::Stmt],
    decorators: &'a [ast::Expr],
    returns: Option<&'a ast::Expr>,
    type_params: &'a [ast::TypeParam],
}

/// A cloned view of a same-unit function signature, used to bind direct call
/// sites without holding a borrow of the function table while lowering
/// arguments.
#[derive(Clone)]
pub(crate) struct CallSignature {
    name: String,
    capture_count: usize,
    params: Vec<(String, Option<Const>, ParamKind)>,
    pub(crate) default_exprs: Vec<Option<HExpr>>,
    is_async: bool,
    is_generator: bool,
}

impl CallSignature {
    fn kind_at(&self, index: usize) -> ParamKind {
        self.params[index].2
    }

    /// First parameter that is not positional (`*args`, keyword-only or
    /// `**kwargs`).
    fn positional_end(&self) -> usize {
        self.params[self.capture_count.min(self.params.len())..]
            .iter()
            .position(|(_, _, kind)| !kind.is_positional())
            .map(|offset| self.capture_count + offset)
            .unwrap_or(self.params.len())
    }

    /// Index of the `**kwargs` slot, or the parameter count.
    fn kwarg_index(&self) -> usize {
        self.params
            .iter()
            .position(|(_, _, kind)| *kind == ParamKind::KwArg)
            .unwrap_or(self.params.len())
    }

    fn has_dynamic_signature(&self) -> bool {
        self.params.iter().any(|(_, _, kind)| !kind.is_positional())
    }
}

impl<'a> FnDecl<'a> {
    fn from_def(f: &'a ast::StmtFunctionDef) -> FnDecl<'a> {
        FnDecl {
            range: f.range,
            name: f.name.as_str(),
            is_async: false,
            args: f.args.as_ref(),
            body: &f.body,
            decorators: &f.decorator_list,
            returns: f.returns.as_deref(),
            type_params: &f.type_params,
        }
    }

    fn from_async_def(f: &'a ast::StmtAsyncFunctionDef) -> FnDecl<'a> {
        FnDecl {
            range: f.range,
            name: f.name.as_str(),
            is_async: true,
            args: f.args.as_ref(),
            body: &f.body,
            decorators: &f.decorator_list,
            returns: f.returns.as_deref(),
            type_params: &f.type_params,
        }
    }
}

/// How a dataclass/NamedTuple field produces its value when a constructor
/// call omits the argument.
#[derive(Clone, Debug)]
enum FieldDefault<'a> {
    /// No default: the parameter is required.
    Required,
    /// Literal default (`x: int = 0`). The generated `__init__` stores it on
    /// the parameter, so call sites fill it at compile time.
    Literal(Const),
    /// `field(default=expr)`. The parameter defaults to `None`; the generated
    /// body evaluates `expr` when the argument is `None`.
    Expr(&'a ast::Expr),
    /// `field(default_factory=factory)`. The parameter defaults to `None`; the
    /// generated body calls the factory when the argument is `None`.
    Factory(&'a ast::Expr),
}

/// One annotated dataclass/NamedTuple field, with the default the frontend
/// read from the class-body AST.
#[derive(Clone, Debug)]
struct DataclassField<'a> {
    name: &'a str,
    ty: Ty,
    default: FieldDefault<'a>,
}

/// A class header/body registered before any function body is lowered.
#[derive(Clone, Debug)]
struct ClassPlan {
    /// Indices of the base expressions in `ClassDef::bases`, in source order.
    bases: Vec<usize>,
    /// Index of the `metaclass=` keyword expression, if present.
    metaclass: Option<usize>,
    /// Indices of the decorator expressions that are applied at runtime,
    /// bottom-up after the class object is created.
    decorators: Vec<usize>,
    /// Method name, function id, the source range start of its
    /// `FunctionDef` and its binding kind. Methods are lowered after their
    /// signatures are registered. A synthesized method (the
    /// dataclass/NamedTuple `__init__`) has no source body and therefore
    /// stores `None`; the runtime module `None` index denotes that case.
    methods: Vec<(String, FunctionId, Option<u32>, MethodKind)>,
    /// Class attribute name and the index of its assignment in the class
    /// body. Values are lowered when the class statement itself is lowered.
    /// Only the static path collects attributes.
    attributes: Vec<(String, usize)>,
    /// True when the class body executes statements the static plan cannot
    /// express (`del`, `if`, `for`, `with`, `try`, `raise`, calls, …). Such a
    /// body runs at runtime against a namespace `dict`, preserving CPython's
    /// class-namespace semantics.
    dynamic: bool,
    /// Names bound anywhere in a dynamic class body, including nested control
    /// flow. Reads of these names resolve against the runtime namespace.
    bindings: HashSet<String>,
}

/// Inputs shared by the static and dynamic class-emission helpers.
///
/// Grouping them keeps the helpers below to two parameters as the number of
/// per-class details (source statement, plan, binding target, mirroring mode)
/// grows.
struct ClassEmission<'a> {
    /// The source `class` statement.
    class: &'a ast::StmtClassDef,
    /// Registered header/body plan.
    plan: &'a ClassPlan,
    /// Class name used for `__qualname__` and the runtime class object.
    name: &'a str,
    /// Local that receives the class object.
    target: LocalId,
    /// Whether the module global table receives the finished class value.
    mirror_globals: bool,
    line: u32,
    col: u32,
}

/// Cell analysis for one function, computed from its AST before the body is
/// lowered. Names are resolved to local ids once the body's slots exist.
#[derive(Default)]
struct CellPlan {
    /// Locals this function owns a cell for: captured by a nested function and
    /// mutated here (or through `nonlocal`).
    owned: HashSet<String>,
    /// Capture parameters that already hold an enclosing function's cell.
    aliases: HashSet<String>,
}

/// Active class-body namespace while lowering a dynamic class body.
///
/// A class body that executes statements keeps CPython's class namespace in a
/// runtime `dict` local: assignments and `del` write that dict, reads of bound
/// names come from it, and the class object is built from it when the body
/// finishes.
#[derive(Clone)]
struct ClassNs {
    /// Local slot holding the namespace `dict`.
    ns: LocalId,
    /// Names bound anywhere in the class body (methods, assignments, loop and
    /// handler targets).
    bindings: HashSet<String>,
    /// Method bodies by definition span start: the function id and binding
    /// kind the class namespace must receive.
    methods: HashMap<u32, (FunctionId, MethodKind)>,
}

struct FuncState {
    fid: FunctionId,
    is_module: bool,
    locals: HashMap<String, LocalId>,
    /// Locals whose slot holds a heap cell; reads and writes go through the
    /// cell ABI.
    cells: HashSet<LocalId>,
    /// Capture locals holding an enclosing function's cell (no fresh cell).
    cell_aliases: HashSet<LocalId>,
    /// Names currently shadowed by a comprehension target, with the number of
    /// active bindings (nested comprehensions can bind the same name). An
    /// assignment expression may not rebind one of them.
    comp_shadowed: HashMap<String, usize>,
    /// Names declared `global` in this function scope. They never get a local
    /// slot: reads lower to `GlobalGet` and writes to `GlobalSet`.
    globals: HashSet<String>,
    loop_depth: u32,
    /// True while lowering a lambda body: only its parameters are visible.
    in_lambda: bool,
    /// `async def` body being lowered: `await` is allowed.
    is_async: bool,
    /// Generator body being lowered: `yield` is allowed.
    is_generator: bool,
    /// Synthetic generator-expression frame. Its locals are private to the
    /// suspended fiber, so an assignment expression cannot write back to the
    /// enclosing function.
    genexp_frame: bool,
    /// Class-namespace context of a dynamic class body being lowered; `None`
    /// in ordinary module/function scopes.
    class_ns: Option<ClassNs>,
}

impl FuncState {
    fn lookup(&self, name: &str) -> Option<LocalId> {
        self.locals.get(name).copied()
    }

    fn is_global(&self, name: &str) -> bool {
        self.globals.contains(name)
    }

    fn is_cell(&self, local: LocalId) -> bool {
        self.cells.contains(&local)
    }
}

/// A member resolved from `hydro_ir::stdlib` or a runtime-only supplement.
enum ModuleMember {
    Function(&'static FunctionSpec),
    Constant(&'static ConstantSpec),
}

/// `module.__dict__`: every module exposes its namespace mapping, including
/// the native modules whose static spec cannot list it.
static MODULE_DICT_MEMBER: ConstantSpec = ConstantSpec {
    name: "__dict__",
    ty: Type::Dict,
};

/// `builtins.eval`: present on the module even though the static spec table
/// predates the runtime evaluator. Parameters mirror CPython's signature so
/// the frontend binds `globals=`/`locals=` by name.
static BUILTINS_EVAL_MEMBER: FunctionSpec = FunctionSpec {
    name: "eval",
    min_args: 1,
    max_args: 3,
    ret: Type::Any,
    params: &["source", "globals", "locals"],
    dynamic_keywords: false,
};

/// `builtins.exec`: same call shape as `eval`; the runtime reports an explicit
/// `NotImplementedError` because there is no statement compiler.
static BUILTINS_EXEC_MEMBER: FunctionSpec = FunctionSpec {
    name: "exec",
    min_args: 1,
    max_args: 3,
    ret: Type::Any,
    params: &["source", "globals", "locals"],
    dynamic_keywords: false,
};

/// `types.CellType`: a cached native class (also callable as the cell
/// constructor), so `isinstance(cell, types.CellType)` and
/// `types.CellType(value)` are both valid.
static TYPES_CELLTYPE_MEMBER: FunctionSpec = FunctionSpec {
    name: "CellType",
    min_args: 0,
    max_args: 1,
    ret: Type::Any,
    params: &[],
    dynamic_keywords: false,
};

fn module_member(module: &'static ModuleSpec, name: &str) -> Option<ModuleMember> {
    if let Some(function) = module.functions.iter().find(|f| f.name == name) {
        return Some(ModuleMember::Function(function));
    }
    if let Some(constant) = module.constants.iter().find(|c| c.name == name) {
        return Some(ModuleMember::Constant(constant));
    }
    // Members the runtime serves without a static spec entry. Keeping the
    // table here (instead of the shared stdlib metadata) is deliberate: these
    // values are produced by the runtime on demand.
    match (module.name, name) {
        (_, "__dict__") => Some(ModuleMember::Constant(&MODULE_DICT_MEMBER)),
        ("builtins", "eval") => Some(ModuleMember::Function(&BUILTINS_EVAL_MEMBER)),
        ("builtins", "exec") => Some(ModuleMember::Function(&BUILTINS_EXEC_MEMBER)),
        ("types", "CellType") => Some(ModuleMember::Function(&TYPES_CELLTYPE_MEMBER)),
        _ => None,
    }
}

/// Result of looking up a compiled extension module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExtensionLookup {
    /// A loadable shared object at this canonical absolute path.
    Found(PathBuf),
    /// A shared object exists, but its wheel ABI tag cannot be loaded.
    Incompatible { path: PathBuf, reason: String },
    /// No shared object with a supported suffix exists.
    Missing,
}

/// Looks up a compiled extension for `name` in `dir`.
///
/// `name` may be dotted: `pkg.sub_ext` searches `<dir>/pkg/sub_ext...so`. Only
/// `<leaf>.so`, `<leaf>.abi3.so` and `<leaf>.cpython-314-<platform>.so` (the
/// `PYTHON_ABI_TAG` target) are loadable; any other `<leaf>.*.so` is
/// reported as incompatible instead of being silently preferred.
fn extension_lookup_in(dir: &Path, name: &str) -> ExtensionLookup {
    let mut relative = PathBuf::new();
    for segment in name.split('.') {
        if segment.is_empty() || segment.contains(['/', '\\']) {
            return ExtensionLookup::Missing;
        }
        relative.push(segment);
    }
    if relative.as_os_str().is_empty() {
        return ExtensionLookup::Missing;
    }
    extension_candidate(dir, &relative)
}

/// Looks up a compiled extension for `name` visible to `source`: the source
/// file's directory first, then the environment-configured search path (see
/// [`extension_search_paths_from`]).
pub(crate) fn find_extension_for_source(source: &Path, name: &str) -> ExtensionLookup {
    lookup_extension_in_search_dirs(source, &extension_search_paths(), name)
}

/// Looks up `name` in the source file's directory, then in each of
/// `search_dirs`.
///
/// The first directory that contains a matching shared object decides the
/// result, mirroring source-file precedence; an incompatible candidate is
/// never skipped in favour of a later directory.
///
/// `search_dirs` is ordinarily the list produced by
/// [`extension_search_paths_from`]; taking it as an argument keeps the lookup
/// policy testable without touching the process environment.
fn lookup_extension_in_search_dirs(
    source: &Path,
    search_dirs: &[PathBuf],
    name: &str,
) -> ExtensionLookup {
    let parent = source
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    for dir in std::iter::once(parent).chain(search_dirs.iter().map(PathBuf::as_path)) {
        match extension_lookup_in(dir, name) {
            ExtensionLookup::Missing => continue,
            found => return found,
        }
    }
    ExtensionLookup::Missing
}

/// Test-only convenience wrapper preserving the old `Option` shape.
#[cfg(test)]
fn find_extension_in_search_dirs(
    source: &Path,
    search_dirs: &[PathBuf],
    name: &str,
) -> Option<PathBuf> {
    match lookup_extension_in_search_dirs(source, search_dirs, name) {
        ExtensionLookup::Found(path) => Some(path),
        ExtensionLookup::Incompatible { .. } | ExtensionLookup::Missing => None,
    }
}

/// Directories searched by [`find_extension_for_source`], in order.
fn extension_search_paths() -> Vec<PathBuf> {
    let hydro_extension_path = std::env::var_os("HYDRO_EXTENSION_PATH");
    let pythonpath = std::env::var_os("PYTHONPATH");
    let hydro_site_packages = std::env::var_os("HYDRO_SITE_PACKAGES");
    let virtual_env = std::env::var_os("VIRTUAL_ENV").map(PathBuf::from);
    let conda_prefix = std::env::var_os("CONDA_PREFIX").map(PathBuf::from);
    extension_search_paths_from(
        hydro_extension_path.as_deref(),
        pythonpath.as_deref(),
        hydro_site_packages.as_deref(),
        virtual_env.as_deref(),
        conda_prefix.as_deref(),
    )
}

/// Builds the extension search path from raw environment values.
///
/// Order: `HYDRO_EXTENSION_PATH`, `PYTHONPATH`, then `HYDRO_SITE_PACKAGES`, then
/// the site-packages directories of `VIRTUAL_ENV` and `CONDA_PREFIX`. The
/// `HYDRO_SITE_PACKAGES` entries act as an explicit override: they are consulted
/// before any auto-detected virtual-environment site-packages directory.
///
/// Missing and non-directory entries are dropped and duplicates are searched
/// only once. The result is deterministic.
fn extension_search_paths_from(
    hydro_extension_path: Option<&OsStr>,
    pythonpath: Option<&OsStr>,
    hydro_site_packages: Option<&OsStr>,
    virtual_env: Option<&Path>,
    conda_prefix: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for list in [hydro_extension_path, pythonpath, hydro_site_packages]
        .into_iter()
        .flatten()
    {
        dirs.extend(std::env::split_paths(list));
    }
    for prefix in [virtual_env, conda_prefix].into_iter().flatten() {
        dirs.extend(prefix_site_packages(prefix));
    }

    let mut seen = HashSet::new();
    dirs.retain(|dir| {
        if !dir.is_dir() {
            return false;
        }
        let key = fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
        seen.insert(key)
    });
    dirs
}

/// Site-packages directories of an environment prefix, in deterministic order.
///
/// `<prefix>/Lib/site-packages` (Windows layout) comes first, followed by
/// every `<prefix>/lib/python*/site-packages` in sorted order.
pub(crate) fn prefix_site_packages(prefix: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let windows = prefix.join("Lib").join("site-packages");
    if windows.is_dir() {
        dirs.push(windows);
    }
    if let Ok(entries) = fs::read_dir(prefix.join("lib")) {
        let mut posix: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("python"))
            .map(|entry| entry.path().join("site-packages"))
            .filter(|dir| dir.is_dir())
            .collect();
        posix.sort();
        dirs.extend(posix);
    }
    dirs
}

/// Picks the shared object for `name` (a relative path without `.so`) in `dir`.
///
/// A plain `<name>.so` wins over `<name>.abi3.so`, which wins over any
/// `<name>.cpython-314-<platform>.so` (the `PYTHON_ABI_TAG` target; first in
/// sorted order). Every other `<name>.*.so` tag is incompatible: Hydro cannot
/// load it and reports the offending file instead of falling back to it.
fn extension_candidate(dir: &Path, name: &Path) -> ExtensionLookup {
    let stem = dir.join(name);
    let direct = stem.with_extension("so");
    if direct.is_file() {
        return ExtensionLookup::Found(canonical(direct));
    }
    let abi3 = stem.with_extension("abi3.so");
    if abi3.is_file() {
        return ExtensionLookup::Found(canonical(abi3));
    }

    let Some(parent) = stem.parent() else {
        return ExtensionLookup::Missing;
    };
    let Some(leaf) = stem.file_name().and_then(|leaf| leaf.to_str()) else {
        return ExtensionLookup::Missing;
    };
    let prefix = format!("{leaf}.");
    let Ok(entries) = fs::read_dir(parent) else {
        return ExtensionLookup::Missing;
    };
    let mut compatible = Vec::new();
    let mut incompatible = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(tag) = file_name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".so"))
        else {
            continue;
        };
        if tag.is_empty() {
            continue;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let abi_platform = tag
            .strip_prefix(PYTHON_ABI_TAG)
            .and_then(|rest| rest.strip_prefix('-'));
        match abi_platform {
            Some(platform) if !platform.is_empty() => compatible.push((tag.to_string(), path)),
            _ => incompatible.push((tag.to_string(), path)),
        }
    }
    compatible.sort();
    if let Some((_, path)) = compatible.into_iter().next() {
        return ExtensionLookup::Found(canonical(path));
    }
    incompatible.sort();
    if let Some((tag, path)) = incompatible.into_iter().next() {
        return ExtensionLookup::Incompatible {
            path: canonical(path),
            reason: format!(
                "unsupported extension ABI tag '{tag}' (loadable suffixes are `.so`, `.abi3.so` and `.{PYTHON_ABI_TAG}-<platform>.so` for Python {PYTHON_VERSION_STRING})"
            ),
        };
    }
    ExtensionLookup::Missing
}

fn canonical(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

/// Resolves the module named by an `import`/`from ... import` statement.
///
/// `level` is the number of leading dots and `package` the importing unit's
/// `__package__`: for a module that is the name of its parent package, for a
/// package `__init__.py` its own name. Returns the absolute dotted module name
/// used to key the module cache.
pub(crate) fn resolve_import_name(
    level: u32,
    module: Option<&str>,
    package: &str,
) -> Result<String, String> {
    if level == 0 {
        return match module {
            Some(name) if !name.is_empty() => Ok(name.to_string()),
            _ => Err("import statement has no module name".to_string()),
        };
    }
    if package.is_empty() {
        return Err("relative import with no known parent package".to_string());
    }
    let parts: Vec<&str> = package.split('.').collect();
    let up = level as usize - 1;
    if up >= parts.len() {
        return Err("attempted relative import beyond top-level package".to_string());
    }
    let base = parts[..parts.len() - up].join(".");
    Ok(match module {
        Some(name) if !name.is_empty() => format!("{base}.{name}"),
        _ => base,
    })
}

const SUPPORTED_MODULES_HELP: &str = "supported modules: math, io, functools, operator, time, sys, json, os, os.path, _thread, threading, queue, multiprocessing, pickle";

/// Truth value of an `if` guard that the import contract fixes statically.
///
/// Returns `Some(true)` when the body always runs, `Some(false)` when it
/// never runs and `None` for a dynamic condition. Two guards are static:
///
/// - `TYPE_CHECKING` (bare or `typing.TYPE_CHECKING`) is `False` at runtime
///   and this frontend never evaluates annotations, so the guarded body is
///   type-checking-only. Its imports must not be linked or bound.
/// - `__name__ == "__main__"` (and `!=`) is only true for the entry unit, so
///   demo blocks of imported modules are dead code; loading their imports
///   would link unrelated modules.
///
/// A local variable that shadows one of these names is not detected; the
/// guard is then still treated as static.
pub(crate) fn static_if_branch(test: &ast::Expr, dunder_name: &str) -> Option<bool> {
    if is_type_checking(test) {
        return Some(false);
    }
    if let ast::Expr::BoolOp(boolean) = test {
        let mut values = Vec::new();
        for value in &boolean.values {
            values.push(static_if_branch(value, dunder_name));
        }
        return match boolean.op {
            ast::BoolOp::And => {
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|value| *value == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            }
            ast::BoolOp::Or => {
                if values.contains(&Some(true)) {
                    Some(true)
                } else if values.iter().all(|value| *value == Some(false)) {
                    Some(false)
                } else {
                    None
                }
            }
        };
    }
    let ast::Expr::Compare(compare) = test else {
        return None;
    };
    if compare.ops.len() != 1 || compare.comparators.len() != 1 {
        return None;
    }
    let (op, left, right) = (compare.ops[0], &*compare.left, &compare.comparators[0]);
    let (literal, name) = match (left, right) {
        (ast::Expr::Name(name), ast::Expr::Constant(constant))
        | (ast::Expr::Constant(constant), ast::Expr::Name(name)) => (constant, name),
        _ => return None,
    };
    if name.id.as_str() != "__name__" {
        return None;
    }
    let ast::Constant::Str(value) = &literal.value else {
        return None;
    };
    if value.as_str() != "__main__" {
        return None;
    }
    let is_entry = dunder_name == "__main__";
    match op {
        ast::CmpOp::Eq => Some(is_entry),
        ast::CmpOp::NotEq => Some(!is_entry),
        _ => None,
    }
}

/// Whether `test` is the `typing.TYPE_CHECKING` sentinel.
fn is_type_checking(test: &ast::Expr) -> bool {
    match test {
        ast::Expr::Name(name) => name.id.as_str() == "TYPE_CHECKING",
        ast::Expr::Attribute(attribute) => {
            attribute.attr.as_str() == "TYPE_CHECKING"
                && matches!(
                    &*attribute.value,
                    ast::Expr::Name(name) if name.id.as_str() == "typing"
                )
        }
        _ => false,
    }
}

/// What a user module exports to importers.
///
/// Functions keep their final [`FunctionId`] (dependencies are lowered first),
/// so calls link directly. Classes and module-level values live in the runtime
/// global table and are read with a module-qualified [`HExprKind::GlobalGet`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserExport {
    Function(FunctionId),
    Global,
}

/// A user module (a `.py` file or package `__init__.py`) visible to this unit.
///
/// The loader resolves the file and lowers dependencies first, so exported
/// functions already have final [`FunctionId`]s by the time this unit is
/// lowered. Namespace packages (directories without `__init__.py`) are
/// represented with empty exports: importing them binds a module but there is
/// no body to initialize.
#[derive(Clone, Debug, Default)]
pub struct UserModule {
    /// Full dotted module name (`helper`, `pkg.sub`, ...).
    pub name: String,
    /// Top-level names exported by the module: functions and global values.
    pub exports: HashMap<String, UserExport>,
    /// Literal module-level `__all__` of the unit, when it is a tuple/list of
    /// string constants. `None` means the star export set is every public
    /// (non-underscore) name, like CPython without `__all__`.
    pub all: Option<Vec<String>>,
}

/// Returns the literal `__all__` of a module body, if there is one.
///
/// Only a tuple or list of string constants is honoured; an `__all__` built at
/// runtime cannot be evaluated while compiling, so it falls back to exporting
/// every non-underscore name (the same fallback CPython uses when `__all__` is
/// absent). A later assignment wins, matching module execution order.
pub(crate) fn module_all_names(stmts: &[ast::Stmt]) -> Option<Vec<String>> {
    fn literal(value: &ast::Expr) -> Option<Vec<String>> {
        let elements = match value {
            ast::Expr::List(list) => &list.elts,
            ast::Expr::Tuple(tuple) => &tuple.elts,
            _ => return None,
        };
        let mut names = Vec::with_capacity(elements.len());
        for element in elements {
            let ast::Expr::Constant(constant) = element else {
                return None;
            };
            let ast::Constant::Str(name) = &constant.value else {
                return None;
            };
            names.push(name.clone());
        }
        Some(names)
    }

    let mut all = None;
    for stmt in stmts {
        match stmt {
            ast::Stmt::Assign(assign) => {
                if assign
                    .targets
                    .iter()
                    .any(|target| matches!(target, ast::Expr::Name(name) if name.id.as_str() == "__all__"))
                {
                    all = literal(&assign.value);
                }
            }
            ast::Stmt::AnnAssign(assign) if matches!(&*assign.target, ast::Expr::Name(name) if name.id.as_str() == "__all__") =>
            {
                all = assign.value.as_deref().and_then(literal);
            }
            ast::Stmt::AugAssign(assign) if matches!(&*assign.target, ast::Expr::Name(name) if name.id.as_str() == "__all__") =>
            {
                all = None;
            }
            _ => {}
        }
    }
    all
}

/// A name bound by an import nested inside a function, class or compound body.
///
/// The loader does not execute nested imports, so the module may not be
/// linked. Reading the binding compiles to [`HExprKind::DeferredImport`],
/// which raises `RuntimeError` at runtime (`dynamic import ... is not
/// supported`); a function that never touches the binding runs normally.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DeferredImport {
    /// Dotted module named by the import.
    module: String,
    /// Imported member (`from x import member`), or `None` for a module alias
    /// (`import x [as y]`).
    member: Option<String>,
}

pub struct Lowerer<'a> {
    source: &'a str,
    filename: &'a str,
    infer: &'a mut Infer,
    /// PEP 750 template literals of the unit, keyed by source range. The AST
    /// carries a reserved placeholder `Name` for each literal; lowering
    /// resolves it through this table and never treats it as a variable.
    templates: Vec<TemplateString>,
    functions: Vec<HFunction>,
    fn_ids: HashMap<String, FunctionId>,
    module_local_names: HashSet<String>,
    /// `import X as Y` bindings: alias -> module specification.
    module_aliases: HashMap<String, &'static ModuleSpec>,
    /// `import X [as Y]` bindings of user modules: alias -> module name.
    user_module_aliases: HashMap<String, String>,
    /// `import ext [as e]` bindings of compiled extensions: alias -> shared
    /// object path (canonical absolute).
    extension_aliases: HashMap<String, String>,
    /// `from ext import member [as x]` bindings: name -> (path, member).
    imported_extension_members: HashMap<String, (String, String)>,
    /// User modules visible to this unit, keyed by the name in the import.
    user_modules: HashMap<String, UserModule>,
    /// `from X import f [as g]` bindings of user functions: name -> function id.
    imported_user_functions: HashMap<String, FunctionId>,
    /// `from X import value [as g]` bindings of user module globals (classes
    /// and constants): name -> (module full name, exported name).
    imported_user_globals: HashMap<String, (String, String)>,
    /// `from . import name` bindings inside a package `__init__.py` that refer
    /// to this unit's own functions; resolved after signatures are registered.
    pending_self_imports: Vec<(String, String, TextRange)>,
    /// `from . import *` inside a package `__init__.py`: expanded after the
    /// unit's own functions and globals are known.
    pending_self_stars: Vec<TextRange>,
    /// Literal `__all__` of this unit, used by a pending self star import.
    module_all: Option<Vec<String>>,
    /// Names bound by imports nested inside functions, classes or compound
    /// bodies that could not be linked statically. Reading one raises
    /// `RuntimeError` at runtime.
    deferred_imports: HashMap<String, DeferredImport>,
    /// Binding names established by module-level imports. A nested import
    /// never replaces one of these, so top-level imports keep precedence.
    top_level_imports: HashSet<String>,
    /// Modules the loader recorded as imported only from nested blocks. Empty
    /// when lowering a single source file without the loader, where every
    /// nested import is treated as deferred.
    deferred_modules: HashSet<String>,
    /// User modules this unit imports but cannot link because they are still
    /// initializing. Their members are read late from the runtime global
    /// table; see [`Lowerer::is_initializing_ancestor`].
    late_modules: HashSet<String>,
    /// Base id for this unit's functions. Multi-module programs reserve a
    /// contiguous id range per module; single-source checks use `0`.
    fid_base: FunctionId,
    /// Value bound to the module body's `__name__`.
    dunder_name: String,
    /// Value of `__package__`: the package a relative import is resolved
    /// against. Empty for the entry unit.
    package: String,
    /// Module-level names mirrored into the runtime global table.
    global_names: HashSet<String>,
    /// Global names actually read by a function or method of this unit.
    /// Single-source checks and the entry module only mirror those; imported
    /// units mirror every exported name because importers read them.
    used_globals: HashSet<String>,
    /// Mirror every module-level assignment and class definition into the
    /// global table. Enabled for imported units, whose globals are read by
    /// other compilation units. The entry module leaves it off: it has no
    /// importers, and mirroring every assignment would add a global store to
    /// each top-level loop iteration.
    mirror_all_globals: bool,
    /// Compile a REPL/session fragment instead of a standalone module.
    ///
    /// Session fragments share one runtime global table: names bound by an
    /// earlier fragment are visible here, module-level reads that no local
    /// binding shadows resolve to a [`HExprKind::GlobalGet`] (including names
    /// that would otherwise resolve to a builtin, function or exception), and
    /// import bindings are mirrored into the table so they survive the
    /// fragment.
    session_mode: bool,
    /// Names bound by earlier fragments of the same session. They are part of
    /// [`Self::global_names`] but tracked separately so a session-global read
    /// can be distinguished from a name this fragment binds itself.
    session_globals: HashSet<String>,
    /// `from X import f [as g]` bindings: name -> (module, function spec).
    imported_functions: HashMap<String, (&'static ModuleSpec, &'static FunctionSpec)>,
    /// `from X import c [as d]` bindings: name -> (module, constant spec).
    imported_constants: HashMap<String, (&'static ModuleSpec, &'static ConstantSpec)>,
    /// Next synthetic local name for desugared `for` unpack targets.
    unpack_counter: u32,
    /// Next synthetic `$compN` local for comprehension targets.
    comp_counter: u32,
    /// Next synthetic `$withN` local holding a `with` context object.
    with_counter: u32,
    /// Next synthetic `$augN` local holding a subscript augmented-assignment
    /// object or index so it is evaluated exactly once.
    aug_counter: u32,
    /// Next synthetic `$cmpN` local holding the middle operand of a chained
    /// comparison so it is evaluated exactly once.
    cmp_counter: u32,
    /// Next synthetic `$multiN` local holding the right-hand side of a
    /// chained assignment so it is evaluated exactly once.
    multi_counter: u32,
    /// Next synthetic `$forN` local receiving a loop target that is a cell.
    for_counter: u32,
    /// Next synthetic local of an asynchronous lowering: `$aiterN`,
    /// `$aitemN`, `$aexcN`, `$aloopN`.
    async_counter: u32,
    /// Next synthetic `$decorN` local holding a decorator callable or the
    /// function value decorated in place.
    decorator_counter: u32,
    /// Next synthetic `$matchN` local holding a `match` subject or a value
    /// extracted from a pattern.
    match_counter: u32,
    /// Module-level functions with at least one runtime decorator. Calls and
    /// value reads resolve through the module local/global binding, never
    /// through `fn_ids`, because the decorator may replace the function.
    decorated_functions: HashMap<String, FunctionId>,
    /// Function frames in which an `await` was lowered (source-level or
    /// synthesized, e.g. by an asynchronous comprehension). A generator
    /// expression whose frame awaits is an async generator.
    await_frames: HashSet<FunctionId>,
    /// Per-function cell analysis: locals whose binding lives in a heap cell
    /// and capture parameters that alias an enclosing cell.
    cell_plans: HashMap<FunctionId, CellPlan>,
    /// Registered class headers, keyed by the class statement's start offset.
    /// Method signatures exist before any body is lowered; the key lets
    /// `lower_stmt` find the plan without threading it through every call.
    class_defs: HashMap<usize, ClassPlan>,
    /// Functions defined inside a module-level `if`/`try`/loop block, keyed by
    /// the definition's start offset. Their binding is created only when the
    /// enclosing statement runs, so it always goes through the runtime global
    /// table; `fn_ids` never links them directly.
    conditional_defs: HashMap<usize, FunctionId>,
    /// Classes declared directly by this unit. A `raise` of one of these
    /// classes uses the documented `RuntimeError` fallback (see
    /// [`Lowerer::lower_raise`]) instead of being rejected.
    class_names: HashSet<String>,
    /// Class constructors exported by already-lowered dependency units,
    /// keyed by `(module, class)`. Used to bind imported class calls.
    imported_classes: HashMap<(String, String), (FunctionId, CallSignature)>,
    /// Signatures of functions exported by already-lowered dependency units.
    imported_function_signatures: HashMap<FunctionId, CallSignature>,
    /// Signatures of functions declared in this unit (captured before the
    /// function table is moved out by `lower_module`).
    function_signatures: HashMap<FunctionId, CallSignature>,
    /// Same-unit class name -> its `__init__` `FunctionId`. Constructor calls
    /// use it to bind keywords into parameter slots and fill omitted defaults
    /// at compile time; classes without an `__init__` are absent.
    class_inits: HashMap<String, FunctionId>,
    /// Constructor signatures captured while lowering; `lower_module` moves
    /// the function table out, so exports cannot re-read it afterwards.
    class_signatures: HashMap<String, (FunctionId, CallSignature)>,
    /// Methods declared by a class, used to resolve a classmethod's `cls`
    /// parameter as the enclosing class constructor.
    class_methods: HashMap<FunctionId, (String, MethodKind, String)>,
    /// Enclosing classmethod while lowering its body.
    current_class_method: Option<(String, MethodKind, String)>,
    /// Class attributes visible while evaluating a class body's attribute
    /// values; the innermost scope is last.
    class_attr_scopes: Vec<HashMap<String, HExpr>>,
    /// Resolve otherwise-unknown module-level names to `GlobalGet` while
    /// lowering a parameter default expression.
    in_default_expr: bool,
    pub diags: Vec<Diagnostic>,
}

impl<'a> Lowerer<'a> {
    pub fn new(source: &'a str, filename: &'a str, infer: &'a mut Infer) -> Lowerer<'a> {
        Lowerer {
            source,
            filename,
            infer,
            templates: Vec::new(),
            functions: vec![HFunction {
                name: MODULE_NAME.to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Ty::Concrete(Type::None),
                locals: Vec::new(),
                body: Vec::new(),
                is_async: false,
                is_generator: false,
                is_module: true,
                line: 1,
                col: 1,
            }],
            fn_ids: HashMap::new(),
            module_local_names: HashSet::new(),
            module_aliases: HashMap::new(),
            user_module_aliases: HashMap::new(),
            extension_aliases: HashMap::new(),
            imported_extension_members: HashMap::new(),
            user_modules: HashMap::new(),
            imported_user_functions: HashMap::new(),
            imported_user_globals: HashMap::new(),
            pending_self_imports: Vec::new(),
            pending_self_stars: Vec::new(),
            module_all: None,
            deferred_imports: HashMap::new(),
            top_level_imports: HashSet::new(),
            deferred_modules: HashSet::new(),
            late_modules: HashSet::new(),
            fid_base: 0,
            dunder_name: "__main__".to_string(),
            package: String::new(),
            global_names: HashSet::new(),
            used_globals: HashSet::new(),
            mirror_all_globals: false,
            session_mode: false,
            session_globals: HashSet::new(),
            imported_functions: HashMap::new(),
            imported_constants: HashMap::new(),
            unpack_counter: 0,
            comp_counter: 0,
            with_counter: 0,
            aug_counter: 0,
            cmp_counter: 0,
            multi_counter: 0,
            for_counter: 0,
            async_counter: 0,
            decorator_counter: 0,
            match_counter: 0,
            decorated_functions: HashMap::new(),
            await_frames: HashSet::new(),
            cell_plans: HashMap::new(),
            class_defs: HashMap::new(),
            conditional_defs: HashMap::new(),
            class_names: HashSet::new(),
            imported_classes: HashMap::new(),
            imported_function_signatures: HashMap::new(),
            function_signatures: HashMap::new(),
            class_inits: HashMap::new(),
            class_signatures: HashMap::new(),
            class_methods: HashMap::new(),
            current_class_method: None,
            class_attr_scopes: Vec::new(),
            in_default_expr: false,
            diags: Vec::new(),
        }
    }

    /// Reserves the [`FunctionId`] range starting at `base` for this unit.
    pub fn set_imported_classes(
        &mut self,
        classes: HashMap<(String, String), (FunctionId, CallSignature)>,
        functions: HashMap<FunctionId, CallSignature>,
    ) {
        self.imported_classes = classes;
        self.imported_function_signatures = functions;
    }

    /// Class constructors declared by this unit, with absolute function ids.
    /// Function signatures declared by this unit, keyed by absolute id.
    pub(crate) fn exported_function_signatures(&self) -> Vec<(FunctionId, CallSignature)> {
        self.function_signatures
            .iter()
            .map(|(id, signature)| (*id, signature.clone()))
            .collect()
    }

    pub(crate) fn exported_class_signatures(&self) -> Vec<(String, FunctionId, CallSignature)> {
        self.class_signatures
            .iter()
            .map(|(name, (init, signature))| (name.clone(), *init, signature.clone()))
            .collect()
    }

    pub fn set_fid_base(&mut self, base: FunctionId) {
        self.fid_base = base;
    }

    /// Installs the PEP 750 template literals of this unit.
    ///
    /// The parser emits a reserved placeholder `Name` for each template
    /// literal and returns the resolved structure out of band. Lowering a
    /// placeholder without an entry here is a diagnostic, never a variable
    /// read: a template must never silently become an ordinary name.
    pub fn set_templates(&mut self, templates: &[TemplateString]) {
        self.templates = templates.to_vec();
    }

    /// Sets the string bound to `__name__` in the module body.
    ///
    /// Imported modules call this with their full dotted name; the entry unit
    /// keeps the `__main__` default.
    pub fn set_dunder_name(&mut self, name: &str) {
        self.dunder_name = name.to_string();
    }

    /// Sets `__package__`, used to resolve relative imports.
    pub fn set_package(&mut self, package: &str) {
        self.package = package.to_string();
    }

    /// Mirrors every module-level assignment and class definition into the
    /// runtime global table. Imported units enable this because an importer
    /// may read any exported name. The entry module leaves it disabled: only
    /// the names its own functions read are mirrored (see
    /// `collect_function_read_globals`).
    pub fn set_mirror_all_globals(&mut self, enabled: bool) {
        self.mirror_all_globals = enabled;
    }

    /// Compiles this unit as a REPL/session fragment.
    ///
    /// Session fragments share a runtime global table: names bound by earlier
    /// fragments are seeded with [`Self::set_session_globals`], unshadowed
    /// module-level reads resolve to `GlobalGet` instead of being rejected,
    /// session globals take precedence over builtins/functions/exceptions, and
    /// import bindings are mirrored into the table. Session compilation still
    /// expects [`Self::set_mirror_all_globals`] so every module-body binding
    /// reaches the table.
    pub fn set_session_mode(&mut self, enabled: bool) {
        self.session_mode = enabled;
    }

    /// Seeds the names bound by earlier fragments of the same session.
    ///
    /// They join [`Self::global_names`], so functions of this fragment read
    /// them from the runtime global table, and they shadow builtins, functions
    /// and exception classes exactly like a module-level binding.
    pub fn set_session_globals(&mut self, names: impl IntoIterator<Item = String>) {
        for name in names {
            self.global_names.insert(name.clone());
            self.session_globals.insert(name);
        }
    }

    /// Makes resolved user modules (and their exported functions) visible.
    pub fn set_user_modules(&mut self, modules: HashMap<String, UserModule>) {
        self.user_modules = modules;
    }

    /// Names user modules this unit imports without a direct link because they
    /// are still initializing (the loader records them). Their members are
    /// bound late, through the runtime global table of the module.
    pub fn set_late_modules(&mut self, modules: HashSet<String>) {
        self.late_modules = modules;
    }

    /// Names the modules the loader recorded as imported only from nested
    /// blocks. Those imports were not linked; the lowerer binds their names to
    /// the deferred-import runtime stub unless the module is already a visible
    /// dependency (`from helper import f` after a module-level import).
    ///
    /// An empty set means the unit was lowered without the loader (single-
    /// source checks), where every nested import may be deferred.
    pub fn set_deferred_imports(&mut self, modules: HashSet<String>) {
        self.deferred_modules = modules;
    }

    /// Whether a nested import of `module` may be bound as deferred.
    fn may_defer(&self, module: &str) -> bool {
        self.deferred_modules.is_empty() || self.deferred_modules.contains(module)
    }

    /// Names this unit exports to importers: top-level functions plus every
    /// module-level value mirrored into the global table (classes included).
    pub fn exported_names(&self) -> HashMap<String, UserExport> {
        let mut exports = HashMap::with_capacity(self.fn_ids.len() + self.global_names.len());
        for (name, id) in &self.fn_ids {
            // A decorated function is replaced by the decorator's result at
            // runtime, so importers read the mirrored module global instead
            // of linking the raw function id.
            if self.decorated_functions.contains_key(name) {
                exports.insert(name.clone(), UserExport::Global);
                continue;
            }
            exports.insert(name.clone(), UserExport::Function(*id));
        }
        for name in &self.global_names {
            exports.entry(name.clone()).or_insert(UserExport::Global);
        }
        exports
    }

    pub fn lower_module(&mut self, module: &ast::ModModule) -> LHir {
        // 1. Imports bind global names. Module-level imports are collected
        //    first so their names always win over names bound by nested
        //    imports; nested imports are collected afterwards and either link
        //    against visible dependencies or become deferred runtime stubs.
        //    Everything happens before any signature or body is lowered, so
        //    imported names are visible inside functions regardless of where
        //    the import appears in the file (and so class decorators can be
        //    recognised). Import statements themselves emit no IR.
        //
        //    The exported global names are known first: `from . import name`
        //    inside a package may refer to a name defined by this unit.
        self.module_all = module_all_names(&module.body);
        collect_global_names(&module.body, &mut self.global_names);
        // Functions may read module globals, so those names must be mirrored
        // even when the assignment is lowered before the function body. Scan
        // the function and method bodies up front; this also keeps the entry
        // module, which does not mirror every global, correct regardless of
        // lowering order.
        self.collect_function_read_globals(&module.body);
        // A function may assign a module global without the module body
        // assigning it: `global x` makes the name an export and forces the
        // runtime global table to be its home.
        let mut declared_globals = HashSet::new();
        collect_declared_globals(&module.body, &mut declared_globals);
        for name in declared_globals {
            self.global_names.insert(name.clone());
            self.used_globals.insert(name);
        }
        self.collect_top_imports(&module.body);
        self.collect_nested_imports(&module.body, true);

        // 2. Signatures next: forward calls and recursion are both legal.
        //    Class methods are registered here too, but only by qualified
        //    name in the diagnostic/function table: they must never enter
        //    `fn_ids`, so methods of different classes may share a name.
        //
        //    Definitions nested in module-level `if`/`try`/loop blocks are
        //    registered as well. They execute conditionally, so they bind at
        //    runtime through the module global table instead of being linked
        //    by `fn_ids`; `conditional_defs` maps their source offset to the
        //    compiled function.
        let mut def_ids: HashMap<usize, FunctionId> = HashMap::new();
        self.register_module_definitions(&module.body, true, &mut def_ids);

        // Self-package imports (`from . import f` in a package init) can now
        // resolve functions defined by this unit.
        self.resolve_self_imports();

        // 3. Module locals are known before we lower any function body; this
        //    lets us explain why a module-level name is not visible there.
        let mut module_names = Vec::new();
        collect_names(&module.body, &mut module_names, false);
        // A decorated function's binding is the decorator's result, so the
        // name needs an ordinary module local slot.
        for name in self.decorated_functions.keys() {
            if !module_names.iter().any(|existing| existing == name) {
                module_names.push(name.clone());
            }
        }
        // A session fragment that only augments a name bound by an earlier
        // fragment must not allocate a fresh local for it: the read and the
        // write both go through the runtime global table. Names the fragment
        // binds some other way keep their local slot.
        if self.session_mode {
            let mut direct = HashSet::new();
            collect_source_bound_names(&module.body, &mut direct, false);
            module_names
                .retain(|name| direct.contains(name) || !self.session_globals.contains(name));
        }
        let mut module_locals: HashMap<String, LocalId> = HashMap::new();
        for name in module_names {
            if module_locals.contains_key(&name) {
                continue;
            }
            let id = self.functions[0].locals.len() as LocalId;
            let ty = self.infer.fresh();
            self.functions[0].locals.push(HLocal {
                name: name.clone(),
                ty,
            });
            module_locals.insert(name, id);
        }
        self.module_local_names = module_locals.keys().cloned().collect();

        // 4. Module dunders are module globals: the module body assigns a
        //    local and mirrors it into the runtime global table, so functions
        //    read the value with a `GlobalGet`. Only names the unit actually
        //    loads are injected (method bodies included), so units that never
        //    reference them gain no statements.
        let dunder_inits = self.lower_dunder_bindings(module, &mut module_locals);

        // 5. Function bodies (all signatures are already registered). Class
        //    method bodies are lowered here as well: their signatures live in
        //    the same function table, only the lookup name differs. The walk
        //    descends into module-level control-flow blocks and nested class
        //    bodies, because definitions registered there still need their
        //    bodies lowered unconditionally.
        self.lower_definition_bodies(&module.body, &def_ids);

        // 6. Module body. Function definitions are declarations only (their
        //    values are mirrored for late importers, see
        //    `mirror_module_function`); normal imports were resolved in step 1
        //    and emit no statement. A session fragment mirrors its import
        //    bindings, so its imports lower through `lower_stmt`.
        let mut fs = FuncState {
            fid: 0,
            is_module: true,
            locals: module_locals,
            cells: HashSet::new(),
            cell_aliases: HashSet::new(),
            comp_shadowed: HashMap::new(),
            globals: HashSet::new(),
            loop_depth: 0,
            in_lambda: false,
            is_async: false,
            is_generator: false,
            genexp_frame: false,
            class_ns: None,
        };
        let mut body = Vec::new();
        for stmt in &module.body {
            let start = u32::from(stmt.range().start()) as usize;
            match stmt {
                ast::Stmt::FunctionDef(f) => {
                    if let Some(&fid) = def_ids.get(&start) {
                        if self.has_runtime_function_decorators(&f.decorator_list) {
                            let (line, col) = self.span(f.range);
                            self.lower_module_function_definition(
                                &f.decorator_list,
                                fid,
                                f.range,
                                &mut fs,
                                &mut body,
                                line,
                                col,
                            );
                            continue;
                        }
                    }
                    self.mirror_module_function(start, &def_ids, &mut body);
                    continue;
                }
                ast::Stmt::AsyncFunctionDef(f) => {
                    if let Some(&fid) = def_ids.get(&start) {
                        if self.has_runtime_function_decorators(&f.decorator_list) {
                            let (line, col) = self.span(f.range);
                            self.lower_module_function_definition(
                                &f.decorator_list,
                                fid,
                                f.range,
                                &mut fs,
                                &mut body,
                                line,
                                col,
                            );
                            continue;
                        }
                    }
                    self.mirror_module_function(start, &def_ids, &mut body);
                    continue;
                }
                ast::Stmt::ClassDef(c) => {
                    let has_decorators = self
                        .class_defs
                        .get(&(u32::from(c.range.start()) as usize))
                        .is_some_and(|plan| !plan.decorators.is_empty());
                    if has_decorators {
                        let (line, col) = self.span(c.range);
                        self.lower_decorated_class(c, &mut fs, &mut body, line, col);
                        continue;
                    }
                }
                ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_) if !self.session_mode => continue,
                _ => {}
            }
            if let Some(s) = self.lower_stmt(stmt, &mut fs) {
                self.push_lowered_stmt(stmt, s, &fs, &mut body);
            }
        }
        body.splice(0..0, dunder_inits);
        self.functions[0].body = body;

        self.synthesize_pytest(&module.body, &mut fs);

        LHir {
            source_name: self.filename.to_string(),
            functions: std::mem::take(&mut self.functions),
        }
    }

    /// Registers module-level definitions, including those nested in
    /// module-level control-flow blocks.
    ///
    /// A definition directly in the module body is linked statically in
    /// `fn_ids`. One inside an `if`/`try`/loop executes conditionally: CPython
    /// still compiles its code object, but the name binds only on the branch
    /// that runs. Such a definition is therefore created without static
    /// linkage and binds through the runtime global table at its statement.
    /// Every read of the name resolves to that runtime binding, so mutually
    /// exclusive definitions in different branches are all represented.
    fn register_module_definitions(
        &mut self,
        body: &[ast::Stmt],
        top_level: bool,
        def_ids: &mut HashMap<usize, FunctionId>,
    ) {
        for stmt in body {
            let start = u32::from(stmt.range().start()) as usize;
            match stmt {
                ast::Stmt::FunctionDef(f) => {
                    let id = if top_level {
                        self.create_function(f)
                    } else {
                        self.create_conditional_function(f)
                    };
                    if let Some(id) = id {
                        def_ids.insert(start, id);
                    }
                }
                ast::Stmt::AsyncFunctionDef(f) => {
                    let id = if top_level {
                        self.create_async_function(f)
                    } else {
                        self.create_conditional_async_function(f)
                    };
                    if let Some(id) = id {
                        def_ids.insert(start, id);
                    }
                }
                ast::Stmt::ClassDef(c) => {
                    let name = c.name.to_string();
                    let plan = self.register_class(c);
                    self.class_defs.insert(start, plan);
                    if !top_level {
                        // The class object is created only when the enclosing
                        // block runs, so it binds through the global table.
                        self.global_names.insert(name.clone());
                        self.used_globals.insert(name);
                    }
                }
                other => {
                    for block in class_body_blocks(other) {
                        self.register_module_definitions(block, false, def_ids);
                    }
                }
            }
        }
    }

    /// Registers a function defined inside a module-level control-flow block.
    fn create_conditional_function(&mut self, f: &ast::StmtFunctionDef) -> Option<FunctionId> {
        self.create_conditional_decl(FnDecl::from_def(f))
    }

    /// `async def` counterpart of [`Self::create_conditional_function`].
    fn create_conditional_async_function(
        &mut self,
        f: &ast::StmtAsyncFunctionDef,
    ) -> Option<FunctionId> {
        self.create_conditional_decl(FnDecl::from_async_def(f))
    }

    fn create_conditional_decl(&mut self, decl: FnDecl<'_>) -> Option<FunctionId> {
        // `@overload` declares a type-only stub and binds nothing.
        if decl.decorators.iter().any(is_overload_decorator) {
            return None;
        }
        let name = decl.name.to_string();
        // A conditional definition can replace a statically linked top-level
        // function of the same name. Drop that linkage so every later read
        // observes the runtime binding instead of the first definition.
        self.fn_ids.remove(&name);
        self.global_names.insert(name.clone());
        let start = u32::from(decl.range.start()) as usize;
        let id = self.create_function_with_captures(&decl, name, false, &[], &[], None)?;
        self.conditional_defs.insert(start, id);
        Some(id)
    }

    /// Lowers the bodies of every registered function and method.
    ///
    /// The walk descends into module-level control-flow blocks (where
    /// conditional definitions live) and into class bodies (where nested
    /// classes and their methods live). Method bodies are located through
    /// [`class_method_bodies`], which already follows a class body's own
    /// control flow.
    fn lower_definition_bodies(
        &mut self,
        body: &[ast::Stmt],
        def_ids: &HashMap<usize, FunctionId>,
    ) {
        for stmt in body {
            let start = u32::from(stmt.range().start()) as usize;
            match stmt {
                ast::Stmt::FunctionDef(f) => {
                    let Some(&fid) = def_ids.get(&start) else {
                        continue;
                    };
                    let lowered = self.lower_function_body(fid, &f.body);
                    self.functions[fid as usize].body = lowered;
                }
                ast::Stmt::AsyncFunctionDef(f) => {
                    let Some(&fid) = def_ids.get(&start) else {
                        continue;
                    };
                    let lowered = self.lower_function_body(fid, &f.body);
                    self.functions[fid as usize].body = lowered;
                }
                ast::Stmt::ClassDef(c) => {
                    if let Some(plan) = self.class_defs.get(&start).cloned() {
                        let bodies = class_method_bodies(&c.body);
                        for (_, fid, method_start, _) in plan.methods {
                            // Synthesized methods already carry their body.
                            let Some(method_start) = method_start else {
                                continue;
                            };
                            let Some(method_body) = bodies.get(&method_start) else {
                                continue;
                            };
                            let lowered = self.lower_function_body(fid, method_body);
                            self.functions[fid as usize].body = lowered;
                        }
                    }
                    // Nested classes are separate `ClassDef` statements inside
                    // this class body; lower their method bodies too.
                    self.lower_definition_bodies(&c.body, def_ids);
                }
                other => {
                    for block in class_body_blocks(other) {
                        self.lower_definition_bodies(block, def_ids);
                    }
                }
            }
        }
    }

    /// Lower a conditional module-level `def` at the statement where it runs.
    ///
    /// The compiled function value is created on that path only, decorators
    /// are evaluated first and applied bottom-up, and the result is stored in
    /// the module's runtime global table. Wrapping the sequence in `if True:`
    /// keeps it one HIR statement; the enclosing branch condition still
    /// decides whether it runs.
    #[allow(clippy::too_many_arguments)]
    fn lower_conditional_function_definition(
        &mut self,
        decorators: &[ast::Expr],
        fid: FunctionId,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let name = self.functions.get(fid as usize)?.name.clone();
        let mut then_body = Vec::new();
        let decorators = self.runtime_function_decorators(decorators);
        let temps = self.lower_decorator_temps(&decorators, fs, &mut then_body);
        let (target, _) = self.hidden_temp(fs, "$condeff", Type::Any);
        then_body.push(HStmt {
            kind: HStmtKind::Assign {
                target,
                value: HExpr {
                    kind: HExprKind::FunctionRef(self.fid_base + fid),
                    ty: Ty::Concrete(Type::Function),
                    line,
                    col,
                },
            },
            line,
            col,
        });
        self.apply_decorator_temps(&temps, target, fs, &mut then_body, line, col);
        then_body.push(HStmt {
            kind: HStmtKind::GlobalSet {
                module: self.dunder_name.clone(),
                name,
                value: HExpr {
                    kind: HExprKind::Local(target),
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                },
            },
            line,
            col,
        });
        Some(HStmt {
            kind: HStmtKind::If {
                cond: self.bool_const(true, line, col),
                then_body,
                else_body: Vec::new(),
            },
            line,
            col,
        })
    }

    /// Expands pytest fixtures and parametrization into zero-argument
    /// wrappers.
    ///
    /// The runtime registry only knows function names and arities, so a test
    /// that takes fixture parameters cannot be called directly. This pass
    /// renames each such test to `<name>__hydro_impl` and appends a generated
    /// wrapper that resolves the fixtures and calls the implementation.
    /// Parametrized tests produce one wrapper per parameter set, named
    /// `test_x[<id>]`, matching pytest's report names.
    ///
    /// Fixture wrappers are named `__hydro_fixture_<name>` and return the raw
    /// fixture value. A `yield` fixture therefore returns its generator; the
    /// runtime helper `pytest._fixture_resolve` advances it and schedules the
    /// teardown. Module-scoped fixtures go through `pytest._fixture_module`,
    /// which caches the resolved value for the child run.
    fn synthesize_pytest(&mut self, body: &[ast::Stmt], fs: &mut FuncState) {
        let mut fixtures: HashMap<String, (FunctionId, &'static str)> = HashMap::new();
        for stmt in body {
            let ast::Stmt::FunctionDef(function) = stmt else {
                continue;
            };
            let Some(scope) = pytest_fixture_scope(&function.decorator_list) else {
                continue;
            };
            let Some(&implementation) = self.fn_ids.get(function.name.as_str()) else {
                continue;
            };
            let wrapper = self.build_pytest_fixture(function, implementation, &fixtures, fs);
            fixtures.insert(function.name.to_string(), (wrapper, scope));
        }

        for stmt in body {
            let ast::Stmt::FunctionDef(function) = stmt else {
                continue;
            };
            self.build_pytest_tests(function, &fixtures, fs);
        }
    }

    /// Reference to a fixture value for a generated wrapper.
    fn pytest_fixture_reference(
        &mut self,
        name: &str,
        fixtures: &HashMap<String, (FunctionId, &'static str)>,
        line: u32,
        col: u32,
    ) -> Option<HExpr> {
        if let Some((wrapper, scope)) = fixtures.get(name) {
            let (wrapper, scope) = (*wrapper, *scope);
            let raw = self.pytest_call(wrapper, Vec::new(), line, col);
            return Some(if scope == "function" {
                self.pytest_module_call("_fixture_resolve", vec![raw], line, col)
            } else {
                self.pytest_module_call(
                    "_fixture_module",
                    vec![
                        HExpr {
                            kind: HExprKind::Const(Const::Str(name.to_string())),
                            ty: Ty::Concrete(Type::Str),
                            line,
                            col,
                        },
                        HExpr {
                            kind: HExprKind::FunctionRef(wrapper),
                            ty: Ty::Concrete(Type::Function),
                            line,
                            col,
                        },
                    ],
                    line,
                    col,
                )
            });
        }
        // Fixtures from imported units (conftest.py): the wrapper naming
        // convention identifies them. Cross-unit scope metadata is not part
        // of the export table, so they keep function scope (the common case).
        let wrapper_name = format!("__hydro_fixture_{name}");
        let mut modules: Vec<&UserModule> = self.user_modules.values().collect();
        modules.sort_by(|left, right| left.name.cmp(&right.name));
        let wrapper =
            modules
                .iter()
                .rev()
                .find_map(|module| match module.exports.get(&wrapper_name) {
                    Some(UserExport::Function(wrapper)) => Some(*wrapper),
                    _ => None,
                });
        let wrapper = wrapper?;
        let raw = self.pytest_call(wrapper, Vec::new(), line, col);
        Some(self.pytest_module_call("_fixture_resolve", vec![raw], line, col))
    }

    fn pytest_call(
        &mut self,
        function: FunctionId,
        args: Vec<HExpr>,
        line: u32,
        col: u32,
    ) -> HExpr {
        HExpr {
            kind: HExprKind::Call {
                callee: Callee::Function(function),
                args,
            },
            ty: self.infer.fresh(),
            line,
            col,
        }
    }

    fn pytest_module_call(
        &mut self,
        function: &str,
        args: Vec<HExpr>,
        line: u32,
        col: u32,
    ) -> HExpr {
        HExpr {
            kind: HExprKind::ModuleCall {
                module: "pytest".to_string(),
                function: function.to_string(),
                args,
                keywords: Vec::new(),
            },
            ty: self.infer.fresh(),
            line,
            col,
        }
    }

    fn pytest_stmt(kind: HStmtKind, line: u32, col: u32) -> HStmt {
        HStmt { kind, line, col }
    }

    /// Builds the raw zero-argument wrapper for one fixture.
    fn build_pytest_fixture(
        &mut self,
        function: &ast::StmtFunctionDef,
        implementation: FunctionId,
        fixtures: &HashMap<String, (FunctionId, &'static str)>,
        _fs: &mut FuncState,
    ) -> FunctionId {
        let (line, col) = self.span(function.range);
        let mut locals: Vec<HLocal> = Vec::new();
        let mut body = Vec::new();
        let names = pytest_parameter_names(&function.args);
        for name in &names {
            let local = locals.len() as LocalId;
            locals.push(HLocal {
                name: name.clone(),
                ty: self.infer.fresh(),
            });
            let value = match self.pytest_fixture_reference(name, fixtures, line, col) {
                Some(value) => value,
                None => {
                    self.error_at(
                        function.range,
                        format!("pytest fixture '{name}' is not defined"),
                    );
                    HExpr {
                        kind: HExprKind::Const(Const::None),
                        ty: Ty::Concrete(Type::None),
                        line,
                        col,
                    }
                }
            };
            body.push(Self::pytest_stmt(
                HStmtKind::Assign {
                    target: local,
                    value,
                },
                line,
                col,
            ));
        }
        let args = (0..names.len() as LocalId)
            .map(|local| HExpr {
                kind: HExprKind::Local(local),
                ty: self.infer.fresh(),
                line,
                col,
            })
            .collect();
        body.push(Self::pytest_stmt(
            HStmtKind::Return(Some(self.pytest_call(implementation, args, line, col))),
            line,
            col,
        ));

        let id = self.functions.len() as FunctionId;
        let wrapper_name = format!("__hydro_fixture_{}", function.name);
        self.functions.push(HFunction {
            name: wrapper_name.clone(),
            params: Vec::new(),
            capture_count: 0,
            ret_ty: self.infer.fresh(),
            locals,
            body,
            is_async: false,
            is_generator: false,
            is_module: false,
            line,
            col,
        });
        // HIR function references carry absolute ids, like `create_function`
        // records in `fn_ids`. Imported units export the wrapper by this name
        // so a test file can reference a conftest fixture without importing it.
        let absolute = self.fid_base + id;
        self.fn_ids.insert(wrapper_name, absolute);
        absolute
    }

    /// Generates the zero-argument wrappers for one test function.
    fn build_pytest_tests(
        &mut self,
        function: &ast::StmtFunctionDef,
        fixtures: &HashMap<String, (FunctionId, &'static str)>,
        fs: &mut FuncState,
    ) {
        let name = function.name.to_string();
        if !name.starts_with("test") {
            return;
        }
        let skip = pytest_skip(&function.decorator_list);
        let xfail = pytest_xfail(&function.decorator_list);
        let skipif = pytest_skipif(&function.decorator_list);
        let usefixtures = pytest_usefixtures(&function.decorator_list);
        let (param_names, cases) = self.pytest_parametrize(&function.decorator_list, fs);
        let parameters = pytest_parameter_names(&function.args);

        let needs_wrapper = skip.is_some()
            || xfail.is_some()
            || skipif.is_some()
            || !usefixtures.is_empty()
            || !parameters.is_empty()
            || !param_names.is_empty();
        if !needs_wrapper {
            return;
        }
        let Some(&implementation) = self.fn_ids.get(name.as_str()) else {
            return;
        };
        let (line, col) = self.span(function.range);

        let rows: Vec<Vec<(String, HExpr, String)>> = if param_names.is_empty() {
            vec![Vec::new()]
        } else {
            cases
        };
        let local = implementation
            .checked_sub(self.fid_base)
            .unwrap_or(implementation) as usize;
        self.functions[local].name = format!("{name}__hydro_impl");
        if rows.is_empty() {
            // An empty `parametrize` list collects no tests.
            return;
        }

        let mut used_names: HashSet<String> = HashSet::new();
        for row in rows {
            let values: HashMap<&str, &HExpr> = row
                .iter()
                .map(|(name, value, _)| (name.as_str(), value))
                .collect();
            let mut locals: Vec<HLocal> = Vec::new();
            let mut body: Vec<HStmt> = Vec::new();
            body.extend(self.pytest_skip_statements(&skip, &skipif, line, col, fs));
            let mut arguments: Vec<HExpr> = Vec::with_capacity(parameters.len());
            for parameter in &parameters {
                let local = locals.len() as LocalId;
                locals.push(HLocal {
                    name: parameter.clone(),
                    ty: self.infer.fresh(),
                });
                let value = if let Some(value) = values.get(parameter.as_str()) {
                    (*value).clone()
                } else {
                    match self.pytest_fixture_reference(parameter, fixtures, line, col) {
                        Some(value) => value,
                        None => {
                            self.error_at(
                                function.range,
                                format!("pytest fixture '{parameter}' is not defined"),
                            );
                            HExpr {
                                kind: HExprKind::Const(Const::None),
                                ty: Ty::Concrete(Type::None),
                                line,
                                col,
                            }
                        }
                    }
                };
                body.push(Self::pytest_stmt(
                    HStmtKind::Assign {
                        target: local,
                        value,
                    },
                    line,
                    col,
                ));
                arguments.push(HExpr {
                    kind: HExprKind::Local(local),
                    ty: self.infer.fresh(),
                    line,
                    col,
                });
            }
            for fixture in &usefixtures {
                if parameters.iter().any(|parameter| parameter == fixture) {
                    continue;
                }
                if let Some(value) = self.pytest_fixture_reference(fixture, fixtures, line, col) {
                    body.push(Self::pytest_stmt(HStmtKind::Expr(value), line, col));
                } else {
                    self.error_at(
                        function.range,
                        format!("pytest fixture '{fixture}' is not defined"),
                    );
                }
            }
            let call = self.pytest_call(implementation, arguments, line, col);
            if let Some(reason) = &xfail {
                body.push(Self::pytest_stmt(
                    HStmtKind::Try {
                        body: vec![Self::pytest_stmt(HStmtKind::Expr(call), line, col)],
                        handlers: vec![HHandler {
                            exc: None,
                            binding: None,
                            binding_cell: false,
                            body: vec![Self::pytest_stmt(
                                HStmtKind::Expr(self.pytest_module_call(
                                    "_xfail",
                                    vec![HExpr {
                                        kind: HExprKind::Const(Const::Str(reason.clone())),
                                        ty: Ty::Concrete(Type::Str),
                                        line,
                                        col,
                                    }],
                                    line,
                                    col,
                                )),
                                line,
                                col,
                            )],
                        }],
                        else_body: Vec::new(),
                        finally_body: Vec::new(),
                    },
                    line,
                    col,
                ));
            } else {
                body.push(Self::pytest_stmt(HStmtKind::Expr(call), line, col));
            }

            let suffix = {
                let parts: Vec<String> = row
                    .iter()
                    .map(|(_, _, text)| sanitize_pytest_id(text))
                    .collect();
                let parts = if parts.is_empty() {
                    vec!["0".to_string()]
                } else {
                    parts
                };
                let base = parts.join("-");
                let mut candidate = base.clone();
                let mut counter = 1;
                while used_names.contains(&candidate) {
                    candidate = format!("{base}-{counter}");
                    counter += 1;
                }
                used_names.insert(candidate.clone());
                candidate
            };
            let wrapper_name = if param_names.is_empty() {
                name.clone()
            } else {
                format!("{name}[{suffix}]")
            };
            self.functions.push(HFunction {
                name: wrapper_name,
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Ty::Concrete(Type::None),
                locals,
                body,
                is_async: false,
                is_generator: false,
                is_module: false,
                line,
                col,
            });
        }
    }

    /// Builds the skip statements a test wrapper starts with.
    fn pytest_skip_statements(
        &mut self,
        skip: &Option<String>,
        skipif: &Option<ast::Expr>,
        line: u32,
        col: u32,
        fs: &mut FuncState,
    ) -> Vec<HStmt> {
        let mut statements = Vec::new();
        if let Some(reason) = skip {
            statements.push(Self::pytest_stmt(
                HStmtKind::Expr(self.pytest_skip_call(reason, line, col)),
                line,
                col,
            ));
            return statements;
        }
        if let Some(condition) = skipif {
            let test = self.expr(condition, fs);
            statements.push(Self::pytest_stmt(
                HStmtKind::If {
                    cond: test,
                    then_body: vec![Self::pytest_stmt(
                        HStmtKind::Expr(self.pytest_skip_call("conditionally skipped", line, col)),
                        line,
                        col,
                    )],
                    else_body: Vec::new(),
                },
                line,
                col,
            ));
        }
        statements
    }

    fn pytest_skip_call(&mut self, reason: &str, line: u32, col: u32) -> HExpr {
        self.pytest_module_call(
            "skip",
            vec![HExpr {
                kind: HExprKind::Const(Const::Str(reason.to_string())),
                ty: Ty::Concrete(Type::Str),
                line,
                col,
            }],
            line,
            col,
        )
    }

    /// Expands every `parametrize` decorator into concrete argument rows.
    #[allow(clippy::type_complexity)]
    fn pytest_parametrize(
        &mut self,
        decorators: &[ast::Expr],
        fs: &mut FuncState,
    ) -> (Vec<String>, Vec<Vec<(String, HExpr, String)>>) {
        let mut names: Vec<String> = Vec::new();
        let mut cases: Vec<Vec<(String, HExpr, String)>> = vec![Vec::new()];
        for decorator in decorators {
            if decorator_final_name(decorator).as_deref() != Some("parametrize") {
                continue;
            }
            let ast::Expr::Call(call) = decorator else {
                continue;
            };
            if call.args.len() < 2 {
                self.error_at(decorator.range(), "parametrize requires names and values");
                continue;
            }
            let Some(arguments) = pytest_string_list(&call.args[0]) else {
                self.error_at(
                    call.args[0].range(),
                    "parametrize names must be a string literal",
                );
                continue;
            };
            let Some(values) = pytest_sequence(&call.args[1]) else {
                self.error_at(
                    call.args[1].range(),
                    "parametrize values must be a literal list or tuple",
                );
                continue;
            };
            names.extend(arguments.iter().cloned());
            let mut decorator_cases: Vec<Vec<(String, HExpr, String)>> = Vec::new();
            for value in values {
                let mut row = Vec::with_capacity(arguments.len());
                if arguments.len() == 1 {
                    row.push((
                        arguments[0].clone(),
                        self.expr(value, fs),
                        self.source_text(value.range()),
                    ));
                } else {
                    let Some(elements) = pytest_sequence(value) else {
                        self.error_at(
                            value.range(),
                            "a parametrize row must be a tuple when several names are given",
                        );
                        continue;
                    };
                    if elements.len() != arguments.len() {
                        self.error_at(
                            value.range(),
                            format!(
                                "parametrize row has {} value(s) for {} name(s)",
                                elements.len(),
                                arguments.len()
                            ),
                        );
                        continue;
                    }
                    for (name, element) in arguments.iter().zip(elements) {
                        row.push((
                            name.clone(),
                            self.expr(element, fs),
                            self.source_text(element.range()),
                        ));
                    }
                }
                decorator_cases.push(row);
            }
            let mut combined = Vec::with_capacity(cases.len() * decorator_cases.len());
            for case in &cases {
                for decorator_case in &decorator_cases {
                    let mut merged = case.clone();
                    merged.extend(decorator_case.iter().cloned());
                    combined.push(merged);
                }
            }
            cases = combined;
        }
        (names, cases)
    }

    /// Mirrors a module-level function into the runtime global table.
    ///
    /// Importers normally link an imported function directly by id. That is
    /// impossible for a module the loader marked late — one that is still
    /// initializing when the importer is lowered (an ancestor package, or a
    /// module reached by a deferred import that closes a cycle). Those members
    /// are read by name from the module's global table, whose function entries
    /// this statement creates. Only imported units mirror, because only they
    /// have importers.
    fn mirror_module_function(
        &mut self,
        start: usize,
        def_ids: &HashMap<usize, FunctionId>,
        body: &mut Vec<HStmt>,
    ) {
        if !self.mirror_all_globals {
            return;
        }
        let Some(&fid) = def_ids.get(&start) else {
            return;
        };
        let function = &self.functions[fid as usize];
        let (name, line, col) = (function.name.clone(), function.line, function.col);
        body.push(HStmt {
            kind: HStmtKind::GlobalSet {
                module: self.dunder_name.clone(),
                name,
                value: HExpr {
                    kind: HExprKind::FunctionRef(self.fid_base + fid),
                    ty: Ty::Concrete(Type::Function),
                    line,
                    col,
                },
            },
            line,
            col,
        });
    }

    /// Bind the module dunders this unit loads: `__name__` (the full dotted
    /// module name, `__main__` for the entry), `__file__` and `__doc__`.
    /// Each binding becomes an `Assign` of a constant followed by the matching
    /// `GlobalSet`, so functions and methods of the unit can read it with a
    /// `GlobalGet`. Nothing is emitted for a dunder the unit never loads.
    fn lower_dunder_bindings(
        &mut self,
        module: &ast::ModModule,
        module_locals: &mut HashMap<String, LocalId>,
    ) -> Vec<HStmt> {
        let bindings: Vec<(&'static str, Const)> = vec![
            ("__name__", Const::Str(self.dunder_name.clone())),
            ("__file__", Const::Str(self.filename.to_string())),
            (
                "__doc__",
                module_docstring(&module.body)
                    .map(Const::Str)
                    .unwrap_or(Const::None),
            ),
        ];

        let mut inits = Vec::new();
        for (name, value) in bindings {
            if !module_references_name(&module.body, name) && !self.template_loads_name(name) {
                continue;
            }
            let ty = Ty::Concrete(value.ty());
            let local = match module_locals.get(name) {
                Some(&local) => {
                    // A user assignment may already have claimed the slot;
                    // pin it to the injected value's type.
                    let slot = self.functions[0].locals[local as usize].ty;
                    let _ = self.infer.unify(slot, ty);
                    local
                }
                None => {
                    let local = self.functions[0].locals.len() as LocalId;
                    self.functions[0].locals.push(HLocal {
                        name: name.to_string(),
                        ty,
                    });
                    module_locals.insert(name.to_string(), local);
                    self.module_local_names.insert(name.to_string());
                    local
                }
            };
            // Functions read the value through the runtime global table.
            self.global_names.insert(name.to_string());
            self.used_globals.insert(name.to_string());
            let read = HExpr {
                kind: HExprKind::Local(local),
                ty,
                line: 1,
                col: 1,
            };
            inits.push(HStmt {
                kind: HStmtKind::Assign {
                    target: local,
                    value: HExpr {
                        kind: HExprKind::Const(value),
                        ty,
                        line: 1,
                        col: 1,
                    },
                },
                line: 1,
                col: 1,
            });
            inits.push(HStmt {
                kind: HStmtKind::GlobalSet {
                    module: self.dunder_name.clone(),
                    name: name.to_string(),
                    value: read,
                },
                line: 1,
                col: 1,
            });
        }
        inits
    }

    // ------------------------------------------------------------------
    // Positions / diagnostics
    // ------------------------------------------------------------------

    fn span(&self, range: TextRange) -> (u32, u32) {
        line_col(self.source, u32::from(range.start()) as usize)
    }

    /// Source text covered by `range`, used for pytest assertion messages.
    fn source_text(&self, range: TextRange) -> String {
        let start = (u32::from(range.start()) as usize).min(self.source.len());
        let end = (u32::from(range.end()) as usize).min(self.source.len());
        self.source.get(start..end).unwrap_or("").trim().to_string()
    }

    /// Start position plus the exclusive end column of `range`, clamped to the
    /// start line so a caret snippet never runs past its line.
    fn span_range(&self, range: TextRange) -> (u32, u32, u32) {
        let bytes = self.source.as_bytes();
        let start = (u32::from(range.start()) as usize).min(bytes.len());
        let (line, col) = line_col(self.source, start);
        let end = u32::from(range.end()) as usize;
        let (end_line, end_col) = line_col(self.source, end);
        let end_col = if end_line > line {
            let mut line_end = start;
            while line_end < bytes.len() && bytes[line_end] != b'\n' {
                line_end += 1;
            }
            col + (line_end - start) as u32
        } else {
            end_col
        };
        (line, col, end_col.max(col.saturating_add(1)))
    }

    fn error_at_code(&mut self, range: TextRange, code: &'static str, message: impl Into<String>) {
        let (line, col, end_col) = self.span_range(range);
        self.diags.push(
            Diagnostic::new(self.filename, line, col, message)
                .with_code(code)
                .with_span(end_col),
        );
    }

    fn error_at(&mut self, range: TextRange, message: impl Into<String>) {
        let (line, col, end_col) = self.span_range(range);
        self.diags.push(
            Diagnostic::new(self.filename, line, col, message)
                .with_code(codes::UNSUPPORTED)
                .with_span(end_col)
                .with_help("this construct is not implemented yet; see docs/TODO.md"),
        );
    }

    /// Reports an undefined name and suggests the closest local, function or
    /// builtin when it is within a small edit distance.
    fn error_name(
        &mut self,
        range: TextRange,
        name: &str,
        fs: &FuncState,
        message: impl Into<String>,
    ) {
        let (line, col, end_col) = self.span_range(range);
        let mut diagnostic = Diagnostic::new(self.filename, line, col, message)
            .with_code(codes::NAME)
            .with_span(end_col);
        if let Some(candidate) = self.suggest_name(name, fs) {
            diagnostic = diagnostic.with_help(format!("did you mean `{candidate}`?"));
        }
        self.diags.push(diagnostic);
    }

    fn suggest_name(&self, name: &str, fs: &FuncState) -> Option<String> {
        let candidates = fs
            .locals
            .keys()
            .map(String::as_str)
            .chain(self.fn_ids.keys().map(String::as_str))
            .chain(BUILTIN_NAMES.iter().copied());
        diag::closest(name, candidates).map(str::to_string)
    }

    fn error_expr(&mut self, range: TextRange) -> HExpr {
        let (line, col) = self.span(range);
        HExpr {
            kind: HExprKind::Const(Const::None),
            ty: Ty::Concrete(Type::None),
            line,
            col,
        }
    }

    // ------------------------------------------------------------------
    // Imports
    // ------------------------------------------------------------------

    /// Records module-level names that this unit's own functions and methods
    /// read, so their module-body assignments and class definitions mirror
    /// into the runtime global table.
    ///
    /// This must run before the module body is lowered: a `def` nested in a
    /// module-level compound statement is only discovered while its enclosing
    /// block is lowered, so a preceding assignment would otherwise be missed.
    /// Names are intersected with [`Self::global_names`]: only bindings this
    /// module actually creates can be mirrored.
    fn collect_function_read_globals(&mut self, stmts: &[ast::Stmt]) {
        let mut names = Vec::new();
        collect_module_function_free_names(stmts, &mut names);
        // Templates hide their interpolation loads from the AST walk.
        names.extend(self.template_free_names());
        for name in names {
            if self.global_names.contains(&name) {
                self.used_globals.insert(name);
            }
        }
    }

    /// Collects module-level imports, which the loader links eagerly.
    ///
    /// Import statements emit no IR: they only affect name resolution.
    /// Module-level imports execute before any function of the unit, so the
    /// names they bind are visible everywhere and can be called directly. An
    /// import under an `if`, loop, `try`, class or function body is collected
    /// by [`Self::collect_nested_imports`] and deferred instead.
    fn collect_top_imports(&mut self, stmts: &[ast::Stmt]) {
        for stmt in stmts {
            match stmt {
                ast::Stmt::Import(s) => {
                    for alias in &s.names {
                        self.collect_import(alias, false);
                    }
                }
                ast::Stmt::ImportFrom(s) => self.collect_import_from(s, false),
                _ => {}
            }
        }
    }

    /// Collects imports nested inside functions, classes and compound bodies.
    ///
    /// The loader does not execute these imports, so a name they bind that
    /// cannot be linked (a user module that is not already a dependency)
    /// becomes a [`DeferredImport`]: reading it raises `RuntimeError` instead
    /// of printing a compile error or silently ignoring the import. Guards
    /// fixed by the import contract are still skipped (`if TYPE_CHECKING:`
    /// and `if __name__ == "__main__":`, see [`static_if_branch`]), so
    /// type-only imports never bind anything.
    ///
    /// `root` is true for the module body, whose direct imports were already
    /// handled by [`Self::collect_top_imports`]; compound statements below it
    /// are all nested.
    fn collect_nested_imports(&mut self, stmts: &[ast::Stmt], root: bool) {
        for stmt in stmts {
            match stmt {
                ast::Stmt::Import(s) => {
                    if !root {
                        for alias in &s.names {
                            self.collect_import(alias, true);
                        }
                    }
                }
                ast::Stmt::ImportFrom(s) => {
                    if !root {
                        self.collect_import_from(s, true);
                    }
                }
                ast::Stmt::FunctionDef(s) => self.collect_nested_imports(&s.body, false),
                ast::Stmt::AsyncFunctionDef(s) => self.collect_nested_imports(&s.body, false),
                ast::Stmt::ClassDef(s) => self.collect_nested_imports(&s.body, false),
                ast::Stmt::If(s) => {
                    let branch = static_if_branch(&s.test, &self.dunder_name);
                    match branch {
                        Some(true) => self.collect_nested_imports(&s.body, false),
                        Some(false) => self.collect_nested_imports(&s.orelse, false),
                        None => {
                            self.collect_nested_imports(&s.body, false);
                            self.collect_nested_imports(&s.orelse, false);
                        }
                    }
                }
                ast::Stmt::For(s) => {
                    self.collect_nested_imports(&s.body, false);
                    self.collect_nested_imports(&s.orelse, false);
                }
                ast::Stmt::AsyncFor(s) => {
                    self.collect_nested_imports(&s.body, false);
                    self.collect_nested_imports(&s.orelse, false);
                }
                ast::Stmt::While(s) => {
                    self.collect_nested_imports(&s.body, false);
                    self.collect_nested_imports(&s.orelse, false);
                }
                ast::Stmt::Try(s) => {
                    self.collect_nested_imports(&s.body, false);
                    for handler in &s.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        self.collect_nested_imports(&handler.body, false);
                    }
                    self.collect_nested_imports(&s.orelse, false);
                    self.collect_nested_imports(&s.finalbody, false);
                }
                ast::Stmt::With(s) => self.collect_nested_imports(&s.body, false),
                ast::Stmt::AsyncWith(s) => self.collect_nested_imports(&s.body, false),
                _ => {}
            }
        }
    }

    /// Claims a binding name for an import.
    ///
    /// Top-level imports always win: a nested import never replaces a name
    /// bound by one. Returns false when the nested import must be ignored.
    fn claim_import_binding(&mut self, binding: &str, nested: bool) -> bool {
        if nested {
            return !self.top_level_imports.contains(binding);
        }
        self.top_level_imports.insert(binding.to_string());
        true
    }

    /// Records a name bound by a nested import that could not be linked.
    fn record_deferred(&mut self, binding: &str, module: &str, member: Option<&str>) {
        self.deferred_imports.insert(
            binding.to_string(),
            DeferredImport {
                module: module.to_string(),
                member: member.map(str::to_string),
            },
        );
    }

    fn collect_import(&mut self, alias: &ast::Alias, nested: bool) {
        let imported = alias.name.as_str();
        let binding = match &alias.asname {
            Some(name) => name.as_str(),
            None => imported.split('.').next().unwrap_or(imported),
        };
        if !self.claim_import_binding(binding, nested) {
            return;
        }
        // A dotted import without an alias binds the top-level package; an
        // alias binds the full module.
        let module = match &alias.asname {
            Some(_) => imported,
            None => binding,
        };
        if let Some(spec) = stdlib::module(imported) {
            self.module_aliases.insert(binding.to_string(), spec);
            return;
        }
        if self.user_modules.contains_key(imported) {
            self.user_module_aliases
                .insert(binding.to_string(), module.to_string());
            return;
        }
        match self.find_extension(imported) {
            ExtensionLookup::Found(path) => {
                if alias.asname.is_none() && imported.contains('.') {
                    self.error_at_code(
                        alias.range,
                        codes::UNSUPPORTED,
                        "dotted extension imports require an alias",
                    );
                    return;
                }
                self.extension_aliases
                    .insert(binding.to_string(), path.to_string_lossy().into_owned());
                return;
            }
            ExtensionLookup::Incompatible { path, reason } => {
                self.error_incompatible_extension(alias.range, &path, &reason);
                return;
            }
            ExtensionLookup::Missing => {}
        }
        if nested && self.may_defer(imported) {
            // The loader did not execute the import, so the module is not
            // linked: bind the alias to the runtime error stub.
            self.record_deferred(binding, module, None);
            return;
        }
        // A module that is still initializing has no known exports, so a plain
        // `import` would bind a module value with nothing to read attributes
        // from. Members can still be bound with `from X import name`.
        if self.late_modules.contains(imported) {
            self.error_at_code(
                alias.range,
                codes::UNSUPPORTED,
                format!("module '{imported}' is still initializing; import its members with 'from {imported} import <name>'"),
            );
            return;
        }
        self.error_unsupported_module(alias.range, imported);
    }

    fn collect_import_from(&mut self, s: &ast::StmtImportFrom, nested: bool) {
        let level = s.level.as_ref().map(|level| level.to_u32()).unwrap_or(0);
        let module_name = match resolve_import_name(
            level,
            s.module.as_ref().map(|module| module.as_str()),
            &self.package,
        ) {
            Ok(name) => name,
            Err(message) => {
                self.error_at_code(s.range, codes::UNSUPPORTED, message);
                return;
            }
        };
        let imported = module_name.as_str();

        // `from __future__ import annotations` (and every other future
        // feature) changes no semantics in this frontend: annotations are
        // hints and are never evaluated.
        if imported == "__future__" {
            return;
        }

        if let Some(module) = stdlib::module(imported) {
            for alias in &s.names {
                let member = alias.name.as_str();
                if member == "*" {
                    self.import_native_star(module, nested);
                    continue;
                }
                let binding = match &alias.asname {
                    Some(name) => name.as_str(),
                    None => member,
                };
                if !self.claim_import_binding(binding, nested) {
                    continue;
                }
                match module_member(module, member) {
                    Some(ModuleMember::Function(function)) => {
                        self.imported_functions
                            .insert(binding.to_string(), (module, function));
                    }
                    Some(ModuleMember::Constant(constant)) => {
                        self.imported_constants
                            .insert(binding.to_string(), (module, constant));
                    }
                    None => self.error_unknown_member(alias.range, module, member),
                }
            }
            return;
        }

        let base = self.user_modules.get(imported).cloned();
        let self_package = imported == self.dunder_name;
        let any_submodule = s.names.iter().any(|alias| {
            alias.name.as_str() != "*"
                && self
                    .user_modules
                    .contains_key(&format!("{imported}.{}", alias.name.as_str()))
        });
        if base.is_some() || self_package || any_submodule {
            for alias in &s.names {
                let member = alias.name.as_str();
                if member == "*" {
                    if self_package {
                        // The package's own exports are only complete after
                        // its signatures are registered.
                        self.pending_self_stars.push(alias.range);
                    } else if let Some(module) = &base {
                        self.import_user_star(imported, module, nested);
                    } else {
                        self.error_at_code(
                            alias.range,
                            codes::UNSUPPORTED,
                            format!(
                                "'from {imported} import *' is not supported for a module that is not loaded"
                            ),
                        );
                    }
                    continue;
                }
                let binding = match &alias.asname {
                    Some(name) => name.as_str(),
                    None => member,
                };
                if !self.claim_import_binding(binding, nested) {
                    continue;
                }
                // `from pkg import sub` binds the submodule itself.
                let submodule = format!("{imported}.{member}");
                if self.user_modules.contains_key(&submodule) {
                    self.user_module_aliases
                        .insert(binding.to_string(), submodule);
                    continue;
                }
                let export = if self_package {
                    self.self_export(member)
                } else {
                    base.as_ref()
                        .and_then(|module| module.exports.get(member))
                        .copied()
                };
                match export {
                    Some(UserExport::Function(id)) => {
                        self.imported_user_functions.insert(binding.to_string(), id);
                    }
                    Some(UserExport::Global) => {
                        self.imported_user_globals.insert(
                            binding.to_string(),
                            (imported.to_string(), member.to_string()),
                        );
                    }
                    None if self_package && !nested => self.pending_self_imports.push((
                        binding.to_string(),
                        member.to_string(),
                        alias.range,
                    )),
                    None if nested && self.may_defer(imported) => {
                        self.record_deferred(binding, imported, Some(member));
                    }
                    None => self.error_at_code(
                        alias.range,
                        codes::ATTRIBUTE,
                        format!("module '{imported}' does not export a function named '{member}'"),
                    ),
                }
            }
            return;
        }

        // A module that is still initializing when this unit is lowered (the
        // loader records it in `late`): its exports are not known yet, so
        // members are bound late through the runtime global table of that
        // module. Initializers mirror their functions and values there, and
        // the read happens when a function of this unit runs — after the other
        // module finished initializing.
        if self.is_initializing_ancestor(imported) || self.late_modules.contains(imported) {
            for alias in &s.names {
                let member = alias.name.as_str();
                if member == "*" {
                    self.error_at_code(
                        alias.range,
                        codes::UNSUPPORTED,
                        format!(
                            "'from {imported} import *' is not supported while '{imported}' is still initializing"
                        ),
                    );
                    continue;
                }
                let binding = match &alias.asname {
                    Some(name) => name.as_str(),
                    None => member,
                };
                if !self.claim_import_binding(binding, nested) {
                    continue;
                }
                let submodule = format!("{imported}.{member}");
                if self.user_modules.contains_key(&submodule) {
                    self.user_module_aliases
                        .insert(binding.to_string(), submodule);
                } else {
                    self.imported_user_globals.insert(
                        binding.to_string(),
                        (imported.to_string(), member.to_string()),
                    );
                }
            }
            return;
        }

        match self.find_extension(imported) {
            ExtensionLookup::Found(path) => {
                let path = path.to_string_lossy().into_owned();
                self.collect_extension_members(s, imported, &path, nested);
            }
            ExtensionLookup::Incompatible { path, reason } => {
                self.error_incompatible_extension(s.range, &path, &reason);
            }
            ExtensionLookup::Missing if nested && self.may_defer(imported) => {
                // The loader never probes nested modules, so the module may
                // simply not be a dependency. Record the bindings as deferred
                // instead of rejecting the program at compile time.
                for alias in &s.names {
                    let member = alias.name.as_str();
                    if member == "*" {
                        self.error_at_code(
                            alias.range,
                            codes::UNSUPPORTED,
                            format!(
                                "'from {imported} import *' is not supported for a module that is not loaded"
                            ),
                        );
                        continue;
                    }
                    let binding = match &alias.asname {
                        Some(name) => name.as_str(),
                        None => member,
                    };
                    if !self.claim_import_binding(binding, true) {
                        continue;
                    }
                    self.record_deferred(binding, imported, Some(member));
                }
            }
            ExtensionLookup::Missing => self.error_unsupported_module(s.range, imported),
        }
    }

    /// Expands `from <native module> import *` into the spec's public
    /// functions and constants. Underscore-prefixed spec entries are internal,
    /// exactly like CPython without `__all__`.
    fn import_native_star(&mut self, module: &'static ModuleSpec, nested: bool) {
        for function in module.functions {
            if function.name.starts_with('_') {
                continue;
            }
            if !self.claim_import_binding(function.name, nested) {
                continue;
            }
            self.imported_functions
                .insert(function.name.to_string(), (module, function));
        }
        for constant in module.constants {
            if constant.name.starts_with('_') {
                continue;
            }
            if !self.claim_import_binding(constant.name, nested) {
                continue;
            }
            self.imported_constants
                .insert(constant.name.to_string(), (module, constant));
        }
    }

    /// Expands `from <user module> import *`.
    ///
    /// A literal `__all__` is honoured in order and filtered to names this
    /// compilation can actually resolve (an exported function/global or a
    /// visible submodule). Without `__all__`, every non-underscore export and
    /// direct submodule is bound in stable sorted order.
    fn import_user_star(&mut self, module_name: &str, module: &UserModule, nested: bool) {
        let names: Vec<String> = match &module.all {
            Some(all) => {
                let mut names = Vec::with_capacity(all.len());
                for name in all {
                    let submodule = format!("{module_name}.{name}");
                    if (module.exports.contains_key(name)
                        || self.user_modules.contains_key(&submodule))
                        && !names.contains(name)
                    {
                        names.push(name.clone());
                    }
                }
                names
            }
            None => {
                let mut names: Vec<String> = module
                    .exports
                    .keys()
                    .filter(|name| !name.starts_with('_'))
                    .cloned()
                    .collect();
                let prefix = format!("{module_name}.");
                for candidate in self.user_modules.keys() {
                    if let Some(rest) = candidate.strip_prefix(&prefix) {
                        if !rest.contains('.') && !rest.starts_with('_') {
                            names.push(rest.to_string());
                        }
                    }
                }
                names.sort();
                names.dedup();
                names
            }
        };
        for name in names {
            if !self.claim_import_binding(&name, nested) {
                continue;
            }
            let submodule = format!("{module_name}.{name}");
            if self.user_modules.contains_key(&submodule) {
                self.user_module_aliases.insert(name, submodule);
                continue;
            }
            match module.exports.get(&name).copied() {
                Some(UserExport::Function(id)) => {
                    self.imported_user_functions.insert(name, id);
                }
                Some(UserExport::Global) => {
                    self.imported_user_globals
                        .insert(name.clone(), (module_name.to_string(), name));
                }
                // Filtered by the `__all__` check above; anonymous
                // (`__all__` absent) names are all exports.
                None => {}
            }
        }
    }

    /// Expands `from . import *` once this unit's own signatures are known.
    fn resolve_self_stars(&mut self) {
        let ranges = std::mem::take(&mut self.pending_self_stars);
        for _range in ranges {
            let names: Vec<String> = match &self.module_all {
                Some(all) => all.clone(),
                None => {
                    let mut names: Vec<String> = self
                        .fn_ids
                        .keys()
                        .cloned()
                        .chain(self.global_names.iter().cloned())
                        .filter(|name| !name.starts_with('_'))
                        .collect();
                    names.sort();
                    names.dedup();
                    names
                }
            };
            for name in names {
                if !self.claim_import_binding(&name, false) {
                    continue;
                }
                match self.self_export(&name) {
                    Some(UserExport::Function(id)) => {
                        self.imported_user_functions.insert(name, id);
                    }
                    Some(UserExport::Global) => {
                        self.imported_user_globals
                            .insert(name.clone(), (self.dunder_name.clone(), name));
                    }
                    None => {}
                }
            }
        }
    }

    /// Export of a name defined by the unit itself, used by
    /// `from . import name` inside a package `__init__.py`.
    fn self_export(&self, name: &str) -> Option<UserExport> {
        if let Some(&id) = self.fn_ids.get(name) {
            return Some(UserExport::Function(id));
        }
        self.global_names
            .contains(name)
            .then_some(UserExport::Global)
    }

    /// Whether `name` is a proper ancestor package of this unit.
    ///
    /// Such a package is still initializing when the loader visits this unit
    /// (its `__init__.py` triggered the load), so it is not among the visible
    /// user modules and its exports are not known yet. [`Self::late_modules`]
    /// covers the same situation for modules recorded by the loader; both bind
    /// members without a resolved export.
    fn is_initializing_ancestor(&self, name: &str) -> bool {
        self.dunder_name.len() > name.len()
            && self.dunder_name.starts_with(name)
            && self.dunder_name.as_bytes()[name.len()] == b'.'
    }

    /// Resolves `from . import name` bindings that refer to names defined by
    /// this package `__init__.py` itself.
    fn resolve_self_imports(&mut self) {
        for (binding, member, range) in std::mem::take(&mut self.pending_self_imports) {
            match self.self_export(&member) {
                Some(UserExport::Function(id)) => {
                    self.imported_user_functions.insert(binding, id);
                }
                Some(UserExport::Global) => {
                    self.imported_user_globals
                        .insert(binding, (self.dunder_name.clone(), member));
                }
                None => self.error_at_code(
                    range,
                    codes::ATTRIBUTE,
                    format!(
                        "module '{}' does not export a function named '{member}'",
                        self.dunder_name
                    ),
                ),
            }
        }
        self.resolve_self_stars();
    }

    /// Looks up a compiled extension for `name` (possibly dotted).
    fn find_extension(&self, name: &str) -> ExtensionLookup {
        find_extension_for_source(Path::new(self.filename), name)
    }

    /// Reports a shared object whose wheel ABI tag Hydro cannot load.
    fn error_incompatible_extension(&mut self, range: TextRange, path: &Path, reason: &str) {
        self.error_at_code(
            range,
            codes::UNSUPPORTED,
            format!(
                "compiled extension '{}' is incompatible: {reason}",
                path.display()
            ),
        );
    }

    /// Records `from ext import member [as x]` bindings. Star imports are
    /// rejected: an extension's members are not knowable statically.
    fn collect_extension_members(
        &mut self,
        s: &ast::StmtImportFrom,
        module: &str,
        path: &str,
        nested: bool,
    ) {
        for alias in &s.names {
            let member = alias.name.as_str();
            if member == "*" {
                self.error_at_code(
                    alias.range,
                    codes::UNSUPPORTED,
                    format!("'from {module} import *' is not supported for extension modules"),
                );
                continue;
            }
            let binding = match &alias.asname {
                Some(name) => name.as_str(),
                None => member,
            };
            if !self.claim_import_binding(binding, nested) {
                continue;
            }
            self.imported_extension_members
                .insert(binding.to_string(), (path.to_string(), member.to_string()));
        }
    }

    /// Mirrors the bindings of a module-level `import` statement into the
    /// session global table.
    ///
    /// A REPL fragment is compiled separately from the fragment that follows
    /// it, so import names must live in the runtime table rather than only in
    /// the binding tables of this compilation unit. Extension module aliases
    /// have no first-class value in the IR and are therefore not mirrored;
    /// their members (`from ext import name`) are.
    fn lower_session_import(&mut self, s: &ast::StmtImport, line: u32, col: u32) -> Option<HStmt> {
        let mut stores = Vec::new();
        for alias in &s.names {
            let binding = import_binding(alias);
            let value = if let Some(module) = self.user_module_aliases.get(&binding).cloned() {
                self.module_ref_expr(&module, line, col)
            } else if let Some(module) = self.module_aliases.get(&binding).copied() {
                self.module_ref_expr(module.name, line, col)
            } else {
                continue;
            };
            stores.push(self.session_global_store(&binding, value, line, col));
        }
        self.session_import_statement(stores, line, col)
    }

    /// Mirrors the bindings of a module-level `from ... import ...` statement
    /// into the session global table.
    fn lower_session_import_from(
        &mut self,
        s: &ast::StmtImportFrom,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let mut stores = Vec::new();
        for alias in &s.names {
            if alias.name.as_str() == "*" {
                continue;
            }
            let binding = import_binding(alias);
            let value = if let Some(&function) = self.imported_user_functions.get(&binding) {
                HExpr {
                    kind: HExprKind::FunctionRef(function),
                    ty: Ty::Concrete(Type::Function),
                    line,
                    col,
                }
            } else if let Some((module, name)) = self.imported_user_globals.get(&binding).cloned() {
                HExpr {
                    kind: HExprKind::GlobalGet { module, name },
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                }
            } else if let Some(module) = self.user_module_aliases.get(&binding).cloned() {
                self.module_ref_expr(&module, line, col)
            } else if let Some((module, function)) = self.imported_functions.get(&binding).copied()
            {
                self.module_attr_expr(module.name, function.name, line, col)
            } else if let Some((module, constant)) = self.imported_constants.get(&binding).copied()
            {
                self.module_attr_expr(module.name, constant.name, line, col)
            } else if let Some((path, member)) =
                self.imported_extension_members.get(&binding).cloned()
            {
                HExpr {
                    kind: HExprKind::ExtensionAttr { path, member },
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                }
            } else {
                continue;
            };
            stores.push(self.session_global_store(&binding, value, line, col));
        }
        self.session_import_statement(stores, line, col)
    }

    fn module_ref_expr(&self, module: &str, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::ModuleRef {
                module: module.to_string(),
            },
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        }
    }

    fn module_attr_expr(&self, module: &str, name: &str, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::ModuleAttr {
                module: module.to_string(),
                name: name.to_string(),
            },
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        }
    }

    fn session_global_store(&self, name: &str, value: HExpr, line: u32, col: u32) -> HStmt {
        HStmt {
            kind: HStmtKind::GlobalSet {
                module: self.dunder_name.clone(),
                name: name.to_string(),
                value,
            },
            line,
            col,
        }
    }

    /// Wraps several import bindings in one statement. Every binding must run
    /// where its import statement runs, so an `if True:` block keeps them
    /// together without introducing a new scope.
    fn session_import_statement(
        &mut self,
        mut stores: Vec<HStmt>,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        match stores.len() {
            0 => None,
            1 => stores.pop(),
            _ => Some(HStmt {
                kind: HStmtKind::If {
                    cond: HExpr {
                        kind: HExprKind::Const(Const::Bool(true)),
                        ty: Ty::Concrete(Type::Bool),
                        line,
                        col,
                    },
                    then_body: stores,
                    else_body: Vec::new(),
                },
                line,
                col,
            }),
        }
    }

    /// Resolves the base of an attribute chain to an imported extension.
    ///
    /// A local of the same name shadows the import, exactly like module
    /// resolution.
    fn resolve_extension_alias(&self, expr: &ast::Expr, fs: &FuncState) -> Option<String> {
        let ast::Expr::Name(name) = expr else {
            return None;
        };
        if fs.lookup(name.id.as_str()).is_some() {
            return None;
        }
        self.extension_aliases.get(name.id.as_str()).cloned()
    }

    /// Lowers `ext.member(...)` / `member(...)` for a compiled extension.
    ///
    /// Keyword arguments are not supported by the extension ABI yet; the call
    /// itself is checked at runtime by `hydro_extension_call`.
    fn extension_call(
        &mut self,
        path: &str,
        member: &str,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        if !call.keywords.is_empty() {
            self.error_at_code(
                call.range,
                codes::CALL,
                format!("keyword arguments are not supported for extension function '{member}'"),
            );
            return self.error_expr(call.range);
        }
        let mut args = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            args.push(self.expr(arg, fs));
        }
        HExpr {
            kind: HExprKind::Call {
                callee: Callee::Extension {
                    path: path.to_string(),
                    member: member.to_string(),
                },
                args,
            },
            // Extensions expose no static types; the result is dynamic.
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        }
    }

    /// Resolves the base of an attribute chain to an imported module.
    ///
    /// `os.path` is the only nested module: an `os` alias followed by a
    /// single `path` attribute resolves to `os.path`. A local of the same
    /// name shadows the import, exactly like a direct `module.function(...)`.
    fn resolve_module_chain(
        &self,
        expr: &ast::Expr,
        fs: &FuncState,
    ) -> Option<&'static ModuleSpec> {
        match expr {
            ast::Expr::Name(name) => {
                if fs.lookup(name.id.as_str()).is_some() {
                    return None;
                }
                self.module_aliases.get(name.id.as_str()).copied()
            }
            ast::Expr::Attribute(attr) => {
                let base = self.resolve_module_chain(&attr.value, fs)?;
                if base.name == "os" && attr.attr.as_str() == "path" {
                    stdlib::module("os.path")
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Resolves a plain name or attribute chain to a loaded user module name.
    ///
    /// `pkg` and `pkg.sub` are both modules when a dotted import loaded the
    /// package chain; a local of the same name shadows the import, matching
    /// [`Self::resolve_module_chain`].
    fn resolve_user_module_name(&self, expr: &ast::Expr, fs: &FuncState) -> Option<String> {
        match expr {
            ast::Expr::Name(name) => {
                if fs.lookup(name.id.as_str()).is_some() {
                    return None;
                }
                self.user_module_aliases.get(name.id.as_str()).cloned()
            }
            ast::Expr::Attribute(attr) => {
                let base = self.resolve_user_module_name(&attr.value, fs)?;
                let full = format!("{base}.{}", attr.attr);
                self.user_modules.contains_key(&full).then_some(full)
            }
            _ => None,
        }
    }

    /// Resolves a plain name to an imported user module. Locals shadow
    /// imports, matching [`Self::resolve_module_chain`].
    fn resolve_user_module(&self, expr: &ast::Expr, fs: &FuncState) -> Option<UserModule> {
        let name = self.resolve_user_module_name(expr, fs)?;
        self.user_modules.get(&name).cloned()
    }

    /// Resolves an expression rooted at a name bound by a deferred import.
    ///
    /// Returns the module plus the member chain: `import pkg as p` then
    /// `p.sub.f` yields `("pkg", Some("sub.f"))`, while the bare `p` yields
    /// `("pkg", None)`. Locals shadow the binding, like every other import.
    fn resolve_deferred_import(
        &self,
        expr: &ast::Expr,
        fs: &FuncState,
    ) -> Option<(String, Option<String>)> {
        match expr {
            ast::Expr::Name(name) => {
                if fs.lookup(name.id.as_str()).is_some() {
                    return None;
                }
                let binding = self.deferred_imports.get(name.id.as_str())?;
                Some((binding.module.clone(), binding.member.clone()))
            }
            ast::Expr::Attribute(attr) => {
                let (module, member) = self.resolve_deferred_import(&attr.value, fs)?;
                let member = match member {
                    Some(member) => format!("{member}.{}", attr.attr),
                    None => attr.attr.to_string(),
                };
                Some((module, Some(member)))
            }
            _ => None,
        }
    }

    /// Builds the placeholder expression for a deferred import binding.
    ///
    /// The value is dynamically typed and raises `RuntimeError` when evaluated;
    /// calls wrap it in an indirect call so arguments keep normal Python
    /// evaluation order (the callee is evaluated first, so the error wins).
    fn deferred_import_expr(
        &self,
        module: &str,
        member: Option<&str>,
        line: u32,
        col: u32,
    ) -> HExpr {
        HExpr {
            kind: HExprKind::DeferredImport {
                module: module.to_string(),
                member: member.map(str::to_string),
            },
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        }
    }

    /// Lowers `module.f(...)` for a name exported by a user module.
    ///
    /// Functions link directly; classes and other module-level values are read
    /// from the runtime global table and called indirectly.
    fn user_module_call(
        &mut self,
        module: &UserModule,
        member: &str,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        let export = module.exports.get(member).copied();
        let Some(export) = export else {
            self.error_at_code(
                call.func.range(),
                codes::ATTRIBUTE,
                format!(
                    "module '{}' does not export a function named '{member}'",
                    module.name
                ),
            );
            return self.error_expr(call.range);
        };
        if !call.keywords.is_empty() {
            self.error_at_code(
                call.range,
                codes::CALL,
                "keyword arguments are not supported for user-defined functions",
            );
            return self.error_expr(call.range);
        }
        match export {
            UserExport::Function(id) => {
                let mut args = Vec::with_capacity(call.args.len());
                for arg in &call.args {
                    args.push(self.expr(arg, fs));
                }
                HExpr {
                    kind: HExprKind::Call {
                        callee: Callee::Function(id),
                        args,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            UserExport::Global => {
                let Some(args) = self.plain_call_args(call, fs) else {
                    return self.error_expr(call.range);
                };
                let (callee_line, callee_col) = self.span(call.func.range());
                let callee = HExpr {
                    kind: HExprKind::GlobalGet {
                        module: module.name.clone(),
                        name: member.to_string(),
                    },
                    ty: Ty::Concrete(Type::Any),
                    line: callee_line,
                    col: callee_col,
                };
                HExpr {
                    kind: HExprKind::IndirectCall {
                        callee: Box::new(callee),
                        args,
                        star: None,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
        }
    }

    fn error_unsupported_module(&mut self, range: TextRange, name: &str) {
        let (line, col, end_col) = self.span_range(range);
        self.diags.push(
            Diagnostic::new(
                self.filename,
                line,
                col,
                format!("module '{name}' is not supported yet"),
            )
            .with_code(codes::UNSUPPORTED)
            .with_span(end_col)
            .with_help(SUPPORTED_MODULES_HELP),
        );
    }

    fn error_unknown_member(
        &mut self,
        range: TextRange,
        module: &'static ModuleSpec,
        member: &str,
    ) {
        self.error_at_code(
            range,
            codes::ATTRIBUTE,
            format!("module '{}' has no attribute '{}'", module.name, member),
        );
    }

    fn error_module_value(&mut self, range: TextRange, name: &str) {
        let (line, col, end_col) = self.span_range(range);
        self.diags.push(
            Diagnostic::new(
                self.filename,
                line,
                col,
                format!("module values are not supported yet: '{name}' is a module"),
            )
            .with_code(codes::UNSUPPORTED)
            .with_span(end_col)
            .with_help("use an attribute or call a module function instead"),
        );
    }

    fn module_spec_attr_expr(
        &mut self,
        module: &'static ModuleSpec,
        member: &str,
        line: u32,
        col: u32,
    ) -> Option<HExpr> {
        match module_member(module, member)? {
            ModuleMember::Function(function) => Some(HExpr {
                kind: HExprKind::ModuleAttr {
                    module: module.name.to_string(),
                    name: function.name.to_string(),
                },
                ty: Ty::Concrete(Type::Function),
                line,
                col,
            }),
            ModuleMember::Constant(constant) => Some(HExpr {
                kind: HExprKind::ModuleAttr {
                    module: module.name.to_string(),
                    name: constant.name.to_string(),
                },
                ty: Ty::Concrete(constant.ty),
                line,
                col,
            }),
        }
    }

    /// Lowers a call to a function of an imported module (`module.f(...)` or
    /// `os.path.f(...)`), reporting unknown members and non-callable
    /// constants.
    fn module_attribute_call(
        &mut self,
        module: &'static ModuleSpec,
        member: &str,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        match module_member(module, member) {
            Some(ModuleMember::Function(function)) => {
                let Some((args, keywords)) = self.module_arguments(call, fs, function, line, col)
                else {
                    return self.error_expr(call.range);
                };
                HExpr {
                    kind: HExprKind::ModuleCall {
                        module: module.name.to_string(),
                        function: function.name.to_string(),
                        args,
                        keywords,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            Some(ModuleMember::Constant(constant)) => {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!("'{}' object is not callable", constant.ty.name()),
                );
                self.error_expr(call.range)
            }
            None => {
                self.error_unknown_member(call.range, module, member);
                self.error_expr(call.range)
            }
        }
    }

    // ------------------------------------------------------------------
    // Functions
    // ------------------------------------------------------------------

    fn create_function(&mut self, f: &ast::StmtFunctionDef) -> Option<FunctionId> {
        self.create_function_decl(FnDecl::from_def(f), true)
    }

    fn create_async_function(&mut self, f: &ast::StmtAsyncFunctionDef) -> Option<FunctionId> {
        self.create_function_decl(FnDecl::from_async_def(f), true)
    }

    fn create_function_decl(&mut self, decl: FnDecl<'_>, global: bool) -> Option<FunctionId> {
        let name = decl.name.to_string();

        // `@overload` declares a type-only signature; the following
        // implementation carries the same name. Dropping the stub keeps the
        // duplicate-definition check for real definitions.
        if decl.decorators.iter().any(is_overload_decorator) {
            return None;
        }

        if global && self.fn_ids.contains_key(&name) {
            self.error_at_code(
                decl.range,
                codes::DECL,
                format!("duplicate function definition '{name}'"),
            );
            return None;
        }
        // A user function may shadow a builtin (`def print(...)`): name
        // resolution prefers `fn_ids` over the builtin table.

        self.create_function_named(&decl, name, global, None)
    }

    /// Register a method as a plain function named `Class.method`.
    ///
    /// Methods never enter [`Lowerer::fn_ids`]: a bare `method` name is not
    /// callable, and different classes may define the same method name.
    /// `@property`, `@classmethod` and `@staticmethod` are recorded as the
    /// method's binding kind; known no-op decorators are ignored.
    fn create_method_function(
        &mut self,
        f: &ast::StmtFunctionDef,
        class: &str,
    ) -> Option<(FunctionId, MethodKind)> {
        let kind = self.method_decorator_kind(&f.decorator_list);
        let name = format!("{class}.{}", f.name);
        self.create_function_named(&FnDecl::from_def(f), name, false, Some(kind))
            .map(|id| (id, kind))
    }

    fn create_async_method_function(
        &mut self,
        f: &ast::StmtAsyncFunctionDef,
        class: &str,
    ) -> Option<(FunctionId, MethodKind)> {
        let kind = self.method_decorator_kind(&f.decorator_list);
        let name = format!("{class}.{}", f.name);
        self.create_function_named(&FnDecl::from_async_def(f), name, false, Some(kind))
            .map(|id| (id, kind))
    }

    /// Shared signature lowering. `global` controls the module-level checks
    /// (duplicate definitions, builtin shadowing) that do not apply to
    /// methods. `decorator_kind` is `Some` for class methods, whose decorators
    /// were already classified and diagnosed by the caller.
    fn create_function_named(
        &mut self,
        decl: &FnDecl<'_>,
        name: String,
        global: bool,
        decorator_kind: Option<MethodKind>,
    ) -> Option<FunctionId> {
        self.create_function_with_captures(decl, name, global, &[], &[], decorator_kind)
    }

    /// Lower a function signature with optional leading closure-capture
    /// parameters. Captures are typed `Any` and become locals too, so the
    /// body resolves their names exactly like parameters. `capture_cells`
    /// marks the captures whose enclosing local is a cell.
    fn create_function_with_captures(
        &mut self,
        decl: &FnDecl<'_>,
        name: String,
        global: bool,
        captures: &[String],
        capture_cells: &[bool],
        decorator_kind: Option<MethodKind>,
    ) -> Option<FunctionId> {
        // Method decorators were classified by `create_method_function`, which
        // also reported the unsupported ones. Function decorators are applied
        // at runtime (see [`Self::lower_decorators`]); only the shapes this
        // frontend cannot apply are diagnosed here.
        if decorator_kind.is_none() {
            self.validate_function_decorators(decl.decorators);
        }
        if !decl.type_params.is_empty() {
            self.error_at(decl.range, "type parameters are not supported in Phase 1");
        }

        let args = decl.args;
        let mut params = Vec::new();
        let mut locals = Vec::new();
        let mut seen = HashSet::new();
        let mut aliases: HashSet<String> = HashSet::new();

        for (index, capture) in captures.iter().enumerate() {
            if !seen.insert(capture.clone()) {
                continue;
            }
            if capture_cells.get(index).copied().unwrap_or(false) {
                aliases.insert(capture.clone());
            }
            let local = locals.len() as LocalId;
            let ty = Ty::Concrete(Type::Any);
            params.push(HParam {
                local,
                name: capture.clone(),
                ty,
                default: None,
                default_expr: None,
                kind: ParamKind::Positional,
            });
            locals.push(HLocal {
                name: capture.clone(),
                ty,
            });
        }

        // Positional-only parameters (`/`) bind from the positional argument
        // stream only; the frontend binder and the runtime metadata keep them
        // distinct from ordinary positional parameters so a keyword of the
        // same name is forwarded to `**kwargs` or rejected.
        for arg in &args.posonlyargs {
            let default = self.param_default(arg.default.as_deref());
            let default_expr = self.lowered_default(arg.default.as_deref());
            self.add_param(
                &arg.def,
                default,
                default_expr,
                ParamKind::PosOnly,
                &mut params,
                &mut locals,
                &mut seen,
            );
        }
        for arg in &args.args {
            let default = self.param_default(arg.default.as_deref());
            let default_expr = self.lowered_default(arg.default.as_deref());
            self.add_param(
                &arg.def,
                default,
                default_expr,
                ParamKind::Positional,
                &mut params,
                &mut locals,
                &mut seen,
            );
        }
        // Signature order mirrors Python: positional, `*args`, keyword-only,
        // `**kwargs`. The slots are ordinary parameters in the generated
        // function; calls fill the list/dict or leave the literal defaults.
        if let Some(vararg) = &args.vararg {
            self.add_param(
                vararg,
                None,
                None,
                ParamKind::VarArg,
                &mut params,
                &mut locals,
                &mut seen,
            );
        }
        for arg in &args.kwonlyargs {
            let default = self.param_default(arg.default.as_deref());
            let default_expr = self.lowered_default(arg.default.as_deref());
            self.add_param(
                &arg.def,
                default,
                default_expr,
                ParamKind::KwOnly,
                &mut params,
                &mut locals,
                &mut seen,
            );
        }
        if let Some(kwarg) = &args.kwarg {
            self.add_param(
                kwarg,
                None,
                None,
                ParamKind::KwArg,
                &mut params,
                &mut locals,
                &mut seen,
            );
        }

        // A body containing `yield` makes the function a generator; combined
        // with `async def` the function is an async generator and retains
        // both flags.
        let is_generator = body_has_yield(decl.body);

        let ret_ty = match decl.returns {
            Some(ann) => self.annotation_ty(ann),
            None => self.infer.fresh(),
        };

        let (line, col) = self.span(decl.range);
        let id = self.functions.len() as FunctionId;

        // Cell analysis: a local becomes a cell when a nested function
        // captures it and this function (or a descendant's `nonlocal`)
        // mutates it. Loop targets bind a fresh iteration value and only count
        // as mutations when they are also assigned in the loop body.
        let mut nonlocal_decls = Vec::new();
        collect_scope_nonlocals(decl.body, &mut nonlocal_decls);
        let nonlocal_names: HashSet<String> =
            nonlocal_decls.into_iter().map(|(name, _)| name).collect();
        let mut scope_globals = HashSet::new();
        collect_scope_globals(decl.body, &mut scope_globals);
        let mut bound: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
        let mut assigned = Vec::new();
        collect_names(decl.body, &mut assigned, true);
        bound.extend(assigned);
        collect_import_bindings(decl.body, &mut bound);
        bound.retain(|name| !scope_globals.contains(name) && !nonlocal_names.contains(name));
        let mut mutated = HashSet::new();
        collect_scope_mutations(decl.body, &mut mutated);
        collect_nested_nonlocals(decl.body, &mut mutated);
        let mut needed = HashSet::new();
        collect_nested_needed(decl.body, &mut needed);
        needed.extend(self.nested_function_template_names(decl.body));
        let owned = bound
            .iter()
            .filter(|name| needed.contains(*name) && mutated.contains(*name))
            .filter(|name| !aliases.contains(*name))
            .cloned()
            .collect();
        self.cell_plans.insert(id, CellPlan { owned, aliases });

        self.functions.push(HFunction {
            name: name.clone(),
            params,
            capture_count: captures.len(),
            ret_ty,
            locals,
            body: Vec::new(),
            is_async: decl.is_async,
            is_generator,
            is_module: false,
            line,
            col,
        });
        if let Some(signature) = self.call_signature(self.fid_base + id) {
            self.function_signatures
                .insert(self.fid_base + id, signature);
        }
        if global {
            self.fn_ids.insert(name.clone(), self.fid_base + id);
            if self.has_runtime_function_decorators(decl.decorators) {
                // The name binds the decorator's result, not the function
                // value: reads and calls resolve through the module local
                // (module body) or the mirrored global table (functions).
                self.decorated_functions
                    .insert(name.clone(), self.fid_base + id);
                self.global_names.insert(name);
            }
        }
        Some(id)
    }

    /// Diagnose decorator shapes the frontend cannot apply to a plain
    /// function.
    ///
    /// Every other decorator is applied at runtime, bottom-up, exactly like
    /// CPython: the decorator expressions are evaluated top-down when the
    /// `def` statement runs, the function value is created, and each
    /// decorator is called with the value produced by the one below it. The
    /// pytest markers are compile-time directives consumed by the pytest
    /// synthesis pass, and `@overload` declares a type-only stub that never
    /// produces a function value.
    fn validate_function_decorators(&mut self, decorators: &[ast::Expr]) {
        for decorator in decorators {
            let name = decorator_final_name(decorator);
            if matches!(
                name.as_deref(),
                Some("property" | "classmethod" | "staticmethod")
            ) {
                self.error_at_code(
                    decorator.range(),
                    codes::UNSUPPORTED,
                    format!(
                        "'@{}' is only supported on class methods",
                        name.unwrap_or_default()
                    ),
                );
            }
        }
    }

    /// Classify a method's decorators.
    ///
    /// `@property`, `@classmethod` and `@staticmethod` select the binding
    /// kind; the no-op decorators this gate accepts are ignored without
    /// evaluating their arguments (so `@lru_cache(maxsize=1024)` needs no
    /// runtime support). Any other decorator keeps the existing diagnostic
    /// and leaves the method an ordinary one.
    fn method_decorator_kind(&mut self, decorators: &[ast::Expr]) -> MethodKind {
        let mut kind = MethodKind::Normal;
        for decorator in decorators {
            match decorator_final_name(decorator).as_deref() {
                Some("property") => kind = MethodKind::Property,
                Some("classmethod") => kind = MethodKind::Classmethod,
                Some("staticmethod") => kind = MethodKind::Staticmethod,
                _ if is_ignored_function_decorator(decorator) => {}
                _ => self.error_at(decorator.range(), "decorators are not supported in Phase 1"),
            }
        }
        kind
    }

    /// Validate a class header/body and register every method signature.
    ///
    /// The class body may contain method definitions, class attributes
    /// (`name = value`, `name: type = value`), dataclass/NamedTuple field
    /// declarations (`name: type` without a value), `pass` and a docstring;
    /// everything else (nested classes, `if` blocks, ...) is rejected with a
    /// diagnostic. The returned plan is used to lower the class statement and
    /// its method bodies later.
    fn register_class<'s>(&mut self, s: &'s ast::StmtClassDef) -> ClassPlan {
        let class = s.name.to_string();
        self.class_names.insert(class.clone());
        // `@dataclass` is consumed at compile time: the frontend only needs
        // to recognise the imported name to synthesize `__init__`. Every other
        // class decorator is evaluated and applied at runtime, bottom-up, in
        // source order. `@overload` is a type-only marker and `@rich_repr`
        // would install a generated `__repr__` that Hydro cannot synthesize,
        // so it is diagnosed instead of being silently ignored.
        let mut is_dataclass = false;
        let mut decorators = Vec::new();
        for (index, decorator) in s.decorator_list.iter().enumerate() {
            if self.is_dataclass_decorator(decorator) {
                is_dataclass = true;
            } else if is_overload_decorator(decorator) {
                // Type-only class stub; no runtime effect.
            } else if is_pytest_decorator(decorator) {
                self.error_at_code(
                    decorator.range(),
                    codes::UNSUPPORTED,
                    "pytest markers cannot decorate a class",
                );
            } else if is_noop_class_decorator(decorator) {
                self.error_at_code(
                    decorator.range(),
                    codes::UNSUPPORTED,
                    "'@rich_repr' is not supported: Hydro cannot synthesize the generated __repr__",
                );
            } else {
                decorators.push(index);
            }
        }
        if !s.type_params.is_empty() {
            self.error_at(s.range, "type parameters are not supported yet");
        }
        let mut metaclass = None;
        for keyword in &s.keywords {
            match keyword.arg.as_ref().map(|name| name.as_str()) {
                Some("metaclass") if metaclass.is_none() => {
                    metaclass = Some(
                        s.keywords
                            .iter()
                            .position(|candidate| std::ptr::eq(candidate, keyword))
                            .unwrap_or(0),
                    );
                }
                Some("metaclass") => self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    "duplicate metaclass keyword",
                ),
                other => self.error_at_code(
                    keyword.range,
                    codes::UNSUPPORTED,
                    format!(
                        "class keyword '{}' is not supported; only metaclass= is valid in a class header",
                        other.unwrap_or("**")
                    ),
                ),
            }
        }

        // Every base expression is evaluated in source order and passed to the
        // runtime, which resolves the MRO (C3) and validates the operands.
        let bases: Vec<usize> = (0..s.bases.len()).collect();
        // `class Point(NamedTuple):` gets a synthesized `__init__` exactly like
        // a dataclass. Only the class form is supported (no functional form).
        let is_namedtuple = s
            .bases
            .first()
            .is_some_and(|base| self.is_typing_namedtuple(base));

        let mut methods: Vec<(String, FunctionId, Option<u32>, MethodKind)> = Vec::new();
        let mut fields: Vec<DataclassField<'s>> = Vec::new();
        let mut attributes: Vec<(String, usize)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let dynamic = class_body_needs_namespace(&s.body);
        for (idx, stmt) in s.body.iter().enumerate() {
            if dynamic {
                // A dynamic body only needs its annotated fields here; every
                // statement (and method definition) is executed or bound at
                // runtime. Method bodies are registered after this loop.
                if let ast::Stmt::AnnAssign(ann) = stmt {
                    let ast::Expr::Name(target) = &*ann.target else {
                        continue;
                    };
                    let field = target.id.to_string();
                    if !seen.insert(field.clone()) {
                        continue;
                    }
                    let ty = self.annotation_ty(&ann.annotation);
                    let default = if is_dataclass || is_namedtuple {
                        match ann.value.as_deref() {
                            None => FieldDefault::Required,
                            Some(value) => self.annotated_field_default(value),
                        }
                    } else {
                        FieldDefault::Required
                    };
                    fields.push(DataclassField {
                        name: target.id.as_str(),
                        ty,
                        default,
                    });
                }
                continue;
            }
            if matches!(stmt, ast::Stmt::Pass(_)) {
                continue;
            }
            if let ast::Stmt::Expr(e) = stmt {
                if is_docstring_expr(&e.value) {
                    continue;
                }
            }
            // A named target is a class member: a bare annotation is a
            // dataclass/NamedTuple field (and plain annotation elsewhere), an
            // annotated assignment is a field plus a frozen class attribute,
            // and a plain assignment is a frozen class attribute.
            if let ast::Stmt::AnnAssign(ann) = stmt {
                let ast::Expr::Name(target) = &*ann.target else {
                    self.error_at(
                        ann.target.range(),
                        "class field declaration target must be a name",
                    );
                    continue;
                };
                let field = target.id.to_string();
                if !seen.insert(field.clone()) {
                    self.error_at_code(
                        target.range,
                        codes::DECL,
                        format!("duplicate class field '{class}.{field}'"),
                    );
                    continue;
                }
                let ty = self.annotation_ty(&ann.annotation);
                // Only a synthesized `__init__` consumes defaults; plain
                // classes keep their frozen class attributes untouched.
                let default = if is_dataclass || is_namedtuple {
                    match ann.value.as_deref() {
                        None => FieldDefault::Required,
                        Some(value) => self.annotated_field_default(value),
                    }
                } else {
                    FieldDefault::Required
                };
                fields.push(DataclassField {
                    name: target.id.as_str(),
                    ty,
                    default,
                });
                if ann.value.is_some() {
                    attributes.push((field, idx));
                }
                continue;
            }
            if let ast::Stmt::Assign(assign) = stmt {
                let [ast::Expr::Name(target)] = assign.targets.as_slice() else {
                    self.error_at(assign.range, "class attribute target must be a name");
                    continue;
                };
                let name = target.id.to_string();
                if !seen.insert(name.clone()) {
                    self.error_at_code(
                        target.range,
                        codes::DECL,
                        format!("duplicate class member '{class}.{name}'"),
                    );
                    continue;
                }
                // `__sub__ = difference` aliases an earlier method of the same
                // class body. The alias must behave exactly like the method
                // (receiver binding included), so it becomes another entry in
                // the method table instead of a frozen attribute value.
                if let ast::Expr::Name(value) = &*assign.value {
                    if let Some((_, function, _, kind)) = methods
                        .iter()
                        .find(|(method, _, _, _)| method.as_str() == value.id.as_str())
                    {
                        let (function, kind) = (*function, *kind);
                        methods.push((name, function, None, kind));
                        continue;
                    }
                }
                // `name = classmethod(method)` / `staticmethod(...)` /
                // `property(method)` over an earlier method of this class
                // keeps the method's function id and only changes its binding
                // kind. Any other argument (a class value such as
                // `classmethod(GenericAlias)`, an imported callable, or a
                // multi-argument property) stays a class attribute whose value
                // the runtime wraps in the corresponding descriptor object.
                if let ast::Expr::Call(call) = &*assign.value {
                    if call.keywords.is_empty() && call.args.len() == 1 {
                        if let (ast::Expr::Name(callee), ast::Expr::Name(argument)) =
                            (&*call.func, &call.args[0])
                        {
                            let kind = match callee.id.as_str() {
                                "classmethod" => Some(MethodKind::Classmethod),
                                "staticmethod" => Some(MethodKind::Staticmethod),
                                "property" => Some(MethodKind::Property),
                                _ => None,
                            };
                            if let Some(kind) = kind {
                                if let Some((_, function, _, _)) =
                                    methods.iter().find(|(method, _, _, _)| {
                                        method.as_str() == argument.id.as_str()
                                    })
                                {
                                    let function = *function;
                                    methods.push((name, function, None, kind));
                                    continue;
                                }
                            }
                        }
                    }
                }
                attributes.push((name, idx));
                continue;
            }
            let (method, range) = match stmt {
                ast::Stmt::FunctionDef(f) => (f.name.to_string(), f.range),
                ast::Stmt::AsyncFunctionDef(f) => (f.name.to_string(), f.range),
                other => {
                    self.error_at(
                        other.range(),
                        "class body may only contain method definitions, field declarations and attributes (plus pass or a docstring)",
                    );
                    continue;
                }
            };
            // `@overload` signatures are type-only: skip the stub so the real
            // implementation keeps the name. Property setters/deleters are
            // likewise unnecessary: assignment stores an instance attribute,
            // which shadows the getter.
            let skipped_method = match stmt {
                ast::Stmt::FunctionDef(f) => f
                    .decorator_list
                    .iter()
                    .any(|d| is_overload_decorator(d) || is_property_accessor_decorator(d)),
                ast::Stmt::AsyncFunctionDef(f) => f
                    .decorator_list
                    .iter()
                    .any(|d| is_overload_decorator(d) || is_property_accessor_decorator(d)),
                _ => false,
            };
            if skipped_method {
                continue;
            }
            if !seen.insert(method.clone()) {
                self.error_at_code(
                    range,
                    codes::DECL,
                    format!("duplicate method definition '{class}.{method}'"),
                );
                continue;
            }
            let registered = match stmt {
                ast::Stmt::FunctionDef(f) => self.create_method_function(f, &class),
                ast::Stmt::AsyncFunctionDef(f) => self.create_async_method_function(f, &class),
                _ => unreachable!("method statement matched above"),
            };
            if let Some((id, kind)) = registered {
                self.class_methods
                    .insert(id, (class.clone(), kind, first_param_name(self, id)));
                methods.push((method, id, Some(u32::from(range.start())), kind));
            }
        }

        // A dynamic class body executes at runtime: its method definitions
        // (including those inside `if`/`for`/`try` branches) must still be
        // registered now so their bodies are lowered with the module, and the
        // set of names bound anywhere in the body drives namespace reads.
        let mut bindings = HashSet::new();
        if dynamic {
            self.collect_dynamic_class_methods(&class, &s.body, &mut methods);
            collect_source_bound_names(&s.body, &mut bindings, true);
            // The class statement itself binds these in the namespace.
            bindings.insert("__module__".to_string());
            bindings.insert("__qualname__".to_string());
            // A dynamic class body may declare `__slots__`; the runtime
            // parses the value from the class namespace exactly like CPython
            // (`parse_slots`), so no frontend diagnostic is needed.
        }

        // Nested class definitions execute when the enclosing class body runs
        // and are registered before any body is lowered, including those
        // inside class-body control flow.
        self.register_nested_classes(&s.body);

        // A dataclass or class-form NamedTuple gets an ordinary `__init__`
        // unless it declares its own. Only annotated fields take part; methods
        // and class attributes never become parameters.
        if (is_dataclass || is_namedtuple)
            && !methods
                .iter()
                .any(|(name, _, _, _)| name.as_str() == "__init__")
        {
            let (line, col) = self.span(s.range);
            let id = self.create_dataclass_init(&class, &fields, line, col);
            methods.push(("__init__".to_string(), id, None, MethodKind::Normal));
        }

        // Constructor calls of this unit bind keywords and omitted defaults
        // against the registered `__init__` signature. `fid_base` is added so
        // `call_signature` resolves the id in multi-module programs too.
        if let Some((_, id, _, _)) = methods
            .iter()
            .find(|(name, _, _, _)| name.as_str() == "__init__")
        {
            let init = self.fid_base + *id;
            self.class_inits.insert(class.clone(), init);
            if let Some(signature) = self.call_signature(init) {
                self.class_signatures
                    .insert(class.clone(), (init, signature));
            }
        }

        ClassPlan {
            bases,
            metaclass,
            decorators,
            methods,
            attributes,
            dynamic,
            bindings,
        }
    }

    /// Registers every nested class definition reachable from a class body.
    ///
    /// Nested classes execute when the enclosing class body runs (possibly
    /// under a class-body `if`/`try`/loop) and therefore need their own plans
    /// before bodies are lowered. Method bodies are separate scopes and are
    /// not visited.
    fn register_nested_classes(&mut self, body: &[ast::Stmt]) {
        for stmt in body {
            match stmt {
                ast::Stmt::ClassDef(c) => {
                    let start = u32::from(c.range.start()) as usize;
                    if self.class_defs.contains_key(&start) {
                        continue;
                    }
                    let plan = self.register_class(c);
                    self.class_defs.insert(start, plan);
                }
                ast::Stmt::FunctionDef(_) | ast::Stmt::AsyncFunctionDef(_) => {}
                other => {
                    for block in class_body_blocks(other) {
                        self.register_nested_classes(block);
                    }
                }
            }
        }
    }

    /// Registers every method definition reachable from a dynamic class body.
    ///
    /// The definitions execute conditionally at runtime (`if not
    /// TYPE_CHECKING:` and friends), but their signatures and bodies are
    /// compiled unconditionally, exactly like CPython compiles both branches
    /// of a class-body `if`.
    fn collect_dynamic_class_methods(
        &mut self,
        class: &str,
        body: &[ast::Stmt],
        methods: &mut Vec<(String, FunctionId, Option<u32>, MethodKind)>,
    ) {
        for stmt in body {
            match stmt {
                ast::Stmt::FunctionDef(f) => {
                    if f.decorator_list
                        .iter()
                        .any(|d| is_overload_decorator(d) || is_property_accessor_decorator(d))
                    {
                        continue;
                    }
                    if let Some((id, kind)) = self.create_method_function(f, class) {
                        self.class_methods
                            .insert(id, (class.to_string(), kind, first_param_name(self, id)));
                        methods.push((
                            f.name.to_string(),
                            id,
                            Some(u32::from(f.range.start())),
                            kind,
                        ));
                    }
                }
                ast::Stmt::AsyncFunctionDef(f) => {
                    if f.decorator_list
                        .iter()
                        .any(|d| is_overload_decorator(d) || is_property_accessor_decorator(d))
                    {
                        continue;
                    }
                    if let Some((id, kind)) = self.create_async_method_function(f, class) {
                        self.class_methods
                            .insert(id, (class.to_string(), kind, first_param_name(self, id)));
                        methods.push((
                            f.name.to_string(),
                            id,
                            Some(u32::from(f.range.start())),
                            kind,
                        ));
                    }
                }
                ast::Stmt::ClassDef(_) => {}
                other => {
                    for block in class_body_blocks(other) {
                        self.collect_dynamic_class_methods(class, block, methods);
                    }
                }
            }
        }
    }

    /// Whether a class decorator is the stdlib `dataclass` decorator.
    ///
    /// Only the imported name is recognised (`from dataclasses import
    /// dataclass [as name]`, or `import dataclasses` plus
    /// `@dataclasses.dataclass`). Parametrized forms (`@dataclass(frozen=True)`)
    /// are recognised by their callee and their arguments are never evaluated.
    /// An unresolved decorator therefore keeps its "not supported" diagnostic.
    fn is_dataclass_decorator(&self, expr: &ast::Expr) -> bool {
        let is_dataclass_member = |name: &str| {
            self.imported_functions
                .get(name)
                .is_some_and(|(module, function)| {
                    module.name == "dataclasses" && function.name == "dataclass"
                })
                || self
                    .imported_constants
                    .get(name)
                    .is_some_and(|(module, constant)| {
                        module.name == "dataclasses" && constant.name == "dataclass"
                    })
        };
        let expr = match expr {
            ast::Expr::Call(call) => &*call.func,
            other => other,
        };
        match expr {
            ast::Expr::Name(name) => is_dataclass_member(name.id.as_str()),
            ast::Expr::Attribute(attr) => {
                if attr.attr.as_str() != "dataclass" {
                    return false;
                }
                let ast::Expr::Name(module) = &*attr.value else {
                    return false;
                };
                self.module_aliases
                    .get(module.id.as_str())
                    .is_some_and(|spec| spec.name == "dataclasses")
            }
            _ => false,
        }
    }

    /// Whether `expr` names `typing.NamedTuple`, imported either directly
    /// (`from typing import NamedTuple`) or through the module
    /// (`import typing [as t]` plus `t.NamedTuple`).
    fn is_typing_namedtuple(&self, expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Name(name) => {
                self.imported_constants
                    .get(name.id.as_str())
                    .is_some_and(|(module, constant)| {
                        module.name == "typing" && constant.name == "NamedTuple"
                    })
            }
            ast::Expr::Attribute(attr) => {
                if attr.attr.as_str() != "NamedTuple" {
                    return false;
                }
                let ast::Expr::Name(module) = &*attr.value else {
                    return false;
                };
                self.module_aliases
                    .get(module.id.as_str())
                    .is_some_and(|spec| spec.name == "typing")
            }
            _ => false,
        }
    }

    /// Whether `expr` names `dataclasses.field`, imported either directly
    /// (`from dataclasses import field [as name]`) or through the module
    /// (`import dataclasses [as d]` plus `d.field`).
    fn is_dataclasses_field(&self, expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Name(name) => {
                // A same-unit `def field(...)` shadows the import.
                !self.fn_ids.contains_key(name.id.as_str())
                    && self.imported_functions.get(name.id.as_str()).is_some_and(
                        |(module, function)| {
                            module.name == "dataclasses" && function.name == "field"
                        },
                    )
            }
            ast::Expr::Attribute(attr) => {
                if attr.attr.as_str() != "field" {
                    return false;
                }
                let ast::Expr::Name(module) = &*attr.value else {
                    return false;
                };
                self.module_aliases
                    .get(module.id.as_str())
                    .is_some_and(|spec| spec.name == "dataclasses")
            }
            _ => false,
        }
    }

    /// Recognises a `field(...)` call in a class-body assignment.
    fn dataclasses_field_call<'e>(&self, expr: &'e ast::Expr) -> Option<&'e ast::ExprCall> {
        let ast::Expr::Call(call) = expr else {
            return None;
        };
        self.is_dataclasses_field(&call.func).then_some(call)
    }

    /// Classifies the value of an annotated class field.
    ///
    /// The `field(...)` call is read from the AST, never executed: its
    /// `default`/`default_factory` arguments become the synthesized
    /// `__init__`'s behavior. Every other literal value is a parameter
    /// default; a non-literal value stays a required parameter (its class
    /// attribute is still frozen into the class namespace as before).
    fn annotated_field_default<'e>(&mut self, value: &'e ast::Expr) -> FieldDefault<'e> {
        if let Some(call) = self.dataclasses_field_call(value) {
            return self.dataclass_field_default(call);
        }
        match self.const_default(value) {
            Some(constant) => FieldDefault::Literal(constant),
            None => FieldDefault::Required,
        }
    }

    /// Extracts `field(default=...)` / `field(default_factory=...)`.
    ///
    /// Keyword binding errors (unknown names, duplicated arguments) stay with
    /// the ordinary call lowering of the class attribute, so this only reads
    /// the values it understands.
    fn dataclass_field_default<'e>(&mut self, call: &'e ast::ExprCall) -> FieldDefault<'e> {
        let mut default: Option<&'e ast::Expr> = None;
        let mut factory: Option<&'e ast::Expr> = None;
        if let Some(arg) = call.args.first() {
            default = Some(arg);
        }
        for keyword in &call.keywords {
            match keyword.arg.as_deref() {
                Some("default") if default.is_none() => default = Some(&keyword.value),
                Some("default_factory") if factory.is_none() => factory = Some(&keyword.value),
                _ => {}
            }
        }
        match (default, factory) {
            (Some(_), Some(_)) => {
                self.error_at(
                    call.range,
                    "field() cannot specify both default and default_factory",
                );
                FieldDefault::Required
            }
            (Some(default), None) => FieldDefault::Expr(default),
            (None, Some(factory)) => FieldDefault::Factory(factory),
            (None, None) => FieldDefault::Required,
        }
    }

    /// Synthesize a dataclass/NamedTuple `__init__(self, <fields...>)`.
    ///
    /// The result is an ordinary method: `self` is local 0, each field is one
    /// following local, and the body assigns every field through `SetAttr` in
    /// declaration order before returning `None`. Only annotated fields take
    /// part; methods, properties and class attributes never become parameters.
    ///
    /// Defaults follow the class-body AST:
    /// - a literal default (`x: int = 0`) is stored on the parameter, so a
    ///   same-unit constructor call can fill it while binding the call;
    /// - `field(default=expr)` and `field(default_factory=factory)` make the
    ///   parameter default to `None` and evaluate `expr` or call `factory` in
    ///   the body when the argument is `None`.
    fn create_dataclass_init<'s>(
        &mut self,
        class: &str,
        fields: &[DataclassField<'s>],
        line: u32,
        col: u32,
    ) -> FunctionId {
        let mut params = Vec::with_capacity(fields.len() + 1);
        let mut locals = Vec::with_capacity(fields.len() + 1);
        let self_ty = Ty::Concrete(Type::Any);
        params.push(HParam {
            local: 0,
            name: "self".to_string(),
            ty: self_ty,
            default: None,
            default_expr: None,
            kind: ParamKind::Positional,
        });
        locals.push(HLocal {
            name: "self".to_string(),
            ty: self_ty,
        });

        for field in fields {
            let local = locals.len() as LocalId;
            let default = match &field.default {
                FieldDefault::Required => None,
                FieldDefault::Literal(constant) => Some(constant.clone()),
                // The body turns `None` into the real default or a factory
                // result, so a class-value call that omits the argument still
                // records a literal `None` parameter default.
                FieldDefault::Expr(_) | FieldDefault::Factory(_) => Some(Const::None),
            };
            params.push(HParam {
                local,
                name: field.name.to_string(),
                ty: field.ty,
                default,
                default_expr: None,
                kind: ParamKind::Positional,
            });
            locals.push(HLocal {
                name: field.name.to_string(),
                ty: field.ty,
            });
        }

        // Register the function before lowering the default expressions:
        // a comprehension or call in `field(...)` may need a temporary local.
        let id = self.functions.len() as FunctionId;
        self.functions.push(HFunction {
            name: format!("{class}.__init__"),
            params,
            capture_count: 0,
            ret_ty: Ty::Concrete(Type::None),
            locals,
            body: Vec::new(),
            is_async: false,
            is_generator: false,
            is_module: false,
            line,
            col,
        });

        // Default expressions are evaluated by the class body, where field
        // names are not bound: lower them with an empty local scope so a name
        // matching a field resolves as a module global instead of a parameter.
        let mut fs = FuncState {
            fid: id,
            is_module: false,
            locals: HashMap::new(),
            cells: HashSet::new(),
            cell_aliases: HashSet::new(),
            comp_shadowed: HashMap::new(),
            globals: HashSet::new(),
            loop_depth: 0,
            in_lambda: false,
            is_async: false,
            is_generator: false,
            genexp_frame: false,
            class_ns: None,
        };

        let self_expr = HExpr {
            kind: HExprKind::Local(0),
            ty: self_ty,
            line,
            col,
        };
        let mut body = Vec::with_capacity(fields.len() + 1);
        for (index, field) in fields.iter().enumerate() {
            let local = (index + 1) as LocalId;
            let argument = HExpr {
                kind: HExprKind::Local(local),
                ty: field.ty,
                line,
                col,
            };
            let value = match &field.default {
                FieldDefault::Required | FieldDefault::Literal(_) => argument,
                FieldDefault::Expr(default) => {
                    let fallback = self.expr(default, &mut fs);
                    self.default_if_none(argument, fallback, line, col)
                }
                FieldDefault::Factory(factory) => {
                    let callee = self.expr(factory, &mut fs);
                    let fallback = HExpr {
                        kind: HExprKind::IndirectCall {
                            callee: Box::new(callee),
                            args: Vec::new(),
                            star: None,
                        },
                        ty: self.infer.fresh(),
                        line,
                        col,
                    };
                    self.default_if_none(argument, fallback, line, col)
                }
            };
            body.push(HStmt {
                kind: HStmtKind::SetAttr {
                    object: self_expr.clone(),
                    name: field.name.to_string(),
                    value,
                },
                line,
                col,
            });
        }
        body.push(HStmt {
            kind: HStmtKind::Return(None),
            line,
            col,
        });
        self.functions[id as usize].body = body;
        id
    }

    /// `fallback if argument is None else argument`, evaluated lazily.
    fn default_if_none(&mut self, argument: HExpr, fallback: HExpr, line: u32, col: u32) -> HExpr {
        let test = HExpr {
            kind: HExprKind::Bin {
                op: BinOp::Is,
                lhs: Box::new(argument.clone()),
                rhs: Box::new(HExpr {
                    kind: HExprKind::Const(Const::None),
                    ty: Ty::Concrete(Type::None),
                    line,
                    col,
                }),
            },
            ty: Ty::Concrete(Type::Bool),
            line,
            col,
        };
        HExpr {
            kind: HExprKind::IfExp {
                test: Box::new(test),
                then: Box::new(fallback),
                orelse: Box::new(argument),
            },
            ty: self.infer.fresh(),
            line,
            col,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_param(
        &mut self,
        arg: &ast::Arg,
        default: Option<Const>,
        default_expr: Option<HExpr>,
        kind: ParamKind,
        params: &mut Vec<HParam>,
        locals: &mut Vec<HLocal>,
        seen: &mut HashSet<String>,
    ) {
        let name = arg.arg.to_string();
        if !seen.insert(name.clone()) {
            self.error_at_code(
                arg.range,
                codes::DECL,
                format!("duplicate parameter name '{name}'"),
            );
            return;
        }
        // `*args` binds a list and `**kwargs` a dict. The variadic local is
        // dynamically retyped by real code (`args = (...,)`), so it stays
        // `Any`; an annotation on it only describes the elements.
        let ty = match kind {
            ParamKind::VarArg => Ty::Concrete(Type::Any),
            ParamKind::KwArg => Ty::Concrete(Type::Dict),
            _ => match &arg.annotation {
                Some(ann) => self.annotation_ty(ann),
                None => self.infer.fresh(),
            },
        };
        let local = locals.len() as LocalId;
        params.push(HParam {
            local,
            name: name.clone(),
            ty,
            default,
            default_expr,
            kind,
        });
        locals.push(HLocal { name, ty });
    }

    /// Lowers a non-literal parameter default in a module-like scope: it is
    /// evaluated when the function value is created. Diagnostics from
    /// unsupported shapes are rolled back, leaving the default `None`.
    fn lowered_default(&mut self, default: Option<&ast::Expr>) -> Option<HExpr> {
        let default = default?;
        let diagnostics = self.diags.len();
        let mut fs = FuncState {
            fid: 0,
            is_module: true,
            locals: HashMap::new(),
            cells: HashSet::new(),
            cell_aliases: HashSet::new(),
            comp_shadowed: HashMap::new(),
            globals: HashSet::new(),
            loop_depth: 0,
            in_lambda: false,
            is_async: false,
            is_generator: false,
            genexp_frame: false,
            class_ns: None,
        };
        let previous = self.in_default_expr;
        self.in_default_expr = true;
        let result = self.expr(default, &mut fs);
        self.in_default_expr = previous;
        if self.diags.len() > diagnostics {
            self.diags.truncate(diagnostics);
            return None;
        }
        Some(result)
    }

    fn param_default(&mut self, default: Option<&ast::Expr>) -> Option<Const> {
        let default = default?;
        if let Some(constant) = self.const_default(default) {
            return Some(constant);
        }
        // Common defaults are module globals or imported module constants
        // (`NO_CHANGE`, `box.HEAVY_HEAD`). Preserve them by name instead of
        // degrading to `None`; the runtime resolves them when the function or
        // method value is created/called.
        match default {
            ast::Expr::Name(name) if self.global_names.contains(name.id.as_str()) => {
                return Some(Const::Global {
                    module: self.dunder_name.clone(),
                    name: name.id.to_string(),
                })
            }
            ast::Expr::Attribute(attr) => {
                if let ast::Expr::Name(root) = &*attr.value {
                    if let Some(module) = self.user_module_aliases.get(root.id.as_str()) {
                        return Some(Const::Global {
                            module: module.clone(),
                            name: attr.attr.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
        Some(Const::None)
    }

    /// Recognise a literal constant usable as a parameter default, including a
    /// unary minus on a numeric literal. Returns `None` after reporting when
    /// the expression is not a literal.
    fn const_default(&mut self, expr: &ast::Expr) -> Option<Const> {
        match expr {
            ast::Expr::Constant(c) => match &c.value {
                ast::Constant::Int(value) => Some(int_const_from_decimal(value.to_str_radix(10))),
                ast::Constant::Float(v) => Some(Const::Float(*v)),
                ast::Constant::Bool(v) => Some(Const::Bool(*v)),
                ast::Constant::Str(v) => Some(Const::Str(v.clone())),
                ast::Constant::None => Some(Const::None),
                ast::Constant::Bytes(value) => Some(Const::Bytes(value.clone())),
                ast::Constant::Ellipsis => Some(Const::Ellipsis),
                _ => None,
            },
            ast::Expr::Tuple(tuple) => {
                let mut items = Vec::with_capacity(tuple.elts.len());
                for item in &tuple.elts {
                    items.push(self.const_default(item)?);
                }
                Some(Const::Tuple(items))
            }
            ast::Expr::UnaryOp(unary) if matches!(unary.op, ast::UnaryOp::USub) => {
                match self.const_default(&unary.operand)? {
                    Const::Int(v) => Some(negate_int_const(&v.to_string())),
                    Const::BigInt(v) => Some(negate_int_const(&v)),
                    Const::Float(v) => Some(Const::Float(-v)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Static type of an annotation.
    ///
    /// Only the builtin names carry type information; every other annotation
    /// (`Optional[int]`, `"RenderableType"`, `typing.Any`, ...) is accepted
    /// and treated as dynamic. Annotations are hints here, so none of them
    /// produces a diagnostic.
    fn annotation_ty(&mut self, ann: &ast::Expr) -> Ty {
        match ann {
            ast::Expr::Name(n) => match n.id.as_str() {
                "Any" => Ty::Concrete(Type::Any),
                "int" => Ty::Concrete(Type::Int),
                "BigInt" => Ty::Concrete(Type::BigInt),
                "float" => Ty::Concrete(Type::Float),
                "bool" => Ty::Concrete(Type::Bool),
                "str" => Ty::Concrete(Type::Str),
                "list" => Ty::Concrete(Type::List),
                "dict" => Ty::Concrete(Type::Dict),
                "tuple" => Ty::Concrete(Type::Tuple),
                "set" => Ty::Concrete(Type::Set),
                "range" => Ty::Concrete(Type::Range),
                "None" => Ty::Concrete(Type::None),
                _ => Ty::Concrete(Type::Any),
            },
            ast::Expr::Constant(c) if matches!(c.value, ast::Constant::None) => {
                Ty::Concrete(Type::None)
            }
            _ => Ty::Concrete(Type::Any),
        }
    }

    /// Lower a function body and set up its [`FuncState`].
    ///
    /// The `async`/generator flags were decided when the signature was
    /// registered, so `await`/`yield` lowering only has to read them.
    fn lower_function_body(&mut self, fid: FunctionId, body: &[ast::Stmt]) -> Vec<HStmt> {
        let mut names = Vec::new();
        collect_names(body, &mut names, true);
        let mut globals = HashSet::new();
        collect_scope_globals(body, &mut globals);
        let mut nonlocals = Vec::new();
        collect_scope_nonlocals(body, &mut nonlocals);
        let nonlocal_names: HashSet<String> =
            nonlocals.iter().map(|(name, _)| name.clone()).collect();

        let mut local_map: HashMap<String, LocalId> = HashMap::new();
        for (idx, local) in self.functions[fid as usize].locals.iter().enumerate() {
            local_map.insert(local.name.clone(), idx as LocalId);
        }
        for name in names {
            // A name declared `global` refers to the runtime global table and
            // a `nonlocal` name to an enclosing scope; neither gets a slot.
            if local_map.contains_key(&name)
                || globals.contains(&name)
                || nonlocal_names.contains(&name)
            {
                continue;
            }
            let id = self.functions[fid as usize].locals.len() as LocalId;
            let ty = self.infer.fresh();
            self.functions[fid as usize].locals.push(HLocal {
                name: name.clone(),
                ty,
            });
            local_map.insert(name, id);
        }

        let plan = self.cell_plans.remove(&fid).unwrap_or_default();
        let cell_aliases: HashSet<LocalId> = plan
            .aliases
            .iter()
            .filter_map(|name| local_map.get(name).copied())
            .collect();
        let cells: HashSet<LocalId> = plan
            .owned
            .iter()
            .filter_map(|name| local_map.get(name).copied())
            .chain(cell_aliases.iter().copied())
            .collect();
        let param_ids: HashSet<LocalId> = {
            let function = &self.functions[fid as usize];
            function.params.iter().map(|param| param.local).collect()
        };
        let function = &self.functions[fid as usize];
        let mut fs = FuncState {
            fid,
            is_module: false,
            locals: local_map,
            cells,
            cell_aliases,
            comp_shadowed: HashMap::new(),
            globals,
            loop_depth: 0,
            in_lambda: false,
            is_async: function.is_async,
            is_generator: function.is_generator,
            genexp_frame: false,
            class_ns: None,
        };

        // `nonlocal` must name a binding captured from an enclosing function.
        for (name, range) in &nonlocals {
            if fs.is_global(name) {
                self.error_at_code(
                    *range,
                    codes::DECL,
                    format!("name '{name}' is nonlocal and global"),
                );
            } else {
                match fs.lookup(name) {
                    Some(local) if fs.cell_aliases.contains(&local) => {}
                    Some(_) => self.error_at_code(
                        *range,
                        codes::DECL,
                        format!("name '{name}' is assigned to before nonlocal declaration"),
                    ),
                    None => {
                        self.error_at_code(
                            *range,
                            codes::NAME,
                            format!("no binding for nonlocal '{name}' found"),
                        );
                        // Give the invalid declaration a local slot so later
                        // references do not cascade into name diagnostics.
                        let id = self.functions[fid as usize].locals.len() as LocalId;
                        let ty = self.infer.fresh();
                        self.functions[fid as usize].locals.push(HLocal {
                            name: name.clone(),
                            ty,
                        });
                        fs.locals.insert(name.clone(), id);
                    }
                }
            }
        }

        let saved_method = self.current_class_method.clone();
        self.current_class_method = self.class_methods.get(&fid).cloned();
        let mut result = self.lower_block(body, &mut fs);
        self.current_class_method = saved_method;

        // Cells are created once at function entry. Parameter cells start
        // with their argument; every other cell starts unbound (`None`).
        let (line, col) = {
            let function = &self.functions[fid as usize];
            (function.line, function.col)
        };
        let mut owned: Vec<LocalId> = fs.cells.difference(&fs.cell_aliases).copied().collect();
        owned.sort_unstable();
        if !owned.is_empty() {
            let mut init = Vec::with_capacity(owned.len());
            for target in owned {
                let value = param_ids.contains(&target).then_some(HExpr {
                    kind: HExprKind::Local(target),
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                });
                init.push(HStmt {
                    kind: HStmtKind::CellInit { target, value },
                    line,
                    col,
                });
            }
            init.append(&mut result);
            result = init;
        }
        result
    }

    /// Lower a `def`/`async def` nested inside another function body.
    ///
    /// The nested function is appended to the function table with its
    /// captured names as leading `Any` parameters. The enclosing function
    /// stores the closure value in the nested function's local slot, so later
    /// references use the ordinary local/indirect-call path. A captured local
    /// whose binding is a cell passes the cell itself, so reads and `nonlocal`
    /// writes observe the enclosing function's current value; other captures
    /// keep by-value semantics.
    fn lower_nested_function(
        &mut self,
        decl: FnDecl<'_>,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let name = decl.name.to_string();
        let Some(target) = fs.lookup(&name) else {
            self.error_at_code(
                decl.range,
                codes::DECL,
                format!("nested function '{name}' has no local slot"),
            );
            return None;
        };

        // Depth-first free-variable analysis. Names needed by deeper nested
        // functions are included so they can pass through this scope; lambdas
        // and classes are separate, non-closure scopes.
        let mut loaded = Vec::new();
        collect_free_name_loads(decl.body, &mut loaded);
        // Templates are placeholder names in the AST; add the loads hidden in
        // their interpolations so the closure captures them.
        let mut loaded_seen: HashSet<String> = loaded.iter().cloned().collect();
        for name in self.template_free_names_in(decl.range, &HashSet::new()) {
            if loaded_seen.insert(name.clone()) {
                loaded.push(name);
            }
        }

        // Names that belong to the nested function itself never become
        // captures: parameters and everything `collect_names` sees as a local.
        let mut nested_names = Vec::new();
        collect_names(decl.body, &mut nested_names, true);
        let mut nested_locals: HashSet<String> = nested_names.into_iter().collect();
        // Imports inside the nested body resolve through the module binding
        // tables; they must not become captures of a same-named enclosing local.
        collect_import_bindings(decl.body, &mut nested_locals);
        // `global` names resolve through the runtime global table, never
        // through an enclosing local.
        collect_scope_globals(decl.body, &mut nested_locals);
        // `nonlocal` names resolve to an enclosing function's binding even
        // when the nested body only assigns to them.
        let mut nested_nonlocals = Vec::new();
        collect_scope_nonlocals(decl.body, &mut nested_nonlocals);
        for (nonlocal_name, _) in nested_nonlocals {
            nested_locals.remove(&nonlocal_name);
        }
        for arg in decl
            .args
            .posonlyargs
            .iter()
            .chain(decl.args.args.iter())
            .chain(decl.args.kwonlyargs.iter())
        {
            nested_locals.insert(arg.def.arg.to_string());
        }
        if let Some(vararg) = &decl.args.vararg {
            nested_locals.insert(vararg.arg.to_string());
        }
        if let Some(kwarg) = &decl.args.kwarg {
            nested_locals.insert(kwarg.arg.to_string());
        }

        // A self-reference would capture a slot that is not assigned until
        // after the closure is built, so treat it as unsupported recursion.
        if loaded.iter().any(|loaded_name| loaded_name == &name) && !nested_locals.contains(&name) {
            self.error_at_code(
                decl.range,
                codes::UNSUPPORTED,
                "recursive nested functions are not supported",
            );
            return None;
        }

        // Deterministic capture order: first use in the nested body.
        let mut capture_names = Vec::new();
        let mut capture_locals = Vec::new();
        let mut capture_cells = Vec::new();
        for loaded_name in &loaded {
            if nested_locals.contains(loaded_name) {
                continue;
            }
            if let Some(local) = fs.lookup(loaded_name) {
                capture_names.push(loaded_name.clone());
                capture_locals.push(local);
                capture_cells.push(fs.is_cell(local));
            }
        }

        let outer_name = self.functions[fs.fid as usize].name.clone();
        let display = format!("{outer_name}.{name}");
        let fid = self.create_function_with_captures(
            &decl,
            display,
            false,
            &capture_names,
            &capture_cells,
            None,
        )?;
        let body = self.lower_function_body(fid, decl.body);
        self.functions[fid as usize].body = body;

        let function = self.fid_base + fid;
        let decorators = self.runtime_function_decorators(decl.decorators);
        if decorators.is_empty() {
            return Some(HStmt {
                kind: HStmtKind::Closure {
                    target,
                    function,
                    captures: capture_locals,
                },
                line,
                col,
            });
        }
        // Decorated nested function: evaluate the decorators top-down, create
        // the closure in a hidden local, apply the decorators bottom-up and
        // bind the final value to the function's name. The block statement
        // keeps this a single lowered statement.
        let mut body = Vec::new();
        let temps = self.lower_decorator_temps(&decorators, fs, &mut body);
        let (function_temp, _) = self.hidden_temp(fs, "$decor", Type::Any);
        body.push(HStmt {
            kind: HStmtKind::Closure {
                target: function_temp,
                function,
                captures: capture_locals,
            },
            line,
            col,
        });
        self.apply_decorator_temps(&temps, function_temp, fs, &mut body, line, col);
        body.push(HStmt {
            kind: HStmtKind::Assign {
                target,
                value: HExpr {
                    kind: HExprKind::Local(function_temp),
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                },
            },
            line,
            col,
        });
        Some(HStmt {
            kind: HStmtKind::If {
                cond: HExpr {
                    kind: HExprKind::Const(Const::Bool(true)),
                    ty: Ty::Concrete(Type::Bool),
                    line,
                    col,
                },
                then_body: body,
                else_body: Vec::new(),
            },
            line,
            col,
        })
    }

    // ------------------------------------------------------------------
    // Statements
    // ------------------------------------------------------------------

    fn lower_block(&mut self, stmts: &[ast::Stmt], fs: &mut FuncState) -> Vec<HStmt> {
        let mut out = Vec::new();
        for stmt in stmts {
            if let Some(h) = self.lower_stmt(stmt, fs) {
                self.push_lowered_stmt(stmt, h, fs, &mut out);
            }
        }
        out
    }

    /// Appends one lowered statement and, for module-body assignments and
    /// class definitions, the matching [`HStmtKind::GlobalSet`].
    ///
    /// Imported modules mirror every module-level value: an importer may read
    /// it through the runtime global table, so whether the value is used cannot
    /// be decided while lowering a dependency. The entry module (and a
    /// single-source check) has no importers and only mirrors the names its own
    /// functions read, keeping top-level loops free of a global store per
    /// iteration. The global store reads the just-assigned local, so the
    /// right-hand side is evaluated exactly once even when it has side effects.
    /// Compound module-level blocks (`if`, `for`, `try`, ...) lower through
    /// [`Self::lower_block`], so conditional assignments are mirrored on the
    /// path that actually runs.
    fn push_lowered_stmt(
        &mut self,
        stmt: &ast::Stmt,
        h: HStmt,
        fs: &FuncState,
        out: &mut Vec<HStmt>,
    ) {
        let target = if fs.is_module {
            global_assignment_target(stmt, &h)
                .filter(|(name, _)| self.mirror_all_globals || self.used_globals.contains(name))
        } else {
            None
        };
        let (line, col) = (h.line, h.col);
        let class_global = match &h.kind {
            HStmtKind::ClassDef { target, name, .. }
                if fs.is_module
                    && (self.mirror_all_globals || self.used_globals.contains(name)) =>
            {
                Some((name.clone(), *target))
            }
            _ => None,
        };
        out.push(h);
        if let Some((name, local)) = class_global {
            let ty = self
                .functions
                .get(fs.fid as usize)
                .and_then(|function| function.locals.get(local as usize))
                .map(|local| local.ty)
                .unwrap_or(Ty::Concrete(Type::Any));
            out.push(HStmt {
                kind: HStmtKind::GlobalSet {
                    module: self.dunder_name.clone(),
                    name,
                    value: HExpr {
                        kind: HExprKind::Local(local),
                        ty,
                        line,
                        col,
                    },
                },
                line,
                col,
            });
        }
        let Some((name, local)) = target else {
            return;
        };
        let ty = match self
            .functions
            .get(fs.fid as usize)
            .and_then(|function| function.locals.get(local as usize))
        {
            Some(local) => local.ty,
            None => Ty::Concrete(Type::Any),
        };
        out.push(HStmt {
            kind: HStmtKind::GlobalSet {
                module: self.dunder_name.clone(),
                name,
                value: HExpr {
                    kind: HExprKind::Local(local),
                    ty,
                    line,
                    col,
                },
            },
            line,
            col,
        });
    }

    fn lower_stmt(&mut self, stmt: &ast::Stmt, fs: &mut FuncState) -> Option<HStmt> {
        let (line, col) = self.span(stmt.range());
        let kind = match stmt {
            ast::Stmt::Expr(s) => match &*s.value {
                ast::Expr::Yield(y) => return self.lower_yield_stmt(y, fs, line, col),
                _ => HStmtKind::Expr(self.expr(&s.value, fs)),
            },
            ast::Stmt::Assign(s) => {
                // Python evaluates the right-hand side first, then the target
                // expression(s); `Unpack` keeps that order at runtime.
                let value = self.expr(&s.value, fs);
                if s.targets.len() > 1 {
                    return self.lower_chained_assign(s, value, fs, line, col);
                }
                self.lower_store(&s.targets[0], value, fs)?
            }
            ast::Stmt::AnnAssign(s) => {
                // `x: T` without a value is a declaration only.
                let value = s.value.as_deref()?;
                // An explicit annotation makes the target dynamically typed;
                // a later assignment may use a different concrete type (e.g.
                // `x: str | None = None` followed by a str assignment).
                if let ast::Expr::Name(name) = &*s.target {
                    if let Some(local) = fs.lookup(name.id.as_str()) {
                        if let Some(function) = self.functions.get_mut(fs.fid as usize) {
                            if let Some(local_info) = function.locals.get_mut(local as usize) {
                                local_info.ty = Ty::Concrete(Type::Any);
                            }
                        }
                    }
                }
                let value = self.expr(value, fs);
                self.lower_store(&s.target, value, fs)?
            }
            ast::Stmt::AugAssign(s) => {
                if let ast::Expr::Subscript(sub) = &*s.target {
                    if matches!(&*sub.slice, ast::Expr::Slice(_)) {
                        self.error_at_code(
                            sub.slice.range(),
                            codes::INDEX,
                            "augmented slice assignment is not supported yet",
                        );
                        return None;
                    }
                    return self.lower_aug_subscript(s, sub, fs, line, col);
                }
                if let ast::Expr::Attribute(attr) = &*s.target {
                    return self.lower_aug_attribute(s, attr, fs, line, col);
                }
                let ast::Expr::Name(target) = &*s.target else {
                    self.error_at(
                        s.target.range(),
                        "augmented assignment target must be a single name or a subscript in Phase 2.0",
                    );
                    return None;
                };
                let op = self.bin_op(s.op, s.range)?;
                if let Some(ns) = fs.class_ns.as_ref().map(|state| state.ns) {
                    // `name op= value` in a class body: read the namespace
                    // entry, apply the operator and store it back.
                    let namespace = HExpr {
                        kind: HExprKind::Local(ns),
                        ty: Ty::Concrete(Type::Dict),
                        line,
                        col,
                    };
                    let key = HExpr {
                        kind: HExprKind::Const(Const::Str(target.id.to_string())),
                        ty: Ty::Concrete(Type::Str),
                        line,
                        col,
                    };
                    let lhs = HExpr {
                        kind: HExprKind::Subscript {
                            object: Box::new(namespace.clone()),
                            index: Box::new(key.clone()),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    };
                    let rhs = self.expr(&s.value, fs);
                    let value = HExpr {
                        kind: HExprKind::Bin {
                            op,
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                        ty: self.infer.fresh(),
                        line,
                        col,
                    };
                    return Some(HStmt {
                        kind: HStmtKind::SetSubscript {
                            object: namespace,
                            index: key,
                            value,
                        },
                        line,
                        col,
                    });
                }
                if fs.is_global(target.id.as_str()) {
                    // `global x; x += v` reads the global, applies the
                    // operator and stores the result back.
                    self.used_globals.insert(target.id.to_string());
                    let lhs = HExpr {
                        kind: HExprKind::GlobalGet {
                            module: self.dunder_name.clone(),
                            name: target.id.to_string(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    };
                    let rhs = self.expr(&s.value, fs);
                    let value = HExpr {
                        kind: HExprKind::Bin {
                            op,
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                        ty: self.infer.fresh(),
                        line,
                        col,
                    };
                    HStmtKind::GlobalSet {
                        module: self.dunder_name.clone(),
                        name: target.id.to_string(),
                        value,
                    }
                } else if let Some(local) = fs.lookup(target.id.as_str()) {
                    let value = self.expr(&s.value, fs);
                    if fs.is_cell(local) {
                        // Read and write both go through the shared cell so a
                        // nested `nonlocal` writer sees the update.
                        let ty = self.infer.fresh();
                        let lhs = self.local_read(fs, local, ty, line, col);
                        let value = HExpr {
                            kind: HExprKind::Bin {
                                op,
                                lhs: Box::new(lhs),
                                rhs: Box::new(value),
                            },
                            ty: self.infer.fresh(),
                            line,
                            col,
                        };
                        HStmtKind::CellAssign {
                            target: local,
                            value,
                        }
                    } else {
                        HStmtKind::AugAssign {
                            target: local,
                            op,
                            value,
                        }
                    }
                } else if self.session_mode && self.global_names.contains(target.id.as_str()) {
                    // A REPL fragment updating a name bound by an earlier
                    // fragment has no local slot: read and write the session
                    // global table.
                    self.used_globals.insert(target.id.to_string());
                    let lhs = HExpr {
                        kind: HExprKind::GlobalGet {
                            module: self.dunder_name.clone(),
                            name: target.id.to_string(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    };
                    let rhs = self.expr(&s.value, fs);
                    let value = HExpr {
                        kind: HExprKind::Bin {
                            op,
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                        ty: self.infer.fresh(),
                        line,
                        col,
                    };
                    HStmtKind::GlobalSet {
                        module: self.dunder_name.clone(),
                        name: target.id.to_string(),
                        value,
                    }
                } else {
                    self.error_name(
                        target.range,
                        target.id.as_str(),
                        fs,
                        format!("assignment to unknown name '{}'", target.id),
                    );
                    return None;
                }
            }
            ast::Stmt::If(s) => {
                // Guards fixed by the import contract are decided before the
                // dead branch is lowered: its imports were not collected, so
                // lowering it would report missing names for type-only or demo
                // code.
                if let Some(runs) = static_if_branch(&s.test, &self.dunder_name) {
                    let body = self.lower_block(if runs { &s.body } else { &s.orelse }, fs);
                    if body.is_empty() {
                        return None;
                    }
                    let cond = HExpr {
                        kind: HExprKind::Const(Const::Bool(runs)),
                        ty: Ty::Concrete(Type::Bool),
                        line,
                        col,
                    };
                    let (then_body, else_body) = if runs {
                        (body, Vec::new())
                    } else {
                        (Vec::new(), body)
                    };
                    return Some(HStmt {
                        kind: HStmtKind::If {
                            cond,
                            then_body,
                            else_body,
                        },
                        line,
                        col,
                    });
                }
                let cond = self.expr(&s.test, fs);
                // `if True:`/`if False:` from a literal: lowering the branch
                // that cannot run would only produce dead HIR (and spurious
                // diagnostics for constructs the branch never reaches).
                let (then_body, else_body) = match &cond.kind {
                    HExprKind::Const(Const::Bool(true)) => {
                        (self.lower_block(&s.body, fs), Vec::new())
                    }
                    HExprKind::Const(Const::Bool(false)) => {
                        (Vec::new(), self.lower_block(&s.orelse, fs))
                    }
                    _ => (
                        self.lower_block(&s.body, fs),
                        self.lower_block(&s.orelse, fs),
                    ),
                };
                HStmtKind::If {
                    cond,
                    then_body,
                    else_body,
                }
            }
            ast::Stmt::While(s) => {
                let cond = self.expr(&s.test, fs);
                // `while False:` never runs its body; drop the loop unless an
                // `else:` clause still has to run. `while True:` stays a real
                // loop so `break` keeps working.
                let never_runs = matches!(&cond.kind, HExprKind::Const(Const::Bool(false)));
                if never_runs && s.orelse.is_empty() {
                    return None;
                }
                let body = if never_runs {
                    Vec::new()
                } else {
                    fs.loop_depth += 1;
                    let body = self.lower_block(&s.body, fs);
                    fs.loop_depth -= 1;
                    // Each iteration rebinds mutated cells, so closures made
                    // in the body capture that iteration's binding.
                    let mut inits = self.loop_cell_inits(fs, &s.body, None, line, col);
                    inits.extend(body);
                    inits
                };
                // `else` is not part of the loop: `break` and `continue` in it
                // bind to an enclosing loop, so it is lowered after the loop
                // depth is restored.
                let else_body = self.lower_block(&s.orelse, fs);
                HStmtKind::While {
                    cond,
                    body,
                    else_body,
                }
            }
            ast::Stmt::For(s) => return self.lower_for(s, fs, line, col),
            ast::Stmt::Return(s) => {
                if fs.is_module {
                    self.error_at_code(s.range, codes::CONTROL, "return outside a function");
                    return None;
                }
                match &s.value {
                    Some(value) => HStmtKind::Return(Some(self.expr(value, fs))),
                    None => HStmtKind::Return(None),
                }
            }
            ast::Stmt::Break(s) => {
                if fs.loop_depth == 0 {
                    self.error_at_code(s.range, codes::CONTROL, "break outside a loop");
                    return None;
                }
                HStmtKind::Break
            }
            ast::Stmt::Continue(s) => {
                if fs.loop_depth == 0 {
                    self.error_at_code(s.range, codes::CONTROL, "continue outside a loop");
                    return None;
                }
                HStmtKind::Continue
            }
            ast::Stmt::Pass(_) => HStmtKind::Pass,
            ast::Stmt::Raise(s) => return self.lower_raise(s, fs, line, col),
            ast::Stmt::Try(s) => return self.lower_try(s, fs, line, col),
            ast::Stmt::FunctionDef(s) => {
                if fs.class_ns.is_some() {
                    return self.lower_class_body_function(
                        s.name.as_str(),
                        u32::from(s.range.start()),
                        fs,
                        line,
                        col,
                    );
                }
                if fs.is_module {
                    let start = u32::from(s.range.start()) as usize;
                    let Some(&fid) = self.conditional_defs.get(&start) else {
                        self.error_at(
                            s.range,
                            "function definitions are only supported directly at module level",
                        );
                        return None;
                    };
                    return self.lower_conditional_function_definition(
                        &s.decorator_list,
                        fid,
                        fs,
                        line,
                        col,
                    );
                }
                return self.lower_nested_function(FnDecl::from_def(s), fs, line, col);
            }
            ast::Stmt::AsyncFunctionDef(s) => {
                if fs.class_ns.is_some() {
                    return self.lower_class_body_function(
                        s.name.as_str(),
                        u32::from(s.range.start()),
                        fs,
                        line,
                        col,
                    );
                }
                if fs.is_module {
                    let start = u32::from(s.range.start()) as usize;
                    let Some(&fid) = self.conditional_defs.get(&start) else {
                        self.error_at(
                            s.range,
                            "function definitions are only supported directly at module level",
                        );
                        return None;
                    };
                    return self.lower_conditional_function_definition(
                        &s.decorator_list,
                        fid,
                        fs,
                        line,
                        col,
                    );
                }
                return self.lower_nested_function(FnDecl::from_async_def(s), fs, line, col);
            }
            ast::Stmt::ClassDef(s) => return self.lower_class_stmt(s, fs, line, col),
            // Imports were resolved into the binding tables in step 1. A
            // session fragment also mirrors them into the runtime global
            // table so a later fragment sees the same names.
            ast::Stmt::Import(s) => {
                if fs.class_ns.is_some() {
                    return self.lower_class_body_import(&s.names, fs, line, col);
                }
                if !self.session_mode || !fs.is_module {
                    return None;
                }
                return self.lower_session_import(s, line, col);
            }
            ast::Stmt::ImportFrom(s) => {
                if fs.class_ns.is_some() {
                    return self.lower_class_body_import_from(s, fs, line, col);
                }
                if !self.session_mode || !fs.is_module {
                    return None;
                }
                return self.lower_session_import_from(s, line, col);
            }
            ast::Stmt::With(s) => return self.lower_with(s, fs, line, col),
            ast::Stmt::AsyncWith(s) => return self.lower_async_with(s, fs, line, col),
            ast::Stmt::AsyncFor(s) => return self.lower_async_for(s, fs, line, col),
            ast::Stmt::Assert(s) => {
                let test = self.expr(&s.test, fs);
                // Python evaluates the message only when the test fails;
                // codegen keeps it inside the failure branch.
                let msg = match s.msg.as_deref() {
                    Some(msg) => Some(self.expr(msg, fs)),
                    // pytest reports the assertion's source text for a bare
                    // `assert`; Hydro does not rewrite the expression, so the
                    // statement text is the closest real message.
                    None if pytest_mode() => Some(HExpr {
                        kind: HExprKind::Const(Const::Str(self.source_text(s.range))),
                        ty: Ty::Concrete(Type::Str),
                        line,
                        col,
                    }),
                    None => None,
                };
                HStmtKind::Assert { test, msg }
            }
            ast::Stmt::Delete(s) => return self.lower_delete(s, fs, line, col),
            ast::Stmt::Global(_) => {
                // Declarations are handled by `collect_scope_globals` before
                // the body is lowered; the statement itself emits nothing.
                return None;
            }
            ast::Stmt::Nonlocal(s) => {
                // Validated when the enclosing function body's state was
                // built; the declaration itself emits no code.
                if fs.is_module {
                    self.error_at_code(
                        s.range,
                        codes::SYNTAX,
                        "nonlocal declaration not allowed at module level",
                    );
                }
                return None;
            }
            ast::Stmt::Match(s) => return self.lower_match(s, fs, line, col),
            ast::Stmt::TypeAlias(s) => {
                self.error_at(s.range, "type aliases are not supported in Phase 1");
                return None;
            }
            ast::Stmt::TryStar(s) => return self.lower_try_star(s, fs, line, col),
        };
        Some(HStmt { kind, line, col })
    }

    // ------------------------------------------------------------------
    // `match` statements
    // ------------------------------------------------------------------

    /// Lower `match subject:` into a chain of guarded `if` statements.
    ///
    /// The subject is evaluated once into a hidden local; each case is tested
    /// only while no earlier case matched. Patterns lower to ordinary runtime
    /// predicates (`==`, `is`, `isinstance`, `len`, membership, subscripts and
    /// attribute reads) plus assignments for captures, so no new runtime
    /// protocol is needed. Irrefutable patterns, duplicate captures and
    /// OR-alternatives that bind different names are compile-time diagnostics.
    fn lower_match(
        &mut self,
        s: &ast::StmtMatch,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        // Validate every case before lowering, so a malformed pattern does not
        // produce a partially matched program.
        let mut valid = true;
        for (index, case) in s.cases.iter().enumerate() {
            if !self.validate_match_case(case, index + 1 == s.cases.len()) {
                valid = false;
            }
        }
        if !valid {
            return None;
        }

        let (subject_local, subject) = self.hidden_temp(fs, "$match", Type::Any);
        let mut body = Vec::new();
        let subject_value = self.expr(&s.subject, fs);
        body.push(self.match_assign(subject_local, subject_value, line, col));
        let (done_local, done) = self.bool_temp(fs, line, col);
        body.push(self.match_assign(done_local, self.bool_const(false, line, col), line, col));
        for case in &s.cases {
            let mut case_body = Vec::new();
            let ok = self.compile_match_pattern(
                &case.pattern,
                subject.clone(),
                fs,
                &mut case_body,
                line,
                col,
            )?;
            let mut success = Vec::new();
            let statements = self.lower_block(&case.body, fs);
            match &case.guard {
                Some(guard) => {
                    let guard = self.expr(guard, fs);
                    let mut guarded = statements;
                    guarded.push(self.match_assign(
                        done_local,
                        self.bool_const(true, line, col),
                        line,
                        col,
                    ));
                    success.push(HStmt {
                        kind: HStmtKind::If {
                            cond: guard,
                            then_body: guarded,
                            else_body: Vec::new(),
                        },
                        line,
                        col,
                    });
                }
                None => {
                    success.extend(statements);
                    success.push(self.match_assign(
                        done_local,
                        self.bool_const(true, line, col),
                        line,
                        col,
                    ));
                }
            }
            case_body.push(HStmt {
                kind: HStmtKind::If {
                    cond: HExpr {
                        kind: HExprKind::Local(ok),
                        ty: Ty::Concrete(Type::Bool),
                        line,
                        col,
                    },
                    then_body: success,
                    else_body: Vec::new(),
                },
                line,
                col,
            });
            body.push(HStmt {
                kind: HStmtKind::If {
                    cond: HExpr {
                        kind: HExprKind::Un {
                            op: hydro_ir::UnOp::Not,
                            operand: Box::new(done.clone()),
                        },
                        ty: Ty::Concrete(Type::Bool),
                        line,
                        col,
                    },
                    then_body: case_body,
                    else_body: Vec::new(),
                },
                line,
                col,
            });
        }
        Some(HStmt {
            kind: HStmtKind::If {
                cond: self.bool_const(true, line, col),
                then_body: body,
                else_body: Vec::new(),
            },
            line,
            col,
        })
    }

    /// Validate one `case`: patterns must be well formed, OR alternatives must
    /// bind the same names, and an irrefutable unguarded case must be last.
    fn validate_match_case(&mut self, case: &ast::MatchCase, is_last: bool) -> bool {
        let mut ok = true;
        let mut names = Vec::new();
        if !self.validate_pattern(&case.pattern, true, &mut names, &mut ok) {
            ok = false;
        }
        if let Some(duplicate) = names
            .iter()
            .find(|name| {
                let mut count = 0;
                for candidate in &names {
                    if candidate == *name {
                        count += 1;
                    }
                }
                count > 1
            })
            .cloned()
        {
            self.error_at_code(
                case.pattern.range(),
                codes::SYNTAX,
                format!("multiple assignments to name '{duplicate}' in pattern"),
            );
            ok = false;
        }
        if case.guard.is_none() && pattern_is_irrefutable(&case.pattern) && !is_last {
            let label = match &case.pattern {
                ast::Pattern::MatchAs(as_pattern) => as_pattern
                    .name
                    .as_ref()
                    .map(|name| format!("name capture '{}'", name))
                    .unwrap_or_else(|| "wildcard pattern".to_string()),
                _ => "pattern".to_string(),
            };
            self.error_at_code(
                case.pattern.range(),
                codes::SYNTAX,
                format!("{label} makes the remaining patterns unreachable"),
            );
            ok = false;
        }
        ok
    }

    /// Validate a pattern recursively, collecting the names it binds.
    fn validate_pattern(
        &mut self,
        pattern: &ast::Pattern,
        in_sequence: bool,
        names: &mut Vec<String>,
        ok: &mut bool,
    ) -> bool {
        match pattern {
            ast::Pattern::MatchValue(value) => {
                // A bare name would be a capture, so a value pattern is always
                // a dotted name or a literal expression; any expression the
                // frontend can lower is accepted.
                if !matches!(
                    &*value.value,
                    ast::Expr::Attribute(_) | ast::Expr::Subscript(_) | ast::Expr::Call(_)
                ) && !is_literal_pattern_expr(&value.value)
                {
                    self.error_at_code(
                        value.value.range(),
                        codes::SYNTAX,
                        "value pattern must be a dotted name or a literal",
                    );
                    *ok = false;
                }
            }
            ast::Pattern::MatchSingleton(_) => {}
            ast::Pattern::MatchSequence(sequence) => {
                let stars = sequence
                    .patterns
                    .iter()
                    .filter(|pattern| matches!(pattern, ast::Pattern::MatchStar(_)))
                    .count();
                if stars > 1 {
                    self.error_at_code(
                        sequence.range,
                        codes::SYNTAX,
                        "multiple starred names in a sequence pattern",
                    );
                    *ok = false;
                }
                for child in &sequence.patterns {
                    self.validate_pattern(child, true, names, ok);
                }
            }
            ast::Pattern::MatchMapping(mapping) => {
                let mut literal_keys: Vec<String> = Vec::new();
                for key in &mapping.keys {
                    if let Some(text) = literal_pattern_key(key) {
                        if literal_keys.contains(&text) {
                            self.error_at_code(
                                key.range(),
                                codes::SYNTAX,
                                format!("mapping pattern checks duplicate key {text}"),
                            );
                            *ok = false;
                        }
                        literal_keys.push(text);
                    }
                }
                for child in &mapping.patterns {
                    self.validate_pattern(child, false, names, ok);
                }
                if let Some(rest) = &mapping.rest {
                    names.push(rest.to_string());
                }
            }
            ast::Pattern::MatchClass(class) => {
                for child in &class.patterns {
                    self.validate_pattern(child, false, names, ok);
                }
                for child in &class.kwd_patterns {
                    self.validate_pattern(child, false, names, ok);
                }
            }
            ast::Pattern::MatchStar(star) => {
                if !in_sequence {
                    self.error_at_code(
                        star.range,
                        codes::SYNTAX,
                        "star pattern is only valid inside a sequence pattern",
                    );
                    *ok = false;
                }
                if let Some(name) = &star.name {
                    names.push(name.to_string());
                }
            }
            ast::Pattern::MatchAs(as_pattern) => {
                if let Some(inner) = &as_pattern.pattern {
                    self.validate_pattern(inner, false, names, ok);
                }
                if let Some(name) = &as_pattern.name {
                    names.push(name.to_string());
                }
            }
            ast::Pattern::MatchOr(or_pattern) => {
                // Every alternative must bind the same names, otherwise a
                // capture in the case body could be unbound.
                let mut alternatives: Vec<HashSet<String>> = Vec::new();
                for alternative in &or_pattern.patterns {
                    let mut alternative_names = Vec::new();
                    self.validate_pattern(alternative, in_sequence, &mut alternative_names, ok);
                    alternatives.push(alternative_names.into_iter().collect());
                }
                if let Some(first) = alternatives.first().cloned() {
                    for other in &alternatives[1..] {
                        if *other != first {
                            self.error_at_code(
                                or_pattern.range,
                                codes::SYNTAX,
                                "alternative patterns bind different names",
                            );
                            *ok = false;
                        }
                    }
                    names.extend(first);
                }
            }
        }
        *ok
    }

    fn match_assign(&self, target: LocalId, value: HExpr, line: u32, col: u32) -> HStmt {
        HStmt {
            kind: HStmtKind::Assign { target, value },
            line,
            col,
        }
    }

    fn bool_const(&self, value: bool, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::Const(Const::Bool(value)),
            ty: Ty::Concrete(Type::Bool),
            line,
            col,
        }
    }

    fn bool_temp(&mut self, fs: &mut FuncState, line: u32, col: u32) -> (LocalId, HExpr) {
        let (local, mut read) = self.hidden_temp(fs, "$match", Type::Bool);
        read.ty = Ty::Concrete(Type::Bool);
        read.line = line;
        read.col = col;
        (local, read)
    }

    fn local_read_expr(&self, local: LocalId, ty: Type, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::Local(local),
            ty: Ty::Concrete(ty),
            line,
            col,
        }
    }

    /// A builtin call expression (`len(x)`, `isinstance(x, T)`, ...).
    fn builtin_expr(
        &self,
        builtin: Builtin,
        args: Vec<HExpr>,
        ty: Type,
        line: u32,
        col: u32,
    ) -> HExpr {
        HExpr {
            kind: HExprKind::Call {
                callee: Callee::Builtin(builtin),
                args,
            },
            ty: Ty::Concrete(ty),
            line,
            col,
        }
    }

    fn bin_expr(&self, op: BinOp, lhs: HExpr, rhs: HExpr, ty: Type, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::Bin {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            ty: Ty::Concrete(ty),
            line,
            col,
        }
    }

    fn and_expr(&self, items: Vec<HExpr>, line: u32, col: u32) -> Option<HExpr> {
        let mut iter = items.into_iter();
        let mut result = iter.next()?;
        for item in iter {
            result = HExpr {
                kind: HExprKind::BoolOp {
                    op: hydro_ir::BoolOp::And,
                    lhs: Box::new(result),
                    rhs: Box::new(item),
                },
                ty: Ty::Concrete(Type::Bool),
                line,
                col,
            };
        }
        Some(result)
    }

    fn or_expr(&self, items: Vec<HExpr>, line: u32, col: u32) -> Option<HExpr> {
        let mut iter = items.into_iter();
        let mut result = iter.next()?;
        for item in iter {
            result = HExpr {
                kind: HExprKind::BoolOp {
                    op: hydro_ir::BoolOp::Or,
                    lhs: Box::new(result),
                    rhs: Box::new(item),
                },
                ty: Ty::Concrete(Type::Bool),
                line,
                col,
            };
        }
        Some(result)
    }

    /// Compile one pattern into `out`, returning a boolean local that is
    /// `True` when the pattern matched. Capture assignments run only after the
    /// pattern (and all nested sub-patterns) matched.
    fn compile_match_pattern(
        &mut self,
        pattern: &ast::Pattern,
        subject: HExpr,
        fs: &mut FuncState,
        out: &mut Vec<HStmt>,
        line: u32,
        col: u32,
    ) -> Option<LocalId> {
        let (ok, _) = self.bool_temp(fs, line, col);
        out.push(self.match_assign(ok, self.bool_const(false, line, col), line, col));
        match pattern {
            ast::Pattern::MatchAs(as_pattern) => {
                if let Some(inner) = &as_pattern.pattern {
                    let inner_ok =
                        self.compile_match_pattern(inner, subject.clone(), fs, out, line, col)?;
                    out.push(self.match_assign(
                        ok,
                        self.local_read_expr(inner_ok, Type::Bool, line, col),
                        line,
                        col,
                    ));
                } else {
                    out.push(self.match_assign(ok, self.bool_const(true, line, col), line, col));
                }
                if let Some(name) = &as_pattern.name {
                    let target = self.pattern_binding(name.as_str(), pattern.range(), fs)?;
                    let bind = self.pattern_assign(target, subject.clone(), fs, line, col);
                    out.push(HStmt {
                        kind: HStmtKind::If {
                            cond: self.local_read_expr(ok, Type::Bool, line, col),
                            then_body: vec![bind],
                            else_body: Vec::new(),
                        },
                        line,
                        col,
                    });
                }
                Some(ok)
            }
            ast::Pattern::MatchStar(star) => {
                if let Some(name) = &star.name {
                    let _ = self.pattern_binding(name.as_str(), star.range, fs)?;
                }
                self.error_at_code(
                    star.range,
                    codes::SYNTAX,
                    "star pattern is only valid inside a sequence pattern",
                );
                None
            }
            ast::Pattern::MatchValue(value) => {
                let value_expr = self.expr(&value.value, fs);
                let test = self.bin_expr(BinOp::Eq, subject, value_expr, Type::Bool, line, col);
                out.push(self.match_assign(ok, test, line, col));
                Some(ok)
            }
            ast::Pattern::MatchSingleton(singleton) => {
                let constant = match &singleton.value {
                    ast::Constant::None => Const::None,
                    ast::Constant::Bool(value) => Const::Bool(*value),
                    ast::Constant::Ellipsis => Const::Ellipsis,
                    other => {
                        self.error_at_code(
                            singleton.range,
                            codes::SYNTAX,
                            format!("unsupported singleton pattern {other:?}"),
                        );
                        return None;
                    }
                };
                let test = self.bin_expr(
                    BinOp::Is,
                    subject,
                    HExpr {
                        kind: HExprKind::Const(constant),
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    },
                    Type::Bool,
                    line,
                    col,
                );
                out.push(self.match_assign(ok, test, line, col));
                Some(ok)
            }
            ast::Pattern::MatchOr(or_pattern) => {
                let alternatives = or_pattern.patterns.clone();
                let body = self.compile_match_or(&alternatives, subject, fs, ok, 0, (line, col))?;
                out.extend(body);
                Some(ok)
            }
            ast::Pattern::MatchSequence(sequence) => {
                self.compile_match_sequence(sequence, subject, fs, out, ok, (line, col))?;
                Some(ok)
            }
            ast::Pattern::MatchMapping(mapping) => {
                self.compile_match_mapping(mapping, subject, fs, out, ok, (line, col))?;
                Some(ok)
            }
            ast::Pattern::MatchClass(class) => {
                self.compile_match_class(class, subject, fs, out, ok, (line, col))?;
                Some(ok)
            }
        }
    }

    fn compile_match_or(
        &mut self,
        alternatives: &[ast::Pattern],
        subject: HExpr,
        fs: &mut FuncState,
        ok: LocalId,
        index: usize,
        (line, col): (u32, u32),
    ) -> Option<Vec<HStmt>> {
        let alternative = alternatives.get(index)?;
        let mut body = Vec::new();
        let alternative_ok =
            self.compile_match_pattern(alternative, subject.clone(), fs, &mut body, line, col)?;
        let then_body = vec![self.match_assign(ok, self.bool_const(true, line, col), line, col)];
        let else_body = if index + 1 < alternatives.len() {
            self.compile_match_or(alternatives, subject, fs, ok, index + 1, (line, col))?
        } else {
            Vec::new()
        };
        body.push(HStmt {
            kind: HStmtKind::If {
                cond: self.local_read_expr(alternative_ok, Type::Bool, line, col),
                then_body,
                else_body,
            },
            line,
            col,
        });
        Some(body)
    }

    /// `isinstance(subject, T)` for every supported sequence type.
    fn sequence_test(&self, subject: &HExpr, line: u32, col: u32) -> HExpr {
        let mut tests = Vec::new();
        for builtin in [Builtin::List, Builtin::Tuple, Builtin::Str, Builtin::Bytes] {
            let type_value = HExpr {
                kind: HExprKind::BuiltinRef(builtin),
                ty: Ty::Concrete(Type::Function),
                line,
                col,
            };
            tests.push(self.builtin_expr(
                Builtin::Isinstance,
                vec![subject.clone(), type_value],
                Type::Bool,
                line,
                col,
            ));
        }
        // `range` has no first-class type value; `type(range(0))` is its
        // cached class object.
        let range_value = self.builtin_expr(
            Builtin::Range,
            vec![
                HExpr {
                    kind: HExprKind::Const(Const::Int(0)),
                    ty: Ty::Concrete(Type::Int),
                    line,
                    col,
                },
                HExpr {
                    kind: HExprKind::Const(Const::Int(0)),
                    ty: Ty::Concrete(Type::Int),
                    line,
                    col,
                },
            ],
            Type::Range,
            line,
            col,
        );
        let range_type = self.builtin_expr(Builtin::Type, vec![range_value], Type::Any, line, col);
        tests.push(self.builtin_expr(
            Builtin::Isinstance,
            vec![subject.clone(), range_type],
            Type::Bool,
            line,
            col,
        ));
        self.or_expr(tests, line, col)
            .unwrap_or_else(|| self.bool_const(false, line, col))
    }

    fn len_expr(&self, subject: &HExpr, line: u32, col: u32) -> HExpr {
        self.builtin_expr(Builtin::Len, vec![subject.clone()], Type::Int, line, col)
    }

    fn int_const(&self, value: i64, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::Const(Const::Int(value)),
            ty: Ty::Concrete(Type::Int),
            line,
            col,
        }
    }

    /// Build the `if children... { binds } else { ok = False }` statement.
    fn child_gate(
        &mut self,
        ok: LocalId,
        children: Vec<LocalId>,
        parent_binds: Vec<HStmt>,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        if children.is_empty() {
            return None;
        }
        let checks: Vec<HExpr> = children
            .into_iter()
            .map(|child| self.local_read_expr(child, Type::Bool, line, col))
            .collect();
        let cond = self.and_expr(checks, line, col)?;
        Some(HStmt {
            kind: HStmtKind::If {
                cond,
                then_body: parent_binds,
                else_body: vec![self.match_assign(
                    ok,
                    self.bool_const(false, line, col),
                    line,
                    col,
                )],
            },
            line,
            col,
        })
    }

    fn compile_match_sequence(
        &mut self,
        sequence: &ast::PatternMatchSequence,
        subject: HExpr,
        fs: &mut FuncState,
        out: &mut Vec<HStmt>,
        ok: LocalId,
        (line, col): (u32, u32),
    ) -> Option<()> {
        let star = sequence
            .patterns
            .iter()
            .position(|pattern| matches!(pattern, ast::Pattern::MatchStar(_)));
        let count = sequence.patterns.len();
        let length = self.len_expr(&subject, line, col);
        let length_ok = match star {
            // `case [a, *rest, b]`: at least the fixed elements must be there.
            Some(_) => self.bin_expr(
                BinOp::Ge,
                length,
                self.int_const((count - 1) as i64, line, col),
                Type::Bool,
                line,
                col,
            ),
            None => self.bin_expr(
                BinOp::Eq,
                length,
                self.int_const(count as i64, line, col),
                Type::Bool,
                line,
                col,
            ),
        };
        let sequence_test = self.sequence_test(&subject, line, col);
        let guard = self.and_expr(vec![sequence_test, length_ok], line, col)?;
        out.push(self.match_assign(ok, guard, line, col));

        let mut body = Vec::new();
        let mut children = Vec::new();
        let prefix = star.unwrap_or(count);
        for (index, child) in sequence.patterns[..prefix].iter().enumerate() {
            let element = HExpr {
                kind: HExprKind::Subscript {
                    object: Box::new(subject.clone()),
                    index: Box::new(self.int_const(index as i64, line, col)),
                },
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            };
            children.push(self.compile_match_pattern(child, element, fs, &mut body, line, col)?);
        }
        if let Some(star_index) = star {
            if let ast::Pattern::MatchStar(star_pattern) = &sequence.patterns[star_index] {
                if let Some(name) = &star_pattern.name {
                    let target = self.pattern_binding(name.as_str(), star_pattern.range, fs)?;
                    let rest_end = count - star_index - 1;
                    let stop = if rest_end == 0 {
                        self.len_expr(&subject, line, col)
                    } else {
                        self.bin_expr(
                            BinOp::Sub,
                            self.len_expr(&subject, line, col),
                            self.int_const(rest_end as i64, line, col),
                            Type::Int,
                            line,
                            col,
                        )
                    };
                    let slice = HExpr {
                        kind: HExprKind::Slice {
                            object: Box::new(subject.clone()),
                            start: Some(Box::new(self.int_const(star_index as i64, line, col))),
                            stop: Some(Box::new(stop)),
                            step: None,
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    };
                    let bind = self.pattern_assign(target, slice, fs, line, col);
                    body.push(bind);
                }
            }
            // The trailing elements are read from the end of the sequence.
            let after = count - star_index - 1;
            for offset in 0..after {
                let from_end = (after - offset) as i64;
                let index = self.bin_expr(
                    BinOp::Sub,
                    self.len_expr(&subject, line, col),
                    self.int_const(from_end, line, col),
                    Type::Int,
                    line,
                    col,
                );
                let element = HExpr {
                    kind: HExprKind::Subscript {
                        object: Box::new(subject.clone()),
                        index: Box::new(index),
                    },
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                };
                let Some(child) = sequence.patterns.get(star_index + 1 + offset) else {
                    continue;
                };
                children
                    .push(self.compile_match_pattern(child, element, fs, &mut body, line, col)?);
            }
        }
        if let Some(gate) = self.child_gate(ok, children, Vec::new(), line, col) {
            body.push(gate);
        }
        // Children and captures only run after the structural guard matched;
        // `ok` already carries that result and the gate clears it on failure.
        out.push(HStmt {
            kind: HStmtKind::If {
                cond: self.local_read_expr(ok, Type::Bool, line, col),
                then_body: body,
                else_body: Vec::new(),
            },
            line,
            col,
        });
        Some(())
    }

    fn compile_match_mapping(
        &mut self,
        mapping: &ast::PatternMatchMapping,
        subject: HExpr,
        fs: &mut FuncState,
        out: &mut Vec<HStmt>,
        ok: LocalId,
        (line, col): (u32, u32),
    ) -> Option<()> {
        let dict_type = HExpr {
            kind: HExprKind::BuiltinRef(Builtin::Dict),
            ty: Ty::Concrete(Type::Function),
            line,
            col,
        };
        let guard = self.builtin_expr(
            Builtin::Isinstance,
            vec![subject.clone(), dict_type],
            Type::Bool,
            line,
            col,
        );
        out.push(self.match_assign(ok, guard, line, col));

        let mut key_locals = Vec::new();
        let mut body = Vec::new();
        for key in &mapping.keys {
            let (key_local, _) = self.hidden_temp(fs, "$match", Type::Any);
            body.push(HStmt {
                kind: HStmtKind::If {
                    cond: self.local_read_expr(ok, Type::Bool, line, col),
                    then_body: {
                        let value = self.expr(key, fs);
                        vec![
                            self.match_assign(key_local, value, line, col),
                            self.match_assign(
                                ok,
                                self.bin_expr(
                                    BinOp::In,
                                    self.local_read_expr(key_local, Type::Any, line, col),
                                    subject.clone(),
                                    Type::Bool,
                                    line,
                                    col,
                                ),
                                line,
                                col,
                            ),
                        ]
                    },
                    else_body: Vec::new(),
                },
                line,
                col,
            });
            key_locals.push(key_local);
        }

        let mut extract = Vec::new();
        let mut children = Vec::new();
        for (key_local, child) in key_locals.iter().zip(&mapping.patterns) {
            let (value_local, _) = self.hidden_temp(fs, "$match", Type::Any);
            let value = HExpr {
                kind: HExprKind::Subscript {
                    object: Box::new(subject.clone()),
                    index: Box::new(self.local_read_expr(*key_local, Type::Any, line, col)),
                },
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            };
            extract.push(self.match_assign(value_local, value, line, col));
            children.push(self.compile_match_pattern(
                child,
                self.local_read_expr(value_local, Type::Any, line, col),
                fs,
                &mut extract,
                line,
                col,
            )?);
        }
        if let Some(rest) = &mapping.rest {
            let target = self.pattern_binding(rest.as_str(), mapping.range, fs)?;
            if let Some(value) = self.match_rest_dict(&subject, &key_locals, fs, line, col) {
                let bind = self.pattern_assign(target, value, fs, line, col);
                extract.push(bind);
            }
        }
        if let Some(gate) = self.child_gate(ok, children, Vec::new(), line, col) {
            extract.push(gate);
        }
        body.push(HStmt {
            kind: HStmtKind::If {
                cond: self.local_read_expr(ok, Type::Bool, line, col),
                then_body: extract,
                else_body: Vec::new(),
            },
            line,
            col,
        });
        out.extend(body);
        Some(())
    }

    /// Build the `**rest` dict: every item of `subject` whose key is not one
    /// of the matched pattern keys.
    fn match_rest_dict(
        &mut self,
        subject: &HExpr,
        keys: &[LocalId],
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HExpr> {
        let (key_var, _) = self.hidden_temp(fs, "$match", Type::Any);
        let (value_var, _) = self.hidden_temp(fs, "$match", Type::Any);
        let key_read = self.local_read_expr(key_var, Type::Any, line, col);
        let value_read = self.local_read_expr(value_var, Type::Any, line, col);
        let key_tuple = HExpr {
            kind: HExprKind::Tuple(
                keys.iter()
                    .map(|key| self.local_read_expr(*key, Type::Any, line, col))
                    .collect(),
            ),
            ty: Ty::Concrete(Type::Tuple),
            line,
            col,
        };
        let condition = HExpr {
            kind: HExprKind::Un {
                op: hydro_ir::UnOp::Not,
                operand: Box::new(self.bin_expr(
                    BinOp::In,
                    key_read.clone(),
                    key_tuple,
                    Type::Bool,
                    line,
                    col,
                )),
            },
            ty: Ty::Concrete(Type::Bool),
            line,
            col,
        };
        let items = HExpr {
            kind: HExprKind::MethodCall {
                object: Box::new(subject.clone()),
                method: "items".to_string(),
                args: Vec::new(),
                star: None,
                keywords: Vec::new(),
                double_star: None,
            },
            ty: Ty::Concrete(Type::List),
            line,
            col,
        };
        // `{k: v for k, v in subject.items() if k not in (...)}`.
        let target = HCompTarget::Unpack(vec![
            HUnpackTarget::Name(key_var),
            HUnpackTarget::Name(value_var),
        ]);
        Some(HExpr {
            kind: HExprKind::DictComp {
                key: Box::new(key_read),
                value: Box::new(value_read),
                clauses: vec![HCompClause {
                    target,
                    iterable: items,
                    conditions: vec![condition],
                    is_async: false,
                }],
            },
            ty: Ty::Concrete(Type::Dict),
            line,
            col,
        })
    }

    fn compile_match_class(
        &mut self,
        class: &ast::PatternMatchClass,
        subject: HExpr,
        fs: &mut FuncState,
        out: &mut Vec<HStmt>,
        ok: LocalId,
        (line, col): (u32, u32),
    ) -> Option<()> {
        let class_value = self.expr(&class.cls, fs);
        let guard = self.builtin_expr(
            Builtin::Isinstance,
            vec![subject.clone(), class_value.clone()],
            Type::Bool,
            line,
            col,
        );
        out.push(self.match_assign(ok, guard, line, col));

        let mut body = Vec::new();
        let mut children = Vec::new();
        if !class.patterns.is_empty() {
            let match_args_name = HExpr {
                kind: HExprKind::Const(Const::Str("__match_args__".to_string())),
                ty: Ty::Concrete(Type::Str),
                line,
                col,
            };
            let has_match_args = self.builtin_expr(
                Builtin::Hasattr,
                vec![class_value.clone(), match_args_name.clone()],
                Type::Bool,
                line,
                col,
            );
            let missing = HExpr {
                kind: HExprKind::Un {
                    op: hydro_ir::UnOp::Not,
                    operand: Box::new(has_match_args),
                },
                ty: Ty::Concrete(Type::Bool),
                line,
                col,
            };
            let message = self.bin_expr(
                BinOp::Add,
                HExpr {
                    kind: HExprKind::GetAttr {
                        object: Box::new(class_value.clone()),
                        name: "__name__".to_string(),
                    },
                    ty: Ty::Concrete(Type::Str),
                    line,
                    col,
                },
                HExpr {
                    kind: HExprKind::Const(Const::Str(format!(
                        "() accepts 0 positional sub-patterns ({} given); define __match_args__",
                        class.patterns.len()
                    ))),
                    ty: Ty::Concrete(Type::Str),
                    line,
                    col,
                },
                Type::Str,
                line,
                col,
            );
            body.push(HStmt {
                kind: HStmtKind::If {
                    cond: missing,
                    then_body: vec![HStmt {
                        kind: HStmtKind::Raise {
                            exc: Some(ExcKind::TypeError),
                            msg: Some(message),
                            cause: None,
                        },
                        line,
                        col,
                    }],
                    else_body: Vec::new(),
                },
                line,
                col,
            });
            let (match_args_local, _) = self.hidden_temp(fs, "$match", Type::Any);
            let match_args = self.builtin_expr(
                Builtin::Getattr,
                vec![class_value, match_args_name],
                Type::Any,
                line,
                col,
            );
            body.push(self.match_assign(match_args_local, match_args, line, col));
            // Enough entries for every positional sub-pattern.
            let enough = self.bin_expr(
                BinOp::Ge,
                self.len_expr(
                    &self.local_read_expr(match_args_local, Type::Any, line, col),
                    line,
                    col,
                ),
                self.int_const(class.patterns.len() as i64, line, col),
                Type::Bool,
                line,
                col,
            );
            let too_short = HExpr {
                kind: HExprKind::Un {
                    op: hydro_ir::UnOp::Not,
                    operand: Box::new(enough),
                },
                ty: Ty::Concrete(Type::Bool),
                line,
                col,
            };
            body.push(HStmt {
                kind: HStmtKind::If {
                    cond: too_short,
                    then_body: vec![HStmt {
                        kind: HStmtKind::Raise {
                            exc: Some(ExcKind::TypeError),
                            msg: Some(HExpr {
                                kind: HExprKind::Const(Const::Str(format!(
                                    "class pattern has {} positional sub-pattern(s) but __match_args__ has fewer entries",
                                    class.patterns.len()
                                ))),
                                ty: Ty::Concrete(Type::Str),
                                line,
                                col,
                            }),
                            cause: None,
                        },
                        line,
                        col,
                    }],
                    else_body: Vec::new(),
                },
                line,
                col,
            });
            for (index, child) in class.patterns.iter().enumerate() {
                let name = HExpr {
                    kind: HExprKind::Subscript {
                        object: Box::new(self.local_read_expr(
                            match_args_local,
                            Type::Any,
                            line,
                            col,
                        )),
                        index: Box::new(self.int_const(index as i64, line, col)),
                    },
                    ty: Ty::Concrete(Type::Str),
                    line,
                    col,
                };
                let (attribute_local, _) = self.hidden_temp(fs, "$match", Type::Any);
                let value = self.builtin_expr(
                    Builtin::Getattr,
                    vec![subject.clone(), name],
                    Type::Any,
                    line,
                    col,
                );
                body.push(self.match_assign(attribute_local, value, line, col));
                children.push(self.compile_match_pattern(
                    child,
                    self.local_read_expr(attribute_local, Type::Any, line, col),
                    fs,
                    &mut body,
                    line,
                    col,
                )?);
            }
        }
        for (attribute, child) in class.kwd_attrs.iter().zip(&class.kwd_patterns) {
            let (attribute_local, _) = self.hidden_temp(fs, "$match", Type::Any);
            let value = HExpr {
                kind: HExprKind::GetAttr {
                    object: Box::new(subject.clone()),
                    name: attribute.to_string(),
                },
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            };
            body.push(self.match_assign(attribute_local, value, line, col));
            children.push(self.compile_match_pattern(
                child,
                self.local_read_expr(attribute_local, Type::Any, line, col),
                fs,
                &mut body,
                line,
                col,
            )?);
        }
        if let Some(gate) = self.child_gate(ok, children, Vec::new(), line, col) {
            body.push(gate);
        }
        out.push(HStmt {
            kind: HStmtKind::If {
                cond: self.local_read_expr(ok, Type::Bool, line, col),
                then_body: body,
                else_body: Vec::new(),
            },
            line,
            col,
        });
        Some(())
    }

    /// Assign to a pattern capture, honoring closure cells.
    fn pattern_assign(
        &self,
        target: LocalId,
        value: HExpr,
        fs: &FuncState,
        line: u32,
        col: u32,
    ) -> HStmt {
        let kind = if fs.is_cell(target) {
            HStmtKind::CellAssign { target, value }
        } else {
            HStmtKind::Assign { target, value }
        };
        HStmt { kind, line, col }
    }

    /// Resolve a pattern capture name to a local slot of the current scope.
    fn pattern_binding(
        &mut self,
        name: &str,
        range: TextRange,
        fs: &mut FuncState,
    ) -> Option<LocalId> {
        if let Some(local) = fs.lookup(name) {
            return Some(local);
        }
        // The pre-pass registers pattern captures as locals; reaching this
        // point means the name was not collected (a malformed pattern AST).
        self.error_name(range, name, fs, format!("unknown pattern name '{name}'"));
        None
    }

    /// Lower `try: ... except* E: ...`.
    ///
    /// Full PEP 654 semantics require splitting an `ExceptionGroup` into the
    /// exceptions each `except*` clause matches, running every clause on its
    /// share and re-raising the unhandled remainder (possibly re-grouped). The
    /// runtime has no exception-group representation yet, so this lowering is
    /// deliberately explicit instead of pretending: the `try` body runs
    /// normally, `else`/`finally` keep their meaning, and the first exception
    /// escaping the body raises `NotImplementedError` explaining that no
    /// `except*` handler ran. The intended runtime protocol is:
    ///
    /// - `hydro_exc_star_begin() -> i64` — start splitting the pending
    ///   exception, returning the group (or a one-element group).
    /// - `hydro_exc_star_take(handler_class: i64) -> i64` — remove and return
    ///   the subset of the current group matched by `handler_class`, or `None`.
    /// - `hydro_exc_star_finish() -> i64` — re-raise the unmatched remainder
    ///   after the last clause (clearing the pending exception when empty).
    ///
    /// These names and their exact argument order are reported to the
    /// orchestrator; no generated code calls them until the runtime provides
    /// them.
    fn lower_try_star(
        &mut self,
        s: &ast::StmtTryStar,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        for handler in &s.handlers {
            let ast::ExceptHandler::ExceptHandler(handler) = handler;
            if handler.type_.is_none() {
                self.error_at_code(
                    handler.range,
                    codes::SYNTAX,
                    "except* requires an exception type",
                );
            }
        }
        let body = self.lower_block(&s.body, fs);
        let else_body = self.lower_block(&s.orelse, fs);
        let finally_body = self.lower_block(&s.finalbody, fs);
        let message = HExpr {
            kind: HExprKind::Const(Const::Str(
                "except* (PEP 654 exception groups) is not implemented: Hydro cannot split an ExceptionGroup; no except* handler was run"
                    .to_string(),
            )),
            ty: Ty::Concrete(Type::Str),
            line,
            col,
        };
        let raise = HStmt {
            kind: HStmtKind::Raise {
                exc: Some(ExcKind::NotImplementedError),
                msg: Some(message),
                cause: None,
            },
            line,
            col,
        };
        Some(HStmt {
            kind: HStmtKind::Try {
                body,
                handlers: vec![HHandler {
                    exc: None,
                    binding: None,
                    binding_cell: false,
                    body: vec![raise],
                }],
                else_body,
                finally_body,
            },
            line,
            col,
        })
    }

    /// Whether a user binding shadows the builtin `super` name.
    fn super_name_is_shadowed(&self, fs: &FuncState) -> bool {
        fs.lookup("super").is_some()
            || fs.is_global("super")
            || self.fn_ids.contains_key("super")
            || self.imported_user_functions.contains_key("super")
            || self.imported_user_globals.contains_key("super")
            || self.imported_functions.contains_key("super")
            || self.imported_constants.contains_key("super")
            || self.global_names.contains("super")
    }

    /// Whether `expr` is a call of the *builtin* `globals` with no arguments.
    ///
    /// Used to lower `globals()[key] = value` into a real module-global store:
    /// the returned dict is a namespace view, so a plain item assignment would
    /// otherwise only mutate that view and stay invisible to static reads.
    fn is_globals_call(&self, expr: &ast::Expr, fs: &FuncState) -> bool {
        let ast::Expr::Call(call) = expr else {
            return false;
        };
        let ast::Expr::Name(name) = &*call.func else {
            return false;
        };
        name.id.as_str() == "globals"
            && call.args.is_empty()
            && call.keywords.is_empty()
            && fs.lookup("globals").is_none()
            && !fs.is_global("globals")
            && !self.fn_ids.contains_key("globals")
            && !self.decorated_functions.contains_key("globals")
            && !self.global_names.contains("globals")
            && !self.imported_user_functions.contains_key("globals")
            && !self.imported_user_globals.contains_key("globals")
            && !self.imported_functions.contains_key("globals")
            && !self.imported_constants.contains_key("globals")
            && !self.module_aliases.contains_key("globals")
            && !self.user_module_aliases.contains_key("globals")
    }

    /// Materialise zero-argument `super()` for the enclosing method.
    ///
    /// The runtime's `$super_new` helper needs the receiver and, to start at
    /// the right MRO position, the class whose body contains the call. We pass
    /// the defining class explicitly so inherited methods resolve correctly
    /// for subclass instances and across multiple inheritance.
    fn lower_super_proxy(
        &mut self,
        fs: &mut FuncState,
        range: TextRange,
        line: u32,
        col: u32,
    ) -> HExpr {
        let receiver_name = if fs.lookup("self").is_some() {
            "self"
        } else {
            "cls"
        };
        let Some(receiver_local) = fs.lookup(receiver_name) else {
            self.error_at_code(
                range,
                codes::NAME,
                "zero-argument super() is only supported inside a method with a 'self' receiver",
            );
            return self.error_expr(range);
        };
        let mut args = Vec::new();
        if let Some((class, _, _)) = self.current_class_method.clone() {
            if self.global_names.contains(&class) {
                self.used_globals.insert(class.clone());
                args.push(HExpr {
                    kind: HExprKind::GlobalGet {
                        module: self.dunder_name.clone(),
                        name: class,
                    },
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                });
            }
        }
        HExpr {
            kind: HExprKind::MethodCall {
                object: Box::new(HExpr {
                    kind: HExprKind::Local(receiver_local),
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                }),
                method: "$super_new".to_string(),
                args,
                star: None,
                keywords: Vec::new(),
                double_star: None,
            },
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        }
    }

    /// Lower a class whose body executes statements.
    ///
    /// The body runs against a fresh namespace `dict` (CPython's `STORE_NAME`
    /// semantics) and the class object is built from that dict afterwards by
    /// `Builtin::ClassFromNamespace`. Base expressions and the `metaclass=`
    /// keyword are evaluated first, matching CPython's order, and are parked
    /// in rooted hidden locals across the body.
    ///
    /// The whole sequence is wrapped in an `if True:` block because HIR
    /// statements nest only through statement bodies; the wrapper is a single
    /// statement for the module body loop and decorator handling.
    fn lower_dynamic_class_value(
        &mut self,
        fs: &mut FuncState,
        emission: ClassEmission<'_>,
    ) -> Vec<HStmt> {
        let ClassEmission {
            class: s,
            plan,
            name,
            target,
            mirror_globals: mirror,
            line,
            col,
        } = emission;
        let mut statements = Vec::new();
        // Bases, in source order, then the metaclass keyword.
        let mut base_refs = Vec::new();
        for index in &plan.bases {
            let Some(base) = s.bases.get(*index) else {
                continue;
            };
            let value = self.expr(base, fs);
            let (temp, temp_ref) = self.hidden_temp(fs, "$classbase", Type::Any);
            statements.push(HStmt {
                kind: HStmtKind::Assign {
                    target: temp,
                    value,
                },
                line,
                col,
            });
            base_refs.push(temp_ref);
        }
        let metaclass = match plan.metaclass.and_then(|index| s.keywords.get(index)) {
            Some(keyword) => {
                let value = self.expr(&keyword.value, fs);
                let (temp, temp_ref) = self.hidden_temp(fs, "$classmeta", Type::Any);
                statements.push(HStmt {
                    kind: HStmtKind::Assign {
                        target: temp,
                        value,
                    },
                    line,
                    col,
                });
                temp_ref
            }
            None => HExpr {
                kind: HExprKind::Const(Const::None),
                ty: Ty::Concrete(Type::None),
                line,
                col,
            },
        };
        // The class namespace itself.
        let (ns, ns_ref) = self.hidden_temp(fs, "$classns", Type::Dict);
        statements.push(HStmt {
            kind: HStmtKind::Assign {
                target: ns,
                value: HExpr {
                    kind: HExprKind::Dict(Vec::new()),
                    ty: Ty::Concrete(Type::Dict),
                    line,
                    col,
                },
            },
            line,
            col,
        });
        // CPython publishes `__module__`/`__qualname__` in the namespace.
        for (dunder, constant) in [
            ("__module__", Const::Str(self.dunder_name.clone())),
            ("__qualname__", Const::Str(name.to_string())),
        ] {
            statements.push(HStmt {
                kind: HStmtKind::SetSubscript {
                    object: ns_ref.clone(),
                    index: HExpr {
                        kind: HExprKind::Const(Const::Str(dunder.to_string())),
                        ty: Ty::Concrete(Type::Str),
                        line,
                        col,
                    },
                    value: HExpr {
                        kind: HExprKind::Const(constant),
                        ty: Ty::Concrete(Type::Str),
                        line,
                        col,
                    },
                },
                line,
                col,
            });
        }
        // Synthesized methods (the dataclass `__init__`) exist before the
        // body runs, so a body definition can still replace them.
        for (method, fid, method_start, kind) in &plan.methods {
            if method_start.is_some() {
                continue;
            }
            let value = self.class_method_binding_value(*fid, *kind, line, col);
            statements.push(HStmt {
                kind: HStmtKind::SetSubscript {
                    object: ns_ref.clone(),
                    index: HExpr {
                        kind: HExprKind::Const(Const::Str(method.clone())),
                        ty: Ty::Concrete(Type::Str),
                        line,
                        col,
                    },
                    value,
                },
                line,
                col,
            });
        }
        let methods = plan
            .methods
            .iter()
            .filter_map(|(_, fid, method_start, kind)| {
                method_start.as_ref().map(|start| (*start, (*fid, *kind)))
            })
            .collect();
        let previous = fs.class_ns.take();
        fs.class_ns = Some(ClassNs {
            ns,
            bindings: plan.bindings.clone(),
            methods,
        });
        let body = self.lower_block(&s.body, fs);
        fs.class_ns = previous;
        statements.extend(body);
        let class_value = HExpr {
            kind: HExprKind::Call {
                callee: Callee::Builtin(Builtin::ClassFromNamespace),
                args: vec![
                    HExpr {
                        kind: HExprKind::Const(Const::Str(name.to_string())),
                        ty: Ty::Concrete(Type::Str),
                        line,
                        col,
                    },
                    HExpr {
                        kind: HExprKind::Tuple(base_refs),
                        ty: Ty::Concrete(Type::Tuple),
                        line,
                        col,
                    },
                    metaclass,
                    ns_ref.clone(),
                ],
            },
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        };
        statements.push(HStmt {
            kind: HStmtKind::Assign {
                target,
                value: class_value,
            },
            line,
            col,
        });
        if mirror && (self.mirror_all_globals || self.used_globals.contains(name)) {
            statements.push(HStmt {
                kind: HStmtKind::GlobalSet {
                    module: self.dunder_name.clone(),
                    name: name.to_string(),
                    value: HExpr {
                        kind: HExprKind::Local(target),
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    },
                },
                line,
                col,
            });
        }
        statements
    }

    /// Lower a module-level class whose body executes statements.
    ///
    /// The body runs against a fresh namespace `dict` (CPython's `STORE_NAME`
    /// semantics) and the class object is built from that dict afterwards by
    /// `Builtin::ClassFromNamespace`. Base expressions and the `metaclass=`
    /// keyword are evaluated first, matching CPython's order, and are parked
    /// in rooted hidden locals across the body.
    fn lower_dynamic_class_stmt(
        &mut self,
        s: &ast::StmtClassDef,
        plan: &ClassPlan,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let name = s.name.to_string();
        let Some(target) = fs.lookup(&name) else {
            self.error_name(
                s.range,
                &name,
                fs,
                format!("class '{name}' has no local slot"),
            );
            return None;
        };
        let statements = self.lower_dynamic_class_value(
            fs,
            ClassEmission {
                class: s,
                plan,
                name: &name,
                target,
                mirror_globals: true,
                line,
                col,
            },
        );
        Some(HStmt {
            kind: HStmtKind::If {
                cond: self.bool_const(true, line, col),
                then_body: statements,
                else_body: Vec::new(),
            },
            line,
            col,
        })
    }

    /// Bind one class-body method definition into the runtime namespace.
    fn lower_class_body_function(
        &mut self,
        name: &str,
        definition_start: u32,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let (ns, entry) = fs
            .class_ns
            .as_ref()
            .map(|state| (state.ns, state.methods.get(&definition_start).copied()))?;
        // `@overload` stubs and property accessors are type-only markers and
        // bind nothing (the static path skips them the same way).
        let (fid, kind) = entry?;
        let value = self.class_method_binding_value(fid, kind, line, col);
        Some(HStmt {
            kind: HStmtKind::SetSubscript {
                object: HExpr {
                    kind: HExprKind::Local(ns),
                    ty: Ty::Concrete(Type::Dict),
                    line,
                    col,
                },
                index: HExpr {
                    kind: HExprKind::Const(Const::Str(name.to_string())),
                    ty: Ty::Concrete(Type::Str),
                    line,
                    col,
                },
                value,
            },
            line,
            col,
        })
    }

    /// Bind `import ...` names into the class namespace.
    ///
    /// CPython executes an import in a class body with `STORE_NAME`, so the
    /// module value lands in the class namespace rather than the module
    /// globals. Hydro resolves imports at compile time; when the binding is a
    /// native module (or an imported member) the value expression can be
    /// rebuilt here and stored into the namespace dict.
    fn lower_class_body_import(
        &mut self,
        aliases: &[ast::Alias],
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let ns = fs.class_ns.as_ref()?.ns;
        let mut statements = Vec::new();
        let mut ok = true;
        for alias in aliases {
            let binding = import_binding(alias);
            match self.class_namespace_import_value(&binding, alias.range, line, col) {
                Some(value) => {
                    statements.push(class_namespace_store(ns, &binding, value, line, col))
                }
                None => ok = false,
            }
        }
        if !ok {
            return None;
        }
        Some(class_namespace_block(statements, line, col))
    }

    /// Bind `from ... import ...` names into the class namespace.
    fn lower_class_body_import_from(
        &mut self,
        s: &ast::StmtImportFrom,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let ns = fs.class_ns.as_ref()?.ns;
        let mut statements = Vec::new();
        let mut ok = true;
        for alias in &s.names {
            if alias.name.as_str() == "*" {
                self.error_at_code(
                    alias.range,
                    codes::UNSUPPORTED,
                    "'import *' inside a class body is not supported",
                );
                ok = false;
                continue;
            }
            let binding = match &alias.asname {
                Some(name) => name.to_string(),
                None => alias.name.to_string(),
            };
            match self.class_namespace_import_value(&binding, alias.range, line, col) {
                Some(value) => {
                    statements.push(class_namespace_store(ns, &binding, value, line, col))
                }
                None => ok = false,
            }
        }
        if !ok {
            return None;
        }
        Some(class_namespace_block(statements, line, col))
    }

    /// Rebuild the runtime value of a compile-time import binding for a
    /// class-body `import` statement.
    fn class_namespace_import_value(
        &mut self,
        binding: &str,
        range: TextRange,
        line: u32,
        col: u32,
    ) -> Option<HExpr> {
        if let Some(module) = self.module_aliases.get(binding).copied() {
            return Some(HExpr {
                kind: HExprKind::ModuleRef {
                    module: module.name.to_string(),
                },
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            });
        }
        if let Some((module, function)) = self.imported_functions.get(binding).copied() {
            return Some(HExpr {
                kind: HExprKind::ModuleAttr {
                    module: module.name.to_string(),
                    name: function.name.to_string(),
                },
                ty: Ty::Concrete(Type::Function),
                line,
                col,
            });
        }
        if let Some((module, constant)) = self.imported_constants.get(binding).copied() {
            return Some(HExpr {
                kind: HExprKind::ModuleAttr {
                    module: module.name.to_string(),
                    name: constant.name.to_string(),
                },
                ty: Ty::Concrete(constant.ty),
                line,
                col,
            });
        }
        self.error_at_code(
            range,
            codes::UNSUPPORTED,
            format!(
                "class-body import binding '{binding}' is not supported: \
                 only native modules and their members can be stored in a class namespace"
            ),
        );
        None
    }

    fn class_method_binding_value(
        &mut self,
        fid: FunctionId,
        kind: MethodKind,
        line: u32,
        col: u32,
    ) -> HExpr {
        // Plan ids are unit-local; referenced values use the program-wide id.
        let fid = self.fid_base + fid;
        let function = HExpr {
            kind: HExprKind::FunctionRef(fid),
            ty: Ty::Concrete(Type::Function),
            line,
            col,
        };
        let builtin = match kind {
            MethodKind::Normal => return function,
            MethodKind::Classmethod => Builtin::Classmethod,
            MethodKind::Staticmethod => Builtin::Staticmethod,
            MethodKind::Property => Builtin::Property,
        };
        HExpr {
            kind: HExprKind::Call {
                callee: Callee::Builtin(builtin),
                args: vec![function],
            },
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        }
    }

    /// Lower a `class` statement: bases and `metaclass=` in source order,
    /// then the method functions and class attribute values of the body.
    ///
    /// Runtime decorators are applied by [`Self::lower_class_definition`],
    /// which wraps this statement; a class with only compile-time decorators
    /// (or none) lowers directly to [`HStmtKind::ClassDef`].
    fn lower_class_stmt(
        &mut self,
        s: &ast::StmtClassDef,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let Some(plan) = self
            .class_defs
            .get(&(u32::from(s.range.start()) as usize))
            .cloned()
        else {
            self.error_at(
                s.range,
                "class definitions are only supported directly at module level",
            );
            return None;
        };
        if fs.class_ns.is_some() {
            return self.lower_class_in_class_body(s, &plan, fs, line, col);
        }
        if !fs.is_module {
            self.error_at(s.range, "nested class definitions are not supported yet");
            return None;
        }
        let name = s.name.to_string();
        let Some(target) = fs.lookup(&name) else {
            self.error_name(
                s.range,
                &name,
                fs,
                format!("class '{name}' has no local slot"),
            );
            return None;
        };
        if plan.dynamic {
            return self.lower_dynamic_class_stmt(s, &plan, fs, line, col);
        }
        self.lower_static_class_def(
            fs,
            ClassEmission {
                class: s,
                plan: &plan,
                name: &name,
                target,
                mirror_globals: false,
                line,
                col,
            },
        )
    }

    /// Build the static `ClassDef` statement for a class whose body is fully
    /// expressible in the plan (methods, fields and frozen attributes).
    fn lower_static_class_def(
        &mut self,
        fs: &mut FuncState,
        emission: ClassEmission<'_>,
    ) -> Option<HStmt> {
        let ClassEmission {
            class: s,
            plan,
            name,
            target,
            line,
            col,
            ..
        } = emission;
        let bases: Vec<HExpr> = plan
            .bases
            .iter()
            .filter_map(|index| s.bases.get(*index))
            .map(|base| self.expr(base, fs))
            .collect();
        let metaclass = plan
            .metaclass
            .and_then(|index| s.keywords.get(index))
            .map(|keyword| self.expr(&keyword.value, fs));
        let methods: Vec<(String, FunctionId, MethodKind)> = plan
            .methods
            .iter()
            .map(|(name, function, _, kind)| (name.clone(), self.fid_base + *function, *kind))
            .collect();
        let attributes = {
            self.class_attr_scopes.push(HashMap::new());
            // Class-body expressions can read methods and method aliases that
            // were created earlier in the body; the value is the function
            // object, exactly like a name read in CPython's class namespace.
            if let Some(scope) = self.class_attr_scopes.last_mut() {
                for (method, function, _) in &methods {
                    scope.insert(
                        method.clone(),
                        HExpr {
                            kind: HExprKind::FunctionRef(*function),
                            ty: Ty::Concrete(Type::Function),
                            line,
                            col,
                        },
                    );
                }
            }
            let mut attributes = Vec::with_capacity(plan.attributes.len());
            for (attribute, index) in &plan.attributes {
                // The value expression is evaluated when the class statement
                // runs, in declaration order, and may refer to attributes
                // declared before it.
                let value = match &s.body[*index] {
                    ast::Stmt::Assign(assign) => self.expr(&assign.value, fs),
                    ast::Stmt::AnnAssign(assign) => match assign.value.as_deref() {
                        Some(value) => self.expr(value, fs),
                        None => self.error_expr(assign.range),
                    },
                    other => self.error_expr(other.range()),
                };
                if let Some(scope) = self.class_attr_scopes.last_mut() {
                    scope.insert(attribute.clone(), value.clone());
                }
                attributes.push((attribute.clone(), value));
            }
            self.class_attr_scopes.pop();
            attributes
        };
        Some(HStmt {
            kind: HStmtKind::ClassDef {
                target,
                name: name.to_string(),
                bases,
                metaclass,
                methods,
                attributes,
            },
            line,
            col,
        })
    }

    /// Lower a class definition nested inside another class body.
    ///
    /// The class object is built in a hidden local and then stored in the
    /// enclosing class namespace, exactly like CPython's `STORE_NAME` in a
    /// class body. Bases, the metaclass and the nested body are evaluated in
    /// the enclosing class scope, so names bound earlier in the outer body are
    /// visible.
    fn lower_class_in_class_body(
        &mut self,
        s: &ast::StmtClassDef,
        plan: &ClassPlan,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let ns = fs.class_ns.as_ref()?.ns;
        let name = s.name.to_string();
        let (target, target_ref) = self.hidden_temp(fs, "$nestedclass", Type::Any);
        let mut statements = Vec::new();
        if plan.dynamic {
            statements.extend(self.lower_dynamic_class_value(
                fs,
                ClassEmission {
                    class: s,
                    plan,
                    name: &name,
                    target,
                    mirror_globals: false,
                    line,
                    col,
                },
            ));
        } else {
            statements.push(self.lower_static_class_def(
                fs,
                ClassEmission {
                    class: s,
                    plan,
                    name: &name,
                    target,
                    mirror_globals: false,
                    line,
                    col,
                },
            )?);
        }
        statements.push(class_namespace_store(ns, &name, target_ref, line, col));
        Some(class_namespace_block(statements, line, col))
    }

    /// Lower `target = value` (plain or annotated assignment).
    ///
    /// `value` has already been lowered, matching Python's evaluation order
    /// (the right-hand side runs before the target expression). A name
    /// declared `global` stores into the runtime global table instead of a
    /// local slot.
    fn lower_store(
        &mut self,
        target: &ast::Expr,
        value: HExpr,
        fs: &mut FuncState,
    ) -> Option<HStmtKind> {
        match target {
            ast::Expr::Name(name) => {
                let name_text = name.id.as_str();
                if fs.is_global(name_text) {
                    self.used_globals.insert(name_text.to_string());
                    Some(HStmtKind::GlobalSet {
                        module: self.dunder_name.clone(),
                        name: name_text.to_string(),
                        value,
                    })
                } else if let Some(class_ns) = &fs.class_ns {
                    // A dynamic class body binds through `STORE_NAME`: the
                    // name always lands in the class namespace, shadowing any
                    // module binding of the same name.
                    let (line, col) = self.span(name.range);
                    let namespace = HExpr {
                        kind: HExprKind::Local(class_ns.ns),
                        ty: Ty::Concrete(Type::Dict),
                        line,
                        col,
                    };
                    let key = HExpr {
                        kind: HExprKind::Const(Const::Str(name_text.to_string())),
                        ty: Ty::Concrete(Type::Str),
                        line,
                        col,
                    };
                    Some(HStmtKind::SetSubscript {
                        object: namespace,
                        index: key,
                        value,
                    })
                } else if let Some(local) = fs.lookup(name_text) {
                    Some(if fs.is_cell(local) {
                        HStmtKind::CellAssign {
                            target: local,
                            value,
                        }
                    } else {
                        HStmtKind::Assign {
                            target: local,
                            value,
                        }
                    })
                } else {
                    self.error_name(
                        name.range,
                        name_text,
                        fs,
                        format!("assignment to unknown name '{}'", name.id),
                    );
                    None
                }
            }
            ast::Expr::Subscript(sub) => {
                if let ast::Expr::Slice(slice) = &*sub.slice {
                    // The IR keeps Python's evaluation order: the right-hand
                    // side was already lowered by the caller, then the object
                    // and finally the bounds in source order.
                    let object = self.expr(&sub.value, fs);
                    let start = slice.lower.as_ref().map(|e| Box::new(self.expr(e, fs)));
                    let stop = slice.upper.as_ref().map(|e| Box::new(self.expr(e, fs)));
                    let step = slice.step.as_ref().map(|e| Box::new(self.expr(e, fs)));
                    return Some(HStmtKind::SetSlice {
                        object,
                        start,
                        stop,
                        step,
                        value,
                    });
                }
                // The IR keeps Python's evaluation order: value, object, index.
                if self.is_globals_call(&sub.value, fs) {
                    // `globals()[key] = value` binds a real module global so
                    // later static reads and `getattr(module, key)` see it.
                    // Evaluation order matches Python: the right-hand side is
                    // already lowered, then the key.
                    let (line, col) = self.span(sub.range);
                    let index = self.expr(&sub.slice, fs);
                    let module = HExpr {
                        kind: HExprKind::Const(Const::Str(self.dunder_name.clone())),
                        ty: Ty::Concrete(Type::Str),
                        line,
                        col,
                    };
                    return Some(HStmtKind::Expr(HExpr {
                        kind: HExprKind::Call {
                            callee: Callee::Builtin(Builtin::GlobalSetDynamic),
                            args: vec![module, index, value],
                        },
                        ty: Ty::Concrete(Type::None),
                        line,
                        col,
                    }));
                }
                let object = self.expr(&sub.value, fs);
                let index = self.expr(&sub.slice, fs);
                Some(HStmtKind::SetSubscript {
                    object,
                    index,
                    value,
                })
            }
            ast::Expr::Attribute(attr) => {
                let object = self.expr(&attr.value, fs);
                Some(HStmtKind::SetAttr {
                    object,
                    name: attr.attr.to_string(),
                    value,
                })
            }
            ast::Expr::Tuple(_) | ast::Expr::List(_) => {
                let targets = self.unpack_targets(target, fs)?;
                Some(HStmtKind::Unpack { targets, value })
            }
            other => {
                self.error_at(
                    other.range(),
                    "assignment target must be a name, attribute or subscript in Phase 2.0",
                );
                None
            }
        }
    }

    /// Lower `del a, obj.name, obj[key], obj[start:stop:step]`.
    ///
    /// Python evaluates and deletes each target in source order: the deletion
    /// of one target happens before the next target's receiver is evaluated.
    /// The IR keeps that order, so codegen emits the deletion immediately
    /// after each target's operands.
    ///
    /// A module-level name that no local slot holds may still be bound by a
    /// conditional `def`/`class` or an import. Conditional definitions bind in
    /// the runtime global table and are deleted from there; import bindings
    /// exist only at compile time and are simply forgotten. A name with no
    /// binding at all lowers to a global deletion so the runtime raises a real
    /// `NameError` instead of a spurious compile diagnostic.
    fn lower_delete(
        &mut self,
        s: &ast::StmtDelete,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        if fs.class_ns.is_some() {
            return self.lower_class_namespace_delete(s, fs, line, col);
        }
        let mut targets = Vec::with_capacity(s.targets.len());
        for target in &s.targets {
            match target {
                ast::Expr::Name(name) => {
                    let text = name.id.as_str();
                    if fs.is_global(text) {
                        self.used_globals.insert(text.to_string());
                        targets.push(HDeleteTarget::Global {
                            module: self.dunder_name.clone(),
                            name: text.to_string(),
                        });
                        continue;
                    }
                    if let Some(local) = fs.lookup(text) {
                        targets.push(if fs.is_cell(local) {
                            HDeleteTarget::Cell(local)
                        } else {
                            HDeleteTarget::Name(local)
                        });
                        // A module-body name is mirrored into the runtime
                        // global table (see `push_lowered_stmt`), so a later
                        // read from a function must miss it too.
                        if fs.is_module
                            && (self.mirror_all_globals || self.used_globals.contains(text))
                        {
                            targets.push(HDeleteTarget::Global {
                                module: self.dunder_name.clone(),
                                name: text.to_string(),
                            });
                        }
                        continue;
                    }
                    if fs.is_module {
                        if self.forget_module_binding(text) {
                            // An import binding lives only in the compile-time
                            // tables; it was never stored in the runtime
                            // global table.
                            continue;
                        }
                        // A statically linked function without a mirrored
                        // global has no runtime binding to remove.
                        let was_function = self.fn_ids.remove(text).is_some();
                        if was_function
                            && !self.mirror_all_globals
                            && !self.used_globals.contains(text)
                        {
                            continue;
                        }
                        targets.push(HDeleteTarget::Global {
                            module: self.dunder_name.clone(),
                            name: text.to_string(),
                        });
                        continue;
                    }
                    self.error_name(
                        name.range,
                        text,
                        fs,
                        format!("cannot delete unknown name '{}'", name.id),
                    );
                    return None;
                }
                other => targets.push(self.lower_delete_expression(other, fs)?),
            }
        }
        if targets.is_empty() {
            return None;
        }
        Some(HStmt {
            kind: HStmtKind::Delete { targets },
            line,
            col,
        })
    }

    /// Lower an attribute, subscript or slice deletion target.
    fn lower_delete_expression(
        &mut self,
        target: &ast::Expr,
        fs: &mut FuncState,
    ) -> Option<HDeleteTarget> {
        match target {
            ast::Expr::Attribute(attr) => {
                let object = self.expr(&attr.value, fs);
                Some(HDeleteTarget::Attr {
                    object,
                    name: attr.attr.to_string(),
                })
            }
            ast::Expr::Subscript(sub) => {
                if let ast::Expr::Slice(slice) = &*sub.slice {
                    let object = self.expr(&sub.value, fs);
                    let start = slice.lower.as_ref().map(|e| Box::new(self.expr(e, fs)));
                    let stop = slice.upper.as_ref().map(|e| Box::new(self.expr(e, fs)));
                    let step = slice.step.as_ref().map(|e| Box::new(self.expr(e, fs)));
                    Some(HDeleteTarget::Slice {
                        object,
                        start,
                        stop,
                        step,
                    })
                } else {
                    let object = self.expr(&sub.value, fs);
                    let index = self.expr(&sub.slice, fs);
                    Some(HDeleteTarget::Subscript { object, index })
                }
            }
            other => {
                self.error_at(
                    other.range(),
                    "delete target must be a name, attribute or subscript",
                );
                None
            }
        }
    }

    /// Lower `del name` in a class body: `DELETE_NAME` against the class
    /// namespace. Deleting a name the namespace never received raises
    /// `NameError`, exactly like CPython; a plain dict deletion would report
    /// `KeyError` instead.
    fn lower_class_namespace_delete(
        &mut self,
        s: &ast::StmtDelete,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let ns = fs.class_ns.as_ref()?.ns;
        let mut statements = Vec::new();
        for target in &s.targets {
            if let ast::Expr::Name(name) = target {
                let (name_line, name_col) = self.span(name.range);
                let namespace = HExpr {
                    kind: HExprKind::Local(ns),
                    ty: Ty::Concrete(Type::Dict),
                    line: name_line,
                    col: name_col,
                };
                let key = HExpr {
                    kind: HExprKind::Const(Const::Str(name.id.to_string())),
                    ty: Ty::Concrete(Type::Str),
                    line: name_line,
                    col: name_col,
                };
                let present = HExpr {
                    kind: HExprKind::Bin {
                        op: BinOp::In,
                        lhs: Box::new(key.clone()),
                        rhs: Box::new(namespace.clone()),
                    },
                    ty: Ty::Concrete(Type::Bool),
                    line: name_line,
                    col: name_col,
                };
                statements.push(HStmt {
                    kind: HStmtKind::If {
                        cond: present,
                        then_body: vec![HStmt {
                            kind: HStmtKind::Delete {
                                targets: vec![HDeleteTarget::Subscript {
                                    object: namespace,
                                    index: key,
                                }],
                            },
                            line: name_line,
                            col: name_col,
                        }],
                        else_body: vec![HStmt {
                            kind: HStmtKind::Raise {
                                exc: Some(ExcKind::NameError),
                                msg: Some(HExpr {
                                    kind: HExprKind::Const(Const::Str(format!(
                                        "name '{}' is not defined",
                                        name.id
                                    ))),
                                    ty: Ty::Concrete(Type::Str),
                                    line: name_line,
                                    col: name_col,
                                }),
                                cause: None,
                            },
                            line: name_line,
                            col: name_col,
                        }],
                    },
                    line: name_line,
                    col: name_col,
                });
                continue;
            }
            let target = self.lower_delete_expression(target, fs)?;
            statements.push(HStmt {
                kind: HStmtKind::Delete {
                    targets: vec![target],
                },
                line,
                col,
            });
        }
        if statements.is_empty() {
            return None;
        }
        Some(class_namespace_block(statements, line, col))
    }

    /// Removes a compile-time import binding of `name`, reporting whether it
    /// existed. A class nesting a `def` of the same name cannot be shadowed
    /// by this: class definitions are local slots in a module body.
    fn forget_module_binding(&mut self, name: &str) -> bool {
        let mut existed = false;
        existed |= self.module_aliases.remove(name).is_some();
        existed |= self.user_module_aliases.remove(name).is_some();
        existed |= self.extension_aliases.remove(name).is_some();
        existed |= self.imported_extension_members.remove(name).is_some();
        existed |= self.imported_functions.remove(name).is_some();
        existed |= self.imported_constants.remove(name).is_some();
        existed |= self.imported_user_functions.remove(name).is_some();
        existed |= self.imported_user_globals.remove(name).is_some();
        existed |= self.deferred_imports.remove(name).is_some();
        existed
    }

    /// Lower `a = b = value` (chained assignment).
    ///
    /// The right-hand side has already been lowered. Python evaluates it once,
    /// then assigns each target left to right, evaluating each target's
    /// receiver and index only once. The value therefore lands in a hidden
    /// `$multiN` local and every store reads that local. HIR statements cannot
    /// be flattened into an arbitrary list from one statement, so the stores
    /// are wrapped in the same `if True:` block used by the augmented-assign
    /// desugarings.
    fn lower_chained_assign(
        &mut self,
        s: &ast::StmtAssign,
        value: HExpr,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let (temp, temp_ref) = self.multi_temp(fs, s.value.range());
        let mut then_body = Vec::with_capacity(s.targets.len() + 1);
        then_body.push(HStmt {
            kind: HStmtKind::Assign {
                target: temp,
                value,
            },
            line,
            col,
        });
        for target in &s.targets {
            let kind = self.lower_store(target, temp_ref.clone(), fs)?;
            then_body.push(HStmt { kind, line, col });
            // Module-level names are mirrored into the global table after
            // their local assignment, exactly like a single-target `Assign`
            // (see `push_lowered_stmt`); importers read them from there.
            if !fs.is_module {
                continue;
            }
            let ast::Expr::Name(name) = target else {
                continue;
            };
            let name_text = name.id.as_str();
            if !self.mirror_all_globals && !self.used_globals.contains(name_text) {
                continue;
            }
            let Some(local) = fs.lookup(name_text) else {
                continue;
            };
            let ty = self.functions[fs.fid as usize].locals[local as usize].ty;
            then_body.push(HStmt {
                kind: HStmtKind::GlobalSet {
                    module: self.dunder_name.clone(),
                    name: name_text.to_string(),
                    value: HExpr {
                        kind: HExprKind::Local(local),
                        ty,
                        line,
                        col,
                    },
                },
                line,
                col,
            });
        }
        Some(HStmt {
            kind: HStmtKind::If {
                cond: HExpr {
                    kind: HExprKind::Const(Const::Bool(true)),
                    ty: Ty::Concrete(Type::Bool),
                    line,
                    col,
                },
                then_body,
                else_body: Vec::new(),
            },
            line,
            col,
        })
    }

    /// Resolve a tuple/list unpacking target into bindable targets.
    ///
    /// Nested containers recurse, and attribute/subscript elements lower their
    /// receiver expressions here but evaluate them at binding time in codegen.
    /// One starred name/attribute/subscript element (`a, *rest = value`) is
    /// allowed at this level; it receives a fresh list of the remaining
    /// elements. Starred elements are rejected inside a nested container.
    fn unpack_targets(
        &mut self,
        target: &ast::Expr,
        fs: &mut FuncState,
    ) -> Option<Vec<HUnpackTarget>> {
        let elems = match target {
            ast::Expr::Tuple(t) => &t.elts,
            ast::Expr::List(l) => &l.elts,
            other => {
                self.error_at(other.range(), "unpacking target must be a tuple or list");
                return None;
            }
        };
        self.unpack_elements(elems, fs, true)
    }

    fn unpack_elements(
        &mut self,
        elems: &[ast::Expr],
        fs: &mut FuncState,
        allow_star: bool,
    ) -> Option<Vec<HUnpackTarget>> {
        let mut targets = Vec::with_capacity(elems.len());
        let mut ok = true;
        let mut starred = false;
        for elem in elems {
            if let ast::Expr::Starred(starred_elem) = elem {
                if !allow_star {
                    self.error_at(
                        starred_elem.range,
                        "starred unpacking cannot appear inside a nested target",
                    );
                    ok = false;
                } else if starred {
                    self.error_at(
                        starred_elem.range,
                        "multiple starred unpacking targets are not allowed",
                    );
                    ok = false;
                } else {
                    starred = true;
                    match self.unpack_single(&starred_elem.value, fs) {
                        Some(target) => targets.push(HUnpackTarget::Star(Box::new(target))),
                        None => ok = false,
                    }
                }
                continue;
            }
            match elem {
                ast::Expr::Tuple(_) | ast::Expr::List(_) => {
                    let nested = match elem {
                        ast::Expr::Tuple(t) => &t.elts,
                        ast::Expr::List(l) => &l.elts,
                        _ => unreachable!("container checked above"),
                    };
                    match self.unpack_elements(nested, fs, false) {
                        Some(nested) => targets.push(HUnpackTarget::Nested(nested)),
                        None => ok = false,
                    }
                }
                other => match self.unpack_single(other, fs) {
                    Some(target) => targets.push(target),
                    None => ok = false,
                },
            }
        }
        if ok {
            Some(targets)
        } else {
            None
        }
    }

    /// Lower one non-container unpacking element: a name, attribute or
    /// subscript. Container elements and stars are handled by the caller.
    fn unpack_single(&mut self, elem: &ast::Expr, fs: &mut FuncState) -> Option<HUnpackTarget> {
        match elem {
            ast::Expr::Name(n) => match fs.lookup(n.id.as_str()) {
                Some(local) => Some(if fs.is_cell(local) {
                    HUnpackTarget::Cell(local)
                } else {
                    HUnpackTarget::Name(local)
                }),
                None => {
                    self.error_name(
                        n.range,
                        n.id.as_str(),
                        fs,
                        format!("assignment to unknown name '{}'", n.id),
                    );
                    None
                }
            },
            ast::Expr::Attribute(attr) => {
                let object = self.expr(&attr.value, fs);
                Some(HUnpackTarget::Attr {
                    object,
                    name: attr.attr.to_string(),
                })
            }
            ast::Expr::Subscript(sub) => {
                if matches!(&*sub.slice, ast::Expr::Slice(_)) {
                    self.error_at_code(
                        sub.slice.range(),
                        codes::INDEX,
                        "slice assignment is not supported in an unpacking target",
                    );
                    None
                } else {
                    let object = self.expr(&sub.value, fs);
                    let index = self.expr(&sub.slice, fs);
                    Some(HUnpackTarget::Subscript { object, index })
                }
            }
            other => {
                self.error_at(
                    other.range(),
                    "unpacking target must be a name, attribute or subscript",
                );
                None
            }
        }
    }

    /// Create the hidden `$unpackN` local that receives one value of a
    /// desugared `for` unpack target.
    fn unpack_temp(&mut self, fs: &mut FuncState, target: &ast::Expr) -> (LocalId, HExpr) {
        let name = format!("$unpack{}", self.unpack_counter);
        self.unpack_counter += 1;
        let var = self.functions[fs.fid as usize].locals.len() as LocalId;
        self.functions[fs.fid as usize].locals.push(HLocal {
            name: name.clone(),
            ty: Ty::Concrete(Type::Any),
        });
        fs.locals.insert(name, var);
        let (line, col) = self.span(target.range());
        let value = HExpr {
            kind: HExprKind::Local(var),
            ty: self.infer.fresh(),
            line,
            col,
        };
        (var, value)
    }

    /// Create a hidden local in the current function. `prefix` names it for
    /// diagnostics; the counter keeps names unique.
    fn hidden_temp(&mut self, fs: &mut FuncState, prefix: &str, ty: Type) -> (LocalId, HExpr) {
        let counter = if prefix == "$decor" {
            let counter = self.decorator_counter;
            self.decorator_counter += 1;
            counter
        } else {
            let counter = self.match_counter;
            self.match_counter += 1;
            counter
        };
        let name = format!("{prefix}{counter}");
        let var = self.functions[fs.fid as usize].locals.len() as LocalId;
        self.functions[fs.fid as usize].locals.push(HLocal {
            name: name.clone(),
            ty: Ty::Concrete(ty),
        });
        fs.locals.insert(name, var);
        let value = HExpr {
            kind: HExprKind::Local(var),
            ty: Ty::Concrete(ty),
            line: 0,
            col: 0,
        };
        (var, value)
    }

    /// Decorator expressions of a function that are applied at runtime, in
    /// source order. Pytest markers are compile-time directives and
    /// `@overload` never creates a function value; everything else is a real
    /// decorator.
    fn runtime_function_decorators<'d>(&self, decorators: &'d [ast::Expr]) -> Vec<&'d ast::Expr> {
        decorators
            .iter()
            .filter(|decorator| {
                !is_pytest_decorator(decorator) && !is_overload_decorator(decorator)
            })
            .collect()
    }

    /// Whether a function definition needs runtime decorator application.
    fn has_runtime_function_decorators(&self, decorators: &[ast::Expr]) -> bool {
        !self.runtime_function_decorators(decorators).is_empty()
    }

    /// Evaluate the decorator expressions top-down, storing each callable in a
    /// hidden local. CPython evaluates them before the function/class object
    /// is created.
    fn lower_decorator_temps(
        &mut self,
        decorators: &[&ast::Expr],
        fs: &mut FuncState,
        out: &mut Vec<HStmt>,
    ) -> Vec<LocalId> {
        let mut temps = Vec::with_capacity(decorators.len());
        for decorator in decorators {
            let (line, col) = self.span(decorator.range());
            let (local, _) = self.hidden_temp(fs, "$decor", Type::Any);
            let value = self.expr(decorator, fs);
            out.push(HStmt {
                kind: HStmtKind::Assign {
                    target: local,
                    value,
                },
                line,
                col,
            });
            temps.push(local);
        }
        temps
    }

    /// Apply decorator values bottom-up: `target = dec(target)` starting with
    /// the last-evaluated decorator (`@a @b def f` is `f = a(b(f))`).
    fn apply_decorator_temps(
        &mut self,
        temps: &[LocalId],
        target: LocalId,
        fs: &mut FuncState,
        out: &mut Vec<HStmt>,
        line: u32,
        col: u32,
    ) {
        let ty = self.functions[fs.fid as usize]
            .locals
            .get(target as usize)
            .map(|local| local.ty)
            .unwrap_or(Ty::Concrete(Type::Any));
        for decorator in temps.iter().rev() {
            let callee = HExpr {
                kind: HExprKind::Local(*decorator),
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            };
            let argument = HExpr {
                kind: HExprKind::Local(target),
                ty,
                line,
                col,
            };
            let value = HExpr {
                kind: HExprKind::IndirectCall {
                    callee: Box::new(callee),
                    args: vec![argument],
                    star: None,
                },
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            };
            out.push(HStmt {
                kind: HStmtKind::Assign { target, value },
                line,
                col,
            });
        }
    }

    /// Lower a module-level `def`/`async def` that carries runtime
    /// decorators: evaluate the decorators, create the function value (with
    /// its defaults), apply the decorators bottom-up and bind the result to
    /// the module local and, when importers or functions read it, the runtime
    /// global table.
    #[allow(clippy::too_many_arguments)]
    fn lower_module_function_definition(
        &mut self,
        decorators: &[ast::Expr],
        fid: FunctionId,
        range: TextRange,
        fs: &mut FuncState,
        out: &mut Vec<HStmt>,
        line: u32,
        col: u32,
    ) {
        let Some(function) = self.functions.get(fid as usize) else {
            return;
        };
        let name = function.name.clone();
        let Some(target) = fs.lookup(&name) else {
            self.error_name(
                range,
                &name,
                fs,
                format!("function '{name}' has no local slot"),
            );
            return;
        };
        let decorators = self.runtime_function_decorators(decorators);
        let temps = self.lower_decorator_temps(&decorators, fs, out);
        let (function_temp, _) = self.hidden_temp(fs, "$decor", Type::Any);
        out.push(HStmt {
            kind: HStmtKind::Assign {
                target: function_temp,
                value: HExpr {
                    kind: HExprKind::FunctionRef(self.fid_base + fid),
                    ty: Ty::Concrete(Type::Function),
                    line,
                    col,
                },
            },
            line,
            col,
        });
        self.apply_decorator_temps(&temps, function_temp, fs, out, line, col);
        let ty = Ty::Concrete(Type::Any);
        out.push(HStmt {
            kind: HStmtKind::Assign {
                target,
                value: HExpr {
                    kind: HExprKind::Local(function_temp),
                    ty,
                    line,
                    col,
                },
            },
            line,
            col,
        });
        // The name is read through the global table by functions of this unit
        // and by importers, so the decorated value is always mirrored even
        // though the read-usage scan ran before this function was registered.
        out.push(HStmt {
            kind: HStmtKind::GlobalSet {
                module: self.dunder_name.clone(),
                name,
                value: HExpr {
                    kind: HExprKind::Local(target),
                    ty,
                    line,
                    col,
                },
            },
            line,
            col,
        });
    }

    /// Lower a module-level class statement with runtime decorators.
    fn lower_decorated_class(
        &mut self,
        s: &ast::StmtClassDef,
        fs: &mut FuncState,
        out: &mut Vec<HStmt>,
        line: u32,
        col: u32,
    ) {
        let name = s.name.to_string();
        let Some(plan) = self
            .class_defs
            .get(&(u32::from(s.range.start()) as usize))
            .cloned()
        else {
            self.error_at(
                s.range,
                "class definitions are only supported directly at module level",
            );
            return;
        };
        let decorators: Vec<&ast::Expr> = plan
            .decorators
            .iter()
            .filter_map(|index| s.decorator_list.get(*index))
            .collect();
        let temps = self.lower_decorator_temps(&decorators, fs, out);
        if let Some(statement) = self.lower_class_stmt(s, fs, line, col) {
            out.push(statement);
        } else {
            return;
        }
        let Some(target) = fs.lookup(&name) else {
            return;
        };
        self.apply_decorator_temps(&temps, target, fs, out, line, col);
        if self.mirror_all_globals || self.used_globals.contains(&name) {
            let ty = self.functions[fs.fid as usize]
                .locals
                .get(target as usize)
                .map(|local| local.ty)
                .unwrap_or(Ty::Concrete(Type::Any));
            out.push(HStmt {
                kind: HStmtKind::GlobalSet {
                    module: self.dunder_name.clone(),
                    name,
                    value: HExpr {
                        kind: HExprKind::Local(target),
                        ty,
                        line,
                        col,
                    },
                },
                line,
                col,
            });
        }
    }

    /// Create the hidden `$forN` local that receives the value bound by a
    /// `for` statement whose target is a closure cell.
    fn for_temp(&mut self, fs: &mut FuncState, range: TextRange) -> (LocalId, HExpr) {
        let name = format!("$for{}", self.for_counter);
        self.for_counter += 1;
        let var = self.functions[fs.fid as usize].locals.len() as LocalId;
        self.functions[fs.fid as usize].locals.push(HLocal {
            name: name.clone(),
            ty: Ty::Concrete(Type::Any),
        });
        fs.locals.insert(name, var);
        let (line, col) = self.span(range);
        let value = HExpr {
            kind: HExprKind::Local(var),
            ty: self.infer.fresh(),
            line,
            col,
        };
        (var, value)
    }

    /// Fresh cells created at the top of one loop iteration for every function
    /// cell the iteration mutates (including a cell used as the loop target).
    /// Closures created inside the iteration capture that iteration's cell.
    fn loop_cell_inits(
        &mut self,
        fs: &FuncState,
        body: &[ast::Stmt],
        target_expr: Option<&ast::Expr>,
        line: u32,
        col: u32,
    ) -> Vec<HStmt> {
        let mut mutated = HashSet::new();
        collect_scope_mutations(body, &mut mutated);
        if let Some(target) = target_expr {
            let mut names = Vec::new();
            collect_target_names(target, &mut names);
            mutated.extend(names);
        }
        // Only cells captured by a closure defined inside this loop need a
        // per-iteration binding; a closure created outside the loop must keep
        // observing the enclosing function's single cell.
        let mut captured_here = HashSet::new();
        collect_nested_needed(body, &mut captured_here);
        captured_here.extend(self.nested_function_template_names(body));
        let mut cells: Vec<LocalId> = mutated
            .iter()
            .filter(|name| captured_here.contains(*name))
            .filter_map(|name| fs.lookup(name))
            // Alias cells belong to an enclosing function: a fresh cell here
            // would break the shared binding.
            .filter(|local| fs.is_cell(*local) && !fs.cell_aliases.contains(local))
            .collect();
        cells.sort_unstable();
        cells.dedup();
        cells
            .into_iter()
            .map(|target| HStmt {
                kind: HStmtKind::CellInit {
                    target,
                    value: None,
                },
                line,
                col,
            })
            .collect()
    }

    /// Create a hidden local of an asynchronous lowering (`$aiterN`,
    /// `$aitemN`, `$aexcN`, `$aloopN`).
    ///
    /// The name contains `$`, so user code can never refer to the slot and it
    /// is not registered in the scope's name table.
    fn async_temp(
        &mut self,
        fs: &mut FuncState,
        prefix: &str,
        line: u32,
        col: u32,
    ) -> (LocalId, HExpr) {
        let name = format!("{prefix}{}", self.async_counter);
        self.async_counter += 1;
        let var = self.functions[fs.fid as usize].locals.len() as LocalId;
        self.functions[fs.fid as usize].locals.push(HLocal {
            name,
            ty: Ty::Concrete(Type::Any),
        });
        let value = HExpr {
            kind: HExprKind::Local(var),
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        };
        (var, value)
    }

    /// Create a hidden `with` local (`$withN` for the context object, `$wexcN`
    /// for the exception it may suppress) so both stay reachable from the
    /// generated prelude, handler and `finally` block.
    fn with_temp(
        &mut self,
        fs: &mut FuncState,
        prefix: &str,
        range: TextRange,
    ) -> (LocalId, HExpr) {
        let name = format!("{prefix}{}", self.with_counter);
        self.with_counter += 1;
        let var = self.functions[fs.fid as usize].locals.len() as LocalId;
        self.functions[fs.fid as usize].locals.push(HLocal {
            name: name.clone(),
            ty: Ty::Concrete(Type::Any),
        });
        fs.locals.insert(name, var);
        let (line, col) = self.span(range);
        let value = HExpr {
            kind: HExprKind::Local(var),
            ty: self.infer.fresh(),
            line,
            col,
        };
        (var, value)
    }

    /// Create the hidden `$augN` local that holds one half of a subscript
    /// augmented-assignment target so it is evaluated exactly once.
    fn aug_temp(&mut self, fs: &mut FuncState, range: TextRange) -> (LocalId, HExpr) {
        let name = format!("$aug{}", self.aug_counter);
        self.aug_counter += 1;
        let var = self.functions[fs.fid as usize].locals.len() as LocalId;
        self.functions[fs.fid as usize].locals.push(HLocal {
            name: name.clone(),
            ty: Ty::Concrete(Type::Any),
        });
        fs.locals.insert(name, var);
        let (line, col) = self.span(range);
        let value = HExpr {
            kind: HExprKind::Local(var),
            ty: self.infer.fresh(),
            line,
            col,
        };
        (var, value)
    }

    /// Create the hidden `$multiN` local that holds the right-hand side of a
    /// chained assignment so it is evaluated exactly once and each target
    /// reads the same value.
    fn multi_temp(&mut self, fs: &mut FuncState, range: TextRange) -> (LocalId, HExpr) {
        let name = format!("$multi{}", self.multi_counter);
        self.multi_counter += 1;
        let var = self.functions[fs.fid as usize].locals.len() as LocalId;
        self.functions[fs.fid as usize].locals.push(HLocal {
            name: name.clone(),
            ty: Ty::Concrete(Type::Any),
        });
        fs.locals.insert(name, var);
        let (line, col) = self.span(range);
        let value = HExpr {
            kind: HExprKind::Local(var),
            ty: self.infer.fresh(),
            line,
            col,
        };
        (var, value)
    }

    /// Create the hidden `$cmpN` local that holds one middle operand of a
    /// chained comparison so it is evaluated exactly once.
    fn cmp_temp(&mut self, fs: &mut FuncState, range: TextRange) -> (LocalId, HExpr) {
        let name = format!("$cmp{}", self.cmp_counter);
        self.cmp_counter += 1;
        let var = self.functions[fs.fid as usize].locals.len() as LocalId;
        self.functions[fs.fid as usize].locals.push(HLocal {
            name: name.clone(),
            ty: Ty::Concrete(Type::Any),
        });
        fs.locals.insert(name, var);
        let (line, col) = self.span(range);
        let value = HExpr {
            kind: HExprKind::Local(var),
            ty: self.infer.fresh(),
            line,
            col,
        };
        (var, value)
    }

    /// Lower `target[index] op= value`. Python evaluates the target object,
    /// then the index, then the right-hand side, and none of them twice. The
    /// object and index are therefore stored in hidden `$augN` locals; the
    /// read and the write both use those locals. HIR statements cannot be
    /// nested arbitrarily, so the three generated statements are wrapped in an
    /// `if True:` block.
    fn lower_aug_attribute(
        &mut self,
        s: &ast::StmtAugAssign,
        attr: &ast::ExprAttribute,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let op = self.bin_op(s.op, s.range)?;
        let object = self.expr(&attr.value, fs);
        let (object_local, object_ref) = self.aug_temp(fs, attr.value.range());
        let value = self.expr(&s.value, fs);
        let read = HExpr {
            kind: HExprKind::GetAttr {
                object: Box::new(object_ref.clone()),
                name: attr.attr.to_string(),
            },
            ty: self.infer.fresh(),
            line,
            col,
        };
        let updated = HExpr {
            kind: HExprKind::Bin {
                op,
                lhs: Box::new(read),
                rhs: Box::new(value),
            },
            ty: self.infer.fresh(),
            line,
            col,
        };
        let mk_stmt = |kind| HStmt { kind, line, col };
        Some(mk_stmt(HStmtKind::If {
            cond: HExpr {
                kind: HExprKind::Const(Const::Bool(true)),
                ty: Ty::Concrete(Type::Bool),
                line,
                col,
            },
            then_body: vec![
                mk_stmt(HStmtKind::Assign {
                    target: object_local,
                    value: object,
                }),
                mk_stmt(HStmtKind::SetAttr {
                    object: object_ref,
                    name: attr.attr.to_string(),
                    value: updated,
                }),
            ],
            else_body: Vec::new(),
        }))
    }

    fn lower_aug_subscript(
        &mut self,
        s: &ast::StmtAugAssign,
        sub: &ast::ExprSubscript,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let op = self.bin_op(s.op, s.range)?;
        let object = self.expr(&sub.value, fs);
        let index = self.expr(&sub.slice, fs);
        let (object_local, object_ref) = self.aug_temp(fs, sub.value.range());
        let (index_local, index_ref) = self.aug_temp(fs, sub.slice.range());
        let value = self.expr(&s.value, fs);

        let read = HExpr {
            kind: HExprKind::Subscript {
                object: Box::new(object_ref.clone()),
                index: Box::new(index_ref.clone()),
            },
            ty: self.infer.fresh(),
            line,
            col,
        };
        let updated = HExpr {
            kind: HExprKind::Bin {
                op,
                lhs: Box::new(read),
                rhs: Box::new(value),
            },
            ty: self.infer.fresh(),
            line,
            col,
        };
        let mk_stmt = |kind| HStmt { kind, line, col };
        let set = mk_stmt(HStmtKind::SetSubscript {
            object: object_ref,
            index: index_ref,
            value: updated,
        });
        let then_body = vec![
            mk_stmt(HStmtKind::Assign {
                target: object_local,
                value: object,
            }),
            mk_stmt(HStmtKind::Assign {
                target: index_local,
                value: index,
            }),
            set,
        ];
        let kind = HStmtKind::If {
            cond: HExpr {
                kind: HExprKind::Const(Const::Bool(true)),
                ty: Ty::Concrete(Type::Bool),
                line,
                col,
            },
            then_body,
            else_body: Vec::new(),
        };
        Some(HStmt { kind, line, col })
    }

    /// Create a synthetic `$compN` local for a comprehension target. The local
    /// receives whatever the iterable yields, so its static type is `Any`.
    fn comp_local(&mut self, fs: &mut FuncState) -> LocalId {
        let name = format!("$comp{}", self.comp_counter);
        self.comp_counter += 1;
        let local = self.functions[fs.fid as usize].locals.len() as LocalId;
        self.functions[fs.fid as usize].locals.push(HLocal {
            name,
            ty: Ty::Concrete(Type::Any),
        });
        local
    }

    /// Resolve the local slot an assignment expression writes.
    ///
    /// A name already bound in this scope reuses its slot; a new name gets a
    /// function-local slot whose static type is `Any`. Assignment expressions
    /// are not part of the pre-pass that collects statement targets, so a
    /// read that appears *earlier* in the source — or a nested function that
    /// captures a walrus-only name — is not resolved to this slot. At module
    /// level the value is not mirrored into the runtime global table.
    fn walrus_local(&mut self, fs: &mut FuncState, name: &str) -> LocalId {
        if let Some(local) = fs.lookup(name) {
            return local;
        }
        let local = self.functions[fs.fid as usize].locals.len() as LocalId;
        self.functions[fs.fid as usize].locals.push(HLocal {
            name: name.to_string(),
            ty: Ty::Concrete(Type::Any),
        });
        fs.locals.insert(name.to_string(), local);
        local
    }

    /// Bind a comprehension target name to a fresh synthetic local, remembering
    /// the previous binding so it can be restored when the comprehension ends.
    fn bind_comp_name(
        &mut self,
        fs: &mut FuncState,
        name: &str,
        saved: &mut Vec<(String, Option<LocalId>)>,
    ) -> LocalId {
        let local = self.comp_local(fs);
        let previous = fs.locals.insert(name.to_string(), local);
        saved.push((name.to_string(), previous));
        *fs.comp_shadowed.entry(name.to_string()).or_insert(0) += 1;
        local
    }

    fn lower_comp_target(
        &mut self,
        target: &ast::Expr,
        fs: &mut FuncState,
        saved: &mut Vec<(String, Option<LocalId>)>,
    ) -> HCompTarget {
        match target {
            ast::Expr::Name(n) => HCompTarget::Name(self.bind_comp_name(fs, n.id.as_str(), saved)),
            ast::Expr::Tuple(_) | ast::Expr::List(_) => {
                // Validate the whole target before binding anything: a rejected
                // target must not shadow names inside the comprehension.
                if !self.valid_comp_unpack(target) {
                    return HCompTarget::Name(self.comp_local(fs));
                }
                let targets = self.bind_comp_unpack(target, fs, saved);
                HCompTarget::Unpack(targets)
            }
            other => {
                self.error_at(other.range(), "unsupported comprehension target");
                // Keep lowering with a hidden local so later diagnostics still
                // refer to a well-formed clause.
                HCompTarget::Name(self.comp_local(fs))
            }
        }
    }

    /// Check a comprehension unpack target (including nested containers)
    /// without binding names or lowering expressions.
    fn valid_comp_unpack(&mut self, target: &ast::Expr) -> bool {
        let elems = match target {
            ast::Expr::Tuple(t) => &t.elts,
            ast::Expr::List(l) => &l.elts,
            _ => return false,
        };
        let mut ok = true;
        for elem in elems {
            match elem {
                ast::Expr::Name(_) | ast::Expr::Attribute(_) => {}
                ast::Expr::Subscript(sub) => {
                    if matches!(&*sub.slice, ast::Expr::Slice(_)) {
                        self.error_at_code(
                            sub.slice.range(),
                            codes::INDEX,
                            "slice assignment is not supported in a comprehension target",
                        );
                        ok = false;
                    }
                }
                ast::Expr::Tuple(_) | ast::Expr::List(_) => {
                    if !self.valid_comp_unpack(elem) {
                        ok = false;
                    }
                }
                ast::Expr::Starred(s) => {
                    // One trailing star is allowed, as in normal unpacking.
                    if !matches!(
                        &*s.value,
                        ast::Expr::Name(_) | ast::Expr::Attribute(_) | ast::Expr::Subscript(_)
                    ) {
                        self.error_at(
                            s.range,
                            "starred target must be a name, attribute or subscript",
                        );
                        ok = false;
                    }
                }
                other => {
                    self.error_at(other.range(), "unsupported comprehension target");
                    ok = false;
                }
            }
        }
        ok
    }

    fn bind_comp_unpack(
        &mut self,
        target: &ast::Expr,
        fs: &mut FuncState,
        saved: &mut Vec<(String, Option<LocalId>)>,
    ) -> Vec<HUnpackTarget> {
        let elems = match target {
            ast::Expr::Tuple(t) => &t.elts,
            ast::Expr::List(l) => &l.elts,
            _ => return Vec::new(),
        };
        let mut targets = Vec::with_capacity(elems.len());
        for elem in elems {
            let target = match elem {
                ast::Expr::Name(n) => {
                    HUnpackTarget::Name(self.bind_comp_name(fs, n.id.as_str(), saved))
                }
                ast::Expr::Tuple(_) | ast::Expr::List(_) => {
                    HUnpackTarget::Nested(self.bind_comp_unpack(elem, fs, saved))
                }
                ast::Expr::Attribute(attr) => HUnpackTarget::Attr {
                    object: self.expr(&attr.value, fs),
                    name: attr.attr.to_string(),
                },
                ast::Expr::Subscript(sub) => HUnpackTarget::Subscript {
                    object: self.expr(&sub.value, fs),
                    index: self.expr(&sub.slice, fs),
                },
                ast::Expr::Starred(star) => {
                    let inner = match &*star.value {
                        ast::Expr::Name(n) => {
                            HUnpackTarget::Name(self.bind_comp_name(fs, n.id.as_str(), saved))
                        }
                        ast::Expr::Attribute(attr) => HUnpackTarget::Attr {
                            object: self.expr(&attr.value, fs),
                            name: attr.attr.to_string(),
                        },
                        ast::Expr::Subscript(sub) => HUnpackTarget::Subscript {
                            object: self.expr(&sub.value, fs),
                            index: self.expr(&sub.slice, fs),
                        },
                        _ => HUnpackTarget::Name(self.comp_local(fs)),
                    };
                    HUnpackTarget::Star(Box::new(inner))
                }
                // Already rejected by `valid_comp_unpack`.
                _ => HUnpackTarget::Name(self.comp_local(fs)),
            };
            targets.push(target);
        }
        targets
    }

    /// Lower the clauses of a comprehension outward-in. Each iterable is
    /// lowered before its target binds, so `[x for x in x]` sees the outer `x`.
    /// The returned saved bindings must be restored after the element
    /// expression is lowered.
    fn lower_comp_clauses(
        &mut self,
        generators: &[ast::Comprehension],
        fs: &mut FuncState,
    ) -> (Vec<HCompClause>, Vec<(String, Option<LocalId>)>) {
        let mut clauses = Vec::with_capacity(generators.len());
        let mut saved = Vec::new();
        for generator in generators {
            let clause = self.lower_comp_clause(generator, fs, &mut saved);
            clauses.push(clause);
        }
        (clauses, saved)
    }

    /// Lower one `for target in iterable if ...` clause, evaluating the
    /// iterable before its target binds.
    fn lower_comp_clause(
        &mut self,
        generator: &ast::Comprehension,
        fs: &mut FuncState,
        saved: &mut Vec<(String, Option<LocalId>)>,
    ) -> HCompClause {
        let iterable = self.expr(&generator.iter, fs);
        self.lower_comp_clause_from(generator, fs, saved, iterable)
    }

    /// Lower a clause whose iterable was already evaluated (`iterable`).
    fn lower_comp_clause_from(
        &mut self,
        generator: &ast::Comprehension,
        fs: &mut FuncState,
        saved: &mut Vec<(String, Option<LocalId>)>,
        iterable: HExpr,
    ) -> HCompClause {
        if generator.is_async && !fs.is_async {
            // CPython rejects an asynchronous comprehension outside an
            // asynchronous function; the enclosing frame would have no way to
            // await the iterator.
            self.error_at_code(
                generator.target.range(),
                codes::CONTROL,
                "asynchronous comprehension outside of an asynchronous function",
            );
        }
        let target = self.lower_comp_target(&generator.target, fs, saved);
        let mut conditions = Vec::with_capacity(generator.ifs.len());
        for condition in &generator.ifs {
            conditions.push(self.expr(condition, fs));
        }
        HCompClause {
            target,
            iterable,
            conditions,
            is_async: generator.is_async,
        }
    }

    /// Lower `(element for ...)` to a lazy generator.
    ///
    /// The comprehension becomes a synthetic generator function: one hidden
    /// parameter receives the outermost iterable (Python evaluates it when the
    /// generator is created) and one parameter per captured enclosing value.
    /// Calling it creates a suspended fiber, so the clauses and the element run
    /// only as items are requested and never build a result list. With an
    /// `async for` clause the function is an async generator (`is_async` and
    /// `is_generator`) instead of a plain generator.
    fn lower_generator_exp(
        &mut self,
        c: &ast::ExprGeneratorExp,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        let Some(first) = c.generators.first() else {
            return self.error_expr(c.range);
        };
        let async_for_clause = c.generators.iter().any(|generator| generator.is_async);
        if async_for_clause && !fs.is_async {
            self.error_at_code(
                c.range,
                codes::CONTROL,
                "asynchronous comprehension outside of an asynchronous function",
            );
        }
        // `await` inside the frame turns the generator expression into an
        // async generator. The enclosing function must be async for that, so
        // the frame only permits `await` when it is; whether it actually
        // contains one is decided after lowering (`await_frames`).
        let frame_is_async = async_for_clause || fs.is_async;

        // Free names read by the generator frame in first-use order. The
        // outermost iterable is evaluated in the enclosing frame, so names it
        // reads nowhere else are not captures. Everything the frame does read
        // from the enclosing scope becomes a parameter.
        let (capture_names, captures, capture_cells, globals) =
            self.comp_captures(&c.generators, &[c.elt.as_ref()], fs, line, col);

        // The outermost iterable belongs to the enclosing frame.
        let outer_iterable = self.expr(&first.iter, fs);

        let fid = self.functions.len() as FunctionId;
        let mut params = Vec::with_capacity(capture_names.len() + 1);
        let mut locals = Vec::with_capacity(capture_names.len() + 1);
        let push_param = |name: &str, params: &mut Vec<HParam>, locals: &mut Vec<HLocal>| {
            let local = locals.len() as LocalId;
            locals.push(HLocal {
                name: name.to_string(),
                ty: Ty::Concrete(Type::Any),
            });
            params.push(HParam {
                local,
                name: name.to_string(),
                ty: Ty::Concrete(Type::Any),
                default: None,
                default_expr: None,
                kind: ParamKind::Positional,
            });
            local
        };
        let iter_local = push_param("$geniter", &mut params, &mut locals);
        let mut genexp_cells = HashSet::new();
        for (index, name) in capture_names.iter().enumerate() {
            let local = push_param(name, &mut params, &mut locals);
            if capture_cells.get(index).copied().unwrap_or(false) {
                genexp_cells.insert(local);
            }
        }
        let (fid_line, fid_col) = self.span(c.range);
        self.functions.push(HFunction {
            name: "<genexpr>".to_string(),
            params,
            capture_count: 0,
            ret_ty: Ty::Concrete(Type::Any),
            locals: locals.clone(),
            body: Vec::new(),
            is_async: async_for_clause,
            is_generator: true,
            is_module: false,
            line: fid_line,
            col: fid_col,
        });

        let mut gfs = FuncState {
            fid,
            is_module: false,
            locals: locals
                .iter()
                .enumerate()
                .map(|(index, local)| (local.name.clone(), index as LocalId))
                .collect(),
            cells: genexp_cells.clone(),
            cell_aliases: genexp_cells,
            comp_shadowed: HashMap::new(),
            globals,
            loop_depth: 0,
            in_lambda: fs.in_lambda,
            is_async: frame_is_async,
            is_generator: true,
            genexp_frame: true,
            class_ns: None,
        };

        let mut saved = Vec::new();
        let mut clauses = Vec::with_capacity(c.generators.len());
        let first_iterable = HExpr {
            kind: HExprKind::Local(iter_local),
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        };
        clauses.push(self.lower_comp_clause_from(first, &mut gfs, &mut saved, first_iterable));
        for generator in &c.generators[1..] {
            let clause = self.lower_comp_clause(generator, &mut gfs, &mut saved);
            clauses.push(clause);
        }
        let element = self.expr(&c.elt, &mut gfs);
        self.restore_comp_bindings(&mut gfs, saved);

        let element_line = element.line;
        let element_col = element.col;
        let body = self.comp_body(
            clauses,
            vec![HStmt {
                kind: HStmtKind::Yield {
                    value: Some(element),
                },
                line: element_line,
                col: element_col,
            }],
            &mut gfs,
        );
        self.functions[fid as usize].body = body;
        // A frame that awaits is an async generator even without an
        // `async for` clause, e.g. `(await f(x) for x in xs)`.
        self.functions[fid as usize].is_async =
            async_for_clause || self.await_frames.contains(&fid);

        let mut args = Vec::with_capacity(captures.len() + 1);
        args.push(outer_iterable);
        args.extend(captures);
        HExpr {
            kind: HExprKind::Call {
                callee: Callee::Function(self.fid_base + fid),
                args,
            },
            ty: self.infer.fresh(),
            line,
            col,
        }
    }

    /// Lower a list/set/dict comprehension that contains at least one
    /// `async for` clause.
    ///
    /// The comprehension becomes a synthetic async function (a coroutine, not
    /// a generator) that builds the result and returns it; the expression is
    /// the `await` of a call to it. The outermost iterable is evaluated in the
    /// enclosing frame when the expression runs, exactly like a synchronous
    /// comprehension, and the remaining clauses and the element run inside the
    /// coroutine with the same target scoping.
    fn lower_async_comprehension(
        &mut self,
        generators: &[ast::Comprehension],
        kind: AsyncCompKind<'_>,
        range: TextRange,
        fs: &mut FuncState,
    ) -> HExpr {
        let (line, col) = self.span(range);
        let Some(first) = generators.first() else {
            return self.error_expr(range);
        };
        // The result is the `await` of the helper, so the enclosing frame
        // awaits as well; a generator expression containing this value is
        // therefore an async generator.
        self.await_frames.insert(fs.fid);
        let result_ty = kind.result_ty();
        let (capture_names, captures, capture_cells, globals) =
            self.comp_captures(generators, &kind.elements(), fs, line, col);
        let outer_iterable = self.expr(&first.iter, fs);

        let fid = self.functions.len() as FunctionId;
        let mut params = Vec::with_capacity(capture_names.len() + 1);
        let mut locals = Vec::with_capacity(capture_names.len() + 2);
        let push_param = |name: &str, params: &mut Vec<HParam>, locals: &mut Vec<HLocal>| {
            let local = locals.len() as LocalId;
            locals.push(HLocal {
                name: name.to_string(),
                ty: Ty::Concrete(Type::Any),
            });
            params.push(HParam {
                local,
                name: name.to_string(),
                ty: Ty::Concrete(Type::Any),
                default: None,
                default_expr: None,
                kind: ParamKind::Positional,
            });
            local
        };
        let iter_local = push_param("$geniter", &mut params, &mut locals);
        let mut frame_cells = HashSet::new();
        for (index, name) in capture_names.iter().enumerate() {
            let local = push_param(name, &mut params, &mut locals);
            if capture_cells.get(index).copied().unwrap_or(false) {
                frame_cells.insert(local);
            }
        }
        let accum = locals.len() as LocalId;
        locals.push(HLocal {
            name: "$accum".to_string(),
            ty: Ty::Concrete(result_ty),
        });

        self.functions.push(HFunction {
            name: "<asynccomp>".to_string(),
            params,
            capture_count: 0,
            ret_ty: Ty::Concrete(Type::Any),
            locals: locals.clone(),
            body: Vec::new(),
            is_async: true,
            is_generator: false,
            is_module: false,
            line,
            col,
        });

        let mut cfs = FuncState {
            fid,
            is_module: false,
            locals: locals
                .iter()
                .enumerate()
                .map(|(index, local)| (local.name.clone(), index as LocalId))
                .collect(),
            cells: frame_cells.clone(),
            cell_aliases: frame_cells,
            comp_shadowed: HashMap::new(),
            globals,
            loop_depth: 0,
            in_lambda: fs.in_lambda,
            is_async: true,
            is_generator: false,
            genexp_frame: true,
            class_ns: None,
        };
        let accum_ref = HExpr {
            kind: HExprKind::Local(accum),
            ty: Ty::Concrete(result_ty),
            line,
            col,
        };
        let init = HExpr {
            kind: HExprKind::Call {
                callee: Callee::Builtin(match &kind {
                    AsyncCompKind::List(_) => Builtin::List,
                    AsyncCompKind::Set(_) => Builtin::Set,
                    AsyncCompKind::Dict(..) => Builtin::Dict,
                }),
                args: Vec::new(),
            },
            ty: Ty::Concrete(result_ty),
            line,
            col,
        };
        let mut helper_body = vec![HStmt {
            kind: HStmtKind::Assign {
                target: accum,
                value: init,
            },
            line,
            col,
        }];

        let mut saved = Vec::new();
        let mut clauses = Vec::with_capacity(generators.len());
        let first_iterable = HExpr {
            kind: HExprKind::Local(iter_local),
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        };
        clauses.push(self.lower_comp_clause_from(first, &mut cfs, &mut saved, first_iterable));
        for generator in &generators[1..] {
            let clause = self.lower_comp_clause(generator, &mut cfs, &mut saved);
            clauses.push(clause);
        }
        let innermost = match &kind {
            AsyncCompKind::List(element) => {
                let element = self.expr(element, &mut cfs);
                vec![HStmt {
                    kind: HStmtKind::Expr(self.method_call_expr(
                        accum_ref.clone(),
                        "append",
                        vec![element],
                        line,
                        col,
                    )),
                    line,
                    col,
                }]
            }
            AsyncCompKind::Set(element) => {
                let element = self.expr(element, &mut cfs);
                vec![HStmt {
                    kind: HStmtKind::Expr(self.method_call_expr(
                        accum_ref.clone(),
                        "add",
                        vec![element],
                        line,
                        col,
                    )),
                    line,
                    col,
                }]
            }
            AsyncCompKind::Dict(key, value) => {
                // `dict.__setitem__` evaluates the value first, so the key is
                // staged in a hidden local to preserve Python's key-before-
                // value evaluation order.
                let key = self.expr(key, &mut cfs);
                let key_local = self.comp_local(&mut cfs);
                let stored_key = HExpr {
                    kind: HExprKind::Local(key_local),
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                };
                let value = self.expr(value, &mut cfs);
                vec![
                    HStmt {
                        kind: HStmtKind::Assign {
                            target: key_local,
                            value: key,
                        },
                        line,
                        col,
                    },
                    HStmt {
                        kind: HStmtKind::SetSubscript {
                            object: accum_ref.clone(),
                            index: stored_key,
                            value,
                        },
                        line,
                        col,
                    },
                ]
            }
        };
        self.restore_comp_bindings(&mut cfs, saved);

        let loops = self.comp_body(clauses, innermost, &mut cfs);
        helper_body.extend(loops);
        helper_body.push(HStmt {
            kind: HStmtKind::Return(Some(accum_ref)),
            line,
            col,
        });
        self.functions[fid as usize].body = helper_body;

        let mut args = Vec::with_capacity(captures.len() + 1);
        args.push(outer_iterable);
        args.extend(captures);
        let call = HExpr {
            kind: HExprKind::Call {
                callee: Callee::Function(self.fid_base + fid),
                args,
            },
            ty: self.infer.fresh(),
            line,
            col,
        };
        HExpr {
            kind: HExprKind::Await {
                value: Box::new(call),
            },
            // The synthetic coroutine always returns this collection type;
            // the normal `await` rule (dynamic result) does not apply.
            ty: Ty::Concrete(result_ty),
            line,
            col,
        }
    }

    /// Build the loop nest of a comprehension: one `for` per clause (outermost
    /// first), its filters in source order, and `innermost` executed where all
    /// clauses have bound.
    ///
    /// Asynchronous clauses become [`Lowerer::async_iteration`] loops, so this
    /// builder drives generator expressions and the eager comprehensions
    /// alike.
    fn comp_body(
        &mut self,
        clauses: Vec<HCompClause>,
        innermost: Vec<HStmt>,
        fs: &mut FuncState,
    ) -> Vec<HStmt> {
        let mut body = innermost;
        for clause in clauses.into_iter().rev() {
            for condition in clause.conditions.into_iter().rev() {
                let (line, col) = (condition.line, condition.col);
                body = vec![HStmt {
                    kind: HStmtKind::If {
                        cond: condition,
                        then_body: body,
                        else_body: Vec::new(),
                    },
                    line,
                    col,
                }];
            }
            let (line, col) = (clause.iterable.line, clause.iterable.col);
            if clause.is_async {
                // The awaited item binds the clause target before the filters
                // and the inner levels run.
                let (item, item_ref) = self.async_temp(fs, "$aitem", line, col);
                let mut bound = Vec::with_capacity(body.len() + 1);
                match clause.target {
                    HCompTarget::Name(local) => bound.push(HStmt {
                        kind: HStmtKind::Assign {
                            target: local,
                            value: item_ref,
                        },
                        line,
                        col,
                    }),
                    HCompTarget::Unpack(targets) => bound.push(HStmt {
                        kind: HStmtKind::Unpack {
                            targets,
                            value: item_ref,
                        },
                        line,
                        col,
                    }),
                }
                bound.extend(body);
                body =
                    self.async_iteration(clause.iterable, item, Vec::new(), bound, line, col, fs);
                continue;
            }
            let (var, bound) = match clause.target {
                HCompTarget::Name(local) => (local, body),
                HCompTarget::Unpack(targets) => {
                    // The loop variable receives the whole item and the target
                    // unpacks it, as `for a, b in ...` does.
                    let temp = self.comp_local(fs);
                    let mut bound = Vec::with_capacity(body.len() + 1);
                    bound.push(HStmt {
                        kind: HStmtKind::Unpack {
                            targets,
                            value: HExpr {
                                kind: HExprKind::Local(temp),
                                ty: Ty::Concrete(Type::Any),
                                line,
                                col,
                            },
                        },
                        line,
                        col,
                    });
                    bound.extend(body);
                    (temp, bound)
                }
            };
            body = vec![HStmt {
                kind: HStmtKind::ForEach {
                    var,
                    iterable: clause.iterable,
                    body: bound,
                    else_body: Vec::new(),
                },
                line,
                col,
            }];
        }
        body
    }

    /// Build the statements of one asynchronous iteration over `iterable`:
    ///
    /// ```text
    /// $aiter = <iterable>.__aiter__()
    /// while True:
    ///     try:
    ///         <item> = await $aiter.__anext__()
    ///     except StopAsyncIteration:
    ///         <on_exhausted>
    ///         break
    ///     <body>
    /// ```
    ///
    /// `__aiter__()` is evaluated exactly once and each step awaits
    /// `__anext__()`, storing the value in `item` before `body` runs. The
    /// `on_exhausted` statements run only on normal exhaustion, right before
    /// the loop exits. The loop is a plain `while` over awaits, so `break`,
    /// `continue` and exceptions in `body` behave exactly like a synchronous
    /// `for` body.
    #[allow(clippy::too_many_arguments)]
    fn async_iteration(
        &mut self,
        iterable: HExpr,
        item: LocalId,
        on_exhausted: Vec<HStmt>,
        body: Vec<HStmt>,
        line: u32,
        col: u32,
        fs: &mut FuncState,
    ) -> Vec<HStmt> {
        let (aiter, aiter_ref) = self.async_temp(fs, "$aiter", line, col);
        let iter_call = self.method_call_expr(iterable, "__aiter__", Vec::new(), line, col);
        let bind_aiter = HStmt {
            kind: HStmtKind::Assign {
                target: aiter,
                value: iter_call,
            },
            line,
            col,
        };
        let next_call = self.method_call_expr(aiter_ref, "__anext__", Vec::new(), line, col);
        let awaited = HExpr {
            kind: HExprKind::Await {
                value: Box::new(next_call),
            },
            ty: self.infer.fresh(),
            line,
            col,
        };
        let fetch = HStmt {
            kind: HStmtKind::Assign {
                target: item,
                value: awaited,
            },
            line,
            col,
        };
        let mut stop_body = on_exhausted;
        stop_body.push(HStmt {
            kind: HStmtKind::Break,
            line,
            col,
        });
        let try_stmt = HStmt {
            kind: HStmtKind::Try {
                body: vec![fetch],
                handlers: vec![HHandler {
                    exc: Some(ExcKind::StopAsyncIteration),
                    binding: None,
                    binding_cell: false,
                    body: stop_body,
                }],
                else_body: Vec::new(),
                finally_body: Vec::new(),
            },
            line,
            col,
        };
        let mut loop_body = Vec::with_capacity(body.len() + 1);
        loop_body.push(try_stmt);
        loop_body.extend(body);
        let cond = HExpr {
            kind: HExprKind::Const(Const::Bool(true)),
            ty: Ty::Concrete(Type::Bool),
            line,
            col,
        };
        vec![
            bind_aiter,
            HStmt {
                kind: HStmtKind::While {
                    cond,
                    body: loop_body,
                    else_body: Vec::new(),
                },
                line,
                col,
            },
        ]
    }

    /// Free names a comprehension frame reads from the enclosing scope, in
    /// first-use order.
    ///
    /// Returns the capture names, the already-lowered value passed for each
    /// one, whether that value is a closure cell, and the names the frame
    /// reads directly from the module global table. The outermost iterable is
    /// evaluated in the enclosing frame, so names it reads nowhere else are
    /// not captures. `elements` are the expressions evaluated in the innermost
    /// frame (`elt`, or `key`/`value`).
    fn comp_captures(
        &mut self,
        generators: &[ast::Comprehension],
        elements: &[&ast::Expr],
        fs: &FuncState,
        line: u32,
        col: u32,
    ) -> (Vec<String>, Vec<HExpr>, Vec<bool>, HashSet<String>) {
        let mut free = Vec::new();
        let mut seen = HashSet::new();
        let mut bound = HashSet::new();
        for (index, generator) in generators.iter().enumerate() {
            if index > 0 {
                walk_free_expr(&generator.iter, &bound, &mut free, &mut seen);
            }
            // Attribute/subscript receivers of a target are evaluated per item,
            // inside the comprehension frame.
            walk_free_target(&generator.target, &bound, &mut free, &mut seen);
            collect_comp_target_names(&generator.target, &mut bound);
            for condition in &generator.ifs {
                walk_free_expr(condition, &bound, &mut free, &mut seen);
            }
        }
        for element in elements {
            walk_free_expr(element, &bound, &mut free, &mut seen);
        }
        // Template literals are opaque placeholder names to `walk_free_expr`;
        // recover the loads inside their interpolations. The ranges come from
        // the same expressions, so capture order stays source order.
        let mut template_ranges = Vec::new();
        for generator in generators {
            template_ranges.push(generator.iter.range());
            for condition in &generator.ifs {
                template_ranges.push(condition.range());
            }
        }
        for element in elements {
            template_ranges.push(element.range());
        }
        for range in template_ranges {
            for name in self.template_free_names_in(range, &bound) {
                if seen.insert(name.clone()) {
                    free.push(name);
                }
            }
        }

        let mut capture_names = Vec::new();
        let mut captures = Vec::new();
        let mut capture_cells = Vec::new();
        let mut globals = HashSet::new();
        for name in free {
            if fs.is_global(&name) {
                globals.insert(name);
            } else if let Some(local) = fs.lookup(&name) {
                capture_names.push(name);
                capture_cells.push(fs.is_cell(local));
                // A cell capture passes the cell itself, which the frame's
                // parameter aliases; a plain capture passes the value.
                captures.push(HExpr {
                    kind: HExprKind::Local(local),
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                });
            } else if let Some(value) = self
                .class_attr_scopes
                .last()
                .and_then(|scope| scope.get(&name))
                .cloned()
            {
                // A class body has no frame of its own: the attribute value is
                // evaluated where the comprehension is created.
                capture_names.push(name);
                capture_cells.push(false);
                captures.push(value);
            }
        }
        (capture_names, captures, capture_cells, globals)
    }

    /// Restore the bindings shadowed by a comprehension. Reversed so that a
    /// name bound more than once inside one target ends at its outer binding.
    fn restore_comp_bindings(&self, fs: &mut FuncState, saved: Vec<(String, Option<LocalId>)>) {
        for (name, previous) in saved.into_iter().rev() {
            if let Some(count) = fs.comp_shadowed.get_mut(&name) {
                *count = count.saturating_sub(1);
            }
            if fs.comp_shadowed.get(&name) == Some(&0) {
                fs.comp_shadowed.remove(&name);
            }
            match previous {
                Some(local) => {
                    fs.locals.insert(name, local);
                }
                None => {
                    fs.locals.remove(&name);
                }
            }
        }
    }

    /// Lower a `yield value` statement.
    ///
    /// The statement becomes [`HStmtKind::Yield`]; the generator fiber is
    /// suspended by the runtime and resumed with `None` (plain resume). A
    /// `yield` in an expression position (assignment, argument, ...) becomes
    /// [`HExprKind::Yield`] instead, which evaluates to the sent value.
    fn lower_yield_stmt(
        &mut self,
        y: &ast::ExprYield,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        if fs.is_module {
            self.error_at_code(y.range, codes::CONTROL, "yield outside a function");
            return None;
        }
        if fs.in_lambda {
            self.error_at_code(
                y.range,
                codes::UNSUPPORTED,
                "yield is not supported inside a lambda",
            );
            return None;
        }
        if !fs.is_generator {
            self.error_at_code(
                y.range,
                codes::UNSUPPORTED,
                "yield is only supported inside a generator function body",
            );
            return None;
        }
        let value = y.value.as_ref().map(|value| self.expr(value, fs));
        Some(HStmt {
            kind: HStmtKind::Yield { value },
            line,
            col,
        })
    }

    fn lower_for(
        &mut self,
        s: &ast::StmtFor,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        self.lower_for_parts(&s.target, &s.iter, &s.body, &s.orelse, fs, line, col)
    }

    /// Resolve the target of a `for`/`async for` statement: a plain name
    /// already bound in this scope, or a sequence of unpacking targets. Any
    /// other target form is diagnosed here.
    fn resolve_loop_target(
        &mut self,
        target: &ast::Expr,
        fs: &mut FuncState,
    ) -> Option<LoopTarget> {
        let unpack = matches!(target, ast::Expr::Tuple(_) | ast::Expr::List(_));
        let name = match target {
            ast::Expr::Name(n) => match fs.lookup(n.id.as_str()) {
                Some(var) => Some(var),
                None => {
                    self.error_name(
                        n.range,
                        n.id.as_str(),
                        fs,
                        format!("for loop variable '{}' is not defined", n.id),
                    );
                    return None;
                }
            },
            _ => None,
        };
        if !unpack && name.is_none() {
            self.error_at(
                target.range(),
                "for loop target must be a single name in Phase 2.0",
            );
            return None;
        }
        let unpack = if unpack {
            Some(self.unpack_targets(target, fs)?)
        } else {
            None
        };
        Some(LoopTarget { name, unpack })
    }

    /// Lower `for target in iterable:` (and its optional `else:`) to
    /// [`HStmtKind::ForRange`] or [`HStmtKind::ForEach`].
    #[allow(clippy::too_many_arguments)]
    fn lower_for_parts(
        &mut self,
        target: &ast::Expr,
        iter: &ast::Expr,
        loop_body: &[ast::Stmt],
        orelse: &[ast::Stmt],
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        let target_expr = target;
        let LoopTarget {
            name: var,
            unpack: unpack_targets,
        } = self.resolve_loop_target(target, fs)?;

        // `range(...)` keeps its dedicated `ForRange` form; every other
        // iterable is lowered to the runtime iterator form.
        let range_call = match iter {
            ast::Expr::Call(call) => match &*call.func {
                ast::Expr::Name(n) if n.id.as_str() == "range" => Some(call),
                _ => None,
            },
            _ => None,
        };

        let Some(call) = range_call else {
            let iterable = self.expr(iter, fs);
            let (var, cell_target, mut lowered_body) = match (var, unpack_targets) {
                (Some(user_var), _) => {
                    // A cell target is bound through a hidden slot: the cell
                    // itself must stay in the user local.
                    let (var, cell_assign) = if fs.is_cell(user_var) {
                        let (temp, temp_expr) = self.for_temp(fs, target_expr.range());
                        (temp, Some((user_var, temp_expr)))
                    } else {
                        (user_var, None)
                    };
                    fs.loop_depth += 1;
                    let body = self.lower_block(loop_body, fs);
                    fs.loop_depth -= 1;
                    (var, cell_assign, body)
                }
                (None, Some(targets)) => {
                    // `for a, b in xs` desugars to a loop over a hidden local
                    // followed by a straight unpack of that local.
                    let (var, value) = self.unpack_temp(fs, target_expr);
                    let (tline, tcol) = self.span(target_expr.range());
                    fs.loop_depth += 1;
                    let mut body = Vec::with_capacity(loop_body.len() + 1);
                    body.push(HStmt {
                        kind: HStmtKind::Unpack { targets, value },
                        line: tline,
                        col: tcol,
                    });
                    body.extend(self.lower_block(loop_body, fs));
                    fs.loop_depth -= 1;
                    (var, None, body)
                }
                (None, None) => return None,
            };
            let mut body = self.loop_cell_inits(fs, loop_body, Some(target_expr), line, col);
            if let Some((target, value)) = cell_target {
                body.push(HStmt {
                    kind: HStmtKind::CellAssign { target, value },
                    line,
                    col,
                });
            }
            body.append(&mut lowered_body);
            // `else` runs after normal exhaustion only; it is outside the
            // loop, so `break`/`continue` in it bind to an enclosing loop.
            let else_body = self.lower_block(orelse, fs);
            return Some(HStmt {
                kind: HStmtKind::ForEach {
                    var,
                    iterable,
                    body,
                    else_body,
                },
                line,
                col,
            });
        };

        if unpack_targets.is_some() {
            self.error_at(
                target.range(),
                "range() yields a single value and cannot be unpacked",
            );
            return None;
        }
        let var = var.expect("name target validated above");
        let (var, cell_target) = if fs.is_cell(var) {
            let (temp, temp_expr) = self.for_temp(fs, target_expr.range());
            (temp, Some((var, temp_expr)))
        } else {
            (var, None)
        };

        if fs.lookup("range").is_some() {
            self.error_at_code(
                call.range,
                codes::DECL,
                "the builtin range() is shadowed by a variable in this scope",
            );
            return None;
        }
        if !call.keywords.is_empty() {
            self.error_at(call.range, "keyword arguments are not supported in Phase 1");
            return None;
        }
        if call.args.is_empty() || call.args.len() > 3 {
            self.error_at_code(
                call.range,
                codes::CALL,
                format!(
                    "range() takes 1 to 3 positional arguments ({} given)",
                    call.args.len()
                ),
            );
            return None;
        }
        let mut args = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            args.push(self.expr(arg, fs));
        }
        let (start, stop, step) = match args.len() {
            1 => {
                let stop = args.pop().expect("one range argument");
                let start = HExpr {
                    kind: HExprKind::Const(Const::Int(0)),
                    ty: Ty::Concrete(Type::Int),
                    line,
                    col,
                };
                (start, stop, None)
            }
            2 => {
                let step: Option<HExpr> = None;
                let stop = args.pop().expect("range stop");
                let start = args.pop().expect("range start");
                (start, stop, step)
            }
            _ => {
                let step = args.pop();
                let stop = args.pop().expect("range stop");
                let start = args.pop().expect("range start");
                (start, stop, step)
            }
        };

        fs.loop_depth += 1;
        let mut lowered_body = self.lower_block(loop_body, fs);
        fs.loop_depth -= 1;
        let mut body = self.loop_cell_inits(fs, loop_body, Some(target_expr), line, col);
        if let Some((target, value)) = cell_target {
            body.push(HStmt {
                kind: HStmtKind::CellAssign { target, value },
                line,
                col,
            });
        }
        body.append(&mut lowered_body);
        // As for `ForEach`, `else` is outside the loop scope.
        let else_body = self.lower_block(orelse, fs);
        Some(HStmt {
            kind: HStmtKind::ForRange {
                var,
                start,
                stop,
                step,
                body,
                else_body,
            },
            line,
            col,
        })
    }

    /// Whether a name denotes a user-defined or imported exception class
    /// (handled through the documented RuntimeError fallback).
    fn is_user_exception_name(&self, name: &str) -> bool {
        self.class_names.contains(name) || self.imported_user_globals.contains_key(name)
    }

    /// Lowers `raise`, `raise X(...)`, `raise ... from cause`.
    ///
    /// Built-in exception kinds keep their tag-based lowering. A user or
    /// imported exception class has no runtime tag, so it lowers to
    /// `RuntimeError` with message `"<Name>: <message>"`; class identity is
    /// lost, so handlers are approximate. The `from` cause is kept.
    fn lower_raise(
        &mut self,
        s: &ast::StmtRaise,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        if s.cause.is_some() && s.exc.is_none() {
            self.error_at_code(
                s.range,
                codes::CONTROL,
                "raise ... from ... requires an exception",
            );
            return None;
        }

        let (exc, msg) = match &s.exc {
            None => (None, None),
            Some(exc_expr) => match &**exc_expr {
                ast::Expr::Name(n) => match ExcKind::from_name(n.id.as_str()) {
                    Some(kind) => (Some(kind), None),
                    None if self.is_user_exception_name(n.id.as_str()) => (
                        Some(ExcKind::RuntimeError),
                        Some(self.user_exception_message(n.id.as_str(), None, line, col)),
                    ),
                    None => {
                        self.error_at_code(
                            n.range,
                            codes::NAME,
                            format!("unknown exception type '{}'", n.id),
                        );
                        return None;
                    }
                },
                // `raise module.Class`: a class imported from another user
                // module. It has no runtime tag, so the documented
                // `RuntimeError` fallback carries the dotted name.
                ast::Expr::Attribute(_) => {
                    let Some(name) = dotted_expr_name(exc_expr) else {
                        self.error_at(
                            exc_expr.range(),
                            "raise only supports exception names and classes",
                        );
                        return None;
                    };
                    match ExcKind::from_name(&name) {
                        Some(kind) => (Some(kind), None),
                        None => (
                            Some(ExcKind::RuntimeError),
                            Some(self.user_exception_message(&name, None, line, col)),
                        ),
                    }
                }
                ast::Expr::Call(call) => {
                    if !call.keywords.is_empty() {
                        self.error_at(call.range, "keyword arguments are not supported in Phase 1");
                        return None;
                    }
                    if call.args.len() != 1 {
                        self.error_at_code(
                            call.range,
                            codes::CALL,
                            "raise with a message requires exactly one argument",
                        );
                        return None;
                    }
                    let message = self.expr(&call.args[0], fs);
                    match &*call.func {
                        ast::Expr::Name(n) => match ExcKind::from_name(n.id.as_str()) {
                            Some(kind) => (Some(kind), Some(message)),
                            None if self.is_user_exception_name(n.id.as_str()) => (
                                Some(ExcKind::RuntimeError),
                                Some(self.user_exception_message(
                                    n.id.as_str(),
                                    Some(message),
                                    line,
                                    col,
                                )),
                            ),
                            None => {
                                self.error_at_code(
                                    n.range,
                                    codes::NAME,
                                    format!("unknown exception type '{}'", n.id),
                                );
                                return None;
                            }
                        },
                        ast::Expr::Attribute(_) => {
                            let Some(name) = dotted_expr_name(&call.func) else {
                                self.error_at(
                                    call.func.range(),
                                    "raise only supports exception names and classes",
                                );
                                return None;
                            };
                            match ExcKind::from_name(&name) {
                                Some(kind) => (Some(kind), Some(message)),
                                None => (
                                    Some(ExcKind::RuntimeError),
                                    Some(self.user_exception_message(
                                        &name,
                                        Some(message),
                                        line,
                                        col,
                                    )),
                                ),
                            }
                        }
                        _ => {
                            self.error_at(
                                call.func.range(),
                                "raise only supports exception names and classes",
                            );
                            return None;
                        }
                    }
                }
                other => {
                    self.error_at(
                        other.range(),
                        "raise only supports exception names and classes",
                    );
                    return None;
                }
            },
        };
        // Python evaluates the raised exception (and its message) before the
        // explicit cause, so the cause is lowered last.
        let cause = s.cause.as_ref().map(|c| self.expr(c, fs));
        Some(HStmt {
            kind: HStmtKind::Raise { exc, msg, cause },
            line,
            col,
        })
    }

    /// Builds the documented fallback message of `raise <user class>(message)`:
    /// `"<ClassName>: <message>"`, with the message rendered by `str()`.
    fn user_exception_message(
        &mut self,
        name: &str,
        message: Option<HExpr>,
        line: u32,
        col: u32,
    ) -> HExpr {
        let ty = Ty::Concrete(Type::Str);
        let Some(message) = message else {
            return HExpr {
                kind: HExprKind::Const(Const::Str(name.to_string())),
                ty,
                line,
                col,
            };
        };
        let prefix = HExpr {
            kind: HExprKind::Const(Const::Str(format!("{name}: "))),
            ty,
            line,
            col,
        };
        let rendered = HExpr {
            kind: HExprKind::Call {
                callee: Callee::Builtin(Builtin::Str),
                args: vec![message],
            },
            ty,
            line,
            col,
        };
        HExpr {
            kind: HExprKind::Bin {
                op: BinOp::Add,
                lhs: Box::new(prefix),
                rhs: Box::new(rendered),
            },
            ty,
            line,
            col,
        }
    }

    /// Resolves the tags an `except` handler matches. Builtin names keep
    /// their tag; user/imported classes fall back to [`ExcKind::RuntimeError`]
    /// (like `raise`), so their handlers also catch other user-class raises.
    /// Returns `None` after reporting an unsupported handler expression.
    fn handler_exception_kinds(&mut self, type_expr: &ast::Expr) -> Option<Vec<ExcKind>> {
        match type_expr {
            ast::Expr::Name(n) => Some(vec![Self::approximate_exception_kind(n.id.as_str())]),
            ast::Expr::Attribute(_) => {
                if dotted_expr_name(type_expr).is_some() {
                    Some(vec![ExcKind::RuntimeError])
                } else {
                    self.error_at(
                        type_expr.range(),
                        "only simple exception names are supported",
                    );
                    None
                }
            }
            ast::Expr::Tuple(tuple) => {
                if tuple.elts.is_empty() {
                    self.error_at_code(
                        tuple.range,
                        codes::CONTROL,
                        "empty exception tuple in except handler",
                    );
                    return None;
                }
                let mut kinds = Vec::with_capacity(tuple.elts.len());
                for elt in &tuple.elts {
                    match elt {
                        ast::Expr::Name(n) => {
                            kinds.push(Self::approximate_exception_kind(n.id.as_str()))
                        }
                        ast::Expr::Attribute(_) if dotted_expr_name(elt).is_some() => {
                            kinds.push(ExcKind::RuntimeError);
                        }
                        _ => {
                            self.error_at(
                                elt.range(),
                                "only simple exception names are supported in exception tuples",
                            );
                            return None;
                        }
                    }
                }
                Some(kinds)
            }
            other => {
                self.error_at(other.range(), "only simple exception names are supported");
                None
            }
        }
    }

    /// Tag matched by a bare exception name in a handler.
    fn approximate_exception_kind(name: &str) -> ExcKind {
        ExcKind::from_name(name).unwrap_or(ExcKind::RuntimeError)
    }

    /// Lowers `try` with tag-based handlers; see
    /// [`Self::handler_exception_kinds`] for the user-class approximation.
    fn lower_try(
        &mut self,
        s: &ast::StmtTry,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        if s.handlers.is_empty() {
            if !s.orelse.is_empty() {
                self.error_at_code(
                    s.range,
                    codes::CONTROL,
                    "try with else requires at least one except handler",
                );
                return None;
            }
            if s.finalbody.is_empty() {
                self.error_at_code(
                    s.range,
                    codes::CONTROL,
                    "try requires at least one except handler",
                );
                return None;
            }
        }

        let body = self.lower_block(&s.body, fs);
        let mut handlers = Vec::new();
        for (idx, handler) in s.handlers.iter().enumerate() {
            let ast::ExceptHandler::ExceptHandler(h) = handler;
            let is_bare = h.type_.is_none();
            let mut kinds: Vec<ExcKind> = Vec::new();
            match &h.type_ {
                None => {
                    if idx + 1 != s.handlers.len() {
                        self.error_at_code(
                            h.range,
                            codes::CONTROL,
                            "bare except must be the last handler",
                        );
                    }
                }
                Some(type_expr) => {
                    let Some(parsed) = self.handler_exception_kinds(type_expr) else {
                        continue;
                    };
                    kinds = parsed;
                }
            };
            let binding = match &h.name {
                Some(name) => match fs.lookup(name.as_str()) {
                    Some(id) => Some(id),
                    None => {
                        self.error_name(
                            h.range,
                            name.as_str(),
                            fs,
                            format!("unknown handler binding '{}'", name),
                        );
                        None
                    }
                },
                None => None,
            };
            let binding_cell = binding.is_some_and(|local| fs.is_cell(local));
            // Lower the handler body once; tuple handlers share it.
            let handler_body = self.lower_block(&h.body, fs);
            if is_bare {
                handlers.push(HHandler {
                    exc: None,
                    binding,
                    binding_cell,
                    body: handler_body,
                });
            } else {
                for kind in kinds {
                    handlers.push(HHandler {
                        exc: Some(kind),
                        binding,
                        binding_cell,
                        body: handler_body.clone(),
                    });
                }
            }
        }

        let else_body = self.lower_block(&s.orelse, fs);
        let finally_body = self.lower_block(&s.finalbody, fs);

        Some(HStmt {
            kind: HStmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            },
            line,
            col,
        })
    }

    /// Lower `with a as x, b as y:` into nested `try` blocks.
    fn lower_with(
        &mut self,
        s: &ast::StmtWith,
        fs: &mut FuncState,
        _line: u32,
        _col: u32,
    ) -> Option<HStmt> {
        self.lower_with_items(&s.items, &s.body, s.range, fs)
    }

    /// Lower `with a as x, b as y:`.
    ///
    /// Every item expands to
    ///
    /// ```text
    /// $with = <context expression>
    /// $wexc = None
    /// [x =] $with.__enter__()
    /// try:
    ///     <body>
    /// except BaseException as $wexc:
    ///     if not $with.__exit__(type($wexc), $wexc, None):
    ///         raise
    /// finally:
    ///     if $wexc is None:
    ///         $with.__exit__(None, None, None)
    /// ```
    ///
    /// The entry runs before the `try`, so a failing `__enter__` never calls
    /// `__exit__`. The `finally` arm covers normal completion, `break`,
    /// `continue` and `return`, so exactly one `__exit__` call happens on
    /// every path. A truthy result suppresses the body exception; a falsy
    /// result re-raises the original exception through a bare `raise`. An
    /// exception raised by `__exit__` itself replaces the in-flight one, and
    /// the `$wexc` guard then keeps the `finally` arm from calling it twice.
    /// Hydro has no traceback objects, so the third argument is `None`.
    /// Managers stack inside out.
    fn lower_with_items(
        &mut self,
        items: &[ast::WithItem],
        with_body: &[ast::Stmt],
        range: TextRange,
        fs: &mut FuncState,
    ) -> Option<HStmt> {
        if items.is_empty() {
            self.error_at_code(
                range,
                codes::CONTROL,
                "with requires at least one context item",
            );
            return None;
        }

        // Build the nested blocks from the inside out: the first item ends up
        // outermost, so the last entered manager is exited first.
        let mut body = self.lower_block(with_body, fs);
        for item in items.iter().rev() {
            let (line, col) = self.span(item.context_expr.range());
            let value = self.expr(&item.context_expr, fs);
            let (temp, temp_ref) = self.with_temp(fs, "$with", item.context_expr.range());
            let (exc, exc_ref) = self.with_temp(fs, "$wexc", item.context_expr.range());

            let mut prelude = Vec::new();
            prelude.push(HStmt {
                kind: HStmtKind::Assign {
                    target: temp,
                    value,
                },
                line,
                col,
            });
            prelude.push(HStmt {
                kind: HStmtKind::Assign {
                    target: exc,
                    value: self.none_expr(line, col),
                },
                line,
                col,
            });
            // `with EXPR:` calls `__enter__()` even without an `as` target;
            // the result is bound to the target when one is given.
            let enter = self.method_call_expr(temp_ref, "__enter__", Vec::new(), line, col);
            if let Some(stmt) =
                self.with_target_stmt(item.optional_vars.as_deref(), enter, fs, line, col)
            {
                prelude.push(stmt);
            }

            // `except BaseException as $wexc: if not __exit__(...): raise`
            let exc_type = HExpr {
                kind: HExprKind::Call {
                    callee: Callee::Builtin(Builtin::Type),
                    args: vec![exc_ref.clone()],
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
            let exit_exc_none = self.none_expr(line, col);
            let exit_recv = HExpr {
                kind: HExprKind::Local(temp),
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            };
            let exit_with_exc = self.method_call_expr(
                exit_recv,
                "__exit__",
                vec![exc_type, exc_ref.clone(), exit_exc_none],
                line,
                col,
            );
            let suppress_check = HStmt {
                kind: HStmtKind::If {
                    cond: self.not_expr(exit_with_exc, line, col),
                    then_body: vec![HStmt {
                        kind: HStmtKind::Raise {
                            exc: None,
                            msg: None,
                            cause: None,
                        },
                        line,
                        col,
                    }],
                    else_body: Vec::new(),
                },
                line,
                col,
            };

            // `finally: if $wexc is None: __exit__(None, None, None)`
            let exc_is_none = HExpr {
                kind: HExprKind::Bin {
                    op: BinOp::Is,
                    lhs: Box::new(exc_ref),
                    rhs: Box::new(self.none_expr(line, col)),
                },
                ty: Ty::Concrete(Type::Bool),
                line,
                col,
            };
            let exit_norm_recv = HExpr {
                kind: HExprKind::Local(temp),
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            };
            let exit_norm_a1 = self.none_expr(line, col);
            let exit_norm_a2 = self.none_expr(line, col);
            let exit_norm_a3 = self.none_expr(line, col);
            let exit_normally = self.method_call_expr(
                exit_norm_recv,
                "__exit__",
                vec![exit_norm_a1, exit_norm_a2, exit_norm_a3],
                line,
                col,
            );
            let normal_guard = HStmt {
                kind: HStmtKind::If {
                    cond: exc_is_none,
                    then_body: vec![HStmt {
                        kind: HStmtKind::Expr(exit_normally),
                        line,
                        col,
                    }],
                    else_body: Vec::new(),
                },
                line,
                col,
            };

            // The entry runs outside this item's `try`; the inner body stays
            // inside it, so an exception there reaches this item's handler.
            let mut wrapped = prelude;
            wrapped.push(HStmt {
                kind: HStmtKind::Try {
                    body,
                    handlers: vec![HHandler {
                        exc: Some(ExcKind::BaseException),
                        binding: Some(exc),
                        binding_cell: false,
                        body: vec![suppress_check],
                    }],
                    else_body: Vec::new(),
                    finally_body: vec![normal_guard],
                },
                line,
                col,
            });
            body = wrapped;
        }

        // The prelude is at least the two hidden assignments, so the sequence
        // always needs the block wrapper.
        let (line, col) = self.span(range);
        Some(if_true_block(body, line, col))
    }

    /// Statement binding the `as` target of a `with`/`async with` item to its
    /// already-lowered entry value. Without a target the value is discarded,
    /// but it is still evaluated (the entry call must happen).
    fn with_target_stmt(
        &mut self,
        optional_vars: Option<&ast::Expr>,
        value: HExpr,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        match optional_vars {
            None => Some(HStmt {
                kind: HStmtKind::Expr(value),
                line,
                col,
            }),
            Some(ast::Expr::Name(name)) => match fs.lookup(name.id.as_str()) {
                Some(local) => Some(HStmt {
                    kind: if fs.is_cell(local) {
                        HStmtKind::CellAssign {
                            target: local,
                            value,
                        }
                    } else {
                        HStmtKind::Assign {
                            target: local,
                            value,
                        }
                    },
                    line,
                    col,
                }),
                None => {
                    self.error_name(
                        name.range,
                        name.id.as_str(),
                        fs,
                        format!("assignment to unknown name '{}'", name.id),
                    );
                    None
                }
            },
            Some(other) => {
                self.error_at(
                    other.range(),
                    "with ... as only supports a simple name target",
                );
                None
            }
        }
    }

    /// Lower `async for target in iterable:` (and its optional `else:`).
    ///
    /// `__aiter__()` runs once and every step awaits `__anext__()`, catching
    /// `StopAsyncIteration` as the normal end of iteration. The `else` clause
    /// runs only after normal exhaustion, never after `break`, `return` or an
    /// exception; it stays outside the synthetic loop so `break`/`continue`
    /// inside it bind to an enclosing loop.
    fn lower_async_for(
        &mut self,
        s: &ast::StmtAsyncFor,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        if !fs.is_async {
            self.error_at_code(
                s.range,
                codes::CONTROL,
                "'async for' is only allowed inside async functions",
            );
            return None;
        }
        let LoopTarget {
            name,
            unpack: unpack_targets,
        } = self.resolve_loop_target(&s.target, fs)?;
        let iterable = self.expr(&s.iter, fs);

        // A plain-name target receives the awaited value directly; cell and
        // unpack targets need a hidden item slot first.
        let (item, item_ref) = match name {
            Some(local) if !fs.is_cell(local) => (
                local,
                self.local_read(fs, local, Ty::Concrete(Type::Any), line, col),
            ),
            _ => self.async_temp(fs, "$aitem", line, col),
        };

        fs.loop_depth += 1;
        let loop_body = self.lower_block(&s.body, fs);
        fs.loop_depth -= 1;

        // Per-iteration cells are re-created before the target binds, exactly
        // like `ForEach`; the target then stores into the fresh cell.
        let mut after_item = self.loop_cell_inits(fs, &s.body, Some(s.target.as_ref()), line, col);
        match unpack_targets {
            Some(targets) => after_item.push(HStmt {
                kind: HStmtKind::Unpack {
                    targets,
                    value: item_ref,
                },
                line,
                col,
            }),
            None => {
                let local = name.expect("loop target validated above");
                if fs.is_cell(local) {
                    after_item.push(HStmt {
                        kind: HStmtKind::CellAssign {
                            target: local,
                            value: item_ref,
                        },
                        line,
                        col,
                    });
                }
            }
        }
        after_item.extend(loop_body);

        // `else` runs outside the loop, so a `break` there binds to an
        // enclosing loop; `$aloop` records normal exhaustion.
        let else_body = self.lower_block(&s.orelse, fs);
        let (on_exhausted, exhausted) = if else_body.is_empty() {
            (Vec::new(), None)
        } else {
            let (flag, flag_ref) = self.async_temp(fs, "$aloop", line, col);
            let set_flag = HStmt {
                kind: HStmtKind::Assign {
                    target: flag,
                    value: HExpr {
                        kind: HExprKind::Const(Const::Bool(true)),
                        ty: Ty::Concrete(Type::Bool),
                        line,
                        col,
                    },
                },
                line,
                col,
            };
            (vec![set_flag], Some((flag, flag_ref)))
        };

        let mut stmts =
            self.async_iteration(iterable, item, on_exhausted, after_item, line, col, fs);
        let Some((flag, flag_ref)) = exhausted else {
            // The HIR has no block statement: the always-true `if` keeps the
            // desugaring (iterator binding + loop) a single statement.
            return Some(if_true_block(stmts, line, col));
        };
        let init_flag = HStmt {
            kind: HStmtKind::Assign {
                target: flag,
                value: HExpr {
                    kind: HExprKind::Const(Const::Bool(false)),
                    ty: Ty::Concrete(Type::Bool),
                    line,
                    col,
                },
            },
            line,
            col,
        };
        stmts.insert(0, init_flag);
        stmts.push(HStmt {
            kind: HStmtKind::If {
                cond: flag_ref,
                then_body: else_body,
                else_body: Vec::new(),
            },
            line,
            col,
        });
        Some(if_true_block(stmts, line, col))
    }

    /// Lower `async with a as x, b as y:`.
    ///
    /// Every item expands to
    ///
    /// ```text
    /// $with = <context expression>
    /// $aexc = None
    /// [x =] await $with.__aenter__()
    /// try:
    ///     <body>
    /// except BaseException as $aexc:
    ///     if not await $with.__aexit__(type($aexc), $aexc, None):
    ///         raise
    /// finally:
    ///     if $aexc is None:
    ///         await $with.__aexit__(None, None, None)
    /// ```
    ///
    /// The entry runs before the `try`, so a failing `__aenter__` never calls
    /// `__aexit__`. The `finally` arm covers normal completion, `break`,
    /// `continue` and `return`, so exactly one `__aexit__` call happens on
    /// every path. A truthy result suppresses the body exception. Managers
    /// stack inside out. Hydro has no traceback objects, so `__aexit__` receives
    /// `None` as its third argument.
    fn lower_async_with(
        &mut self,
        s: &ast::StmtAsyncWith,
        fs: &mut FuncState,
        _line: u32,
        _col: u32,
    ) -> Option<HStmt> {
        if !fs.is_async {
            self.error_at_code(
                s.range,
                codes::CONTROL,
                "'async with' is only allowed inside async functions",
            );
            return None;
        }
        if s.items.is_empty() {
            self.error_at_code(
                s.range,
                codes::CONTROL,
                "with requires at least one context item",
            );
            return None;
        }

        let mut body = self.lower_block(&s.body, fs);
        for item in s.items.iter().rev() {
            let (line, col) = self.span(item.context_expr.range());
            let value = self.expr(&item.context_expr, fs);
            let (temp, temp_ref) = self.with_temp(fs, "$with", item.context_expr.range());
            let (exc, exc_ref) = self.async_temp(fs, "$aexc", line, col);

            let mut prelude = Vec::new();
            prelude.push(HStmt {
                kind: HStmtKind::Assign {
                    target: temp,
                    value,
                },
                line,
                col,
            });
            prelude.push(HStmt {
                kind: HStmtKind::Assign {
                    target: exc,
                    value: self.none_expr(line, col),
                },
                line,
                col,
            });
            let enter = self.method_call_expr(temp_ref, "__aenter__", Vec::new(), line, col);
            let enter = self.await_expr(enter, line, col);
            if let Some(stmt) =
                self.with_target_stmt(item.optional_vars.as_deref(), enter, fs, line, col)
            {
                prelude.push(stmt);
            }

            // `except BaseException as $aexc: if not await __aexit__(...): raise`
            let exc_type_ty = self.infer.fresh();
            let exc_type = HExpr {
                kind: HExprKind::Call {
                    callee: Callee::Builtin(Builtin::Type),
                    args: vec![exc_ref.clone()],
                },
                ty: exc_type_ty,
                line,
                col,
            };
            let exit_exc_none = self.none_expr(line, col);
            let exit_recv = HExpr {
                kind: HExprKind::Local(temp),
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            };
            let exit_with_exc = self.method_call_expr(
                exit_recv,
                "__aexit__",
                vec![exc_type, exc_ref.clone(), exit_exc_none],
                line,
                col,
            );
            let exit_with_exc = self.await_expr(exit_with_exc, line, col);
            let suppress_check = HStmt {
                kind: HStmtKind::If {
                    cond: self.not_expr(exit_with_exc, line, col),
                    then_body: vec![HStmt {
                        kind: HStmtKind::Raise {
                            exc: None,
                            msg: None,
                            cause: None,
                        },
                        line,
                        col,
                    }],
                    else_body: Vec::new(),
                },
                line,
                col,
            };

            // `finally: if $aexc is None: await __aexit__(None, None, None)`
            let exc_is_none = HExpr {
                kind: HExprKind::Bin {
                    op: BinOp::Is,
                    lhs: Box::new(exc_ref),
                    rhs: Box::new(self.none_expr(line, col)),
                },
                ty: Ty::Concrete(Type::Bool),
                line,
                col,
            };
            let exit_norm_recv = HExpr {
                kind: HExprKind::Local(temp),
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            };
            let exit_norm_a1 = self.none_expr(line, col);
            let exit_norm_a2 = self.none_expr(line, col);
            let exit_norm_a3 = self.none_expr(line, col);
            let exit_normally = self.method_call_expr(
                exit_norm_recv,
                "__aexit__",
                vec![exit_norm_a1, exit_norm_a2, exit_norm_a3],
                line,
                col,
            );
            let exit_normally = self.await_expr(exit_normally, line, col);
            let normal_guard = HStmt {
                kind: HStmtKind::If {
                    cond: exc_is_none,
                    then_body: vec![HStmt {
                        kind: HStmtKind::Expr(exit_normally),
                        line,
                        col,
                    }],
                    else_body: Vec::new(),
                },
                line,
                col,
            };

            // The entry runs outside this item's `try`; the inner body stays
            // inside it, so an exception there reaches this item's handler.
            let mut wrapped = prelude;
            wrapped.push(HStmt {
                kind: HStmtKind::Try {
                    body,
                    handlers: vec![HHandler {
                        exc: Some(ExcKind::BaseException),
                        binding: Some(exc),
                        binding_cell: false,
                        body: vec![suppress_check],
                    }],
                    else_body: Vec::new(),
                    finally_body: vec![normal_guard],
                },
                line,
                col,
            });
            body = wrapped;
        }

        if body.len() == 1 {
            return body.pop();
        }
        let (line, col) = self.span(s.range);
        Some(if_true_block(body, line, col))
    }

    // ------------------------------------------------------------------
    // Expressions
    // ------------------------------------------------------------------

    /// Read a local slot, through its closure cell when the frontend lowered
    /// the binding as a cell.
    fn local_read(&self, fs: &FuncState, local: LocalId, ty: Ty, line: u32, col: u32) -> HExpr {
        let kind = if fs.is_cell(local) {
            HExprKind::CellGet(local)
        } else {
            HExprKind::Local(local)
        };
        HExpr {
            kind,
            ty,
            line,
            col,
        }
    }

    /// `object.method(args...)` with no starred or keyword operands.
    fn method_call_expr(
        &mut self,
        object: HExpr,
        method: &str,
        args: Vec<HExpr>,
        line: u32,
        col: u32,
    ) -> HExpr {
        HExpr {
            kind: HExprKind::MethodCall {
                object: Box::new(object),
                method: method.to_string(),
                args,
                star: None,
                keywords: Vec::new(),
                double_star: None,
            },
            ty: self.infer.fresh(),
            line,
            col,
        }
    }

    /// The runtime `None` sentinel, statically `Any`.
    fn none_expr(&mut self, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::Const(Const::None),
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        }
    }

    /// `not value`.
    fn not_expr(&mut self, value: HExpr, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::Un {
                op: hydro_ir::UnOp::Not,
                operand: Box::new(value),
            },
            ty: self.infer.fresh(),
            line,
            col,
        }
    }

    /// `await value`; the awaited result is only known at runtime.
    fn await_expr(&mut self, value: HExpr, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::Await {
                value: Box::new(value),
            },
            ty: self.infer.fresh(),
            line,
            col,
        }
    }

    fn expr(&mut self, e: &ast::Expr, fs: &mut FuncState) -> HExpr {
        let (line, col) = self.span(e.range());
        match e {
            ast::Expr::Constant(c) => self.const_expr(c, e.range(), line, col),
            ast::Expr::Name(n) => {
                let name = n.id.as_str();
                if let Some(range) = template_placeholder_range(name) {
                    // PEP 750 template literal: the parser emitted a reserved
                    // placeholder `Name`. Resolve it through the unit's
                    // template side table; it is never a variable.
                    return self.template_expr(range, fs, line, col);
                }
                // A dynamic class body keeps its namespace in a runtime dict;
                // every name bound anywhere in the body reads from it first
                // (CPython's `LOAD_NAME` order), before module bindings and
                // builtins.
                if let Some(class_ns) = &fs.class_ns {
                    if class_ns.bindings.contains(name) {
                        let namespace = HExpr {
                            kind: HExprKind::Local(class_ns.ns),
                            ty: Ty::Concrete(Type::Dict),
                            line,
                            col,
                        };
                        let key = HExpr {
                            kind: HExprKind::Const(Const::Str(name.to_string())),
                            ty: Ty::Concrete(Type::Str),
                            line,
                            col,
                        };
                        return HExpr {
                            kind: HExprKind::Subscript {
                                object: Box::new(namespace),
                                index: Box::new(key),
                            },
                            ty: Ty::Concrete(Type::Any),
                            line,
                            col,
                        };
                    }
                }
                // A class body resolves names against its own namespace
                // first (methods and attributes declared earlier), before any
                // enclosing module/function binding or builtin.
                if let Some(attribute) = self
                    .class_attr_scopes
                    .last()
                    .and_then(|scope| scope.get(name))
                    .cloned()
                {
                    attribute
                } else if let Some(local) = fs.lookup(name) {
                    let ty = self.infer.fresh();
                    self.local_read(fs, local, ty, line, col)
                } else if fs.is_global(name) {
                    // `global name`: the value lives in the runtime global
                    // table of this module.
                    self.used_globals.insert(name.to_string());
                    HExpr {
                        kind: HExprKind::GlobalGet {
                            module: self.dunder_name.clone(),
                            name: name.to_string(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else if self.decorated_functions.contains_key(name) {
                    // The name binds the decorator's result, mirrored into the
                    // runtime global table when the module body ran.
                    self.used_globals.insert(name.to_string());
                    HExpr {
                        kind: HExprKind::GlobalGet {
                            module: self.dunder_name.clone(),
                            name: name.to_string(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else if let Some(&id) = self.imported_user_functions.get(name) {
                    HExpr {
                        kind: HExprKind::FunctionRef(id),
                        ty: Ty::Concrete(Type::Function),
                        line,
                        col,
                    }
                } else if let Some((module, exported)) = self.imported_user_globals.get(name) {
                    HExpr {
                        kind: HExprKind::GlobalGet {
                            module: module.clone(),
                            name: exported.clone(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else if let Some((module, function)) = self.imported_functions.get(name).copied()
                {
                    HExpr {
                        kind: HExprKind::ModuleAttr {
                            module: module.name.to_string(),
                            name: function.name.to_string(),
                        },
                        ty: Ty::Concrete(Type::Function),
                        line,
                        col,
                    }
                } else if let Some((module, constant)) = self.imported_constants.get(name).copied()
                {
                    HExpr {
                        kind: HExprKind::ModuleAttr {
                            module: module.name.to_string(),
                            name: constant.name.to_string(),
                        },
                        ty: Ty::Concrete(constant.ty),
                        line,
                        col,
                    }
                } else if let Some((path, member)) = self.imported_extension_members.get(name) {
                    HExpr {
                        kind: HExprKind::ExtensionAttr {
                            path: path.clone(),
                            member: member.clone(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else if let Some(module) = self.module_aliases.get(name).copied() {
                    // A stdlib module is a first-class runtime value; user and
                    // extension modules are still rejected.
                    HExpr {
                        kind: HExprKind::ModuleRef {
                            module: module.name.to_string(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else if self.user_module_aliases.contains_key(name)
                    || self.extension_aliases.contains_key(name)
                {
                    self.error_module_value(n.range, name);
                    self.error_expr(e.range())
                } else if let Some(binding) = self.deferred_imports.get(name) {
                    let module = binding.module.clone();
                    let member = binding.member.clone();
                    self.deferred_import_expr(&module, member.as_deref(), line, col)
                } else if self.session_mode && self.global_names.contains(name) {
                    // A session name shadows builtins, functions and exception
                    // classes: in a REPL `list = ...` or `ValueError = ...`
                    // must win over the builtin of the same name. A local
                    // binding and this fragment's imports were checked above,
                    // so they still take precedence.
                    self.used_globals.insert(name.to_string());
                    HExpr {
                        kind: HExprKind::GlobalGet {
                            module: self.dunder_name.clone(),
                            name: name.to_string(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else if fs.in_lambda
                    && !self.fn_ids.contains_key(name)
                    && builtin_by_name(name).is_none()
                    && ExcKind::from_class_name(name).is_none()
                    && name != "NotImplemented"
                {
                    // A lambda may read builtins and named functions (neither
                    // is captured) but not enclosing locals.
                    self.error_at_code(
                        e.range(),
                        codes::UNSUPPORTED,
                        "closures are not supported yet; lambda can only use its parameters, builtins and named functions",
                    );
                    self.error_expr(e.range())
                } else if let Some(id) = self.fn_ids.get(name) {
                    HExpr {
                        kind: HExprKind::FunctionRef(*id),
                        ty: self.infer.fresh(),
                        line,
                        col,
                    }
                } else if let Some(builtin) = builtin_by_name(name) {
                    HExpr {
                        kind: HExprKind::BuiltinRef(builtin),
                        ty: Ty::Concrete(Type::Function),
                        line,
                        col,
                    }
                } else if self.global_names.contains(name) {
                    // Module-level value read from a function. The module body
                    // mirrors every assignment into the runtime global table,
                    // keyed by this unit's module name.
                    self.used_globals.insert(name.to_string());
                    HExpr {
                        kind: HExprKind::GlobalGet {
                            module: self.dunder_name.clone(),
                            name: name.to_string(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else if name == "NotImplemented" {
                    // Python's singleton constant. It is materialised as the
                    // runtime's reserved immediate; module-level bindings above
                    // shadow it, as in CPython.
                    HExpr {
                        kind: HExprKind::Const(Const::NotImplemented),
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else if let Some(kind) = ExcKind::from_class_name(name) {
                    // A built-in exception class used as a value (a base class
                    // or an expression). Locals, imports, functions and
                    // module-level bindings above shadow the name.
                    HExpr {
                        kind: HExprKind::ExceptionClass(kind),
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else if self.in_default_expr
                    && (self.global_names.contains(name)
                        || self.class_names.contains(name)
                        || self.fn_ids.contains_key(name))
                {
                    self.used_globals.insert(name.to_string());
                    HExpr {
                        kind: HExprKind::GlobalGet {
                            module: self.dunder_name.clone(),
                            name: name.to_string(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else if self.session_mode {
                    // REPL fragments compile against a shared global table:
                    // a name this fragment does not bind may have been bound
                    // by an earlier fragment.
                    self.used_globals.insert(name.to_string());
                    HExpr {
                        kind: HExprKind::GlobalGet {
                            module: self.dunder_name.clone(),
                            name: name.to_string(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }
                } else {
                    if !fs.is_module && self.module_local_names.contains(name) {
                        self.error_at_code(
                            e.range(),
                            codes::NAME,
                            format!(
                                "name '{name}' is not visible inside a function; pass it as a parameter"
                            ),
                        );
                    } else {
                        self.error_name(
                            e.range(),
                            name,
                            fs,
                            format!("name '{name}' is not defined"),
                        );
                    }
                    self.error_expr(e.range())
                }
            }
            ast::Expr::BinOp(b) => {
                let Some(op) = self.bin_op(b.op, b.range) else {
                    return self.error_expr(e.range());
                };
                let lhs = self.expr(&b.left, fs);
                let rhs = self.expr(&b.right, fs);
                // Fold only when both operands are literals and the operation
                // cannot raise; anything fallible stays a runtime operation.
                if let Some(constant) = fold_bin(op, &lhs, &rhs) {
                    let ty = Ty::Concrete(constant.ty());
                    return HExpr {
                        kind: HExprKind::Const(constant),
                        ty,
                        line,
                        col,
                    };
                }
                let ty = self.infer.fresh();
                HExpr {
                    kind: HExprKind::Bin {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    ty,
                    line,
                    col,
                }
            }
            ast::Expr::UnaryOp(u) => {
                let Some(op) = self.un_op(u.op, u.range) else {
                    return self.error_expr(e.range());
                };
                let operand = self.expr(&u.operand, fs);
                if let Some(constant) = fold_un(op, &operand) {
                    let ty = Ty::Concrete(constant.ty());
                    return HExpr {
                        kind: HExprKind::Const(constant),
                        ty,
                        line,
                        col,
                    };
                }
                let ty = self.infer.fresh();
                HExpr {
                    kind: HExprKind::Un {
                        op,
                        operand: Box::new(operand),
                    },
                    ty,
                    line,
                    col,
                }
            }
            ast::Expr::BoolOp(b) => {
                let op = match b.op {
                    ast::BoolOp::And => hydro_ir::BoolOp::And,
                    ast::BoolOp::Or => hydro_ir::BoolOp::Or,
                };
                let mut values = Vec::with_capacity(b.values.len());
                for value in &b.values {
                    values.push(self.expr(value, fs));
                }
                if values.len() < 2 {
                    self.error_at_code(b.range, codes::GENERIC, "malformed boolean expression");
                    return self.error_expr(e.range());
                }
                let mut iter = values.into_iter();
                let mut acc = iter.next().expect("at least two boolean operands");
                for rhs in iter {
                    let ty = self.infer.fresh();
                    acc = HExpr {
                        kind: HExprKind::BoolOp {
                            op,
                            lhs: Box::new(acc),
                            rhs: Box::new(rhs),
                        },
                        ty,
                        line,
                        col,
                    };
                }
                acc
            }
            ast::Expr::Compare(c) => {
                if c.ops.is_empty() || c.ops.len() != c.comparators.len() {
                    self.error_at_code(c.range, codes::GENERIC, "malformed comparison");
                    return self.error_expr(e.range());
                }
                if c.ops.len() == 1 {
                    let op = cmp_op(c.ops[0]);
                    let lhs = self.expr(&c.left, fs);
                    let rhs = self.expr(&c.comparators[0], fs);
                    if let Some(constant) = fold_bin(op, &lhs, &rhs) {
                        let ty = Ty::Concrete(constant.ty());
                        return HExpr {
                            kind: HExprKind::Const(constant),
                            ty,
                            line,
                            col,
                        };
                    }
                    let ty = self.infer.fresh();
                    return HExpr {
                        kind: HExprKind::Bin {
                            op,
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                        ty,
                        line,
                        col,
                    };
                }
                // `a < b < c` is `a < b and b < c`: each middle operand is
                // evaluated exactly once (bound to a hidden `$cmpN` local and
                // read back) and every comparison after the first only runs
                // while the previous one was true.
                let mut lhs = Some(self.expr(&c.left, fs));
                let mut terms = Vec::with_capacity(c.ops.len());
                let last = c.ops.len() - 1;
                for (index, (op, comparator)) in c.ops.iter().zip(c.comparators.iter()).enumerate()
                {
                    let (term_line, term_col) = self.span(comparator.range());
                    let value = self.expr(comparator, fs);
                    let left = lhs.take().expect("every comparison has a left operand");
                    let (rhs, next_lhs) = if index < last {
                        let (target, bound) = self.cmp_temp(fs, comparator.range());
                        (
                            HExpr {
                                kind: HExprKind::NamedExpr {
                                    target,
                                    value: Box::new(value),
                                },
                                ty: self.infer.fresh(),
                                line: term_line,
                                col: term_col,
                            },
                            Some(bound),
                        )
                    } else {
                        (value, None)
                    };
                    terms.push(HExpr {
                        kind: HExprKind::Bin {
                            op: cmp_op(*op),
                            lhs: Box::new(left),
                            rhs: Box::new(rhs),
                        },
                        ty: self.infer.fresh(),
                        line: term_line,
                        col: term_col,
                    });
                    lhs = next_lhs;
                }
                let mut terms = terms.into_iter();
                let mut acc = terms.next().expect("at least one comparison term");
                for rhs in terms {
                    acc = HExpr {
                        kind: HExprKind::BoolOp {
                            op: hydro_ir::BoolOp::And,
                            lhs: Box::new(acc),
                            rhs: Box::new(rhs),
                        },
                        ty: self.infer.fresh(),
                        line,
                        col,
                    };
                }
                acc
            }
            ast::Expr::Call(c) => self.call_expr(c, fs, line, col),
            ast::Expr::Attribute(a) => {
                if let Some(module) = self.resolve_module_chain(&a.value, fs) {
                    let member = a.attr.as_str();
                    if let Some(expr) = self.module_spec_attr_expr(module, member, line, col) {
                        return expr;
                    }
                    if module.name == "os" && member == "path" {
                        self.error_module_value(a.range, "os.path");
                        return self.error_expr(e.range());
                    }
                    self.error_unknown_member(a.range, module, member);
                    return self.error_expr(e.range());
                }
                if let Some(module) = self.resolve_user_module(&a.value, fs) {
                    let member = a.attr.as_str();
                    let (member_line, member_col) = self.span(a.range);
                    return match module.exports.get(member) {
                        Some(UserExport::Function(id)) => HExpr {
                            kind: HExprKind::FunctionRef(*id),
                            ty: Ty::Concrete(Type::Function),
                            line,
                            col,
                        },
                        Some(UserExport::Global) => HExpr {
                            kind: HExprKind::GlobalGet {
                                module: module.name.clone(),
                                name: member.to_string(),
                            },
                            ty: Ty::Concrete(Type::Any),
                            line: member_line,
                            col: member_col,
                        },
                        None => {
                            self.error_at_code(
                                a.range,
                                codes::ATTRIBUTE,
                                format!(
                                    "module '{}' does not export a function named '{member}'",
                                    module.name
                                ),
                            );
                            self.error_expr(e.range())
                        }
                    };
                }
                if let Some(path) = self.resolve_extension_alias(&a.value, fs) {
                    return HExpr {
                        kind: HExprKind::ExtensionAttr {
                            path,
                            member: a.attr.to_string(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    };
                }
                if let Some((module, member)) = self.resolve_deferred_import(e, fs) {
                    return self.deferred_import_expr(&module, member.as_deref(), line, col);
                }
                let object = self.expr(&a.value, fs);
                HExpr {
                    kind: HExprKind::GetAttr {
                        object: Box::new(object),
                        name: a.attr.to_string(),
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            ast::Expr::Subscript(s) => {
                if let ast::Expr::Slice(sl) = &*s.slice {
                    let object = self.expr(&s.value, fs);
                    let start = sl.lower.as_ref().map(|e| Box::new(self.expr(e, fs)));
                    let stop = sl.upper.as_ref().map(|e| Box::new(self.expr(e, fs)));
                    let step = sl.step.as_ref().map(|e| Box::new(self.expr(e, fs)));
                    let ty = self.infer.fresh();
                    return HExpr {
                        kind: HExprKind::Slice {
                            object: Box::new(object),
                            start,
                            stop,
                            step,
                        },
                        ty,
                        line,
                        col,
                    };
                }
                let object = self.expr(&s.value, fs);
                let index = self.expr(&s.slice, fs);
                let ty = self.infer.fresh();
                HExpr {
                    kind: HExprKind::Subscript {
                        object: Box::new(object),
                        index: Box::new(index),
                    },
                    ty,
                    line,
                    col,
                }
            }
            ast::Expr::Slice(s) => {
                self.error_at_code(
                    s.range,
                    codes::INDEX,
                    "slice syntax is only supported inside a subscript",
                );
                self.error_expr(e.range())
            }
            ast::Expr::Tuple(t) => {
                let mut elems = Vec::with_capacity(t.elts.len());
                for elem in &t.elts {
                    elems.push(self.expr(elem, fs));
                }
                HExpr {
                    kind: HExprKind::Tuple(elems),
                    ty: Ty::Concrete(Type::Tuple),
                    line,
                    col,
                }
            }
            ast::Expr::List(l) => {
                if l.elts.iter().any(|e| matches!(e, ast::Expr::Starred(_))) {
                    return self.list_with_star(&l.elts, fs, line, col);
                }
                let mut elems = Vec::with_capacity(l.elts.len());
                for elem in &l.elts {
                    elems.push(self.expr(elem, fs));
                }
                let ty = self.infer.fresh();
                HExpr {
                    kind: HExprKind::List(elems),
                    ty,
                    line,
                    col,
                }
            }
            ast::Expr::Set(s) => {
                let mut elems = Vec::with_capacity(s.elts.len());
                for elem in &s.elts {
                    elems.push(self.expr(elem, fs));
                }
                HExpr {
                    kind: HExprKind::Set(elems),
                    ty: Ty::Concrete(Type::Set),
                    line,
                    col,
                }
            }
            ast::Expr::Dict(d) => {
                if d.keys.iter().any(|key| key.is_none()) {
                    return self.dict_with_unpack(d, fs, line, col);
                }
                let mut pairs = Vec::with_capacity(d.keys.len());
                for (key, value) in d.keys.iter().zip(d.values.iter()) {
                    let Some(key) = key else {
                        // `**` was already routed to `dict_with_unpack`.
                        continue;
                    };
                    let key = self.expr(key, fs);
                    let value = self.expr(value, fs);
                    pairs.push((key, value));
                }
                let ty = self.infer.fresh();
                HExpr {
                    kind: HExprKind::Dict(pairs),
                    ty,
                    line,
                    col,
                }
            }
            ast::Expr::ListComp(c) => {
                if c.generators.iter().any(|generator| generator.is_async) {
                    if !fs.is_async {
                        self.error_at_code(
                            c.range,
                            codes::CONTROL,
                            "asynchronous comprehension outside of an asynchronous function",
                        );
                    }
                    return self.lower_async_comprehension(
                        &c.generators,
                        AsyncCompKind::List(&c.elt),
                        c.range,
                        fs,
                    );
                }
                let (clauses, saved) = self.lower_comp_clauses(&c.generators, fs);
                let element = self.expr(&c.elt, fs);
                self.restore_comp_bindings(fs, saved);
                HExpr {
                    kind: HExprKind::ListComp {
                        element: Box::new(element),
                        clauses,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            ast::Expr::SetComp(c) => {
                if c.generators.iter().any(|generator| generator.is_async) {
                    if !fs.is_async {
                        self.error_at_code(
                            c.range,
                            codes::CONTROL,
                            "asynchronous comprehension outside of an asynchronous function",
                        );
                    }
                    return self.lower_async_comprehension(
                        &c.generators,
                        AsyncCompKind::Set(&c.elt),
                        c.range,
                        fs,
                    );
                }
                let (clauses, saved) = self.lower_comp_clauses(&c.generators, fs);
                let element = self.expr(&c.elt, fs);
                self.restore_comp_bindings(fs, saved);
                HExpr {
                    kind: HExprKind::SetComp {
                        element: Box::new(element),
                        clauses,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            ast::Expr::DictComp(c) => {
                if c.generators.iter().any(|generator| generator.is_async) {
                    if !fs.is_async {
                        self.error_at_code(
                            c.range,
                            codes::CONTROL,
                            "asynchronous comprehension outside of an asynchronous function",
                        );
                    }
                    return self.lower_async_comprehension(
                        &c.generators,
                        AsyncCompKind::Dict(&c.key, &c.value),
                        c.range,
                        fs,
                    );
                }
                let (clauses, saved) = self.lower_comp_clauses(&c.generators, fs);
                let key = self.expr(&c.key, fs);
                let value = self.expr(&c.value, fs);
                self.restore_comp_bindings(fs, saved);
                HExpr {
                    kind: HExprKind::DictComp {
                        key: Box::new(key),
                        value: Box::new(value),
                        clauses,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            ast::Expr::GeneratorExp(c) => self.lower_generator_exp(c, fs, line, col),
            ast::Expr::Lambda(l) => self.lower_lambda(l, line, col),
            ast::Expr::IfExp(i) => {
                // `i.orelse` is only evaluated when the test is falsy; the IR
                // node keeps that laziness for codegen.
                let test = self.expr(&i.test, fs);
                let then = self.expr(&i.body, fs);
                let orelse = self.expr(&i.orelse, fs);
                HExpr {
                    kind: HExprKind::IfExp {
                        test: Box::new(test),
                        then: Box::new(then),
                        orelse: Box::new(orelse),
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            ast::Expr::NamedExpr(n) => {
                let ast::Expr::Name(target) = &*n.target else {
                    self.error_at_code(
                        n.range,
                        codes::GENERIC,
                        "assignment expression target must be a name",
                    );
                    return self.error_expr(e.range());
                };
                let name = target.id.as_str();
                if fs.comp_shadowed.get(name).is_some_and(|count| *count > 0) {
                    self.error_at_code(
                        n.range,
                        codes::UNSUPPORTED,
                        format!(
                            "assignment expression cannot rebind comprehension iteration variable '{name}'"
                        ),
                    );
                    return self.error_expr(e.range());
                }
                if fs.is_global(name) {
                    self.error_at_code(
                        n.range,
                        codes::UNSUPPORTED,
                        "assignment expression to a name declared global is not supported",
                    );
                    return self.error_expr(e.range());
                }
                if fs.genexp_frame {
                    // CPython binds the target in the enclosing function; a
                    // generator frame cannot write there, so binding locally
                    // would silently hide the assignment.
                    self.error_at_code(
                        n.range,
                        codes::UNSUPPORTED,
                        "assignment expressions inside a generator expression are not supported",
                    );
                    return self.error_expr(e.range());
                }
                // The value is evaluated before the binding takes effect, so
                // `x := x + 1` reads the existing local; for a fresh name the
                // value reads the outer binding (Python treats the name as
                // local for the whole function and raises `UnboundLocalError`).
                let value = self.expr(&n.value, fs);
                let local = self.walrus_local(fs, name);
                let kind = if fs.is_cell(local) {
                    HExprKind::CellSet {
                        target: local,
                        value: Box::new(value),
                    }
                } else {
                    HExprKind::NamedExpr {
                        target: local,
                        value: Box::new(value),
                    }
                };
                HExpr {
                    kind,
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            ast::Expr::Await(a) => {
                if !fs.is_async {
                    self.error_at_code(
                        a.range,
                        codes::CONTROL,
                        "await is only allowed inside async functions",
                    );
                }
                self.await_frames.insert(fs.fid);
                HExpr {
                    kind: HExprKind::Await {
                        value: Box::new(self.expr(&a.value, fs)),
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            ast::Expr::Yield(y) => {
                if fs.is_module {
                    self.error_at_code(y.range, codes::CONTROL, "yield outside a function");
                } else if fs.in_lambda {
                    self.error_at_code(
                        y.range,
                        codes::UNSUPPORTED,
                        "yield is not supported inside a lambda",
                    );
                } else if !fs.is_generator {
                    self.error_at_code(
                        y.range,
                        codes::UNSUPPORTED,
                        "yield is only supported inside a generator function body",
                    );
                }
                HExpr {
                    kind: HExprKind::Yield {
                        value: y.value.as_ref().map(|value| Box::new(self.expr(value, fs))),
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            ast::Expr::YieldFrom(y) => {
                if fs.is_module {
                    self.error_at_code(y.range, codes::CONTROL, "yield outside a function");
                } else if fs.in_lambda {
                    self.error_at_code(
                        y.range,
                        codes::UNSUPPORTED,
                        "yield is not supported inside a lambda",
                    );
                } else if !fs.is_generator {
                    self.error_at_code(
                        y.range,
                        codes::UNSUPPORTED,
                        "yield is only supported inside a generator function body",
                    );
                }
                HExpr {
                    kind: HExprKind::YieldFrom {
                        value: Box::new(self.expr(&y.value, fs)),
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
            ast::Expr::JoinedStr(s) => self.joined_str_expr(s, fs, line, col),
            ast::Expr::FormattedValue(_) => {
                // Standalone formatted values only occur inside a JoinedStr,
                // where they are handled by `joined_str_expr`.
                self.error_at_code(
                    e.range(),
                    codes::GENERIC,
                    "unexpected formatted value outside an f-string",
                );
                self.error_expr(e.range())
            }
            ast::Expr::Starred(s) => {
                self.error_at(s.range, "starred expressions are not supported in Phase 1");
                self.error_expr(e.range())
            }
        }
    }

    /// Lower a `lambda` expression into a synthetic function appended to the
    /// unit. The body becomes a single `return`; only the parameters are in
    /// scope, so any other name is reported as an unsupported closure.
    fn lower_lambda(&mut self, l: &ast::ExprLambda, line: u32, col: u32) -> HExpr {
        let mut params = Vec::new();
        let mut locals = Vec::new();
        let mut seen = HashSet::new();
        let mut rejected = false;

        for arg in &l.args.posonlyargs {
            if arg.default.is_some() {
                self.error_at(
                    arg.def.range,
                    "lambda default parameter values are not supported",
                );
                rejected = true;
            }
            self.add_param(
                &arg.def,
                None,
                None,
                ParamKind::PosOnly,
                &mut params,
                &mut locals,
                &mut seen,
            );
        }
        for arg in &l.args.args {
            if arg.default.is_some() {
                self.error_at(
                    arg.def.range,
                    "lambda default parameter values are not supported",
                );
                rejected = true;
            }
            self.add_param(
                &arg.def,
                None,
                None,
                ParamKind::Positional,
                &mut params,
                &mut locals,
                &mut seen,
            );
        }
        if let Some(vararg) = &l.args.vararg {
            self.error_at(
                vararg.range,
                "lambda varargs (*args) parameters are not supported",
            );
            rejected = true;
        }
        if let Some(first) = l.args.kwonlyargs.first() {
            self.error_at(
                first.def.range,
                "lambda keyword-only parameters are not supported",
            );
            rejected = true;
        }
        if let Some(kwarg) = &l.args.kwarg {
            self.error_at(kwarg.range, "lambda **kwargs parameters are not supported");
            rejected = true;
        }
        if rejected {
            return self.error_expr(l.range);
        }

        let fid = self.functions.len() as FunctionId;
        let mut local_map = HashMap::with_capacity(params.len());
        for param in &params {
            local_map.insert(param.name.clone(), param.local);
        }
        self.functions.push(HFunction {
            name: "<lambda>".to_string(),
            params,
            capture_count: 0,
            ret_ty: Ty::Concrete(Type::Any),
            locals,
            body: Vec::new(),
            is_async: false,
            is_generator: false,
            is_module: false,
            line,
            col,
        });

        let mut fs = FuncState {
            fid,
            is_module: false,
            locals: local_map,
            cells: HashSet::new(),
            cell_aliases: HashSet::new(),
            comp_shadowed: HashMap::new(),
            globals: HashSet::new(),
            loop_depth: 0,
            in_lambda: true,
            is_async: false,
            is_generator: false,
            genexp_frame: false,
            class_ns: None,
        };
        let value = self.expr(&l.body, &mut fs);
        let (body_line, body_col) = self.span(l.body.range());
        self.functions[fid as usize].body = vec![HStmt {
            kind: HStmtKind::Return(Some(value)),
            line: body_line,
            col: body_col,
        }];

        HExpr {
            kind: HExprKind::FunctionRef(self.fid_base + fid),
            ty: self.infer.fresh(),
            line,
            col,
        }
    }

    fn joined_str_expr(
        &mut self,
        s: &ast::ExprJoinedStr,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        let mut parts: Vec<HExpr> = Vec::with_capacity(s.values.len());
        for value in &s.values {
            match value {
                ast::Expr::Constant(c) => match &c.value {
                    ast::Constant::Str(text) => {
                        let (part_line, part_col) = self.span(c.range);
                        parts.push(HExpr {
                            kind: HExprKind::Const(Const::Str(text.clone())),
                            ty: Ty::Concrete(Type::Str),
                            line: part_line,
                            col: part_col,
                        });
                    }
                    // Not emitted by the parser for f-strings; lower it like
                    // any other expression instead of guessing.
                    _ => parts.push(self.expr(value, fs)),
                },
                ast::Expr::FormattedValue(fv) => parts.push(self.formatted_value_expr(fv, fs)),
                other => parts.push(self.expr(other, fs)),
            }
        }

        let mut iter = parts.into_iter();
        let Some(mut acc) = iter.next() else {
            // `f""` has no parts at all.
            return HExpr {
                kind: HExprKind::Const(Const::Str(String::new())),
                ty: Ty::Concrete(Type::Str),
                line,
                col,
            };
        };
        // Left-associative string concatenation, as CPython builds it.
        for rhs in iter {
            acc = HExpr {
                kind: HExprKind::Bin {
                    op: BinOp::Add,
                    lhs: Box::new(acc),
                    rhs: Box::new(rhs),
                },
                ty: Ty::Concrete(Type::Str),
                line,
                col,
            };
        }
        acc
    }

    fn formatted_value_expr(&mut self, fv: &ast::ExprFormattedValue, fs: &mut FuncState) -> HExpr {
        let (line, col) = self.span(fv.range);
        // Lower the interpolation first so unsupported inner expressions keep
        // their own diagnostics.
        let value = self.expr(&fv.value, fs);

        let spec = match &fv.format_spec {
            None => String::new(),
            Some(spec) => match static_format_spec(spec) {
                Ok(text) => text,
                Err(()) => {
                    self.error_at(
                        spec.range(),
                        "dynamic f-string format specs are not supported",
                    );
                    return self.error_expr(fv.range);
                }
            },
        };

        let conversion = match fv.conversion {
            ast::ConversionFlag::None => 0u8,
            ast::ConversionFlag::Str => 1,
            ast::ConversionFlag::Repr => 2,
            ast::ConversionFlag::Ascii => 3,
        };

        // Plain `{value}` and `{value!s}` are exactly `str(value)`.
        if spec.is_empty() && conversion <= 1 {
            return HExpr {
                kind: HExprKind::Call {
                    callee: Callee::Builtin(Builtin::Str),
                    args: vec![value],
                },
                ty: Ty::Concrete(Type::Str),
                line,
                col,
            };
        }

        let spec_expr = HExpr {
            kind: HExprKind::Const(Const::Str(spec)),
            ty: Ty::Concrete(Type::Str),
            line,
            col,
        };
        let conversion_expr = HExpr {
            kind: HExprKind::Const(Const::Int(conversion as i64)),
            ty: Ty::Concrete(Type::Int),
            line,
            col,
        };
        HExpr {
            kind: HExprKind::Call {
                callee: Callee::Builtin(Builtin::Format),
                args: vec![value, spec_expr, conversion_expr],
            },
            ty: Ty::Concrete(Type::Str),
            line,
            col,
        }
    }

    /// Lowers a PEP 750 template literal placeholder to an HIR
    /// [`HExprKind::Template`].
    ///
    /// The parser returns each literal's structure out of band, keyed by the
    /// literal's source range. Every interpolation value and format spec is
    /// lowered in source order, so nested template literals (also placeholders)
    /// resolve recursively. A placeholder without a side-table entry is an
    /// explicit diagnostic: a template is never silently treated as a normal
    /// name.
    fn template_expr(
        &mut self,
        range: TextRange,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        let Some(template) = self
            .templates
            .iter()
            .find(|template| template.range == range)
            .cloned()
        else {
            self.error_at_code(
                range,
                codes::UNSUPPORTED,
                "template string literal could not be resolved; recompile this unit with template support",
            );
            return self.error_expr(range);
        };

        let mut interpolations = Vec::with_capacity(template.interpolations.len());
        for interpolation in &template.interpolations {
            let value = self.expr(&interpolation.value, fs);
            let format_spec = interpolation
                .format_spec
                .as_deref()
                .map(|spec| Box::new(self.expr(spec, fs)));
            let conversion = match interpolation.conversion {
                ast::ConversionFlag::None => 0u8,
                ast::ConversionFlag::Str => 1,
                ast::ConversionFlag::Repr => 2,
                ast::ConversionFlag::Ascii => 3,
            };
            interpolations.push(HTemplateInterpolation {
                value,
                expression: interpolation.expression.clone(),
                conversion,
                format_spec,
                self_documenting: interpolation.self_documenting,
            });
        }

        HExpr {
            kind: HExprKind::Template {
                strings: template
                    .strings
                    .iter()
                    .map(|constant| constant.value.clone())
                    .collect(),
                interpolations,
            },
            // A template is a distinct runtime value, not a `str`.
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        }
    }

    /// Names loaded by template interpolations whose literal lies inside
    /// `range`, in source order.
    ///
    /// Free-variable analysis walks the AST, where a template is an opaque
    /// placeholder `Name`; this recovers the loads hidden inside its
    /// interpolations so closure capture, cell selection and module-global
    /// mirroring stay correct.
    fn template_free_names_in(&self, range: TextRange, shadowed: &HashSet<String>) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for template in &self.templates {
            if template.range.start() < range.start() || template.range.end() > range.end() {
                continue;
            }
            for interpolation in &template.interpolations {
                walk_free_expr(&interpolation.value, shadowed, &mut out, &mut seen);
                if let Some(spec) = &interpolation.format_spec {
                    walk_free_expr(spec, shadowed, &mut out, &mut seen);
                }
            }
        }
        out
    }

    /// Names loaded by any template interpolation of this unit.
    fn template_free_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for template in &self.templates {
            for interpolation in &template.interpolations {
                walk_free_expr(&interpolation.value, &HashSet::new(), &mut out, &mut seen);
                if let Some(spec) = &interpolation.format_spec {
                    walk_free_expr(spec, &HashSet::new(), &mut out, &mut seen);
                }
            }
        }
        out
    }

    /// Whether any template interpolation of this unit loads `name`.
    fn template_loads_name(&self, name: &str) -> bool {
        self.template_free_names()
            .iter()
            .any(|loaded| loaded == name)
    }

    /// Names loaded by templates inside nested `def` bodies of `stmts`.
    ///
    /// Cell selection only cares about templates a nested function evaluates;
    /// a template in the current scope must not force a cell on a local.
    fn nested_function_template_names(&self, stmts: &[ast::Stmt]) -> HashSet<String> {
        let mut ranges = Vec::new();
        collect_nested_function_ranges(stmts, &mut ranges);
        let mut out = HashSet::new();
        for range in ranges {
            out.extend(self.template_free_names_in(range, &HashSet::new()));
        }
        out
    }

    /// Lowers `["a", *xs, "b"]` into left-to-right list concatenations: each
    /// plain run is a list literal and each starred operand a `+` operand
    /// (the runtime concat rejects non-lists with `TypeError`). A leading
    /// `*` starts from an empty list, copying the operand.
    fn list_with_star(
        &mut self,
        elts: &[ast::Expr],
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        let mut result: Option<HExpr> = None;
        let mut pending: Vec<HExpr> = Vec::new();
        for elem in elts {
            if let ast::Expr::Starred(starred) = elem {
                self.flush_list_chunk(&mut result, &mut pending, line, col);
                let value = self.expr(&starred.value, fs);
                let (value_line, value_col) = self.span(starred.range);
                // A display accepts any iterable after `*` (including an
                // iterator or generator expression); list concatenation only
                // accepts lists, so copy the operand through `list(...)`.
                let value = self.expr_to_list(value, fs, value_line, value_col);
                result = Some(self.concat_lists(result, value, value_line, value_col));
                continue;
            }
            pending.push(self.expr(elem, fs));
        }
        self.flush_list_chunk(&mut result, &mut pending, line, col);
        result.unwrap_or_else(|| HExpr {
            kind: HExprKind::List(Vec::new()),
            ty: Ty::Concrete(Type::List),
            line,
            col,
        })
    }

    /// Append the pending plain list elements to `result` as one list literal.
    fn flush_list_chunk(
        &mut self,
        result: &mut Option<HExpr>,
        pending: &mut Vec<HExpr>,
        line: u32,
        col: u32,
    ) {
        if pending.is_empty() {
            return;
        }
        let chunk = HExpr {
            kind: HExprKind::List(std::mem::take(pending)),
            ty: Ty::Concrete(Type::List),
            line,
            col,
        };
        let merged = self.concat_lists(result.take(), chunk, line, col);
        *result = Some(merged);
    }

    /// `left + right` with a list result, starting from an empty list when
    /// `left` is absent.
    fn concat_lists(&mut self, left: Option<HExpr>, right: HExpr, line: u32, col: u32) -> HExpr {
        let left = left.unwrap_or_else(|| HExpr {
            kind: HExprKind::List(Vec::new()),
            ty: Ty::Concrete(Type::List),
            line,
            col,
        });
        HExpr {
            kind: HExprKind::Bin {
                op: BinOp::Add,
                lhs: Box::new(left),
                rhs: Box::new(right),
            },
            ty: Ty::Concrete(Type::List),
            line,
            col,
        }
    }

    /// Lowers `{**h, "k": v}` into left-to-right merges: literal pairs keep
    /// their `Dict` node and each unpacked mapping is merged with the internal
    /// `DictMerge` builtin (insertion order, last-wins). A leading `**` is
    /// merged onto an empty dict, copying the operand.
    fn dict_with_unpack(
        &mut self,
        d: &ast::ExprDict,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        let mut result: Option<HExpr> = None;
        let mut pending: Vec<(HExpr, HExpr)> = Vec::new();
        for (key, value) in d.keys.iter().zip(d.values.iter()) {
            let Some(key) = key else {
                self.flush_dict_chunk(&mut result, &mut pending, line, col);
                let mapping = self.expr(value, fs);
                let (value_line, value_col) = self.span(value.range());
                result = Some(match result.take() {
                    Some(left) => self.merge_dicts(left, mapping, value_line, value_col),
                    None => {
                        let empty = HExpr {
                            kind: HExprKind::Dict(Vec::new()),
                            ty: Ty::Concrete(Type::Dict),
                            line,
                            col,
                        };
                        self.merge_dicts(empty, mapping, value_line, value_col)
                    }
                });
                continue;
            };
            let key = self.expr(key, fs);
            let value = self.expr(value, fs);
            pending.push((key, value));
        }
        self.flush_dict_chunk(&mut result, &mut pending, line, col);
        result.unwrap_or_else(|| HExpr {
            kind: HExprKind::Dict(Vec::new()),
            ty: Ty::Concrete(Type::Dict),
            line,
            col,
        })
    }

    /// Append the pending literal key/value pairs to `result`.
    fn flush_dict_chunk(
        &mut self,
        result: &mut Option<HExpr>,
        pending: &mut Vec<(HExpr, HExpr)>,
        line: u32,
        col: u32,
    ) {
        if pending.is_empty() {
            return;
        }
        let chunk = HExpr {
            kind: HExprKind::Dict(std::mem::take(pending)),
            ty: Ty::Concrete(Type::Dict),
            line,
            col,
        };
        *result = Some(match result.take() {
            Some(left) => self.merge_dicts(left, chunk, line, col),
            None => chunk,
        });
    }

    /// `left | right` as the hidden `DictMerge` builtin call.
    fn merge_dicts(&mut self, left: HExpr, right: HExpr, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::Call {
                callee: Callee::Builtin(Builtin::DictMerge),
                args: vec![left, right],
            },
            ty: Ty::Concrete(Type::Dict),
            line,
            col,
        }
    }

    fn const_expr(
        &mut self,
        c: &ast::ExprConstant,
        range: TextRange,
        line: u32,
        col: u32,
    ) -> HExpr {
        let (constant, ty) = match &c.value {
            ast::Constant::Int(value) => {
                let constant = int_const_from_decimal(value.to_str_radix(10));
                let ty = Ty::Concrete(constant.ty());
                (constant, ty)
            }
            ast::Constant::Float(v) => (Const::Float(*v), Ty::Concrete(Type::Float)),
            ast::Constant::Bool(v) => (Const::Bool(*v), Ty::Concrete(Type::Bool)),
            ast::Constant::Str(v) => (Const::Str(v.clone()), Ty::Concrete(Type::Str)),
            ast::Constant::None => (Const::None, Ty::Concrete(Type::None)),
            ast::Constant::Bytes(value) => (Const::Bytes(value.clone()), Ty::Concrete(Type::Bytes)),
            ast::Constant::Complex { .. } => {
                self.error_at(range, "complex literals are not supported in Phase 1");
                (Const::None, Ty::Concrete(Type::Any))
            }
            ast::Constant::Tuple(_) => {
                self.error_at(range, "tuple literals are not supported in Phase 1");
                (Const::None, Ty::Concrete(Type::Any))
            }
            ast::Constant::Ellipsis => (Const::Ellipsis, Ty::Concrete(Type::None)),
        };
        HExpr {
            kind: HExprKind::Const(constant),
            ty,
            line,
            col,
        }
    }

    /// Lowers the arguments of a native module function.
    ///
    /// Native constructors advertise their parameter names in
    /// [`FunctionSpec::params`]; those calls accept Python's usual keyword
    /// spelling (`Thread(target=f)`) and are reordered to positional
    /// arguments. Missing slots are filled with `None`, which the runtime
    /// treats as "use the default".
    ///
    /// A spec with `dynamic_keywords` (`functools.partial`) instead keeps the
    /// names it does not declare, so the runtime can forward them to the
    /// wrapped callee. User-defined functions and builtins have their own
    /// binding paths below; methods bind keywords at runtime.
    fn module_arguments(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        spec: &'static FunctionSpec,
        line: u32,
        col: u32,
    ) -> Option<LoweredArguments> {
        if call.keywords.is_empty() {
            let mut args = Vec::with_capacity(call.args.len());
            for arg in &call.args {
                args.push(self.expr(arg, fs));
            }
            return Some((args, Vec::new()));
        }
        if spec.dynamic_keywords {
            return self.dynamic_module_arguments(call, fs, spec);
        }
        if spec.params.is_empty() {
            self.error_at_code(
                call.range,
                codes::CALL,
                format!("{}() does not accept keyword arguments", spec.name),
            );
            return None;
        }

        let mut slots: Vec<Option<HExpr>> = vec![None; spec.params.len()];
        for (index, arg) in call.args.iter().enumerate() {
            if index >= slots.len() {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!(
                        "{}() takes at most {} positional argument(s)",
                        spec.name,
                        slots.len()
                    ),
                );
                return None;
            }
            slots[index] = Some(self.expr(arg, fs));
        }
        for keyword in &call.keywords {
            let Some(name) = keyword.arg.as_ref() else {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    "**kwargs arguments are not supported",
                );
                return None;
            };
            let Some(index) = spec.params.iter().position(|param| *param == name.as_str()) else {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    format!(
                        "{}() got an unexpected keyword argument '{}'",
                        spec.name,
                        name.as_str()
                    ),
                );
                return None;
            };
            if slots[index].is_some() {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    format!(
                        "{}() got multiple values for argument '{}'",
                        spec.name,
                        name.as_str()
                    ),
                );
                return None;
            }
            slots[index] = Some(self.expr(&keyword.value, fs));
        }

        // Fill omitted slots with `None`; this keeps the positional order and
        // lets the runtime apply the constructor's defaults. A required
        // leading parameter (e.g. a missing `target`) is diagnosed at runtime.
        Some((
            slots
                .into_iter()
                .map(|slot| {
                    slot.unwrap_or(HExpr {
                        kind: HExprKind::Const(Const::None),
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    })
                })
                .collect(),
            Vec::new(),
        ))
    }

    /// Lowers the arguments of a native function with a dynamic keyword set
    /// (`functools.partial(func, *args, **kwargs)`).
    ///
    /// The declared parameters keep their positional order (they may be passed
    /// by name) and every other keyword name is preserved for the callee. The
    /// runtime rejects names the wrapped function does not declare, so nothing
    /// is silently dropped.
    fn dynamic_module_arguments(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        spec: &'static FunctionSpec,
    ) -> Option<LoweredArguments> {
        let mut fixed: Vec<Option<HExpr>> = vec![None; spec.params.len()];
        let mut positional: Vec<HExpr> = Vec::with_capacity(call.args.len());
        for (index, arg) in call.args.iter().enumerate() {
            let value = self.expr(arg, fs);
            if index < fixed.len() {
                fixed[index] = Some(value);
            } else {
                positional.push(value);
            }
        }
        let mut keywords: Vec<(String, HExpr)> = Vec::new();
        for keyword in &call.keywords {
            let Some(name) = keyword.arg.as_ref() else {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    "**kwargs arguments are not supported",
                );
                return None;
            };
            let value = self.expr(&keyword.value, fs);
            match spec.params.iter().position(|param| *param == name.as_str()) {
                Some(index) => {
                    if fixed[index].is_some() {
                        self.error_at_code(
                            keyword.range,
                            codes::CALL,
                            format!(
                                "{}() got multiple values for argument '{}'",
                                spec.name,
                                name.as_str()
                            ),
                        );
                        return None;
                    }
                    fixed[index] = Some(value);
                }
                None => {
                    if keywords.iter().any(|(bound, _)| bound == name.as_str()) {
                        self.error_at_code(
                            keyword.range,
                            codes::CALL,
                            format!(
                                "{}() got multiple values for keyword argument '{}'",
                                spec.name,
                                name.as_str()
                            ),
                        );
                        return None;
                    }
                    keywords.push((name.to_string(), value));
                }
            }
        }

        let mut args: Vec<HExpr> = Vec::with_capacity(fixed.len() + positional.len());
        for (index, slot) in fixed.into_iter().enumerate() {
            match slot {
                Some(value) => args.push(value),
                None if index < spec.min_args => {
                    self.error_at_code(
                        call.range,
                        codes::CALL,
                        format!(
                            "{}() missing required positional argument '{}'",
                            spec.name, spec.params[index]
                        ),
                    );
                    return None;
                }
                None => args.push(HExpr {
                    kind: HExprKind::Const(Const::None),
                    ty: Ty::Concrete(Type::Any),
                    line: 1,
                    col: 1,
                }),
            }
        }
        args.extend(positional);
        Some((args, keywords))
    }

    /// Builds a `CallKw` node: evaluates the argument operands in source
    /// order and forwards constant keyword names to the runtime binder.
    fn call_kw_expr(
        &mut self,
        callee: HExpr,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HExpr> {
        let mut args = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            if matches!(arg, ast::Expr::Starred(_)) {
                return None;
            }
            args.push(self.expr(arg, fs));
        }
        let mut keywords = Vec::with_capacity(call.keywords.len());
        for keyword in &call.keywords {
            let name = keyword.arg.as_ref()?;
            keywords.push((name.to_string(), self.expr(&keyword.value, fs)));
        }
        Some(HExpr {
            kind: HExprKind::CallKw {
                callee: Box::new(callee),
                args,
                keywords,
            },
            ty: self.infer.fresh(),
            line,
            col,
        })
    }

    fn call_expr(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        // Zero-argument `super()`: materialise the runtime super proxy for
        // the enclosing method. `super().name`, `super().method(...)` and
        // chained access all resolve through the proxy's MRO position.
        if let ast::Expr::Name(name) = &*call.func {
            if name.id.as_str() == "super"
                && call.args.is_empty()
                && call.keywords.is_empty()
                && !self.super_name_is_shadowed(fs)
            {
                return self.lower_super_proxy(fs, call.range, line, col);
            }
        }
        // `super().__init__(...)` keeps the dedicated initializer helper: it
        // binds keyword arguments against the base signature directly.
        if let ast::Expr::Attribute(attribute) = &*call.func {
            if attribute.attr.as_str() == "__init__" {
                if let ast::Expr::Call(super_call) = &*attribute.value {
                    if let ast::Expr::Name(name) = &*super_call.func {
                        if name.id.as_str() == "super"
                            && super_call.args.is_empty()
                            && super_call.keywords.is_empty()
                        {
                            // Zero-argument `super().__init__` is lowered to a
                            // runtime helper that invokes the direct base
                            // initializer with this instance. Other `super()`
                            // forms remain unsupported.
                            let Some(local) = fs.lookup("self") else {
                                self.error_at_code(
                                    call.range(),
                                    codes::NAME,
                                    "super().__init__() requires a method receiver named 'self'",
                                );
                                return HExpr {
                                    kind: HExprKind::Const(Const::None),
                                    ty: Ty::Concrete(Type::None),
                                    line,
                                    col,
                                };
                            };
                            let receiver = HExpr {
                                kind: HExprKind::Local(local),
                                ty: self.infer.fresh(),
                                line,
                                col,
                            };
                            let mut args = Vec::with_capacity(call.args.len());
                            for arg in &call.args {
                                args.push(self.expr(arg, fs));
                            }
                            let mut keywords = Vec::with_capacity(call.keywords.len());
                            for keyword in &call.keywords {
                                let Some(identifier) = &keyword.arg else {
                                    self.error_at_code(
                                        keyword.value.range(),
                                        codes::UNSUPPORTED,
                                        "super().__init__(**kwargs) is not supported",
                                    );
                                    continue;
                                };
                                keywords
                                    .push((identifier.to_string(), self.expr(&keyword.value, fs)));
                            }
                            return HExpr {
                                kind: HExprKind::MethodCall {
                                    object: Box::new(receiver),
                                    method: "$super_init".to_string(),
                                    args,
                                    star: None,
                                    keywords,
                                    double_star: None,
                                },
                                ty: self.infer.fresh(),
                                line,
                                col,
                            };
                        }
                    }
                }
            }
        }
        // A name bound by the current dynamic class body resolves through the
        // class namespace before module bindings and builtins (`LOAD_NAME`).
        // That includes calling a class-body function such as
        // `__add__ = _make_binop(ast.Add())`.
        if let ast::Expr::Name(name) = &*call.func {
            if fs
                .class_ns
                .as_ref()
                .is_some_and(|state| state.bindings.contains(name.id.as_str()))
            {
                if call_has_dynamic_arguments(call) {
                    return self.lower_dynamic_call(call, fs, line, col);
                }
                let callee = self.expr(&call.func, fs);
                if !call.keywords.is_empty() {
                    let Some(result) = self.call_kw_expr(callee, call, fs, line, col) else {
                        return self.error_expr(call.range);
                    };
                    return result;
                }
                let mut args = Vec::with_capacity(call.args.len());
                for arg in &call.args {
                    args.push(self.expr(arg, fs));
                }
                return HExpr {
                    kind: HExprKind::IndirectCall {
                        callee: Box::new(callee),
                        args,
                        star: None,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
        }
        // A starred positional or `**kwargs` operand can only be expanded at
        // runtime. Receiver method calls with a single starred operand keep
        // their dedicated ABI (`hydro_method_star`); the other shapes lower to
        // the general starred-call ABI, or to a deferred call when they
        // cannot be represented (keywords, `**`).
        if call_has_dynamic_arguments(call) {
            // `cls(*args, ...)` inside a classmethod: the runtime class is
            // fixed, so the dynamic constructor binder can still expand the
            // starred operand at compile time.
            if let ast::Expr::Name(name_expr) = &*call.func {
                if let Some((class, MethodKind::Classmethod, cls_name)) =
                    self.current_class_method.clone()
                {
                    if name_expr.id.as_str() == cls_name
                        && fs.lookup(&cls_name).is_some()
                        && !fs.is_module
                    {
                        if let Some((init, signature)) = self.resolve_class(&class) {
                            if signature.has_dynamic_signature() {
                                let Some(bind) = self.class_constructor_dynamic_arguments(
                                    &class, &signature, call, fs, line, col,
                                ) else {
                                    return self.error_expr(call.range);
                                };
                                self.used_globals.insert(class.clone());
                                let (callee_line, callee_col) = self.span(name_expr.range);
                                let class_expr = HExpr {
                                    kind: HExprKind::GlobalGet {
                                        module: self.dunder_name.clone(),
                                        name: class.clone(),
                                    },
                                    ty: Ty::Concrete(Type::Any),
                                    line: callee_line,
                                    col: callee_col,
                                };
                                return HExpr {
                                    kind: HExprKind::ClassCall {
                                        class: Box::new(class_expr),
                                        init,
                                        args: bind.args,
                                        dynamic_kwargs: bind.dynamic_kwargs.map(Box::new),
                                        param_names: bind.param_names,
                                        kwarg_index: bind.kwarg_index,
                                        class_name: class,
                                    },
                                    ty: self.infer.fresh(),
                                    line,
                                    col,
                                };
                            }
                        }
                    }
                }
            }
            // A class declared in the unit can still bind a dynamic
            // constructor at compile time: fixed slots are resolved here and
            // `*`/`**` operands fill the generated `*args`/`**kwargs` slots.
            if let ast::Expr::Name(name_expr) = &*call.func {
                let name = name_expr.id.as_str();
                if self.class_callee_resolvable(name, fs) {
                    if let Some((init, signature)) = self.resolve_class(name) {
                        if signature.has_dynamic_signature() {
                            let Some(bind) = self.class_constructor_dynamic_arguments(
                                name, &signature, call, fs, line, col,
                            ) else {
                                return self.error_expr(call.range);
                            };
                            let Some(class) =
                                self.class_callee_expr(name, name_expr, fs, line, col)
                            else {
                                return self.error_expr(call.range);
                            };
                            return HExpr {
                                kind: HExprKind::ClassCall {
                                    class: Box::new(class),
                                    init,
                                    args: bind.args,
                                    dynamic_kwargs: bind.dynamic_kwargs.map(Box::new),
                                    param_names: bind.param_names,
                                    kwarg_index: bind.kwarg_index,
                                    class_name: name.to_string(),
                                },
                                ty: self.infer.fresh(),
                                line,
                                col,
                            };
                        }
                    }
                }
            }
            // A receiver method call keeps the method ABI, which expands the
            // starred operand and merges the keywords (named and `**`) in the
            // runtime binder. A module/user-module/extension member is not a
            // method: its attribute lowers to a first-class function value and
            // the call uses the general starred-call ABI.
            if let ast::Expr::Attribute(attr) = &*call.func {
                if self.resolve_module_chain(&attr.value, fs).is_none()
                    && self.resolve_user_module(&attr.value, fs).is_none()
                    && self.resolve_extension_alias(&attr.value, fs).is_none()
                    && self.resolve_deferred_import(&call.func, fs).is_none()
                {
                    let object = self.expr(&attr.value, fs);
                    let Some((args, star, keywords, double_star)) = self.call_operands(call, fs)
                    else {
                        return self.error_expr(call.range);
                    };
                    return HExpr {
                        kind: HExprKind::MethodCall {
                            object: Box::new(object),
                            method: attr.attr.to_string(),
                            args,
                            star: star.map(Box::new),
                            keywords,
                            double_star: double_star.map(Box::new),
                        },
                        ty: self.infer.fresh(),
                        line,
                        col,
                    };
                }
            }
            return self.lower_dynamic_call(call, fs, line, col);
        }
        let name_expr = match &*call.func {
            ast::Expr::Name(name_expr) => name_expr,
            ast::Expr::Attribute(attr) => {
                if let Some(module) = self.resolve_module_chain(&attr.value, fs) {
                    return self.module_attribute_call(
                        module,
                        attr.attr.as_str(),
                        call,
                        fs,
                        line,
                        col,
                    );
                }
                if let Some(module) = self.resolve_user_module(&attr.value, fs) {
                    if !call.keywords.is_empty() {
                        return self.lower_deferred_call(call, fs, line, col);
                    }
                    return self.user_module_call(&module, attr.attr.as_str(), call, fs, line, col);
                }
                if let Some(path) = self.resolve_extension_alias(&attr.value, fs) {
                    if !call.keywords.is_empty() {
                        return self.lower_deferred_call(call, fs, line, col);
                    }
                    return self.extension_call(&path, attr.attr.as_str(), call, fs, line, col);
                }
                if let Some((module, member)) = self.resolve_deferred_import(&call.func, fs) {
                    let callee = self.deferred_import_expr(&module, member.as_deref(), line, col);
                    if !call.keywords.is_empty() {
                        let Some(result) = self.call_kw_expr(callee, call, fs, line, col) else {
                            return self.error_expr(call.range);
                        };
                        return result;
                    }
                    let Some(args) = self.plain_call_args(call, fs) else {
                        return self.error_expr(call.range);
                    };
                    return HExpr {
                        kind: HExprKind::IndirectCall {
                            callee: Box::new(callee),
                            args,
                            star: None,
                        },
                        ty: self.infer.fresh(),
                        line,
                        col,
                    };
                }
                let object = self.expr(&attr.value, fs);
                let Some((args, star, keywords, double_star)) = self.call_operands(call, fs) else {
                    return self.error_expr(call.range);
                };
                return HExpr {
                    kind: HExprKind::MethodCall {
                        object: Box::new(object),
                        method: attr.attr.to_string(),
                        args,
                        star: star.map(Box::new),
                        keywords,
                        double_star: double_star.map(Box::new),
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
            other => {
                // Any other callee is a function value: Python evaluates the
                // callee before the arguments. Keyword calls go through the
                // runtime metadata binder.
                let callee = self.expr(other, fs);
                if !call.keywords.is_empty() {
                    let Some(result) = self.call_kw_expr(callee, call, fs, line, col) else {
                        return self.error_expr(call.range);
                    };
                    return result;
                }
                let mut args = Vec::with_capacity(call.args.len());
                for arg in &call.args {
                    args.push(self.expr(arg, fs));
                }
                return HExpr {
                    kind: HExprKind::IndirectCall {
                        callee: Box::new(callee),
                        args,
                        star: None,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
        };
        let name = name_expr.id.as_str();
        // `cls(...)` inside a classmethod constructs the enclosing class. The
        // binding is fixed (Hydro has no dynamic subclassing), so the call can
        // use the same constructor binder as a direct class call.
        if let Some((class, MethodKind::Classmethod, cls_name)) = self.current_class_method.clone()
        {
            if name == cls_name && fs.lookup(name).is_some() && !fs.is_module {
                if let Some((init, signature)) = self.resolve_class(&class) {
                    self.used_globals.insert(class.clone());
                    let (callee_line, callee_col) = self.span(name_expr.range);
                    let class_expr = HExpr {
                        kind: HExprKind::GlobalGet {
                            module: self.dunder_name.clone(),
                            name: class.clone(),
                        },
                        ty: Ty::Concrete(Type::Any),
                        line: callee_line,
                        col: callee_col,
                    };
                    if signature.has_dynamic_signature() {
                        let Some(bind) = self.class_constructor_dynamic_arguments(
                            &class, &signature, call, fs, line, col,
                        ) else {
                            return self.error_expr(call.range);
                        };
                        return HExpr {
                            kind: HExprKind::ClassCall {
                                class: Box::new(class_expr),
                                init,
                                args: bind.args,
                                dynamic_kwargs: bind.dynamic_kwargs.map(Box::new),
                                param_names: bind.param_names,
                                kwarg_index: bind.kwarg_index,
                                class_name: class.clone(),
                            },
                            ty: self.infer.fresh(),
                            line,
                            col,
                        };
                    }
                    let Some(args) =
                        self.class_constructor_arguments(&class, &signature, call, fs, line, col)
                    else {
                        return self.error_expr(call.range);
                    };
                    return HExpr {
                        kind: HExprKind::IndirectCall {
                            callee: Box::new(class_expr),
                            args,
                            star: None,
                        },
                        ty: self.infer.fresh(),
                        line,
                        col,
                    };
                }
            }
        }

        if let Some(local) = fs.lookup(name) {
            // A class of this unit reorders keywords into its `__init__`
            // parameter slots and fills omitted defaults at compile time.
            // Only the module body holds the class value directly; inside a
            // function a local slot with the same name shadows it.
            if fs.is_module {
                if let Some((init, signature)) = self.resolve_class(name) {
                    let (callee_line, callee_col) = self.span(name_expr.range);
                    let class = HExpr {
                        kind: HExprKind::Local(local),
                        ty: Ty::Concrete(Type::Any),
                        line: callee_line,
                        col: callee_col,
                    };
                    if signature.has_dynamic_signature() {
                        let Some(bind) = self.class_constructor_dynamic_arguments(
                            name, &signature, call, fs, line, col,
                        ) else {
                            return self.error_expr(call.range);
                        };
                        return HExpr {
                            kind: HExprKind::ClassCall {
                                class: Box::new(class),
                                init,
                                args: bind.args,
                                dynamic_kwargs: bind.dynamic_kwargs.map(Box::new),
                                param_names: bind.param_names,
                                kwarg_index: bind.kwarg_index,
                                class_name: name.to_string(),
                            },
                            ty: self.infer.fresh(),
                            line,
                            col,
                        };
                    }
                    let Some(args) =
                        self.class_constructor_arguments(name, &signature, call, fs, line, col)
                    else {
                        return self.error_expr(call.range);
                    };
                    return HExpr {
                        kind: HExprKind::IndirectCall {
                            callee: Box::new(class),
                            args,
                            star: None,
                        },
                        ty: self.infer.fresh(),
                        line,
                        col,
                    };
                }
            }
            let (callee_line, callee_col) = self.span(name_expr.range);
            let local_ty = self.functions[fs.fid as usize]
                .locals
                .get(local as usize)
                .map(|info| self.infer.resolve(info.ty))
                .unwrap_or(Ty::Concrete(Type::Any));
            let callable = matches!(
                local_ty,
                Ty::Concrete(Type::Function) | Ty::Concrete(Type::Any)
            );
            if !call.keywords.is_empty() || callable {
                let ty = self.infer.fresh();
                let callee = self.local_read(fs, local, ty, callee_line, callee_col);
                let Some(result) = self.call_kw_expr(callee, call, fs, line, col) else {
                    return self.error_expr(call.range);
                };
                return result;
            }
            let ty = self.infer.fresh();
            let callee = self.local_read(fs, local, ty, callee_line, callee_col);
            let mut args = Vec::with_capacity(call.args.len());
            for arg in &call.args {
                args.push(self.expr(arg, fs));
            }
            return HExpr {
                kind: HExprKind::IndirectCall {
                    callee: Box::new(callee),
                    args,
                    star: None,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if fs.is_global(name) {
            // `global name` in this function: the callee is read from the
            // runtime global table.
            self.used_globals.insert(name.to_string());
            let (callee_line, callee_col) = self.span(name_expr.range);
            let callee = HExpr {
                kind: HExprKind::GlobalGet {
                    module: self.dunder_name.clone(),
                    name: name.to_string(),
                },
                ty: Ty::Concrete(Type::Any),
                line: callee_line,
                col: callee_col,
            };
            if !call.keywords.is_empty() {
                let Some(result) = self.call_kw_expr(callee, call, fs, line, col) else {
                    return self.error_expr(call.range);
                };
                return result;
            }
            let Some(args) = self.plain_call_args(call, fs) else {
                return self.error_expr(call.range);
            };
            return HExpr {
                kind: HExprKind::IndirectCall {
                    callee: Box::new(callee),
                    args,
                    star: None,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if self.decorated_functions.contains_key(name) {
            // The decorator may have replaced the function with any callable,
            // so the call goes through the module global and the runtime call
            // protocol (keywords included).
            self.used_globals.insert(name.to_string());
            let (callee_line, callee_col) = self.span(name_expr.range);
            let callee = HExpr {
                kind: HExprKind::GlobalGet {
                    module: self.dunder_name.clone(),
                    name: name.to_string(),
                },
                ty: Ty::Concrete(Type::Any),
                line: callee_line,
                col: callee_col,
            };
            if call_has_dynamic_arguments(call) {
                return self.lower_dynamic_call(call, fs, line, col);
            }
            if !call.keywords.is_empty() {
                let Some(result) = self.call_kw_expr(callee, call, fs, line, col) else {
                    return self.error_expr(call.range);
                };
                return result;
            }
            let Some(args) = self.plain_call_args(call, fs) else {
                return self.error_expr(call.range);
            };
            return HExpr {
                kind: HExprKind::IndirectCall {
                    callee: Box::new(callee),
                    args,
                    star: None,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if let Some(&id) = self.imported_user_functions.get(name) {
            // Exact positional calls link directly; calls with keywords or
            // omitted defaults go through the runtime metadata binder.
            if call.keywords.is_empty() {
                if let Some(signature) = self.call_signature_for(id) {
                    let param_count = signature
                        .params
                        .iter()
                        .filter(|(_, _, kind)| kind.is_positional() || *kind == ParamKind::KwOnly)
                        .count();
                    if let Some(args) = self.plain_call_args(call, fs) {
                        if args.len() == param_count {
                            return HExpr {
                                kind: HExprKind::Call {
                                    callee: Callee::Function(id),
                                    args,
                                },
                                ty: self.infer.fresh(),
                                line,
                                col,
                            };
                        }
                        // Re-evaluate is impossible; plain_call_args only
                        // lowers expressions. Fall through to the value path
                        // using the already-computed arguments.
                        let (callee_line, callee_col) = self.span(name_expr.range);
                        let callee = HExpr {
                            kind: HExprKind::FunctionRef(id),
                            ty: Ty::Concrete(Type::Function),
                            line: callee_line,
                            col: callee_col,
                        };
                        return HExpr {
                            kind: HExprKind::CallKw {
                                callee: Box::new(callee),
                                args,
                                keywords: Vec::new(),
                            },
                            ty: self.infer.fresh(),
                            line,
                            col,
                        };
                    }
                    return self.error_expr(call.range);
                }
            }
            let Some(args) = self.plain_call_args(call, fs) else {
                return self.error_expr(call.range);
            };
            let mut keywords = Vec::with_capacity(call.keywords.len());
            for keyword in &call.keywords {
                let Some(keyword_name) = keyword.arg.as_ref() else {
                    return self.lower_deferred_call(call, fs, line, col);
                };
                keywords.push((keyword_name.to_string(), self.expr(&keyword.value, fs)));
            }
            let (callee_line, callee_col) = self.span(name_expr.range);
            let callee = HExpr {
                kind: HExprKind::FunctionRef(id),
                ty: Ty::Concrete(Type::Function),
                line: callee_line,
                col: callee_col,
            };
            return HExpr {
                kind: HExprKind::CallKw {
                    callee: Box::new(callee),
                    args,
                    keywords,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if let Some((module, exported)) = self.imported_user_globals.get(name).cloned() {
            let (callee_line, callee_col) = self.span(name_expr.range);
            let callee = HExpr {
                kind: HExprKind::GlobalGet {
                    module: module.clone(),
                    name: exported.clone(),
                },
                ty: Ty::Concrete(Type::Any),
                line: callee_line,
                col: callee_col,
            };
            if call.keywords.is_empty() {
                if let Some(signature) = self.resolve_class(name).map(|(_, signature)| signature) {
                    let params = signature.params.len().saturating_sub(1);
                    if let Some(args) = self.plain_call_args(call, fs) {
                        if args.len() == params {
                            return HExpr {
                                kind: HExprKind::IndirectCall {
                                    callee: Box::new(callee),
                                    args,
                                    star: None,
                                },
                                ty: self.infer.fresh(),
                                line,
                                col,
                            };
                        }
                    }
                }
            }
            let Some(result) = self.call_kw_expr(callee, call, fs, line, col) else {
                return self.error_expr(call.range);
            };
            return result;
        }

        if let Some((module, function)) = self.imported_functions.get(name).copied() {
            let Some((args, keywords)) = self.module_arguments(call, fs, function, line, col)
            else {
                return self.error_expr(call.range);
            };
            return HExpr {
                kind: HExprKind::ModuleCall {
                    module: module.name.to_string(),
                    function: function.name.to_string(),
                    args,
                    keywords,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if let Some((module, constant)) = self.imported_constants.get(name).copied() {
            // A dynamically typed constant member is often a class or a
            // factory exposed by a native module (`from _weakref import ref`).
            // It is callable at runtime, so lower the call through its value
            // instead of rejecting it statically.
            if constant.ty == Type::Any {
                if call_has_dynamic_arguments(call) {
                    return self.lower_dynamic_call(call, fs, line, col);
                }
                let callee = HExpr {
                    kind: HExprKind::ModuleAttr {
                        module: module.name.to_string(),
                        name: constant.name.to_string(),
                    },
                    ty: Ty::Concrete(Type::Any),
                    line,
                    col,
                };
                if !call.keywords.is_empty() {
                    let Some(result) = self.call_kw_expr(callee, call, fs, line, col) else {
                        return self.error_expr(call.range);
                    };
                    return result;
                }
                let Some(args) = self.plain_call_args(call, fs) else {
                    return self.error_expr(call.range);
                };
                return HExpr {
                    kind: HExprKind::IndirectCall {
                        callee: Box::new(callee),
                        args,
                        star: None,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
            self.error_at_code(
                call.range,
                codes::CALL,
                format!("'{}' object is not callable", constant.ty.name()),
            );
            return self.error_expr(call.range);
        }

        if let Some((path, member)) = self.imported_extension_members.get(name).cloned() {
            // The extension ABI has no keyword binding.
            if !call.keywords.is_empty() {
                return self.lower_deferred_call(call, fs, line, col);
            }
            return self.extension_call(&path, &member, call, fs, line, col);
        }

        if self.module_aliases.contains_key(name)
            || self.user_module_aliases.contains_key(name)
            || self.extension_aliases.contains_key(name)
        {
            self.error_module_value(name_expr.range, name);
            return self.error_expr(call.range);
        }

        if let Some(binding) = self.deferred_imports.get(name).cloned() {
            let (callee_line, callee_col) = self.span(name_expr.range);
            let callee = self.deferred_import_expr(
                &binding.module,
                binding.member.as_deref(),
                callee_line,
                callee_col,
            );
            if !call.keywords.is_empty() {
                let Some(result) = self.call_kw_expr(callee, call, fs, line, col) else {
                    return self.error_expr(call.range);
                };
                return result;
            }
            let Some(args) = self.plain_call_args(call, fs) else {
                return self.error_expr(call.range);
            };
            return HExpr {
                kind: HExprKind::IndirectCall {
                    callee: Box::new(callee),
                    args,
                    star: None,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        // A lambda may reference builtins and named functions (both resolve
        // without a capture) but still cannot capture enclosing locals, whose
        // slots are simply absent from its scope.
        if fs.in_lambda
            && !self.fn_ids.contains_key(name)
            && builtin_by_name(name).is_none()
            && ExcKind::from_class_name(name).is_none()
        {
            self.error_at_code(
                call.range,
                codes::UNSUPPORTED,
                "closures are not supported yet; lambda can only use its parameters, builtins and named functions",
            );
            return self.error_expr(call.range);
        }

        // Bare `open(...)` is the io.open alias unless the name is shadowed.
        // Locals, imports and user functions were handled above.
        if name == "open" && !self.fn_ids.contains_key(name) {
            let spec =
                stdlib::function("io", "open").expect("io.open is declared in the stdlib table");
            let Some((args, keywords)) = self.module_arguments(call, fs, spec, line, col) else {
                return self.error_expr(call.range);
            };
            return HExpr {
                kind: HExprKind::ModuleCall {
                    module: "io".to_string(),
                    function: "open".to_string(),
                    args,
                    keywords,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        // A named callee of this unit binds keywords and fills literal
        // defaults at compile time, so the call always has the full positional
        // arity.
        if let Some(&id) = self.fn_ids.get(name) {
            // A coroutine/generator with a dynamic signature must see the
            // caller's positional arguments (the runtime creates the fiber and
            // binds the signature on first resume), so it is called through
            // its function value.
            let dynamic_fiber = self.call_signature(id).is_some_and(|signature| {
                signature.has_dynamic_signature() && (signature.is_async || signature.is_generator)
            });
            if dynamic_fiber {
                if !call.keywords.is_empty() {
                    return self.lower_deferred_call(call, fs, line, col);
                }
                let Some(args) = self.plain_call_args(call, fs) else {
                    return self.error_expr(call.range);
                };
                let (callee_line, callee_col) = self.span(name_expr.range);
                let callee = HExpr {
                    kind: HExprKind::FunctionRef(id),
                    ty: Ty::Concrete(Type::Function),
                    line: callee_line,
                    col: callee_col,
                };
                return HExpr {
                    kind: HExprKind::IndirectCall {
                        callee: Box::new(callee),
                        args,
                        star: None,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
            let Some(args) = self.direct_call_arguments(id, call, fs, line, col) else {
                return self.error_expr(call.range);
            };
            return HExpr {
                kind: HExprKind::Call {
                    callee: Callee::Function(id),
                    args,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        // A builtin call binds the few keywords it supports (`max(key=...)`,
        // `min(key=...)`) and rejects the rest.
        if let Some(builtin) = builtin_by_name(name)
            .filter(|_| !(self.session_mode && self.global_names.contains(name)))
        {
            // `print(*args, sep=, end=, file=, flush=)` is the one builtin
            // whose keywords the runtime binder implements. Lower it through
            // the keyword call ABI (the builtin function value), keeping the
            // positional arguments first and the keyword values in source
            // order. `*`/`**` shapes were intercepted by the dynamic branch
            // above and use the same runtime path.
            if builtin == Builtin::Print && !call.keywords.is_empty() {
                return self.lower_print_keywords(call, fs, line, col);
            }
            let Some(mut args) = self.builtin_call_arguments(builtin, call, fs, line, col) else {
                return self.error_expr(call.range);
            };
            if builtin == Builtin::Globals {
                // `globals()` needs the owning module's namespace. The unit
                // name is compile-time knowledge, so it travels as a hidden
                // trailing argument instead of a runtime frame lookup (Hydro
                // keeps no Python frame objects). Sema and codegen expect it.
                args.push(HExpr {
                    kind: HExprKind::Const(Const::Str(self.dunder_name.clone())),
                    ty: Ty::Concrete(Type::Str),
                    line,
                    col,
                });
            }
            return HExpr {
                kind: HExprKind::Call {
                    callee: Callee::Builtin(builtin),
                    args,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if self.global_names.contains(name) || self.imported_user_globals.contains_key(name) {
            // A class used as a callee from a function or method: bind its
            // constructor arguments exactly like a module-body call.
            if let Some((init, signature)) = self.resolve_class(name) {
                let Some(class) = self.class_callee_expr(name, name_expr, fs, line, col) else {
                    return self.error_expr(call.range);
                };
                if signature.has_dynamic_signature() {
                    let Some(bind) = self
                        .class_constructor_dynamic_arguments(name, &signature, call, fs, line, col)
                    else {
                        return self.error_expr(call.range);
                    };
                    return HExpr {
                        kind: HExprKind::ClassCall {
                            class: Box::new(class),
                            init,
                            args: bind.args,
                            dynamic_kwargs: bind.dynamic_kwargs.map(Box::new),
                            param_names: bind.param_names,
                            kwarg_index: bind.kwarg_index,
                            class_name: name.to_string(),
                        },
                        ty: self.infer.fresh(),
                        line,
                        col,
                    };
                }
                let Some(args) =
                    self.class_constructor_arguments(name, &signature, call, fs, line, col)
                else {
                    return self.error_expr(call.range);
                };
                return HExpr {
                    kind: HExprKind::IndirectCall {
                        callee: Box::new(class),
                        args,
                        star: None,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
            // A module-level value (class or function) whose signature is not
            // statically known. Keyword binding (notably constructor kwargs)
            // is not part of this phase: compile the call and report it at
            // runtime instead of the call site.
            if !call.keywords.is_empty() {
                return self.lower_deferred_call(call, fs, line, col);
            }
            let Some(args) = self.plain_call_args(call, fs) else {
                return self.error_expr(call.range);
            };
            self.used_globals.insert(name.to_string());
            let (callee_line, callee_col) = self.span(name_expr.range);
            let callee = HExpr {
                kind: HExprKind::GlobalGet {
                    module: self.dunder_name.clone(),
                    name: name.to_string(),
                },
                ty: Ty::Concrete(Type::Any),
                line: callee_line,
                col: callee_col,
            };
            return HExpr {
                kind: HExprKind::IndirectCall {
                    callee: Box::new(callee),
                    args,
                    star: None,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if let Some(kind) = ExcKind::from_class_name(name) {
            // A built-in exception class is callable like any class object:
            // instantiating it allocates a plain instance (the native classes
            // have no `__init__`). Keyword and starred forms stay deferred so
            // they fail cleanly at runtime instead of at the call site.
            if !call.keywords.is_empty() {
                return self.lower_deferred_call(call, fs, line, col);
            }
            let Some(args) = self.plain_call_args(call, fs) else {
                return self.error_expr(call.range);
            };
            let (callee_line, callee_col) = self.span(name_expr.range);
            let callee = HExpr {
                kind: HExprKind::ExceptionClass(kind),
                ty: Ty::Concrete(Type::Any),
                line: callee_line,
                col: callee_col,
            };
            return HExpr {
                kind: HExprKind::IndirectCall {
                    callee: Box::new(callee),
                    args,
                    star: None,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if self.session_mode {
            // A callable bound by an earlier fragment lives in the session
            // global table; call it through the runtime function-value ABI.
            self.used_globals.insert(name.to_string());
            let (callee_line, callee_col) = self.span(name_expr.range);
            let callee = HExpr {
                kind: HExprKind::GlobalGet {
                    module: self.dunder_name.clone(),
                    name: name.to_string(),
                },
                ty: Ty::Concrete(Type::Any),
                line: callee_line,
                col: callee_col,
            };
            if !call.keywords.is_empty() {
                let Some(result) = self.call_kw_expr(callee, call, fs, line, col) else {
                    return self.error_expr(call.range);
                };
                return result;
            }
            let mut args = Vec::with_capacity(call.args.len());
            for arg in &call.args {
                args.push(self.expr(arg, fs));
            }
            return HExpr {
                kind: HExprKind::IndirectCall {
                    callee: Box::new(callee),
                    args,
                    star: None,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if !fs.is_module && self.module_local_names.contains(name) {
            self.error_at_code(
                call.range,
                codes::NAME,
                format!("name '{name}' is not visible inside a function; pass it as a parameter"),
            );
        } else {
            self.error_name(
                name_expr.range,
                name,
                fs,
                format!("name '{name}' is not defined"),
            );
        }
        self.error_expr(call.range)
    }

    /// Lower a call that needs runtime argument flattening.
    ///
    /// Used for `f(*args)`, `f(**kwargs)` and any combination of a starred
    /// positional operand with keywords on a callee that is not a receiver
    /// method. Python evaluates the callee, then the positional operands (the
    /// starred operand's iterable at its source position), then each keyword
    /// value and finally the `**mapping`. The IR keeps those operand groups in
    /// order and codegen forwards them to the runtime flattener, which raises
    /// `TypeError` for a non-mapping `**` operand, a non-`str` key or a key
    /// bound twice.
    fn lower_dynamic_call(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        let callee = self.starred_callee(&call.func, fs);
        let Some((args, star, keywords, double_star)) = self.call_operands(call, fs) else {
            return self.error_expr(call.range);
        };
        // Shapes the narrower nodes already express exactly: no keyword at
        // all keeps `IndirectCall`, keywords without `*`/`**` keep `CallKw`.
        // Both are preserved so existing lowering behaviour (and its tests)
        // stays byte-identical.
        if double_star.is_none() {
            if keywords.is_empty() {
                return HExpr {
                    kind: HExprKind::IndirectCall {
                        callee: Box::new(callee),
                        args,
                        star: star.map(Box::new),
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
            if star.is_none() {
                return HExpr {
                    kind: HExprKind::CallKw {
                        callee: Box::new(callee),
                        args,
                        keywords,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
        }
        HExpr {
            kind: HExprKind::CallFull {
                callee: Box::new(callee),
                args,
                star: star.map(Box::new),
                keywords,
                double_star: double_star.map(Box::new),
            },
            ty: self.infer.fresh(),
            line,
            col,
        }
    }

    /// Lower the callee of a starred call to a first-class function value.
    ///
    /// Bare `open(...)` is the `io.open` alias when the name is not shadowed
    /// by an earlier binding (mirroring [`Self::call_expr`]); every other
    /// expression resolves through the ordinary value lowering.
    fn starred_callee(&mut self, func: &ast::Expr, fs: &mut FuncState) -> HExpr {
        if let ast::Expr::Name(name) = func {
            let name_text = name.id.as_str();
            let shadowed = fs.lookup(name_text).is_some()
                || fs.is_global(name_text)
                || self.fn_ids.contains_key(name_text)
                || self.imported_user_functions.contains_key(name_text)
                || self.imported_user_globals.contains_key(name_text)
                || self.imported_functions.contains_key(name_text)
                || self.imported_constants.contains_key(name_text)
                || self.imported_extension_members.contains_key(name_text)
                || self.module_aliases.contains_key(name_text)
                || self.user_module_aliases.contains_key(name_text)
                || self.extension_aliases.contains_key(name_text)
                || self.deferred_imports.contains_key(name_text)
                || self.global_names.contains(name_text)
                || builtin_by_name(name_text).is_some();
            if name_text == "open" && !shadowed {
                let (line, col) = self.span(name.range);
                return HExpr {
                    kind: HExprKind::ModuleAttr {
                        module: "io".to_string(),
                        name: "open".to_string(),
                    },
                    ty: Ty::Concrete(Type::Function),
                    line,
                    col,
                };
            }
        }
        self.expr(func, fs)
    }

    /// Lower a call that has to evaluate its operands but cannot be bound to
    /// a static signature. Python evaluates the callee, then positional
    /// operands (including each starred operand's iterable), then keyword
    /// values, left to right; the IR keeps that order and the runtime raises
    /// `RuntimeError` when the call executes.
    fn lower_deferred_call(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        // An attribute callee evaluates its object but not the method lookup:
        // the call cannot bind the method anyway. A module base is not a
        // value, so it is not lowered at all.
        let callee = match &*call.func {
            ast::Expr::Attribute(attr) => {
                let module_base = self.resolve_module_chain(&attr.value, fs).is_some()
                    || self.resolve_user_module(&attr.value, fs).is_some()
                    || self.resolve_extension_alias(&attr.value, fs).is_some()
                    || self.resolve_deferred_import(&call.func, fs).is_some();
                if module_base {
                    None
                } else {
                    Some(self.expr(&attr.value, fs))
                }
            }
            other => Some(self.expr(other, fs)),
        };
        let mut args = Vec::with_capacity(call.args.len() + call.keywords.len());
        for arg in &call.args {
            match arg {
                ast::Expr::Starred(starred) => args.push(self.expr(&starred.value, fs)),
                other => args.push(self.expr(other, fs)),
            }
        }
        for keyword in &call.keywords {
            args.push(self.expr(&keyword.value, fs));
        }
        HExpr {
            kind: HExprKind::DeferredCall {
                callee: callee.map(Box::new),
                args,
            },
            ty: self.infer.fresh(),
            line,
            col,
        }
    }

    /// Lower the arguments of a call that has no keyword binding: positional
    /// values only. Starred arguments are rejected with a dedicated
    /// diagnostic (the function-value ABI has no star slot).
    fn plain_call_args(&mut self, call: &ast::ExprCall, fs: &mut FuncState) -> Option<Vec<HExpr>> {
        let mut args = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            if let ast::Expr::Starred(starred) = arg {
                self.error_at_code(
                    starred.range,
                    codes::CALL,
                    "starred arguments are not supported in this call form",
                );
                return None;
            }
            args.push(self.expr(arg, fs));
        }
        Some(args)
    }

    /// Lower every operand of a call in source order: the positional
    /// arguments, at most one starred operand (which must be the last
    /// positional operand, because the IR evaluates it after them), the named
    /// keyword values and at most one `**mapping` (which must come last).
    ///
    /// The restricted positions are diagnosed at the call site instead of
    /// silently reordering side effects.
    #[allow(clippy::type_complexity)]
    fn call_operands(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
    ) -> Option<LoweredCallOperands> {
        let mut args: Vec<HExpr> = Vec::with_capacity(call.args.len());
        let mut star: Option<HExpr> = None;
        for arg in &call.args {
            if let ast::Expr::Starred(starred) = arg {
                if star.is_some() {
                    self.error_at_code(
                        starred.range,
                        codes::CALL,
                        "only one starred argument is supported",
                    );
                    return None;
                }
                star = Some(self.expr(&starred.value, fs));
                continue;
            }
            if star.is_some() {
                self.error_at_code(
                    arg.range(),
                    codes::CALL,
                    "a positional argument cannot follow a starred argument",
                );
                return None;
            }
            args.push(self.expr(arg, fs));
        }
        let mut keywords: Vec<(String, HExpr)> = Vec::with_capacity(call.keywords.len());
        let mut double_star: Option<HExpr> = None;
        for keyword in &call.keywords {
            let Some(name) = keyword.arg.as_ref() else {
                if double_star.is_some() {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        "only one **mapping argument is allowed",
                    );
                    return None;
                }
                // A `**mapping` must be the last operand: the IR evaluates the
                // named keywords before it.
                double_star = Some(self.expr(&keyword.value, fs));
                continue;
            };
            if double_star.is_some() {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    "a named keyword argument cannot follow a **mapping argument",
                );
                return None;
            }
            if keywords
                .iter()
                .any(|(existing, _)| existing.as_str() == name.as_str())
            {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    format!("got multiple values for keyword argument '{name}'"),
                );
                return None;
            }
            keywords.push((name.to_string(), self.expr(&keyword.value, fs)));
        }
        Some((args, star, keywords, double_star))
    }

    /// Cloned signature of a function of this unit.
    fn call_signature_for(&self, id: FunctionId) -> Option<CallSignature> {
        if let Some(signature) = self.imported_function_signatures.get(&id) {
            return Some(signature.clone());
        }
        self.call_signature(id)
    }

    fn call_signature(&self, id: FunctionId) -> Option<CallSignature> {
        id.checked_sub(self.fid_base)
            .and_then(|index| self.functions.get(index as usize))
            .map(|function| CallSignature {
                name: function.name.clone(),
                capture_count: function.capture_count,
                params: function
                    .params
                    .iter()
                    .map(|param| (param.name.clone(), param.default.clone(), param.kind))
                    .collect(),
                default_exprs: function
                    .params
                    .iter()
                    .map(|param| param.default_expr.clone())
                    .collect(),
                is_async: function.is_async,
                is_generator: function.is_generator,
            })
    }

    /// Lower a direct call to a named function of this unit.
    ///
    /// The generated function takes every parameter slot, so this binder fills
    /// them all in source order: positional parameters, the `*args` list,
    /// keyword-only parameters (by name or literal default) and the
    /// `**kwargs` dict.
    fn direct_call_arguments(
        &mut self,
        id: FunctionId,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<Vec<HExpr>> {
        let Some(signature) = self.call_signature_for(id) else {
            // An imported function carries no signature in this unit; fall
            // back to positional arguments only.
            if !call.keywords.is_empty() {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    "keyword arguments are not supported for user-defined functions",
                );
                return None;
            }
            return self.plain_call_args(call, fs);
        };
        let name = signature.name.clone();
        let capture_count = signature.capture_count;
        let positional_end = signature.positional_end();
        let fixed = positional_end.saturating_sub(capture_count);
        let kwarg_index = signature.kwarg_index();
        let kwonly_start = signature
            .params
            .iter()
            .position(|(_, _, kind)| *kind == ParamKind::KwOnly)
            .unwrap_or(kwarg_index);
        let vararg_index = signature
            .params
            .iter()
            .position(|(_, _, kind)| *kind == ParamKind::VarArg);
        let param_count = signature.params.len();

        let mut slots: Vec<Option<HExpr>> = vec![None; param_count];

        // Positional arguments, evaluated in source order. Arguments past the
        // named positional parameters go to `*args` (or are an error).
        let mut extras: Vec<HExpr> = Vec::new();
        for (position, arg) in call.args.iter().enumerate() {
            if let ast::Expr::Starred(_) = arg {
                // Unbound call shapes are intercepted by `call_expr`.
                self.error_at_code(
                    arg.range(),
                    codes::CALL,
                    "a starred argument requires the runtime call binder",
                );
                return None;
            }
            let value = self.expr(arg, fs);
            if position < fixed {
                slots[capture_count + position] = Some(value);
            } else {
                extras.push(value);
            }
        }
        match vararg_index {
            Some(index) => {
                slots[index] = Some(HExpr {
                    kind: HExprKind::List(extras),
                    ty: Ty::Concrete(Type::List),
                    line,
                    col,
                });
            }
            None if !extras.is_empty() => {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!(
                        "{name}() takes {fixed} positional argument(s) but {} were given",
                        fixed + extras.len()
                    ),
                );
                return None;
            }
            None => {}
        }

        // Keywords: positional and keyword-only parameters bind by name;
        // unknown names go to `**kwargs`.
        let mut extra_kwargs: Vec<(HExpr, HExpr)> = Vec::new();
        for keyword in &call.keywords {
            let Some(keyword_name) = keyword.arg.as_ref() else {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    "**kwargs arguments are not supported here",
                );
                return None;
            };
            let mut bound = None;
            // Positional-only parameters are not keyword-bindable: a keyword
            // with the same name falls through to `**kwargs` or is rejected.
            for index in capture_count..positional_end {
                if signature.kind_at(index) == ParamKind::PosOnly {
                    continue;
                }
                if signature.params[index].0 == keyword_name.as_str() {
                    bound = Some(index);
                    break;
                }
            }
            if bound.is_none() {
                for index in kwonly_start..kwarg_index {
                    if signature.kind_at(index) == ParamKind::KwOnly
                        && signature.params[index].0 == keyword_name.as_str()
                    {
                        bound = Some(index);
                        break;
                    }
                }
            }
            match bound {
                Some(index) => {
                    if slots[index].is_some() {
                        self.error_at_code(
                            keyword.range,
                            codes::CALL,
                            format!("{name}() got multiple values for argument '{keyword_name}'"),
                        );
                        return None;
                    }
                    slots[index] = Some(self.expr(&keyword.value, fs));
                }
                None if kwarg_index < param_count => {
                    let key = HExpr {
                        kind: HExprKind::Const(Const::Str(keyword_name.to_string())),
                        ty: Ty::Concrete(Type::Str),
                        line,
                        col,
                    };
                    extra_kwargs.push((key, self.expr(&keyword.value, fs)));
                }
                None => {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        format!("{name}() got an unexpected keyword argument '{keyword_name}'"),
                    );
                    return None;
                }
            }
        }
        // Omitted keyword-only parameters (and optional positional ones) fall
        // back to their literal defaults.
        for (index, slot) in slots.iter_mut().enumerate().skip(capture_count) {
            if slot.is_some() {
                continue;
            }
            let kind = signature.kind_at(index);
            if kind == ParamKind::VarArg || kind == ParamKind::KwArg {
                continue;
            }
            if let Some(expression) = signature
                .default_exprs
                .get(index)
                .and_then(|expression| expression.clone())
            {
                *slot = Some(expression);
                continue;
            }
            match signature.params[index].1.clone() {
                Some(default) => {
                    // The value is exact, but its *type* stays dynamic: a
                    // default is not an argument the caller typed, so it must
                    // not constrain the parameter's inferred type (Python
                    // accepts differently typed arguments explicitly).
                    *slot = Some(HExpr {
                        kind: HExprKind::Const(default),
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    });
                }
                None => {
                    let label = if kind == ParamKind::KwOnly {
                        "keyword-only argument"
                    } else if kind == ParamKind::PosOnly {
                        "positional argument"
                    } else {
                        "argument"
                    };
                    self.error_at_code(
                        call.range,
                        codes::CALL,
                        format!(
                            "{name}() missing required {label} '{}'",
                            signature.params[index].0
                        ),
                    );
                    return None;
                }
            }
        }
        if kwarg_index < param_count {
            slots[kwarg_index] = Some(HExpr {
                kind: HExprKind::Dict(extra_kwargs),
                ty: Ty::Concrete(Type::Dict),
                line,
                col,
            });
        }

        let mut args = Vec::with_capacity(param_count);
        for slot in slots {
            match slot {
                Some(value) => args.push(value),
                None => {
                    // Closure captures are not callable through a direct
                    // `Callee::Function`; nothing else can leave a slot empty.
                    self.error_at_code(
                        call.range,
                        codes::CALL,
                        format!("internal error: unbound parameter of '{name}'"),
                    );
                    return None;
                }
            }
        }
        Some(args)
    }

    /// Whether `name` denotes a class of this unit at a call site: the module
    /// body holds it in a local slot, while functions/methods read the
    /// mirrored module global. A function-local parameter of the same name
    /// shadows the class and makes this false.
    fn class_callee_resolvable(&self, name: &str, fs: &FuncState) -> bool {
        self.resolve_class(name).is_some()
            && ((fs.is_module && fs.lookup(name).is_some())
                || (!fs.lookup(name).is_some()
                    && (self.global_names.contains(name)
                        || self.imported_user_globals.contains_key(name))))
    }

    /// Materialises the class value for a constructor call: a local read in
    /// the module body, a `GlobalGet` inside functions/methods.
    fn class_callee_expr(
        &mut self,
        name: &str,
        name_expr: &ast::ExprName,
        fs: &FuncState,
        line: u32,
        col: u32,
    ) -> Option<HExpr> {
        let _ = (fs, line, col);
        let (line, col) = self.span(name_expr.range);
        if fs.is_module {
            let local = fs.lookup(name)?;
            return Some(HExpr {
                kind: HExprKind::Local(local),
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            });
        }
        if let Some((module, exported)) = self.imported_user_globals.get(name) {
            return Some(HExpr {
                kind: HExprKind::GlobalGet {
                    module: module.clone(),
                    name: exported.clone(),
                },
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            });
        }
        if !self.global_names.contains(name) {
            return None;
        }
        self.used_globals.insert(name.to_string());
        Some(HExpr {
            kind: HExprKind::GlobalGet {
                module: self.dunder_name.clone(),
                name: name.to_string(),
            },
            ty: Ty::Concrete(Type::Any),
            line,
            col,
        })
    }

    /// Resolves a class constructor available at a call site: a class of this
    /// unit or an imported class exported by an already-lowered dependency.
    fn resolve_class(&self, name: &str) -> Option<(FunctionId, CallSignature)> {
        if let Some((init, signature)) = self.class_signatures.get(name) {
            return Some((*init, signature.clone()));
        }
        if let Some(&init) = self.class_inits.get(name) {
            return self.call_signature(init).map(|signature| (init, signature));
        }
        let (module, exported) = self.imported_user_globals.get(name)?;
        self.imported_classes
            .get(&(module.clone(), exported.clone()))
            .cloned()
    }

    /// Binds `Class(args..., name=value...)` against its `__init__` signature.
    /// The runtime supplies `self`, so only remaining parameters take
    /// arguments; keywords fill their slots and omitted ones use the literal
    /// default (a `field(...)` default is `None`, filled by the body). The
    /// caller always passes the full positional arity.
    fn class_constructor_arguments(
        &mut self,
        class: &str,
        signature: &CallSignature,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<Vec<HExpr>> {
        let fixed = signature.params.len() - 1;
        let mut slots: Vec<Option<HExpr>> = vec![None; fixed];

        for (position, arg) in call.args.iter().enumerate() {
            if let ast::Expr::Starred(starred) = arg {
                // Unbound call shapes are intercepted by `call_expr`.
                self.error_at_code(
                    starred.range,
                    codes::CALL,
                    "a starred argument cannot bind a class constructor here",
                );
                return None;
            }
            let Some(slot) = slots.get_mut(position) else {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!("{class}() takes at most {fixed} positional argument(s)"),
                );
                return None;
            };
            *slot = Some(self.expr(arg, fs));
        }

        for keyword in &call.keywords {
            let Some(keyword_name) = keyword.arg.as_ref() else {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    "**kwargs arguments are not supported here",
                );
                return None;
            };
            let Some(index) = signature.params[1..].iter().position(|(name, _, kind)| {
                *kind != ParamKind::PosOnly && name.as_str() == keyword_name.as_str()
            }) else {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    format!("{class}() got an unexpected keyword argument '{keyword_name}'"),
                );
                return None;
            };
            if slots[index].is_some() {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    format!("{class}() got multiple values for argument '{keyword_name}'"),
                );
                return None;
            }
            slots[index] = Some(self.expr(&keyword.value, fs));
        }

        let mut args = Vec::with_capacity(fixed);
        for (index, slot) in slots.into_iter().enumerate() {
            let (name, default, _) = &signature.params[index + 1];
            match slot {
                Some(value) => args.push(value),
                None => match default.clone() {
                    Some(default) => args.push(HExpr {
                        kind: HExprKind::Const(default),
                        ty: Ty::Concrete(Type::Any),
                        line,
                        col,
                    }),
                    None => {
                        self.error_at_code(
                            call.range,
                            codes::CALL,
                            format!("{class}() missing required argument '{name}'"),
                        );
                        return None;
                    }
                },
            }
        }
        Some(args)
    }

    /// Bind a class constructor whose `__init__` has a dynamic signature
    /// (`*args`, keyword-only parameters or `**kwargs`).
    ///
    /// Fixed positional slots and keyword-only slots are still bound at
    /// compile time; a single starred operand becomes the `*args` list and a
    /// `**mapping` operand is merged into the `**kwargs` dict at runtime.
    /// Shapes that would need an unknown number of positional slots (a
    /// starred operand without a `*args` parameter) stay deferred.
    #[allow(clippy::question_mark)]
    fn class_constructor_dynamic_arguments(
        &mut self,
        class: &str,
        signature: &CallSignature,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<ClassCallBind> {
        let params = &signature.params[1..];
        let fixed_positional = params
            .iter()
            .take_while(|(_, _, kind)| kind.is_positional())
            .count();
        let vararg_index = params
            .iter()
            .position(|(_, _, kind)| *kind == ParamKind::VarArg);
        let kwarg_index = params
            .iter()
            .position(|(_, _, kind)| *kind == ParamKind::KwArg);

        let mut slots: Vec<Option<HExpr>> = vec![None; params.len()];
        let mut star: Option<HExpr> = None;
        let mut saw_star = false;
        let mut vararg_extras: Vec<HExpr> = Vec::new();
        for (position, arg) in call.args.iter().enumerate() {
            if let ast::Expr::Starred(starred) = arg {
                if saw_star || position + 1 != call.args.len() {
                    self.error_at_code(
                        call.range,
                        codes::CALL,
                        "a starred class argument must be the last positional argument",
                    );
                    return None;
                }
                saw_star = true;
                if vararg_index.is_none() {
                    // Without a `*args` slot the number of positional slots is
                    // fixed; only a runtime-bound call could expand it, which
                    // this path deliberately does not attempt.
                    return None;
                }
                star = Some(self.expr(&starred.value, fs));
                continue;
            }
            if position >= fixed_positional {
                if vararg_index.is_some() {
                    vararg_extras.push(self.expr(arg, fs));
                    continue;
                }
                self.error_at_code(
                    arg.range(),
                    codes::CALL,
                    format!("{class}() takes at most {fixed_positional} positional argument(s)"),
                );
                return None;
            }
            slots[position] = Some(self.expr(arg, fs));
        }
        if let Some(index) = vararg_index {
            let value = match (star, vararg_extras.is_empty()) {
                (Some(star), true) => self.expr_to_list(star, fs, line, col),
                (Some(star), false) => {
                    // Positional arguments before `*` stay in source order,
                    // followed by the expanded iterable.
                    let leading = HExpr {
                        kind: HExprKind::List(vararg_extras),
                        ty: Ty::Concrete(Type::List),
                        line,
                        col,
                    };
                    let expanded = self.expr_to_list(star, fs, line, col);
                    HExpr {
                        kind: HExprKind::Bin {
                            op: hydro_ir::BinOp::Add,
                            lhs: Box::new(leading),
                            rhs: Box::new(expanded),
                        },
                        ty: Ty::Concrete(Type::List),
                        line,
                        col,
                    }
                }
                (None, false) => HExpr {
                    kind: HExprKind::List(vararg_extras),
                    ty: Ty::Concrete(Type::List),
                    line,
                    col,
                },
                (None, true) => HExpr {
                    kind: HExprKind::List(Vec::new()),
                    ty: Ty::Concrete(Type::List),
                    line,
                    col,
                },
            };
            slots[index] = Some(value);
        } else if saw_star {
            return None;
        }

        let mut explicit_kwargs: Vec<(String, HExpr)> = Vec::new();
        let mut dstar: Option<HExpr> = None;
        for keyword in &call.keywords {
            let Some(name) = keyword.arg.as_ref() else {
                if dstar.is_some() {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        "only one **mapping argument is allowed",
                    );
                    return None;
                }
                dstar = Some(self.expr(&keyword.value, fs));
                continue;
            };
            let matched = params
                .iter()
                .enumerate()
                .find(|(_, (param, _, kind))| {
                    *kind == ParamKind::Positional && param.as_str() == name.as_str()
                })
                .or_else(|| {
                    params.iter().enumerate().find(|(_, (param, _, kind))| {
                        *kind == ParamKind::KwOnly && param.as_str() == name.as_str()
                    })
                });
            if let Some((index, _)) = matched {
                if slots[index].is_some() {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        format!("{class}() got multiple values for argument '{name}'"),
                    );
                    return None;
                }
                slots[index] = Some(self.expr(&keyword.value, fs));
            } else if kwarg_index.is_some() {
                explicit_kwargs.push((name.to_string(), self.expr(&keyword.value, fs)));
            } else {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    format!("{class}() got an unexpected keyword argument '{name}'"),
                );
                return None;
            }
        }

        // Fill positional and keyword-only slots with their literal defaults.
        for (index, (name, default, kind)) in params.iter().enumerate() {
            if !kind.is_positional() && *kind != ParamKind::KwOnly {
                continue;
            }
            if slots[index].is_some() {
                continue;
            }
            if let Some(expression) = signature
                .default_exprs
                .get(index + 1)
                .and_then(|expression| expression.clone())
            {
                slots[index] = Some(expression);
                continue;
            }
            let Some(default) = default.clone() else {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!("{class}() missing required argument '{name}'"),
                );
                return None;
            };
            slots[index] = Some(HExpr {
                kind: HExprKind::Const(default),
                ty: Ty::Concrete(Type::Any),
                line,
                col,
            });
        }

        let mut dynamic_kwargs = None;
        match (kwarg_index, dstar) {
            (Some(index), Some(mapping)) => {
                // `dict(mapping)` copies the mapping so merging explicit names
                // never mutates the caller's dict. The copy is also the
                // `**kwargs` slot; the runtime router removes keys that bind a
                // named parameter.
                let mut value = HExpr {
                    kind: HExprKind::Call {
                        callee: Callee::Builtin(Builtin::Dict),
                        args: vec![mapping.clone()],
                    },
                    ty: Ty::Concrete(Type::Dict),
                    line,
                    col,
                };
                if !explicit_kwargs.is_empty() {
                    let pairs = explicit_kwargs
                        .into_iter()
                        .map(|(name, value)| {
                            let key = HExpr {
                                kind: HExprKind::Const(Const::Str(name)),
                                ty: Ty::Concrete(Type::Str),
                                line,
                                col,
                            };
                            (key, value)
                        })
                        .collect();
                    value = HExpr {
                        kind: HExprKind::MethodCall {
                            object: Box::new(value),
                            method: "update".to_string(),
                            args: vec![HExpr {
                                kind: HExprKind::Dict(pairs),
                                ty: Ty::Concrete(Type::Dict),
                                line,
                                col,
                            }],
                            star: None,
                            keywords: Vec::new(),
                            double_star: None,
                        },
                        ty: Ty::Concrete(Type::Dict),
                        line,
                        col,
                    };
                }
                slots[index] = Some(value);
                dynamic_kwargs = Some(mapping);
            }
            (Some(index), None) => {
                let pairs = explicit_kwargs
                    .into_iter()
                    .map(|(name, value)| {
                        let key = HExpr {
                            kind: HExprKind::Const(Const::Str(name)),
                            ty: Ty::Concrete(Type::Str),
                            line,
                            col,
                        };
                        (key, value)
                    })
                    .collect();
                slots[index] = Some(HExpr {
                    kind: HExprKind::Dict(pairs),
                    ty: Ty::Concrete(Type::Dict),
                    line,
                    col,
                });
            }
            (None, Some(mapping)) => {
                if !explicit_kwargs.is_empty() {
                    self.error_at_code(
                        call.range,
                        codes::CALL,
                        format!("{class}() got an unexpected keyword argument"),
                    );
                    return None;
                }
                dynamic_kwargs = Some(mapping);
            }
            (None, None) => {}
        }

        let mut args = Vec::with_capacity(params.len());
        for slot in slots {
            let Some(value) = slot else {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!("{class}() cannot bind its dynamic argument slots"),
                );
                return None;
            };
            args.push(value);
        }
        // Positional-only slots carry no bindable name: a `**mapping` key with
        // that name must be routed to `**kwargs`, never into the slot.
        let param_names = params
            .iter()
            .map(|(name, _, kind)| {
                if kind.is_positional() && *kind != ParamKind::PosOnly || *kind == ParamKind::KwOnly
                {
                    name.clone()
                } else {
                    String::new()
                }
            })
            .collect();
        Some(ClassCallBind {
            args,
            dynamic_kwargs,
            param_names,
            kwarg_index,
        })
    }

    /// Wrap `value` in `list(value)` so a starred operand can be stored in a
    /// generated `*args` local.
    fn expr_to_list(&mut self, value: HExpr, _fs: &mut FuncState, line: u32, col: u32) -> HExpr {
        HExpr {
            kind: HExprKind::Call {
                callee: Callee::Builtin(Builtin::List),
                args: vec![value],
            },
            ty: Ty::Concrete(Type::List),
            line,
            col,
        }
    }

    /// Lower a direct builtin call, binding the keywords the builtin supports.
    ///
    /// `max`/`min` accept `key=`: the runtime expects the key slot as the
    /// final argument, so it is appended (with `None` when omitted).
    ///
    /// `sorted` accepts `key=` and `reverse=`: the runtime expects
    /// `(iterable, key, reverse)` with the two optional slots materialised at
    /// their fixed positions.
    /// Lower `print(args..., sep=, end=, file=, flush=)` to the runtime
    /// keyword binder (`hydro_call_kw` on the `print` builtin value).
    ///
    /// Only the four CPython keywords are accepted; a duplicate or unknown
    /// keyword is a compile-time diagnostic. `sep`/`end` may be `None` or a
    /// string, `file` any object with `write` (validated at runtime), and
    /// `flush` any truthy value.
    fn lower_print_keywords(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        let Some(args) = self.plain_call_args(call, fs) else {
            return self.error_expr(call.range);
        };
        let mut keywords: Vec<(String, HExpr)> = Vec::with_capacity(call.keywords.len());
        for keyword in &call.keywords {
            let Some(name) = keyword.arg.as_ref() else {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    "print() does not accept **mapping arguments in this position",
                );
                continue;
            };
            match name.as_str() {
                "sep" | "end" | "file" | "flush" => {}
                other => {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        format!("print() got an unexpected keyword argument '{other}'"),
                    );
                    continue;
                }
            }
            if keywords
                .iter()
                .any(|(existing, _)| existing.as_str() == name.as_str())
            {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    format!("print() got multiple values for keyword argument '{name}'"),
                );
                continue;
            }
            keywords.push((name.to_string(), self.expr(&keyword.value, fs)));
        }
        HExpr {
            kind: HExprKind::CallKw {
                callee: Box::new(HExpr {
                    kind: HExprKind::BuiltinRef(Builtin::Print),
                    ty: Ty::Concrete(Type::Function),
                    line,
                    col,
                }),
                args,
                keywords,
            },
            ty: Ty::Concrete(Type::None),
            line,
            col,
        }
    }

    fn builtin_call_arguments(
        &mut self,
        builtin: Builtin,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<Vec<HExpr>> {
        let mut args = self.plain_call_args(call, fs)?;
        // `property(fget, fset, fdel, doc)`: the four components may also be
        // passed by name, with gaps filled by `None`.
        if builtin == Builtin::Property {
            if args.len() > 4 {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!(
                        "property() takes at most 4 arguments ({} given)",
                        args.len()
                    ),
                );
                return None;
            }
            let positional = args.len();
            let mut slots: Vec<Option<HExpr>> = args.into_iter().map(Some).collect();
            slots.resize(4, None);
            let mut provided = positional;
            for keyword in &call.keywords {
                let Some(name) = keyword.arg.as_ref() else {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        "**mapping arguments are not supported for property()",
                    );
                    return None;
                };
                let index = match name.as_str() {
                    "fget" => 0,
                    "fset" => 1,
                    "fdel" => 2,
                    "doc" => 3,
                    _ => {
                        self.error_at_code(
                            keyword.range,
                            codes::CALL,
                            format!("property() got an unexpected keyword argument '{name}'"),
                        );
                        return None;
                    }
                };
                if slots[index].is_some() {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        format!("property() got multiple values for argument '{name}'"),
                    );
                    return None;
                }
                slots[index] = Some(self.expr(&keyword.value, fs));
                provided = provided.max(index + 1);
            }
            if provided == 0 {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    "property() takes at least 1 argument",
                );
                return None;
            }
            return Some(
                slots
                    .into_iter()
                    .take(provided)
                    .map(|slot| {
                        slot.unwrap_or(HExpr {
                            kind: HExprKind::Const(Const::None),
                            ty: Ty::Concrete(Type::None),
                            line,
                            col,
                        })
                    })
                    .collect(),
            );
        }
        // `eval(source, globals=None, locals=None)` and `exec(...)` expose
        // their parameters by name. Bind the keywords into the positional
        // slots the runtime entry point expects; omitted slots become `None`.
        if matches!(builtin, Builtin::Eval | Builtin::Exec) {
            let positional = args.len();
            if positional > 3 {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!(
                        "{}() takes at most 3 arguments ({} given)",
                        builtin.name(),
                        positional
                    ),
                );
                return None;
            }
            let mut slots: Vec<Option<HExpr>> = args.into_iter().map(Some).collect();
            slots.resize(3, None);
            let mut provided = positional;
            for keyword in &call.keywords {
                let Some(name) = keyword.arg.as_ref() else {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        "**kwargs arguments are not supported",
                    );
                    return None;
                };
                let index = match name.as_str() {
                    "source" => 0,
                    "globals" => 1,
                    "locals" => 2,
                    _ => {
                        self.error_at_code(
                            keyword.range,
                            codes::CALL,
                            format!(
                                "{}() got an unexpected keyword argument '{name}'",
                                builtin.name()
                            ),
                        );
                        return None;
                    }
                };
                if slots[index].is_some() {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        format!(
                            "{}() got multiple values for argument '{name}'",
                            builtin.name()
                        ),
                    );
                    return None;
                }
                slots[index] = Some(self.expr(&keyword.value, fs));
                provided = provided.max(index + 1);
            }
            let none = || HExpr {
                kind: HExprKind::Const(Const::None),
                ty: Ty::Concrete(Type::None),
                line,
                col,
            };
            return Some(
                slots
                    .into_iter()
                    .take(provided)
                    .map(|slot| slot.unwrap_or_else(none))
                    .collect(),
            );
        }
        let accepts_key = matches!(builtin, Builtin::Max | Builtin::Min | Builtin::Sorted);
        let accepts_reverse = builtin == Builtin::Sorted;
        let none = || HExpr {
            kind: HExprKind::Const(Const::None),
            ty: Ty::Concrete(Type::None),
            line,
            col,
        };
        let mut key: Option<HExpr> = None;
        let mut reverse: Option<HExpr> = None;
        for keyword in &call.keywords {
            let Some(name) = keyword.arg.as_ref() else {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    "**kwargs arguments are not supported",
                );
                return None;
            };
            if accepts_key && name.as_str() == "key" {
                // `sorted(iterable, key, ...)`: the second positional slot is
                // already the key.
                if key.is_some() || (accepts_reverse && args.len() >= 2) {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        format!(
                            "{}() got multiple values for argument 'key'",
                            builtin.name()
                        ),
                    );
                    return None;
                }
                key = Some(self.expr(&keyword.value, fs));
                continue;
            }
            if accepts_reverse && name.as_str() == "reverse" {
                if reverse.is_some() || args.len() >= 3 {
                    self.error_at_code(
                        keyword.range,
                        codes::CALL,
                        format!(
                            "{}() got multiple values for argument 'reverse'",
                            builtin.name()
                        ),
                    );
                    return None;
                }
                reverse = Some(self.expr(&keyword.value, fs));
                continue;
            }
            self.error_at_code(
                keyword.range,
                codes::CALL,
                format!(
                    "{}() does not accept the keyword argument '{name}'",
                    builtin.name()
                ),
            );
            return None;
        }
        if accepts_reverse {
            if args.is_empty() {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!("{}() expects at least 1 argument", builtin.name()),
                );
                return None;
            }
            if args.len() > 3 {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!(
                        "{}() takes at most 3 arguments ({} given)",
                        builtin.name(),
                        args.len()
                    ),
                );
                return None;
            }
            if args.len() == 1 {
                args.push(key.unwrap_or_else(none));
            }
            if args.len() == 2 {
                args.push(reverse.unwrap_or_else(none));
            }
            return Some(args);
        }
        if accepts_key {
            if args.is_empty() {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    format!("{}() expects at least 1 argument", builtin.name()),
                );
                return None;
            }
            args.push(key.unwrap_or_else(none));
        }
        Some(args)
    }

    // ------------------------------------------------------------------
    // Operators
    // ------------------------------------------------------------------

    fn bin_op(&mut self, op: ast::Operator, range: TextRange) -> Option<BinOp> {
        Some(match op {
            ast::Operator::Add => BinOp::Add,
            ast::Operator::Sub => BinOp::Sub,
            ast::Operator::Mult => BinOp::Mul,
            ast::Operator::Div => BinOp::Div,
            ast::Operator::FloorDiv => BinOp::FloorDiv,
            ast::Operator::Mod => BinOp::Mod,
            ast::Operator::Pow => BinOp::Pow,
            ast::Operator::MatMult => {
                self.error_at(
                    range,
                    "matrix multiplication (@) is not supported in Phase 1",
                );
                return None;
            }
            ast::Operator::LShift => BinOp::Shl,
            ast::Operator::RShift => BinOp::Shr,
            ast::Operator::BitOr => BinOp::BitOr,
            ast::Operator::BitXor => BinOp::BitXor,
            ast::Operator::BitAnd => BinOp::BitAnd,
        })
    }

    fn un_op(&mut self, op: ast::UnaryOp, _range: TextRange) -> Option<hydro_ir::UnOp> {
        Some(match op {
            ast::UnaryOp::Not => hydro_ir::UnOp::Not,
            ast::UnaryOp::UAdd => hydro_ir::UnOp::Pos,
            ast::UnaryOp::USub => hydro_ir::UnOp::Neg,
            ast::UnaryOp::Invert => hydro_ir::UnOp::Invert,
        })
    }
}

/// Renders a dotted name expression such as `errors.StyleSyntaxError`.
///
/// Only plain names and attribute chains are accepted; anything else (a call,
/// subscript, ...) yields `None`. Used by `raise`/`except` lowering, which
/// approximates imported user exception classes with the `RuntimeError` tag.
fn dotted_expr_name(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Name(n) => Some(n.id.to_string()),
        ast::Expr::Attribute(a) => {
            let base = dotted_expr_name(&a.value)?;
            Some(format!("{base}.{}", a.attr))
        }
        _ => None,
    }
}

/// Map a source comparison operator to its IR binary operator.
fn cmp_op(op: ast::CmpOp) -> BinOp {
    match op {
        ast::CmpOp::Eq => BinOp::Eq,
        ast::CmpOp::NotEq => BinOp::Ne,
        ast::CmpOp::Lt => BinOp::Lt,
        ast::CmpOp::LtE => BinOp::Le,
        ast::CmpOp::Gt => BinOp::Gt,
        ast::CmpOp::GtE => BinOp::Ge,
        ast::CmpOp::Is => BinOp::Is,
        ast::CmpOp::IsNot => BinOp::IsNot,
        ast::CmpOp::In => BinOp::In,
        ast::CmpOp::NotIn => BinOp::NotIn,
    }
}

/// Extracts the static text of an f-string format spec.
///
/// The parser represents `:{...}` as a `JoinedStr`; only literal parts are
/// supported. A nested replacement field (a dynamic spec such as `{x:{w}}`)
/// yields `Err(())` so lowering can report a clear diagnostic.
fn static_format_spec(spec: &ast::Expr) -> Result<String, ()> {
    match spec {
        ast::Expr::Constant(c) => match &c.value {
            ast::Constant::Str(text) => Ok(text.clone()),
            _ => Err(()),
        },
        ast::Expr::JoinedStr(joined) => {
            let mut out = String::new();
            for value in &joined.values {
                match value {
                    ast::Expr::Constant(c) => match &c.value {
                        ast::Constant::Str(text) => out.push_str(text),
                        _ => return Err(()),
                    },
                    _ => return Err(()),
                }
            }
            Ok(out)
        }
        _ => Err(()),
    }
}

fn builtin_by_name(name: &str) -> Option<Builtin> {
    Builtin::from_name(name)
}

/// One `namespace[name] = value` store used while building a dynamic class
/// namespace.
fn class_namespace_store(ns: LocalId, name: &str, value: HExpr, line: u32, col: u32) -> HStmt {
    HStmt {
        kind: HStmtKind::SetSubscript {
            object: HExpr {
                kind: HExprKind::Local(ns),
                ty: Ty::Concrete(Type::Dict),
                line,
                col,
            },
            index: HExpr {
                kind: HExprKind::Const(Const::Str(name.to_string())),
                ty: Ty::Concrete(Type::Str),
                line,
                col,
            },
            value,
        },
        line,
        col,
    }
}

/// Wraps a sequence of class-body statements into one `if True:` statement so
/// it can be returned from `lower_stmt`.
fn class_namespace_block(statements: Vec<HStmt>, line: u32, col: u32) -> HStmt {
    HStmt {
        kind: HStmtKind::If {
            cond: HExpr {
                kind: HExprKind::Const(Const::Bool(true)),
                ty: Ty::Concrete(Type::Bool),
                line,
                col,
            },
            then_body: statements,
            else_body: Vec::new(),
        },
        line,
        col,
    }
}

/// Child statement blocks of one class-body statement, in source order.
///
/// Used by the class-body walkers: a dynamic class body executes control flow
/// at runtime, but its method definitions (and their bodies) still have to be
/// registered before any body is lowered.
fn class_body_blocks(stmt: &ast::Stmt) -> Vec<&[ast::Stmt]> {
    let mut blocks = Vec::new();
    match stmt {
        ast::Stmt::If(s) => {
            blocks.push(s.body.as_slice());
            blocks.push(s.orelse.as_slice());
        }
        ast::Stmt::While(s) => {
            blocks.push(s.body.as_slice());
            blocks.push(s.orelse.as_slice());
        }
        ast::Stmt::For(s) => {
            blocks.push(s.body.as_slice());
            blocks.push(s.orelse.as_slice());
        }
        ast::Stmt::AsyncFor(s) => {
            blocks.push(s.body.as_slice());
            blocks.push(s.orelse.as_slice());
        }
        ast::Stmt::With(s) => blocks.push(&s.body),
        ast::Stmt::AsyncWith(s) => blocks.push(&s.body),
        ast::Stmt::Try(s) => {
            blocks.push(s.body.as_slice());
            for handler in &s.handlers {
                let ast::ExceptHandler::ExceptHandler(handler) = handler;
                blocks.push(handler.body.as_slice());
            }
            blocks.push(s.orelse.as_slice());
            blocks.push(s.finalbody.as_slice());
        }
        ast::Stmt::TryStar(s) => {
            blocks.push(s.body.as_slice());
            for handler in &s.handlers {
                let ast::ExceptHandler::ExceptHandler(handler) = handler;
                blocks.push(handler.body.as_slice());
            }
            blocks.push(s.orelse.as_slice());
            blocks.push(s.finalbody.as_slice());
        }
        ast::Stmt::Match(s) => {
            for case in &s.cases {
                blocks.push(case.body.as_slice());
            }
        }
        _ => {}
    }
    blocks
}

/// Whether a class body needs the runtime-namespace path.
///
/// The static plan can express method definitions, field declarations,
/// docstrings, `pass` and plain assignments (evaluated in declaration order).
/// Anything else — `del`, control flow, calls, `raise`, imports — executes
/// statements and therefore needs the dynamic namespace.
fn class_body_needs_namespace(body: &[ast::Stmt]) -> bool {
    body.iter().any(|stmt| match stmt {
        ast::Stmt::Pass(_) => false,
        ast::Stmt::Expr(expr) => !is_docstring_expr(&expr.value),
        ast::Stmt::Assign(_) | ast::Stmt::AnnAssign(_) => false,
        ast::Stmt::FunctionDef(_) | ast::Stmt::AsyncFunctionDef(_) => false,
        _ => true,
    })
}

/// Bodies of every method definition reachable from `body` (including those
/// nested in control-flow statements, but never inside a method or nested
/// class body), keyed by the definition's source range start.
fn class_method_bodies(body: &[ast::Stmt]) -> HashMap<u32, &[ast::Stmt]> {
    let mut bodies = HashMap::new();
    fn walk<'a>(body: &'a [ast::Stmt], bodies: &mut HashMap<u32, &'a [ast::Stmt]>) {
        for stmt in body {
            match stmt {
                ast::Stmt::FunctionDef(f) => {
                    bodies.insert(u32::from(f.range.start()), &f.body);
                }
                ast::Stmt::AsyncFunctionDef(f) => {
                    bodies.insert(u32::from(f.range.start()), &f.body);
                }
                ast::Stmt::ClassDef(_) => {}
                other => {
                    for block in class_body_blocks(other) {
                        walk(block, bodies);
                    }
                }
            }
        }
    }
    walk(body, &mut bodies);
    bodies
}

/// Names bound by an assignment target: a plain name or a tuple/list of
/// targets. Nested containers are walked so all bound names get slots;
/// attribute/subscript elements bind no names.
fn collect_target_names(target: &ast::Expr, out: &mut Vec<String>) {
    match target {
        ast::Expr::Name(n) => out.push(n.id.to_string()),
        ast::Expr::Tuple(t) => {
            for elem in &t.elts {
                collect_target_names(elem, out);
            }
        }
        ast::Expr::List(l) => {
            for elem in &l.elts {
                collect_target_names(elem, out);
            }
        }
        ast::Expr::Starred(s) => collect_target_names(&s.value, out),
        _ => {}
    }
}

fn is_docstring_expr(expr: &ast::Expr) -> bool {
    matches!(
        expr,
        ast::Expr::Constant(c) if matches!(c.value, ast::Constant::Str(_))
    )
}

/// Final name of a decorator expression: `@property`, `@lru_cache(...)`,
/// `@functools.lru_cache(...)` all yield their simple callee name.
fn decorator_final_name(expr: &ast::Expr) -> Option<String> {
    let expr = match expr {
        ast::Expr::Call(call) => &*call.func,
        other => other,
    };
    match expr {
        ast::Expr::Name(name) => Some(name.id.to_string()),
        ast::Expr::Attribute(attr) => Some(attr.attr.to_string()),
        _ => None,
    }
}

/// `@overload` signatures are type-only and are dropped before registration.
fn is_overload_decorator(expr: &ast::Expr) -> bool {
    decorator_final_name(expr).as_deref() == Some("overload")
}

/// `@name.setter` / `@name.deleter`: property writers are not needed because
/// assigning an instance attribute shadows the getter.
fn is_property_accessor_decorator(expr: &ast::Expr) -> bool {
    matches!(
        decorator_final_name(expr).as_deref(),
        Some("setter" | "deleter")
    )
}

/// Source text for a decorator call's literal string argument.
fn decorator_string(expr: &ast::Expr) -> Option<String> {
    let ast::Expr::Constant(constant) = expr else {
        return None;
    };
    match &constant.value {
        ast::Constant::Str(text) => Some(text.clone()),
        _ => None,
    }
}

/// `@pytest.fixture` scope: `function` unless `scope="module"`/`"session"`.
fn pytest_fixture_scope(decorators: &[ast::Expr]) -> Option<&'static str> {
    for decorator in decorators {
        if decorator_final_name(decorator).as_deref() != Some("fixture") {
            continue;
        }
        if let ast::Expr::Call(call) = decorator {
            for keyword in &call.keywords {
                if keyword.arg.as_deref() == Some("scope") {
                    if let Some(scope) = decorator_string(&keyword.value) {
                        if scope == "module" || scope == "session" {
                            return Some("module");
                        }
                    }
                }
            }
        }
        return Some("function");
    }
    None
}

/// Parameter names of a function definition (positional then keyword-only).
fn pytest_parameter_names(args: &ast::Arguments) -> Vec<String> {
    let mut names = Vec::new();
    for argument in args.args.iter().chain(args.kwonlyargs.iter()) {
        names.push(argument.def.arg.to_string());
    }
    names
}

/// String arguments of a for-loop-like decorator parameter, e.g.
/// `parametrize("a,b", ...)`.
fn pytest_string_list(expr: &ast::Expr) -> Option<Vec<String>> {
    if let Some(text) = decorator_string(expr) {
        return Some(
            text.split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect(),
        );
    }
    let elements = pytest_sequence(expr)?;
    let mut names = Vec::with_capacity(elements.len());
    for element in elements {
        names.push(decorator_string(element)?);
    }
    (!names.is_empty()).then_some(names)
}

/// Literal list/tuple elements of an AST expression.
fn pytest_sequence(expr: &ast::Expr) -> Option<&[ast::Expr]> {
    match expr {
        ast::Expr::List(list) => Some(list.elts.as_slice()),
        ast::Expr::Tuple(tuple) => Some(tuple.elts.as_slice()),
        _ => None,
    }
}

/// Reason string of a decorator call (`@pytest.mark.skip("why")` or
/// `@pytest.mark.skip(reason="why")`).
fn pytest_decorator_reason(decorators: &[ast::Expr], name: &str, fallback: &str) -> Option<String> {
    for decorator in decorators {
        if decorator_final_name(decorator).as_deref() != Some(name) {
            continue;
        }
        if let ast::Expr::Call(call) = decorator {
            if let Some(reason) = call.args.first().and_then(decorator_string) {
                return Some(reason);
            }
            for keyword in &call.keywords {
                if keyword.arg.as_deref() == Some("reason") {
                    if let Some(reason) = decorator_string(&keyword.value) {
                        return Some(reason);
                    }
                }
            }
        }
        return Some(fallback.to_string());
    }
    None
}

fn pytest_skip(decorators: &[ast::Expr]) -> Option<String> {
    pytest_decorator_reason(decorators, "skip", "skipped by pytest.mark.skip")
}

fn pytest_xfail(decorators: &[ast::Expr]) -> Option<String> {
    pytest_decorator_reason(decorators, "xfail", "xfailed by pytest.mark.xfail")
}

/// Condition expression of `@pytest.mark.skipif(condition, reason=...)`.
fn pytest_skipif(decorators: &[ast::Expr]) -> Option<ast::Expr> {
    for decorator in decorators {
        if decorator_final_name(decorator).as_deref() != Some("skipif") {
            continue;
        }
        if let ast::Expr::Call(call) = decorator {
            return call.args.first().cloned();
        }
    }
    None
}

/// Fixture names listed by `@pytest.mark.usefixtures(...)`.
fn pytest_usefixtures(decorators: &[ast::Expr]) -> Vec<String> {
    let mut names = Vec::new();
    for decorator in decorators {
        if decorator_final_name(decorator).as_deref() != Some("usefixtures") {
            continue;
        }
        let ast::Expr::Call(call) = decorator else {
            continue;
        };
        for argument in &call.args {
            if let Some(argument_names) = pytest_string_list(argument) {
                names.extend(argument_names);
            }
        }
    }
    names
}

/// Report id text for one parametrized value: pytest keeps the value's text
/// and replaces whitespace/characters that would break the report name.
fn sanitize_pytest_id(text: &str) -> String {
    let text = text.trim().trim_matches('"').trim_matches('\'');
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_alphanumeric() || matches!(character, '_' | '-' | '.' | '[' | ']') {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "0".to_string()
    } else {
        out
    }
}

/// True while compiling a `tea pytest` child.
///
/// The child sets `HYDRO_PYTEST`; only then are bare `assert` statements given
/// their source text as the `AssertionError` message, preserving the existing
/// "no message" behaviour for normal programs.
fn pytest_mode() -> bool {
    static MODE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("HYDRO_PYTEST").is_some_and(|value| !value.is_empty())
    });
    *MODE
}

/// Decorators accepted and ignored by lowering.
///
/// Their names come from `functools`, `abc`, `typing` and `pytest`; the
/// decorator expressions are never evaluated, so parametrized forms cost
/// nothing.
fn is_ignored_function_decorator(expr: &ast::Expr) -> bool {
    matches!(
        decorator_final_name(expr).as_deref(),
        Some("lru_cache" | "cache" | "wraps" | "abstractmethod" | "overload" | "runtime_checkable")
    ) || is_pytest_decorator(expr)
}

/// Dotted names of a decorator expression (`pytest.mark.parametrize` ->
/// `["pytest", "mark", "parametrize"]`); a call contributes its callee.
fn decorator_path(expr: &ast::Expr) -> Vec<String> {
    let expr = match expr {
        ast::Expr::Call(call) => &*call.func,
        other => other,
    };
    let mut path = Vec::new();
    let mut current = expr;
    loop {
        match current {
            ast::Expr::Attribute(attribute) => {
                path.push(attribute.attr.to_string());
                current = &attribute.value;
            }
            ast::Expr::Name(name) => {
                path.push(name.id.to_string());
                break;
            }
            _ => break,
        }
    }
    path.reverse();
    path
}

/// Decorators provided by `pytest`: `@pytest.fixture`, `@pytest.mark.*` and
/// the marker helpers imported unqualified. Lowering never evaluates them;
/// they are recognised so pytest test modules compile unchanged.
fn is_pytest_decorator(expr: &ast::Expr) -> bool {
    let path = decorator_path(expr);
    let Some(last) = path.last() else {
        return false;
    };
    if path.iter().any(|segment| segment == "pytest") {
        return path
            .iter()
            .rev()
            .nth(1)
            .is_some_and(|segment| segment == "mark")
            || matches!(
                last.as_str(),
                "fixture" | "parametrize" | "skip" | "skipif" | "xfail" | "usefixtures"
            );
    }
    matches!(
        last.as_str(),
        "fixture" | "parametrize" | "skip" | "skipif" | "xfail" | "usefixtures"
    )
}

/// Class decorators accepted and ignored by lowering.
///
/// `@rich_repr` (imported unqualified from `rich.repr`, or spelled
/// `rich.repr.rich_repr`) installs a generated `__repr__` in CPython. The
/// frontend never evaluates decorator expressions, so the class keeps exactly
/// the methods it declares: only the synthesized `__repr__` is missing. Any
/// other class decorator is still diagnosed.
fn is_noop_class_decorator(expr: &ast::Expr) -> bool {
    decorator_final_name(expr).as_deref() == Some("rich_repr")
}

/// Numeric view of a literal operand used by constant folding.
///
/// The runtime treats `bool` as `0`/`1` for numeric operators, so `Bool`
/// shares the `Int` representation here.
#[derive(Clone, Copy)]
enum FoldNum {
    Int(i64),
    Float(f64),
}

/// Bounds of the integers the runtime encodes as tagged immediates. Folds
/// leaving this range are declined; runtime arithmetic promotes to `BigInt`.
const MIN_TAGGED_INT: i64 = i64::MIN >> 3;
const MAX_TAGGED_INT: i64 = i64::MAX >> 3;

/// `2^63` as `f64`; used for exact int/float comparisons near the i64 bounds.
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

fn fold_num(constant: &Const) -> Option<FoldNum> {
    match constant {
        Const::Int(n) => Some(FoldNum::Int(*n)),
        Const::Bool(b) => Some(FoldNum::Int(i64::from(*b))),
        Const::Float(f) => Some(FoldNum::Float(*f)),
        _ => None,
    }
}

fn fold_num_to_f64(n: FoldNum) -> f64 {
    match n {
        FoldNum::Int(i) => i as f64,
        FoldNum::Float(f) => f,
    }
}

/// Wraps a tagged integer fold result, declining values outside the immediate
/// range (the runtime computes those, promoting to a BigInt on overflow).
fn fold_int_result(n: i64) -> Option<Const> {
    (MIN_TAGGED_INT..=MAX_TAGGED_INT)
        .contains(&n)
        .then_some(Const::Int(n))
}

/// Builds the constant for a canonical decimal integer literal.
///
/// Values inside the tagged immediate range stay [`Const::Int`]; every larger
/// magnitude (including values that do not fit `i64`) becomes
/// [`Const::BigInt`].
fn int_const_from_decimal(text: String) -> Const {
    match text.parse::<i64>() {
        Ok(n) if (MIN_TAGGED_INT..=MAX_TAGGED_INT).contains(&n) => Const::Int(n),
        _ => Const::BigInt(text),
    }
}

/// Negates a canonical decimal literal, re-applying the tagged-range rule so
/// `-(-9223372036854775808)`-style chains pick the right constant.
fn negate_int_const(text: &str) -> Const {
    match text.strip_prefix('-') {
        Some(rest) => int_const_from_decimal(rest.to_string()),
        None => int_const_from_decimal(format!("-{text}")),
    }
}

/// Exact `i64`/`f64` comparison with Python semantics; `None` is unordered
/// (a NaN operand). Mirrors the runtime's `cmp_int_float`.
fn cmp_int_float(i: i64, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f == f64::INFINITY {
        return Some(Ordering::Less);
    }
    if f == f64::NEG_INFINITY {
        return Some(Ordering::Greater);
    }
    if f >= TWO_POW_63 {
        return Some(Ordering::Less);
    }
    if f < -TWO_POW_63 {
        return Some(Ordering::Greater);
    }
    let truncated = f.trunc() as i64;
    match i.cmp(&truncated) {
        Ordering::Equal => {
            if f.fract() > 0.0 {
                Some(Ordering::Less)
            } else if f.fract() < 0.0 {
                Some(Ordering::Greater)
            } else {
                Some(Ordering::Equal)
            }
        }
        other => Some(other),
    }
}

fn cmp_fold_num(a: FoldNum, b: FoldNum) -> Option<Ordering> {
    match (a, b) {
        (FoldNum::Int(x), FoldNum::Int(y)) => Some(x.cmp(&y)),
        (FoldNum::Float(x), FoldNum::Float(y)) => x.partial_cmp(&y),
        (FoldNum::Int(x), FoldNum::Float(y)) => cmp_int_float(x, y),
        (FoldNum::Float(x), FoldNum::Int(y)) => cmp_int_float(y, x).map(Ordering::reverse),
    }
}

/// Folds a binary operation whose operands are both literals.
///
/// Returns `None` when the fold is not obviously safe: a non-literal operand,
/// a type combination the sema does not accept, division/modulo by zero, or an
/// integer result outside the runtime's immediate range. Those cases stay
/// runtime operations so exceptions and evaluation order are unchanged.
fn fold_bin(op: BinOp, lhs: &HExpr, rhs: &HExpr) -> Option<Const> {
    let (HExprKind::Const(a), HExprKind::Const(b)) = (&lhs.kind, &rhs.kind) else {
        return None;
    };
    match op {
        BinOp::In | BinOp::NotIn => {
            // Only `str in str` folds; container membership stays a runtime
            // test (hashing, iteration and errors are runtime concerns).
            let (Const::Str(needle), Const::Str(haystack)) = (a, b) else {
                return None;
            };
            let found = haystack.contains(needle.as_str());
            Some(Const::Bool(if op == BinOp::In { found } else { !found }))
        }
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            fold_compare(op, a, b)
        }
        // Identity is a runtime representation test; folding two literals
        // would bake in an interning decision.
        BinOp::Is | BinOp::IsNot => None,
        _ => fold_arith(op, a, b),
    }
}

fn fold_compare(op: BinOp, a: &Const, b: &Const) -> Option<Const> {
    let ordering = match (fold_num(a), fold_num(b)) {
        (Some(x), Some(y)) => cmp_fold_num(x, y),
        _ => match (a, b) {
            (Const::Str(x), Const::Str(y)) => Some(x.cmp(y)),
            _ => return None,
        },
    };
    let result = match ordering {
        Some(ordering) => match op {
            BinOp::Eq => ordering == Ordering::Equal,
            BinOp::Ne => ordering != Ordering::Equal,
            BinOp::Lt => ordering == Ordering::Less,
            BinOp::Le => ordering != Ordering::Greater,
            BinOp::Gt => ordering == Ordering::Greater,
            BinOp::Ge => ordering != Ordering::Less,
            _ => return None,
        },
        // NaN is unordered: only `!=` is true.
        None => op == BinOp::Ne,
    };
    Some(Const::Bool(result))
}

fn fold_arith(op: BinOp, a: &Const, b: &Const) -> Option<Const> {
    if let (Const::Str(x), Const::Str(y)) = (a, b) {
        // `str % ...` is printf-style formatting and stays a runtime call.
        if op != BinOp::Add {
            return None;
        }
        let mut out = String::with_capacity(x.len().checked_add(y.len())?);
        out.push_str(x);
        out.push_str(y);
        return Some(Const::Str(out));
    }
    match (fold_num(a)?, fold_num(b)?) {
        (FoldNum::Int(p), FoldNum::Int(q)) => fold_int_arith(op, p, q),
        (x, y) => fold_float_arith(op, fold_num_to_f64(x), fold_num_to_f64(y)),
    }
}

/// Integer arithmetic with the runtime's exact semantics, including Python's
/// floor division/modulo and `int / int -> float`. Out-of-range results and
/// undefined operations stay unfused (the runtime raises for them).
fn fold_int_arith(op: BinOp, p: i64, q: i64) -> Option<Const> {
    match op {
        BinOp::Add => fold_int_result(p.checked_add(q)?),
        BinOp::Sub => fold_int_result(p.checked_sub(q)?),
        BinOp::Mul => fold_int_result(p.checked_mul(q)?),
        BinOp::Div => {
            if q == 0 {
                return None;
            }
            Some(Const::Float(p as f64 / q as f64))
        }
        BinOp::FloorDiv => {
            if q == 0 {
                return None;
            }
            let quotient = p.checked_div(q)?;
            let remainder = p.checked_rem(q)?;
            let floored = if remainder != 0 && ((remainder < 0) != (q < 0)) {
                quotient.checked_sub(1)?
            } else {
                quotient
            };
            fold_int_result(floored)
        }
        BinOp::Mod => {
            if q == 0 {
                return None;
            }
            let remainder = p.checked_rem(q)?;
            let adjusted = if remainder != 0 && ((remainder < 0) != (q < 0)) {
                remainder.checked_add(q)?
            } else {
                remainder
            };
            fold_int_result(adjusted)
        }
        BinOp::Pow => {
            // Negative exponents produce a float (and can divide by zero);
            // leave them to the runtime.
            if q < 0 {
                return None;
            }
            let mut exponent = q as u64;
            let mut acc: i64 = 1;
            let mut factor = p;
            while exponent > 0 {
                if exponent & 1 == 1 {
                    acc = acc.checked_mul(factor)?;
                }
                exponent >>= 1;
                if exponent > 0 {
                    factor = factor.checked_mul(factor)?;
                }
            }
            fold_int_result(acc)
        }
        _ => None,
    }
}

/// Float arithmetic following the runtime's `hydro_binop` semantics. Division
/// and modulo by zero raise at runtime and therefore stay unfused; the
/// remaining operations cannot raise for finite or infinite f64 operands.
fn fold_float_arith(op: BinOp, x: f64, y: f64) -> Option<Const> {
    let result = match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div => {
            if y == 0.0 {
                return None;
            }
            x / y
        }
        BinOp::FloorDiv => {
            if y == 0.0 {
                return None;
            }
            (x / y).floor()
        }
        BinOp::Mod => {
            if y == 0.0 {
                return None;
            }
            let mut remainder = x % y;
            if remainder != 0.0 && ((remainder < 0.0) != (y < 0.0)) {
                remainder += y;
            }
            remainder
        }
        BinOp::Pow => {
            // `0.0 ** negative` raises at runtime.
            if x == 0.0 && y < 0.0 {
                return None;
            }
            x.powf(y)
        }
        _ => return None,
    };
    Some(Const::Float(result))
}

fn fold_un(op: hydro_ir::UnOp, operand: &HExpr) -> Option<Const> {
    let HExprKind::Const(constant) = &operand.kind else {
        return None;
    };
    match op {
        hydro_ir::UnOp::Neg => match constant {
            Const::Int(n) => Some(negate_int_const(&n.to_string())),
            Const::BigInt(n) => Some(negate_int_const(n)),
            Const::Float(f) => Some(Const::Float(-f)),
            _ => None,
        },
        hydro_ir::UnOp::Not => match constant {
            Const::Bool(b) => Some(Const::Bool(!b)),
            _ => None,
        },
        hydro_ir::UnOp::Invert => match constant {
            // Python: `~x == -x - 1`. Tagged ints are far inside `i64`, so the
            // complement never overflows.
            Const::Int(n) => fold_int_result(!n),
            Const::Bool(b) => fold_int_result(!i64::from(*b)),
            _ => None,
        },
        hydro_ir::UnOp::Pos => None,
    }
}

/// True when a function body contains a `yield`/`yield value`, either as a
/// statement or in any expression position (`x = yield 1`).
///
/// The scan stays inside compound statements but never descends into nested
/// functions, lambdas, classes or comprehensions: a `yield` there belongs to
/// that nested scope (and is diagnosed separately), not to the function being
/// registered.
fn body_has_yield(stmts: &[ast::Stmt]) -> bool {
    stmts.iter().any(|stmt| match stmt {
        ast::Stmt::Expr(s) => expr_has_yield(&s.value),
        ast::Stmt::Assign(s) => expr_has_yield(&s.value) || s.targets.iter().any(expr_has_yield),
        ast::Stmt::AugAssign(s) => expr_has_yield(&s.target) || expr_has_yield(&s.value),
        ast::Stmt::AnnAssign(s) => s.value.as_deref().is_some_and(expr_has_yield),
        ast::Stmt::Return(s) => s.value.as_deref().is_some_and(expr_has_yield),
        ast::Stmt::Delete(s) => s.targets.iter().any(expr_has_yield),
        ast::Stmt::Raise(s) => {
            s.exc.as_deref().is_some_and(expr_has_yield)
                || s.cause.as_deref().is_some_and(expr_has_yield)
        }
        ast::Stmt::Assert(s) => {
            expr_has_yield(&s.test) || s.msg.as_deref().is_some_and(expr_has_yield)
        }
        ast::Stmt::If(s) => {
            expr_has_yield(&s.test) || body_has_yield(&s.body) || body_has_yield(&s.orelse)
        }
        ast::Stmt::While(s) => {
            expr_has_yield(&s.test) || body_has_yield(&s.body) || body_has_yield(&s.orelse)
        }
        ast::Stmt::For(s) => {
            expr_has_yield(&s.iter)
                || expr_has_yield(&s.target)
                || body_has_yield(&s.body)
                || body_has_yield(&s.orelse)
        }
        ast::Stmt::AsyncFor(s) => {
            expr_has_yield(&s.iter)
                || expr_has_yield(&s.target)
                || body_has_yield(&s.body)
                || body_has_yield(&s.orelse)
        }
        ast::Stmt::With(s) => {
            s.items
                .iter()
                .any(|item| expr_has_yield(&item.context_expr))
                || body_has_yield(&s.body)
        }
        ast::Stmt::AsyncWith(s) => {
            s.items
                .iter()
                .any(|item| expr_has_yield(&item.context_expr))
                || body_has_yield(&s.body)
        }
        ast::Stmt::Try(s) => try_parts_have_yield(&s.body, &s.handlers, &s.orelse, &s.finalbody),
        ast::Stmt::TryStar(s) => {
            try_parts_have_yield(&s.body, &s.handlers, &s.orelse, &s.finalbody)
        }
        ast::Stmt::Match(s) => {
            expr_has_yield(&s.subject)
                || s.cases.iter().any(|case| {
                    case.guard.as_deref().is_some_and(expr_has_yield) || body_has_yield(&case.body)
                })
        }
        _ => false,
    })
}

fn try_parts_have_yield(
    body: &[ast::Stmt],
    handlers: &[ast::ExceptHandler],
    orelse: &[ast::Stmt],
    finalbody: &[ast::Stmt],
) -> bool {
    body_has_yield(body)
        || handlers.iter().any(|handler| {
            let ast::ExceptHandler::ExceptHandler(h) = handler;
            body_has_yield(&h.body)
        })
        || body_has_yield(orelse)
        || body_has_yield(finalbody)
}

/// True when `expr` contains a `yield` (or `yield from`) outside a nested
/// scope. Comprehensions and lambdas are nested scopes and are not inspected.
fn expr_has_yield(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Yield(_) | ast::Expr::YieldFrom(_) => true,
        ast::Expr::BoolOp(b) => b.values.iter().any(expr_has_yield),
        ast::Expr::NamedExpr(n) => expr_has_yield(&n.value),
        ast::Expr::BinOp(b) => expr_has_yield(&b.left) || expr_has_yield(&b.right),
        ast::Expr::UnaryOp(u) => expr_has_yield(&u.operand),
        ast::Expr::IfExp(i) => {
            expr_has_yield(&i.test) || expr_has_yield(&i.body) || expr_has_yield(&i.orelse)
        }
        ast::Expr::Dict(d) => {
            d.keys.iter().flatten().any(expr_has_yield) || d.values.iter().any(expr_has_yield)
        }
        ast::Expr::Set(s) => s.elts.iter().any(expr_has_yield),
        ast::Expr::Compare(c) => {
            expr_has_yield(&c.left) || c.comparators.iter().any(expr_has_yield)
        }
        ast::Expr::Call(c) => {
            expr_has_yield(&c.func)
                || c.args.iter().any(expr_has_yield)
                || c.keywords
                    .iter()
                    .any(|keyword| expr_has_yield(&keyword.value))
        }
        ast::Expr::FormattedValue(f) => {
            expr_has_yield(&f.value) || f.format_spec.as_deref().is_some_and(expr_has_yield)
        }
        ast::Expr::JoinedStr(s) => s.values.iter().any(expr_has_yield),
        ast::Expr::Await(a) => expr_has_yield(&a.value),
        ast::Expr::Attribute(a) => expr_has_yield(&a.value),
        ast::Expr::Subscript(s) => expr_has_yield(&s.value) || expr_has_yield(&s.slice),
        ast::Expr::Starred(s) => expr_has_yield(&s.value),
        ast::Expr::List(l) => l.elts.iter().any(expr_has_yield),
        ast::Expr::Tuple(t) => t.elts.iter().any(expr_has_yield),
        ast::Expr::Slice(s) => [s.lower.as_deref(), s.upper.as_deref(), s.step.as_deref()]
            .into_iter()
            .flatten()
            .any(expr_has_yield),
        // Nested scopes and constant/name leaves.
        ast::Expr::Lambda(_)
        | ast::Expr::ListComp(_)
        | ast::Expr::SetComp(_)
        | ast::Expr::DictComp(_)
        | ast::Expr::GeneratorExp(_)
        | ast::Expr::Constant(_)
        | ast::Expr::Name(_) => false,
    }
}

/// Whether any statement of this unit loads `name`, including the bodies of
/// module-level functions and class methods.
///
/// `collect_free_name_loads` already folds in the free names of nested
/// functions; class bodies are separate scopes, so their method bodies are
/// scanned explicitly. The module-level `def`/`class` names themselves are
/// bindings, not loads, and never match.
fn module_references_name(stmts: &[ast::Stmt], name: &str) -> bool {
    let mut names = Vec::new();
    collect_free_name_loads(stmts, &mut names);
    if names.iter().any(|loaded| loaded.as_str() == name) {
        return true;
    }
    for stmt in stmts {
        let ast::Stmt::ClassDef(class) = stmt else {
            continue;
        };
        for member in &class.body {
            let references = match member {
                ast::Stmt::FunctionDef(f) => function_free_names(&f.body, f.args.as_ref())
                    .iter()
                    .any(|loaded| loaded.as_str() == name),
                ast::Stmt::AsyncFunctionDef(f) => function_free_names(&f.body, f.args.as_ref())
                    .iter()
                    .any(|loaded| loaded.as_str() == name),
                _ => false,
            };
            if references {
                return true;
            }
        }
    }
    false
}

/// The module docstring: a leading string-literal expression statement.
fn module_docstring(stmts: &[ast::Stmt]) -> Option<String> {
    let ast::Stmt::Expr(expr) = stmts.first()? else {
        return None;
    };
    let ast::Expr::Constant(constant) = &*expr.value else {
        return None;
    };
    let ast::Constant::Str(text) = &constant.value else {
        return None;
    };
    Some(text.clone())
}

/// Collects the module-level simple-name assignment targets and class
/// definitions that are mirrored into the runtime global table. Function and
/// class bodies are not entered; assignments and class definitions inside
/// module-level compound statements are, because those statements execute at
/// module level.
fn collect_global_names(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Assign(s) => {
                // Chained assignment binds every simple-name target
                // (`a = b = value`), so all of them are exported globals.
                for target in &s.targets {
                    if let ast::Expr::Name(name) = target {
                        out.insert(name.id.to_string());
                    }
                }
            }
            ast::Stmt::AugAssign(s) => {
                if let ast::Expr::Name(name) = &*s.target {
                    out.insert(name.id.to_string());
                }
            }
            ast::Stmt::AnnAssign(s) => {
                if s.value.is_some() {
                    if let ast::Expr::Name(name) = &*s.target {
                        out.insert(name.id.to_string());
                    }
                }
            }
            ast::Stmt::ClassDef(s) => {
                out.insert(s.name.to_string());
            }
            ast::Stmt::If(s) => {
                collect_global_names(&s.body, out);
                collect_global_names(&s.orelse, out);
            }
            ast::Stmt::While(s) => {
                collect_global_names(&s.body, out);
                collect_global_names(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_global_names(&s.body, out);
                collect_global_names(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_global_names(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_global_names(&handler.body, out);
                }
                collect_global_names(&s.orelse, out);
                collect_global_names(&s.finalbody, out);
            }
            ast::Stmt::With(s) => collect_global_names(&s.body, out),
            _ => {}
        }
    }
}

/// Names bound at module level by one scope's statements.
///
/// Unlike [`collect_global_names`], this covers every source-level binding
/// form the session compiler has to flush after a fragment: assignments
/// (chained and unpacked included), annotated/augmented assignments, function
/// and class definitions, loop and `with`/`except` targets, and import
/// bindings. Function and class bodies are separate scopes and are not
/// entered; module-level compound statements are.
///
/// `include_augassign` is false for the local-slot analysis, where an
/// augmented assignment alone does not make a fresh module local (a session
/// fragment updating an earlier name reads and writes the global table).
pub(crate) fn collect_source_bound_names(
    stmts: &[ast::Stmt],
    out: &mut HashSet<String>,
    include_augassign: bool,
) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Assign(s) => {
                for target in &s.targets {
                    collect_target_bound_names(target, out);
                }
            }
            ast::Stmt::AnnAssign(s) => {
                // `x: int` alone binds no value at module level; only the
                // annotated assignment with a value creates the name.
                if s.value.is_some() {
                    collect_target_bound_names(&s.target, out);
                }
            }
            ast::Stmt::AugAssign(s) => {
                if include_augassign {
                    collect_target_bound_names(&s.target, out);
                }
            }
            ast::Stmt::TypeAlias(s) => {
                if let ast::Expr::Name(name) = &*s.name {
                    out.insert(name.id.to_string());
                }
            }
            ast::Stmt::FunctionDef(s) => {
                out.insert(s.name.to_string());
            }
            ast::Stmt::AsyncFunctionDef(s) => {
                out.insert(s.name.to_string());
            }
            ast::Stmt::ClassDef(s) => {
                out.insert(s.name.to_string());
            }
            ast::Stmt::For(s) => {
                collect_target_bound_names(&s.target, out);
                collect_source_bound_names(&s.body, out, include_augassign);
                collect_source_bound_names(&s.orelse, out, include_augassign);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_target_bound_names(&s.target, out);
                collect_source_bound_names(&s.body, out, include_augassign);
                collect_source_bound_names(&s.orelse, out, include_augassign);
            }
            ast::Stmt::While(s) => {
                collect_source_bound_names(&s.body, out, include_augassign);
                collect_source_bound_names(&s.orelse, out, include_augassign);
            }
            ast::Stmt::If(s) => {
                collect_source_bound_names(&s.body, out, include_augassign);
                collect_source_bound_names(&s.orelse, out, include_augassign);
            }
            ast::Stmt::With(s) => {
                for item in &s.items {
                    if let Some(target) = &item.optional_vars {
                        collect_target_bound_names(target, out);
                    }
                }
                collect_source_bound_names(&s.body, out, include_augassign);
            }
            ast::Stmt::AsyncWith(s) => {
                for item in &s.items {
                    if let Some(target) = &item.optional_vars {
                        collect_target_bound_names(target, out);
                    }
                }
                collect_source_bound_names(&s.body, out, include_augassign);
            }
            ast::Stmt::Try(s) => {
                collect_source_bound_names(&s.body, out, include_augassign);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if let Some(name) = &handler.name {
                        out.insert(name.to_string());
                    }
                    collect_source_bound_names(&handler.body, out, include_augassign);
                }
                collect_source_bound_names(&s.orelse, out, include_augassign);
                collect_source_bound_names(&s.finalbody, out, include_augassign);
            }
            ast::Stmt::TryStar(s) => {
                collect_source_bound_names(&s.body, out, include_augassign);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if let Some(name) = &handler.name {
                        out.insert(name.to_string());
                    }
                    collect_source_bound_names(&handler.body, out, include_augassign);
                }
                collect_source_bound_names(&s.orelse, out, include_augassign);
                collect_source_bound_names(&s.finalbody, out, include_augassign);
            }
            ast::Stmt::Import(s) => {
                for alias in &s.names {
                    out.insert(import_binding(alias));
                }
            }
            ast::Stmt::ImportFrom(s) => {
                // `from __future__ import ...` changes compiler flags and
                // binds no name.
                if s.module
                    .as_ref()
                    .is_some_and(|module| module.as_str() == "__future__")
                {
                    continue;
                }
                for alias in &s.names {
                    if alias.name.as_str() == "*" {
                        continue;
                    }
                    out.insert(import_binding(alias));
                }
            }
            _ => {}
        }
    }
}

/// Name bound by one `import` alias (`import a.b` binds `a` unless renamed).
fn import_binding(alias: &ast::Alias) -> String {
    match &alias.asname {
        Some(name) => name.to_string(),
        None => alias
            .name
            .as_str()
            .split('.')
            .next()
            .unwrap_or_else(|| alias.name.as_str())
            .to_string(),
    }
}

/// Records the simple names of an assignment target, including nested
/// tuple/list unpacking and starred elements.
fn collect_target_bound_names(target: &ast::Expr, out: &mut HashSet<String>) {
    match target {
        ast::Expr::Name(name) => {
            out.insert(name.id.to_string());
        }
        ast::Expr::Tuple(tuple) => {
            for element in &tuple.elts {
                collect_target_bound_names(element, out);
            }
        }
        ast::Expr::List(list) => {
            for element in &list.elts {
                collect_target_bound_names(element, out);
            }
        }
        ast::Expr::Starred(starred) => collect_target_bound_names(&starred.value, out),
        _ => {}
    }
}

/// Free names read by the functions and class methods defined at module level,
/// including definitions nested in module-level compound statements.
///
/// Function bodies are not descended into for further definitions: a nested
/// `def` is a binding inside the function, and `function_free_names` already
/// folds the names such nested functions read into its result. Class bodies
/// are separate scopes, so their methods are scanned explicitly; other class
/// body statements execute at module level and read module locals directly.
fn collect_module_function_free_names(stmts: &[ast::Stmt], out: &mut Vec<String>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::FunctionDef(f) => out.extend(function_free_names(&f.body, f.args.as_ref())),
            ast::Stmt::AsyncFunctionDef(f) => {
                out.extend(function_free_names(&f.body, f.args.as_ref()))
            }
            ast::Stmt::ClassDef(c) => {
                for member in &c.body {
                    match member {
                        ast::Stmt::FunctionDef(f) => {
                            out.extend(function_free_names(&f.body, f.args.as_ref()))
                        }
                        ast::Stmt::AsyncFunctionDef(f) => {
                            out.extend(function_free_names(&f.body, f.args.as_ref()))
                        }
                        _ => {}
                    }
                }
            }
            ast::Stmt::If(s) => {
                collect_module_function_free_names(&s.body, out);
                collect_module_function_free_names(&s.orelse, out);
            }
            ast::Stmt::While(s) => {
                collect_module_function_free_names(&s.body, out);
                collect_module_function_free_names(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_module_function_free_names(&s.body, out);
                collect_module_function_free_names(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_module_function_free_names(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_module_function_free_names(&handler.body, out);
                }
                collect_module_function_free_names(&s.orelse, out);
                collect_module_function_free_names(&s.finalbody, out);
            }
            ast::Stmt::With(s) => collect_module_function_free_names(&s.body, out),
            _ => {}
        }
    }
}

/// True when a call has a starred positional operand or a `**` keyword. Such
/// operands cannot be expanded at compile time.
fn call_has_dynamic_arguments(call: &ast::ExprCall) -> bool {
    call.args
        .iter()
        .any(|arg| matches!(arg, ast::Expr::Starred(_)))
        || call.keywords.iter().any(|keyword| keyword.arg.is_none())
}

/// Name of a function's first declared parameter, used for classmethod `cls`.
fn first_param_name(lowerer: &Lowerer<'_>, id: FunctionId) -> String {
    lowerer
        .functions
        .get(id as usize)
        .and_then(|function| function.params.first())
        .map(|param| param.name.clone())
        .unwrap_or_default()
}

/// `global` declarations that belong to one function scope. Nested
/// `def`/`class` bodies are separate scopes and are not visited.
fn collect_scope_globals(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Global(s) => {
                for name in &s.names {
                    out.insert(name.to_string());
                }
            }
            ast::Stmt::If(s) => {
                collect_scope_globals(&s.body, out);
                collect_scope_globals(&s.orelse, out);
            }
            ast::Stmt::While(s) => {
                collect_scope_globals(&s.body, out);
                collect_scope_globals(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_scope_globals(&s.body, out);
                collect_scope_globals(&s.orelse, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_scope_globals(&s.body, out);
                collect_scope_globals(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_scope_globals(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_scope_globals(&handler.body, out);
                }
                collect_scope_globals(&s.orelse, out);
                collect_scope_globals(&s.finalbody, out);
            }
            ast::Stmt::With(s) => collect_scope_globals(&s.body, out),
            ast::Stmt::AsyncWith(s) => collect_scope_globals(&s.body, out),
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    collect_scope_globals(&case.body, out);
                }
            }
            _ => {}
        }
    }
}

/// `nonlocal` declarations of one function scope, with their source ranges.
/// Nested `def`/`class` bodies are separate scopes and are not visited.
fn collect_scope_nonlocals(stmts: &[ast::Stmt], out: &mut Vec<(String, TextRange)>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Nonlocal(s) => {
                for name in &s.names {
                    out.push((name.to_string(), s.range));
                }
            }
            ast::Stmt::If(s) => {
                collect_scope_nonlocals(&s.body, out);
                collect_scope_nonlocals(&s.orelse, out);
            }
            ast::Stmt::While(s) => {
                collect_scope_nonlocals(&s.body, out);
                collect_scope_nonlocals(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_scope_nonlocals(&s.body, out);
                collect_scope_nonlocals(&s.orelse, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_scope_nonlocals(&s.body, out);
                collect_scope_nonlocals(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_scope_nonlocals(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_scope_nonlocals(&handler.body, out);
                }
                collect_scope_nonlocals(&s.orelse, out);
                collect_scope_nonlocals(&s.finalbody, out);
            }
            ast::Stmt::With(s) => collect_scope_nonlocals(&s.body, out),
            ast::Stmt::AsyncWith(s) => collect_scope_nonlocals(&s.body, out),
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    collect_scope_nonlocals(&case.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Names assigned in one function scope without descending into nested
/// functions or classes. `for` targets are skipped: they bind a fresh value
/// per iteration, so a read-only closure capture keeps by-value semantics.
fn collect_scope_mutations(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    let mut targets = Vec::new();
    for stmt in stmts {
        match stmt {
            ast::Stmt::Assign(s) => {
                for target in &s.targets {
                    targets.clear();
                    collect_target_names(target, &mut targets);
                    out.extend(targets.iter().cloned());
                }
            }
            ast::Stmt::AnnAssign(s) => {
                if s.value.is_some() {
                    targets.clear();
                    collect_target_names(&s.target, &mut targets);
                    out.extend(targets.iter().cloned());
                }
            }
            ast::Stmt::AugAssign(s) => {
                if let ast::Expr::Name(name) = &*s.target {
                    out.insert(name.id.to_string());
                }
            }
            ast::Stmt::Delete(s) => {
                for target in &s.targets {
                    if let ast::Expr::Name(name) = target {
                        out.insert(name.id.to_string());
                    }
                }
            }
            ast::Stmt::If(s) => {
                collect_scope_mutations(&s.body, out);
                collect_scope_mutations(&s.orelse, out);
            }
            ast::Stmt::While(s) => {
                collect_scope_mutations(&s.body, out);
                collect_scope_mutations(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_scope_mutations(&s.body, out);
                collect_scope_mutations(&s.orelse, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_scope_mutations(&s.body, out);
                collect_scope_mutations(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_scope_mutations(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if let Some(name) = &handler.name {
                        out.insert(name.to_string());
                    }
                    collect_scope_mutations(&handler.body, out);
                }
                collect_scope_mutations(&s.orelse, out);
                collect_scope_mutations(&s.finalbody, out);
            }
            ast::Stmt::With(s) => {
                for item in &s.items {
                    if let Some(target) = &item.optional_vars {
                        targets.clear();
                        collect_target_names(target, &mut targets);
                        out.extend(targets.iter().cloned());
                    }
                }
                collect_scope_mutations(&s.body, out);
            }
            ast::Stmt::AsyncWith(s) => {
                for item in &s.items {
                    if let Some(target) = &item.optional_vars {
                        targets.clear();
                        collect_target_names(target, &mut targets);
                        out.extend(targets.iter().cloned());
                    }
                }
                collect_scope_mutations(&s.body, out);
            }
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    // A capture is a binding in this scope: a nested function
                    // that captures and mutates it must share a cell.
                    let mut captures = Vec::new();
                    collect_pattern_binding_names(&case.pattern, &mut captures);
                    out.extend(captures);
                    collect_scope_mutations(&case.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Source ranges of `def`/`async def` statements nested in `stmts`.
///
/// The walk follows compound statements exactly like
/// [`collect_nested_needed`] and does not descend into function bodies: a
/// nested function's own range already covers its descendants, so its
/// template interpolations are collected once.
fn collect_nested_function_ranges(stmts: &[ast::Stmt], out: &mut Vec<TextRange>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::FunctionDef(f) => out.push(f.range),
            ast::Stmt::AsyncFunctionDef(f) => out.push(f.range),
            ast::Stmt::If(s) => {
                collect_nested_function_ranges(&s.body, out);
                collect_nested_function_ranges(&s.orelse, out);
            }
            ast::Stmt::While(s) => {
                collect_nested_function_ranges(&s.body, out);
                collect_nested_function_ranges(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_nested_function_ranges(&s.body, out);
                collect_nested_function_ranges(&s.orelse, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_nested_function_ranges(&s.body, out);
                collect_nested_function_ranges(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_nested_function_ranges(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_nested_function_ranges(&handler.body, out);
                }
                collect_nested_function_ranges(&s.orelse, out);
                collect_nested_function_ranges(&s.finalbody, out);
            }
            ast::Stmt::With(s) => collect_nested_function_ranges(&s.body, out),
            ast::Stmt::AsyncWith(s) => collect_nested_function_ranges(&s.body, out),
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    collect_nested_function_ranges(&case.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Names needed by nested named functions (captures), including names their
/// descendants read. `for`/`while`/`try`/`with` bodies are searched; nested
/// function bodies are summarised by `function_free_names`.
fn collect_nested_needed(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::FunctionDef(f) => {
                out.extend(function_free_names(&f.body, f.args.as_ref()));
            }
            ast::Stmt::AsyncFunctionDef(f) => {
                out.extend(function_free_names(&f.body, f.args.as_ref()));
            }
            ast::Stmt::If(s) => {
                collect_nested_needed(&s.body, out);
                collect_nested_needed(&s.orelse, out);
            }
            ast::Stmt::While(s) => {
                collect_nested_needed(&s.body, out);
                collect_nested_needed(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_nested_needed(&s.body, out);
                collect_nested_needed(&s.orelse, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_nested_needed(&s.body, out);
                collect_nested_needed(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_nested_needed(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_nested_needed(&handler.body, out);
                }
                collect_nested_needed(&s.orelse, out);
                collect_nested_needed(&s.finalbody, out);
            }
            ast::Stmt::With(s) => collect_nested_needed(&s.body, out),
            ast::Stmt::AsyncWith(s) => collect_nested_needed(&s.body, out),
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    collect_nested_needed(&case.body, out);
                }
            }
            _ => {}
        }
    }
}

/// `nonlocal` names declared by any descendant scope, used so a binding that a
/// deeply nested function rebinds becomes a cell in every enclosing function.
fn collect_nested_nonlocals(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::FunctionDef(f) => collect_all_nonlocals(&f.body, out),
            ast::Stmt::AsyncFunctionDef(f) => collect_all_nonlocals(&f.body, out),
            ast::Stmt::If(s) => {
                collect_nested_nonlocals(&s.body, out);
                collect_nested_nonlocals(&s.orelse, out);
            }
            ast::Stmt::While(s) => {
                collect_nested_nonlocals(&s.body, out);
                collect_nested_nonlocals(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_nested_nonlocals(&s.body, out);
                collect_nested_nonlocals(&s.orelse, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_nested_nonlocals(&s.body, out);
                collect_nested_nonlocals(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_nested_nonlocals(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_nested_nonlocals(&handler.body, out);
                }
                collect_nested_nonlocals(&s.orelse, out);
                collect_nested_nonlocals(&s.finalbody, out);
            }
            ast::Stmt::With(s) => collect_nested_nonlocals(&s.body, out),
            ast::Stmt::AsyncWith(s) => collect_nested_nonlocals(&s.body, out),
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    collect_nested_nonlocals(&case.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Every `nonlocal` name reachable from `stmts`, including nested scopes.
fn collect_all_nonlocals(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Nonlocal(s) => {
                for name in &s.names {
                    out.insert(name.to_string());
                }
            }
            ast::Stmt::FunctionDef(f) => collect_all_nonlocals(&f.body, out),
            ast::Stmt::AsyncFunctionDef(f) => collect_all_nonlocals(&f.body, out),
            ast::Stmt::ClassDef(c) => collect_all_nonlocals(&c.body, out),
            ast::Stmt::If(s) => {
                collect_all_nonlocals(&s.body, out);
                collect_all_nonlocals(&s.orelse, out);
            }
            ast::Stmt::While(s) => {
                collect_all_nonlocals(&s.body, out);
                collect_all_nonlocals(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_all_nonlocals(&s.body, out);
                collect_all_nonlocals(&s.orelse, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_all_nonlocals(&s.body, out);
                collect_all_nonlocals(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_all_nonlocals(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_all_nonlocals(&handler.body, out);
                }
                collect_all_nonlocals(&s.orelse, out);
                collect_all_nonlocals(&s.finalbody, out);
            }
            ast::Stmt::With(s) => collect_all_nonlocals(&s.body, out),
            ast::Stmt::AsyncWith(s) => collect_all_nonlocals(&s.body, out),
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    collect_all_nonlocals(&case.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Every name declared `global` by a function or method of a module body.
///
/// Such names may be assigned inside a function, so the module body must
/// expose them through the runtime global table (and importers may read them).
fn collect_declared_globals(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::FunctionDef(f) => collect_scope_globals(&f.body, out),
            ast::Stmt::AsyncFunctionDef(f) => collect_scope_globals(&f.body, out),
            ast::Stmt::ClassDef(c) => {
                for stmt in &c.body {
                    match stmt {
                        ast::Stmt::FunctionDef(f) => collect_scope_globals(&f.body, out),
                        ast::Stmt::AsyncFunctionDef(f) => collect_scope_globals(&f.body, out),
                        _ => {}
                    }
                }
            }
            ast::Stmt::If(s) => {
                collect_declared_globals(&s.body, out);
                collect_declared_globals(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_declared_globals(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_declared_globals(&handler.body, out);
                }
                collect_declared_globals(&s.orelse, out);
                collect_declared_globals(&s.finalbody, out);
            }
            ast::Stmt::With(s) => collect_declared_globals(&s.body, out),
            ast::Stmt::AsyncWith(s) => collect_declared_globals(&s.body, out),
            ast::Stmt::While(s) => {
                collect_declared_globals(&s.body, out);
                collect_declared_globals(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_declared_globals(&s.body, out);
                collect_declared_globals(&s.orelse, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_declared_globals(&s.body, out);
                collect_declared_globals(&s.orelse, out);
            }
            _ => {}
        }
    }
}

/// The `(name, local)` pair mirrored by a module-body simple-name
/// `Assign`/`AnnAssign`/`AugAssign`.
///
/// Class definitions are mirrored by `push_lowered_stmt` itself, because the
/// class object is created by the statement rather than by a simple-name
/// store.
fn global_assignment_target(stmt: &ast::Stmt, h: &HStmt) -> Option<(String, LocalId)> {
    match (stmt, &h.kind) {
        (ast::Stmt::Assign(s), HStmtKind::Assign { target, .. }) => match s.targets.as_slice() {
            [ast::Expr::Name(name)] => Some((name.id.to_string(), *target)),
            _ => None,
        },
        (ast::Stmt::AnnAssign(s), HStmtKind::Assign { target, .. }) => match &*s.target {
            ast::Expr::Name(name) => Some((name.id.to_string(), *target)),
            _ => None,
        },
        (ast::Stmt::AugAssign(s), HStmtKind::AugAssign { target, .. }) => match &*s.target {
            ast::Expr::Name(name) => Some((name.id.to_string(), *target)),
            _ => None,
        },
        _ => None,
    }
}

/// Names bound by `import` statements in one scope.
///
/// Nested `def`/`class` bodies are separate scopes and are not visited. Used to
/// keep a deferred import from being captured as a closure variable: an
/// imported name resolves through the module binding tables, never through a
/// parameter of the enclosing function. Dead-branch imports are included
/// (conservatively), because this analysis has no unit context.
fn collect_import_bindings(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Import(s) => {
                for alias in &s.names {
                    let bound = match &alias.asname {
                        Some(name) => name.to_string(),
                        // `import a.b` binds `a` unless renamed.
                        None => alias
                            .name
                            .as_str()
                            .split('.')
                            .next()
                            .unwrap_or_else(|| alias.name.as_str())
                            .to_string(),
                    };
                    out.insert(bound);
                }
            }
            ast::Stmt::ImportFrom(s) => {
                for alias in &s.names {
                    if alias.name.as_str() == "*" {
                        continue;
                    }
                    let bound = match &alias.asname {
                        Some(name) => name.to_string(),
                        None => alias.name.to_string(),
                    };
                    out.insert(bound);
                }
            }
            ast::Stmt::If(s) => {
                collect_import_bindings(&s.body, out);
                collect_import_bindings(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_import_bindings(&s.body, out);
                collect_import_bindings(&s.orelse, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_import_bindings(&s.body, out);
                collect_import_bindings(&s.orelse, out);
            }
            ast::Stmt::While(s) => {
                collect_import_bindings(&s.body, out);
                collect_import_bindings(&s.orelse, out);
            }
            ast::Stmt::Try(s) => {
                collect_import_bindings(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_import_bindings(&handler.body, out);
                }
                collect_import_bindings(&s.orelse, out);
                collect_import_bindings(&s.finalbody, out);
            }
            ast::Stmt::With(s) => collect_import_bindings(&s.body, out),
            ast::Stmt::AsyncWith(s) => collect_import_bindings(&s.body, out),
            _ => {}
        }
    }
}

/// Names that become locals of the enclosing function.
///
/// `include_defs` binds nested `def` names here (true for function bodies,
/// false for the module body, where functions live in the function table).
/// Nested function bodies and lambdas are never descended into.
/// Names bound by a `match` pattern (captures, `as` names, `*rest`, `**rest`).
fn collect_pattern_binding_names(pattern: &ast::Pattern, out: &mut Vec<String>) {
    match pattern {
        ast::Pattern::MatchValue(_) | ast::Pattern::MatchSingleton(_) => {}
        ast::Pattern::MatchSequence(sequence) => {
            for child in &sequence.patterns {
                collect_pattern_binding_names(child, out);
            }
        }
        ast::Pattern::MatchMapping(mapping) => {
            for child in &mapping.patterns {
                collect_pattern_binding_names(child, out);
            }
            if let Some(rest) = &mapping.rest {
                out.push(rest.to_string());
            }
        }
        ast::Pattern::MatchClass(class) => {
            for child in class.patterns.iter().chain(&class.kwd_patterns) {
                collect_pattern_binding_names(child, out);
            }
        }
        ast::Pattern::MatchStar(star) => {
            if let Some(name) = &star.name {
                out.push(name.to_string());
            }
        }
        ast::Pattern::MatchAs(as_pattern) => {
            if let Some(inner) = &as_pattern.pattern {
                collect_pattern_binding_names(inner, out);
            }
            if let Some(name) = &as_pattern.name {
                out.push(name.to_string());
            }
        }
        ast::Pattern::MatchOr(or_pattern) => {
            for alternative in &or_pattern.patterns {
                collect_pattern_binding_names(alternative, out);
            }
        }
    }
}

/// Free-name loads of a pattern: value/class expressions and mapping keys.
/// Capture names are stores and never recorded here.
fn walk_free_pattern(
    pattern: &ast::Pattern,
    shadowed: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    match pattern {
        ast::Pattern::MatchValue(value) => walk_free_expr(&value.value, shadowed, out, seen),
        ast::Pattern::MatchSingleton(_) => {}
        ast::Pattern::MatchSequence(sequence) => {
            for child in &sequence.patterns {
                walk_free_pattern(child, shadowed, out, seen);
            }
        }
        ast::Pattern::MatchMapping(mapping) => {
            for key in &mapping.keys {
                walk_free_expr(key, shadowed, out, seen);
            }
            for child in &mapping.patterns {
                walk_free_pattern(child, shadowed, out, seen);
            }
        }
        ast::Pattern::MatchClass(class) => {
            walk_free_expr(&class.cls, shadowed, out, seen);
            for child in class.patterns.iter().chain(&class.kwd_patterns) {
                walk_free_pattern(child, shadowed, out, seen);
            }
        }
        ast::Pattern::MatchStar(_) => {}
        ast::Pattern::MatchAs(as_pattern) => {
            if let Some(inner) = &as_pattern.pattern {
                walk_free_pattern(inner, shadowed, out, seen);
            }
        }
        ast::Pattern::MatchOr(or_pattern) => {
            for alternative in &or_pattern.patterns {
                walk_free_pattern(alternative, shadowed, out, seen);
            }
        }
    }
}

/// Whether a pattern matches every value: a capture or wildcard (also through
/// `as` bindings and OR alternatives).
fn pattern_is_irrefutable(pattern: &ast::Pattern) -> bool {
    match pattern {
        ast::Pattern::MatchAs(as_pattern) => {
            as_pattern.pattern.is_none()
                || as_pattern
                    .pattern
                    .as_deref()
                    .is_some_and(pattern_is_irrefutable)
        }
        ast::Pattern::MatchOr(or_pattern) => or_pattern.patterns.iter().any(pattern_is_irrefutable),
        _ => false,
    }
}

/// A literal expression usable in a value pattern (`1`, `"x"`, `-2`, `1+2j`).
fn is_literal_pattern_expr(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Constant(_) => true,
        ast::Expr::UnaryOp(unary) => is_literal_pattern_expr(&unary.operand),
        ast::Expr::BinOp(binary) => {
            is_literal_pattern_expr(&binary.left) && is_literal_pattern_expr(&binary.right)
        }
        ast::Expr::Tuple(tuple) => tuple.elts.iter().all(is_literal_pattern_expr),
        _ => false,
    }
}

/// Text form of a literal mapping-pattern key, used to reject duplicates.
fn literal_pattern_key(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Constant(constant) => Some(format!("{constant:?}")),
        ast::Expr::UnaryOp(unary) => {
            let inner = literal_pattern_key(&unary.operand)?;
            Some(format!("{:?}({inner})", unary.op))
        }
        _ => None,
    }
}

fn collect_names(stmts: &[ast::Stmt], out: &mut Vec<String>, include_defs: bool) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::FunctionDef(s) => {
                if include_defs {
                    out.push(s.name.to_string());
                }
            }
            ast::Stmt::AsyncFunctionDef(s) => {
                if include_defs {
                    out.push(s.name.to_string());
                }
            }
            ast::Stmt::ClassDef(s) => out.push(s.name.to_string()),
            ast::Stmt::Assign(s) => {
                for target in &s.targets {
                    collect_target_names(target, out);
                }
            }
            ast::Stmt::AugAssign(s) => {
                if let ast::Expr::Name(n) = &*s.target {
                    out.push(n.id.to_string());
                }
            }
            ast::Stmt::AnnAssign(s) => {
                if s.value.is_some() {
                    collect_target_names(&s.target, out);
                }
            }
            ast::Stmt::For(s) => {
                collect_target_names(&s.target, out);
                collect_names(&s.body, out, include_defs);
                collect_names(&s.orelse, out, include_defs);
            }
            ast::Stmt::While(s) => {
                collect_names(&s.body, out, include_defs);
                collect_names(&s.orelse, out, include_defs);
            }
            ast::Stmt::If(s) => {
                collect_names(&s.body, out, include_defs);
                collect_names(&s.orelse, out, include_defs);
            }
            ast::Stmt::Try(s) => {
                collect_names(&s.body, out, include_defs);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    if let Some(name) = &h.name {
                        out.push(name.to_string());
                    }
                    collect_names(&h.body, out, include_defs);
                }
                collect_names(&s.orelse, out, include_defs);
                collect_names(&s.finalbody, out, include_defs);
            }
            ast::Stmt::With(s) => {
                for item in &s.items {
                    if let Some(target) = &item.optional_vars {
                        collect_target_names(target, out);
                    }
                }
                collect_names(&s.body, out, include_defs);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_target_names(&s.target, out);
                collect_names(&s.body, out, include_defs);
                collect_names(&s.orelse, out, include_defs);
            }
            ast::Stmt::AsyncWith(s) => {
                for item in &s.items {
                    if let Some(target) = &item.optional_vars {
                        collect_target_names(target, out);
                    }
                }
                collect_names(&s.body, out, include_defs);
            }
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    collect_pattern_binding_names(&case.pattern, out);
                    collect_names(&case.body, out, include_defs);
                }
            }
            ast::Stmt::TryStar(s) => {
                collect_names(&s.body, out, include_defs);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    if let Some(name) = &h.name {
                        out.push(name.to_string());
                    }
                    collect_names(&h.body, out, include_defs);
                }
                collect_names(&s.orelse, out, include_defs);
                collect_names(&s.finalbody, out, include_defs);
            }
            _ => {}
        }
    }
}

/// Ordered, de-duplicated names needed by a function body: direct loads plus
/// the free names of nested named functions (so captures can pass through
/// intermediate scopes). Names bound by the body itself are not removed here;
/// callers filter them with `collect_names`/parameters.
fn collect_free_name_loads(stmts: &[ast::Stmt], out: &mut Vec<String>) {
    let mut seen = HashSet::new();
    walk_free_stmts(stmts, &HashSet::new(), out, &mut seen);
}

fn walk_free_stmts(
    stmts: &[ast::Stmt],
    shadowed: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Expr(s) => walk_free_expr(&s.value, shadowed, out, seen),
            ast::Stmt::Assign(s) => {
                for target in &s.targets {
                    walk_free_target(target, shadowed, out, seen);
                }
                walk_free_expr(&s.value, shadowed, out, seen);
            }
            ast::Stmt::AnnAssign(s) => {
                if let Some(value) = &s.value {
                    walk_free_expr(value, shadowed, out, seen);
                }
            }
            ast::Stmt::AugAssign(s) => {
                walk_free_target(&s.target, shadowed, out, seen);
                walk_free_expr(&s.value, shadowed, out, seen);
            }
            ast::Stmt::If(s) => {
                walk_free_expr(&s.test, shadowed, out, seen);
                walk_free_stmts(&s.body, shadowed, out, seen);
                walk_free_stmts(&s.orelse, shadowed, out, seen);
            }
            ast::Stmt::While(s) => {
                walk_free_expr(&s.test, shadowed, out, seen);
                walk_free_stmts(&s.body, shadowed, out, seen);
                walk_free_stmts(&s.orelse, shadowed, out, seen);
            }
            ast::Stmt::For(s) => {
                walk_free_expr(&s.iter, shadowed, out, seen);
                walk_free_stmts(&s.body, shadowed, out, seen);
                walk_free_stmts(&s.orelse, shadowed, out, seen);
            }
            ast::Stmt::AsyncFor(s) => {
                walk_free_expr(&s.iter, shadowed, out, seen);
                walk_free_stmts(&s.body, shadowed, out, seen);
                walk_free_stmts(&s.orelse, shadowed, out, seen);
            }
            ast::Stmt::With(s) => {
                for item in &s.items {
                    walk_free_expr(&item.context_expr, shadowed, out, seen);
                    if let Some(target) = &item.optional_vars {
                        walk_free_target(target, shadowed, out, seen);
                    }
                }
                walk_free_stmts(&s.body, shadowed, out, seen);
            }
            ast::Stmt::AsyncWith(s) => {
                for item in &s.items {
                    walk_free_expr(&item.context_expr, shadowed, out, seen);
                    if let Some(target) = &item.optional_vars {
                        walk_free_target(target, shadowed, out, seen);
                    }
                }
                walk_free_stmts(&s.body, shadowed, out, seen);
            }
            ast::Stmt::Return(s) => {
                if let Some(value) = &s.value {
                    walk_free_expr(value, shadowed, out, seen);
                }
            }
            ast::Stmt::Raise(s) => {
                if let Some(msg) = &s.exc {
                    walk_free_expr(msg, shadowed, out, seen);
                }
            }
            ast::Stmt::Try(s) => {
                walk_free_stmts(&s.body, shadowed, out, seen);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk_free_stmts(&h.body, shadowed, out, seen);
                }
                walk_free_stmts(&s.orelse, shadowed, out, seen);
                walk_free_stmts(&s.finalbody, shadowed, out, seen);
            }
            ast::Stmt::TryStar(s) => {
                walk_free_stmts(&s.body, shadowed, out, seen);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk_free_stmts(&h.body, shadowed, out, seen);
                }
                walk_free_stmts(&s.orelse, shadowed, out, seen);
                walk_free_stmts(&s.finalbody, shadowed, out, seen);
            }
            ast::Stmt::Match(s) => {
                walk_free_expr(&s.subject, shadowed, out, seen);
                for case in &s.cases {
                    walk_free_pattern(&case.pattern, shadowed, out, seen);
                    if let Some(guard) = &case.guard {
                        walk_free_expr(guard, shadowed, out, seen);
                    }
                    walk_free_stmts(&case.body, shadowed, out, seen);
                }
            }
            ast::Stmt::Assert(s) => {
                walk_free_expr(&s.test, shadowed, out, seen);
                if let Some(msg) = &s.msg {
                    walk_free_expr(msg, shadowed, out, seen);
                }
            }
            // A `nonlocal` declaration makes the name a free variable of this
            // scope even when the body only assigns to it.
            ast::Stmt::Nonlocal(s) => {
                for name in &s.names {
                    record_name_load(name.as_str(), shadowed, out, seen);
                }
            }
            // Named nested functions are separate scopes, but names they
            // need from *this* scope must be captured here so they remain
            // available when the inner function runs. Their own locals are
            // already removed by `function_free_names`.
            ast::Stmt::FunctionDef(s) => {
                for name in function_free_names(&s.body, s.args.as_ref()) {
                    record_name_load(&name, shadowed, out, seen);
                }
            }
            ast::Stmt::AsyncFunctionDef(s) => {
                for name in function_free_names(&s.body, s.args.as_ref()) {
                    record_name_load(&name, shadowed, out, seen);
                }
            }
            // Class bodies and lambdas create their own scope and are not
            // closure-converted here.
            _ => {}
        }
    }
}

/// Free names of one function body: ordered loads made by the body and by
/// nested named functions, minus the function's own parameters and locals.
fn function_free_names(body: &[ast::Stmt], args: &ast::Arguments) -> Vec<String> {
    let mut locals = HashSet::new();
    for arg in args
        .posonlyargs
        .iter()
        .chain(args.args.iter())
        .chain(args.kwonlyargs.iter())
    {
        locals.insert(arg.def.arg.to_string());
    }
    if let Some(vararg) = &args.vararg {
        locals.insert(vararg.arg.to_string());
    }
    if let Some(kwarg) = &args.kwarg {
        locals.insert(kwarg.arg.to_string());
    }
    let mut names = Vec::new();
    collect_names(body, &mut names, true);
    locals.extend(names);
    // An import binds the name in this scope, so it is not a free name of the
    // enclosing function (imports are resolved through the module tables).
    collect_import_bindings(body, &mut locals);
    // `global` names resolve through the module's global table, and
    // `nonlocal` names belong to an enclosing scope.
    collect_scope_globals(body, &mut locals);
    let mut nonlocals = Vec::new();
    collect_scope_nonlocals(body, &mut nonlocals);
    for (name, _) in nonlocals {
        locals.remove(&name);
    }

    let mut loads = Vec::new();
    collect_free_name_loads(body, &mut loads);
    loads.retain(|name| !locals.contains(name));
    loads
}

/// Load side of an assignment target. Plain names are stores, but an
/// attribute object or a subscript object/index is still a load.
fn walk_free_target(
    target: &ast::Expr,
    shadowed: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    match target {
        ast::Expr::Name(_) => {}
        ast::Expr::Tuple(t) => {
            for elem in &t.elts {
                walk_free_target(elem, shadowed, out, seen);
            }
        }
        ast::Expr::List(l) => {
            for elem in &l.elts {
                walk_free_target(elem, shadowed, out, seen);
            }
        }
        ast::Expr::Starred(s) => walk_free_target(&s.value, shadowed, out, seen),
        other => walk_free_expr(other, shadowed, out, seen),
    }
}

fn record_name_load(
    name: &str,
    shadowed: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    if shadowed.contains(name) {
        return;
    }
    if seen.insert(name.to_string()) {
        out.push(name.to_string());
    }
}

fn walk_free_expr(
    expr: &ast::Expr,
    shadowed: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    match expr {
        ast::Expr::Name(n) => {
            // A PEP 750 template literal is an opaque placeholder `Name` in
            // the AST. Its interpolation loads are recovered separately from
            // the template side table (`Lowerer::template_free_names_in`);
            // the placeholder itself is never a variable.
            if !is_template_placeholder(n.id.as_str()) {
                record_name_load(n.id.as_str(), shadowed, out, seen);
            }
        }
        ast::Expr::Constant(_) => {}
        ast::Expr::BinOp(b) => {
            walk_free_expr(&b.left, shadowed, out, seen);
            walk_free_expr(&b.right, shadowed, out, seen);
        }
        ast::Expr::UnaryOp(u) => walk_free_expr(&u.operand, shadowed, out, seen),
        ast::Expr::BoolOp(b) => {
            for value in &b.values {
                walk_free_expr(value, shadowed, out, seen);
            }
        }
        ast::Expr::Compare(c) => {
            walk_free_expr(&c.left, shadowed, out, seen);
            for comparator in &c.comparators {
                walk_free_expr(comparator, shadowed, out, seen);
            }
        }
        ast::Expr::Call(c) => {
            walk_free_expr(&c.func, shadowed, out, seen);
            for arg in &c.args {
                walk_free_expr(arg, shadowed, out, seen);
            }
            for keyword in &c.keywords {
                walk_free_expr(&keyword.value, shadowed, out, seen);
            }
        }
        ast::Expr::Attribute(a) => walk_free_expr(&a.value, shadowed, out, seen),
        ast::Expr::Subscript(s) => {
            walk_free_expr(&s.value, shadowed, out, seen);
            walk_free_expr(&s.slice, shadowed, out, seen);
        }
        ast::Expr::Slice(s) => {
            for part in [s.lower.as_deref(), s.upper.as_deref(), s.step.as_deref()]
                .into_iter()
                .flatten()
            {
                walk_free_expr(part, shadowed, out, seen);
            }
        }
        ast::Expr::Tuple(t) => {
            for elem in &t.elts {
                walk_free_expr(elem, shadowed, out, seen);
            }
        }
        ast::Expr::List(l) => {
            for elem in &l.elts {
                walk_free_expr(elem, shadowed, out, seen);
            }
        }
        ast::Expr::Set(s) => {
            for elem in &s.elts {
                walk_free_expr(elem, shadowed, out, seen);
            }
        }
        ast::Expr::Dict(d) => {
            for (key, value) in d.keys.iter().zip(d.values.iter()) {
                if let Some(key) = key {
                    walk_free_expr(key, shadowed, out, seen);
                }
                walk_free_expr(value, shadowed, out, seen);
            }
        }
        ast::Expr::ListComp(c) => {
            walk_free_comp(&c.generators, &[c.elt.as_ref()], shadowed, out, seen)
        }
        ast::Expr::SetComp(c) => {
            walk_free_comp(&c.generators, &[c.elt.as_ref()], shadowed, out, seen)
        }
        ast::Expr::DictComp(c) => walk_free_comp(
            &c.generators,
            &[c.key.as_ref(), c.value.as_ref()],
            shadowed,
            out,
            seen,
        ),
        ast::Expr::GeneratorExp(c) => {
            walk_free_comp(&c.generators, &[c.elt.as_ref()], shadowed, out, seen)
        }
        ast::Expr::IfExp(i) => {
            walk_free_expr(&i.test, shadowed, out, seen);
            walk_free_expr(&i.body, shadowed, out, seen);
            walk_free_expr(&i.orelse, shadowed, out, seen);
        }
        ast::Expr::NamedExpr(n) => {
            walk_free_expr(&n.value, shadowed, out, seen);
        }
        ast::Expr::Await(a) => walk_free_expr(&a.value, shadowed, out, seen),
        ast::Expr::Yield(y) => {
            if let Some(value) = &y.value {
                walk_free_expr(value, shadowed, out, seen);
            }
        }
        ast::Expr::YieldFrom(y) => walk_free_expr(&y.value, shadowed, out, seen),
        ast::Expr::JoinedStr(s) => {
            for value in &s.values {
                walk_free_expr(value, shadowed, out, seen);
            }
        }
        ast::Expr::FormattedValue(f) => {
            walk_free_expr(&f.value, shadowed, out, seen);
            if let Some(spec) = &f.format_spec {
                walk_free_expr(spec, shadowed, out, seen);
            }
        }
        ast::Expr::Starred(s) => walk_free_expr(&s.value, shadowed, out, seen),
        // A lambda is a separate scope; existing lambda lowering diagnoses
        // any use of enclosing names.
        ast::Expr::Lambda(_) => {}
    }
}

fn walk_free_comp(
    generators: &[ast::Comprehension],
    elements: &[&ast::Expr],
    shadowed: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let mut bound = shadowed.clone();
    for generator in generators {
        walk_free_expr(&generator.iter, &bound, out, seen);
        collect_comp_target_names(&generator.target, &mut bound);
        for condition in &generator.ifs {
            walk_free_expr(condition, &bound, out, seen);
        }
    }
    for element in elements {
        walk_free_expr(element, &bound, out, seen);
    }
}

/// Wrap a lowered statement sequence in an always-true `if`.
///
/// The HIR has no block statement, so a multi-statement desugaring that must
/// stay one statement in an enclosing block uses this wrapper, like the
/// subscript augmented-assignment lowering. The condition is a constant, so
/// the extra branch is resolved statically.
fn if_true_block(stmts: Vec<HStmt>, line: u32, col: u32) -> HStmt {
    HStmt {
        kind: HStmtKind::If {
            cond: HExpr {
                kind: HExprKind::Const(Const::Bool(true)),
                ty: Ty::Concrete(Type::Bool),
                line,
                col,
            },
            then_body: stmts,
            else_body: Vec::new(),
        },
        line,
        col,
    }
}

fn collect_comp_target_names(target: &ast::Expr, bound: &mut HashSet<String>) {
    match target {
        ast::Expr::Name(n) => {
            bound.insert(n.id.to_string());
        }
        ast::Expr::Tuple(t) => {
            for elem in &t.elts {
                collect_comp_target_names(elem, bound);
            }
        }
        ast::Expr::List(l) => {
            for elem in &l.elts {
                collect_comp_target_names(elem, bound);
            }
        }
        ast::Expr::Starred(s) => collect_comp_target_names(&s.value, bound),
        _ => {}
    }
}

#[cfg(test)]
mod tests;
