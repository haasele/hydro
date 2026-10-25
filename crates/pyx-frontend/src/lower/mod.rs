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

use pyx_ir::stdlib::{self, ConstantSpec, FunctionSpec, ModuleSpec};
use pyx_ir::{
    BinOp, Builtin, Callee, Const, ExcKind, FunctionId, LocalId, MethodKind, ParamKind, Type,
};
use pyx_ir::{PYTHON_ABI_TAG, PYTHON_VERSION_STRING};
use rustpython_parser::ast::{self, Ranged};
use rustpython_parser::text_size::TextRange;

use crate::diag::{self, codes, line_col, Diagnostic};
use crate::sema::{Infer, Ty};

pub const MODULE_NAME: &str = "<module>";

const BUILTIN_NAMES: [&str; 24] = [
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
    /// of running the body.
    pub is_async: bool,
    /// The body contains a `yield`: calling the function creates a generator
    /// object instead of running the body.
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
    /// `class Name(Base):` with already-lowered method functions. The runtime
    /// class object is stored in `target`.
    ClassDef {
        target: LocalId,
        name: String,
        base: Option<HExpr>,
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
#[derive(Clone, Debug)]
pub struct HCompClause {
    pub target: HCompTarget,
    pub iterable: HExpr,
    pub conditions: Vec<HExpr>,
}

#[derive(Clone, Debug)]
pub struct HExpr {
    pub kind: HExprKind,
    pub ty: Ty,
    pub line: u32,
    pub col: u32,
}

/// Positional arguments plus `(name, value)` keyword pairs of a lowered call.
type LoweredArguments = (Vec<HExpr>, Vec<(String, HExpr)>);

/// Positional arguments, an optional starred operand and keyword pairs.
type LoweredCallArguments = (Vec<HExpr>, Option<HExpr>, Vec<(String, HExpr)>);

#[derive(Clone, Debug)]
pub enum HExprKind {
    Const(Const),
    Local(LocalId),
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
        op: pyx_ir::UnOp,
        operand: Box<HExpr>,
    },
    BoolOp {
        op: pyx_ir::BoolOp,
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
    /// `(element for ...)`, materialised as a list in Phase 2.8.
    GeneratorExp {
        element: Box<HExpr>,
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
        /// flattens a list/tuple into positional arguments; the frontend
        /// rejects combining it with plain positional arguments.
        star: Option<Box<HExpr>>,
        /// Keyword arguments in source order; the runtime binds them.
        keywords: Vec<(String, HExpr)>,
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
    /// `module.function(args...)` resolved from `pyx_ir::stdlib`.
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
    /// `module.name` (constant or callable value) resolved from `pyx_ir::stdlib`.
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
}

/// Signature-relevant fields shared by `def` and `async def`.
///
/// The only difference between the two forms is `is_async`: calling an async
/// function creates a coroutine object, while `async with`/`async for` reuse
/// the synchronous lowerings.
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
struct CallSignature {
    name: String,
    capture_count: usize,
    params: Vec<(String, Option<Const>, ParamKind)>,
    is_async: bool,
    is_generator: bool,
}

impl CallSignature {
    fn kind_at(&self, index: usize) -> ParamKind {
        self.params[index].2
    }

    /// First parameter that is not positional.
    fn positional_end(&self) -> usize {
        self.params[self.capture_count.min(self.params.len())..]
            .iter()
            .position(|(_, _, kind)| *kind != ParamKind::Positional)
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
        self.params
            .iter()
            .any(|(_, _, kind)| *kind != ParamKind::Positional)
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
    /// Index of the single base expression in `ClassDef::bases`, if any.
    base: Option<usize>,
    /// Method name, function id, the index of its `FunctionDef` in the class
    /// body and its binding kind. Methods are lowered after their signatures
    /// are registered. A synthesized method (the dataclass/NamedTuple
    /// `__init__`) has no source body and therefore stores `None`.
    methods: Vec<(String, FunctionId, Option<usize>, MethodKind)>,
    /// Class attribute name and the index of its assignment in the class
    /// body. Values are lowered when the class statement itself is lowered.
    attributes: Vec<(String, usize)>,
}

struct FuncState {
    fid: FunctionId,
    is_module: bool,
    locals: HashMap<String, LocalId>,
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
}

impl FuncState {
    fn lookup(&self, name: &str) -> Option<LocalId> {
        self.locals.get(name).copied()
    }

    fn is_global(&self, name: &str) -> bool {
        self.globals.contains(name)
    }
}

/// A member resolved from `pyx_ir::stdlib`.
enum ModuleMember {
    Function(&'static FunctionSpec),
    Constant(&'static ConstantSpec),
}

fn module_member(module: &'static ModuleSpec, name: &str) -> Option<ModuleMember> {
    if let Some(function) = module.functions.iter().find(|f| f.name == name) {
        return Some(ModuleMember::Function(function));
    }
    module
        .constants
        .iter()
        .find(|c| c.name == name)
        .map(ModuleMember::Constant)
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
    let pyx_extension_path = std::env::var_os("PYX_EXTENSION_PATH");
    let pythonpath = std::env::var_os("PYTHONPATH");
    let pyx_site_packages = std::env::var_os("PYX_SITE_PACKAGES");
    let virtual_env = std::env::var_os("VIRTUAL_ENV").map(PathBuf::from);
    let conda_prefix = std::env::var_os("CONDA_PREFIX").map(PathBuf::from);
    extension_search_paths_from(
        pyx_extension_path.as_deref(),
        pythonpath.as_deref(),
        pyx_site_packages.as_deref(),
        virtual_env.as_deref(),
        conda_prefix.as_deref(),
    )
}

/// Builds the extension search path from raw environment values.
///
/// Order: `PYX_EXTENSION_PATH`, `PYTHONPATH`, then `PYX_SITE_PACKAGES`, then
/// the site-packages directories of `VIRTUAL_ENV` and `CONDA_PREFIX`. The
/// `PYX_SITE_PACKAGES` entries act as an explicit override: they are consulted
/// before any auto-detected virtual-environment site-packages directory.
///
/// Missing and non-directory entries are dropped and duplicates are searched
/// only once. The result is deterministic.
fn extension_search_paths_from(
    pyx_extension_path: Option<&OsStr>,
    pythonpath: Option<&OsStr>,
    pyx_site_packages: Option<&OsStr>,
    virtual_env: Option<&Path>,
    conda_prefix: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for list in [pyx_extension_path, pythonpath, pyx_site_packages]
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
/// sorted order). Every other `<name>.*.so` tag is incompatible: PyX cannot
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
/// lowered.
#[derive(Clone, Debug, Default)]
pub struct UserModule {
    /// Full dotted module name (`helper`, `pkg.sub`, ...).
    pub name: String,
    /// Top-level names exported by the module: functions and global values.
    pub exports: HashMap<String, UserExport>,
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
    /// Single-source checks only mirror those; multi-module programs mirror
    /// every exported name because importers read them.
    used_globals: HashSet<String>,
    /// Mirror every module-level assignment and class definition into the
    /// global table. Enabled when linking imported modules, whose globals are
    /// read by other compilation units.
    mirror_all_globals: bool,
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
    /// Registered class headers, keyed by the class statement's start offset.
    /// Method signatures exist before any body is lowered; the key lets
    /// `lower_stmt` find the plan without threading it through every call.
    class_defs: HashMap<usize, ClassPlan>,
    /// Classes declared directly by this unit. A `raise` of one of these
    /// classes uses the documented `RuntimeError` fallback (see
    /// [`Lowerer::lower_raise`]) instead of being rejected.
    class_names: HashSet<String>,
    /// Same-unit class name -> its `__init__` `FunctionId`. Constructor calls
    /// use it to bind keywords into parameter slots and fill omitted defaults
    /// at compile time; classes without an `__init__` are absent.
    class_inits: HashMap<String, FunctionId>,
    /// Class attributes visible while evaluating a class body's attribute
    /// values; the innermost scope is last.
    class_attr_scopes: Vec<HashMap<String, HExpr>>,
    pub diags: Vec<Diagnostic>,
}

impl<'a> Lowerer<'a> {
    pub fn new(source: &'a str, filename: &'a str, infer: &'a mut Infer) -> Lowerer<'a> {
        Lowerer {
            source,
            filename,
            infer,
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
            imported_functions: HashMap::new(),
            imported_constants: HashMap::new(),
            unpack_counter: 0,
            comp_counter: 0,
            with_counter: 0,
            aug_counter: 0,
            cmp_counter: 0,
            multi_counter: 0,
            class_defs: HashMap::new(),
            class_names: HashSet::new(),
            class_inits: HashMap::new(),
            class_attr_scopes: Vec::new(),
            diags: Vec::new(),
        }
    }

    /// Reserves the [`FunctionId`] range starting at `base` for this unit.
    pub fn set_fid_base(&mut self, base: FunctionId) {
        self.fid_base = base;
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
    /// runtime global table. Multi-module programs enable this because an
    /// importer may read any exported name.
    pub fn set_mirror_all_globals(&mut self, enabled: bool) {
        self.mirror_all_globals = enabled;
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
        collect_global_names(&module.body, &mut self.global_names);
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
        let mut def_ids: HashMap<usize, FunctionId> = HashMap::new();
        for (idx, stmt) in module.body.iter().enumerate() {
            match stmt {
                ast::Stmt::FunctionDef(f) => {
                    if let Some(id) = self.create_function(f) {
                        def_ids.insert(idx, id);
                    }
                }
                ast::Stmt::AsyncFunctionDef(f) => {
                    if let Some(id) = self.create_async_function(f) {
                        def_ids.insert(idx, id);
                    }
                }
                ast::Stmt::ClassDef(c) => {
                    let plan = self.register_class(c);
                    self.class_defs
                        .insert(u32::from(c.range.start()) as usize, plan);
                }
                _ => {}
            }
        }

        // Self-package imports (`from . import f` in a package init) can now
        // resolve functions defined by this unit.
        self.resolve_self_imports();

        // 3. Module locals are known before we lower any function body; this
        //    lets us explain why a module-level name is not visible there.
        let mut module_names = Vec::new();
        collect_names(&module.body, &mut module_names, false);
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
        //    the same function table, only the lookup name differs.
        for (idx, stmt) in module.body.iter().enumerate() {
            match stmt {
                ast::Stmt::FunctionDef(f) => {
                    let Some(&fid) = def_ids.get(&idx) else {
                        continue;
                    };
                    let body = self.lower_function_body(fid, &f.body);
                    self.functions[fid as usize].body = body;
                }
                ast::Stmt::AsyncFunctionDef(f) => {
                    let Some(&fid) = def_ids.get(&idx) else {
                        continue;
                    };
                    let body = self.lower_function_body(fid, &f.body);
                    self.functions[fid as usize].body = body;
                }
                ast::Stmt::ClassDef(c) => {
                    let Some(plan) = self
                        .class_defs
                        .get(&(u32::from(c.range.start()) as usize))
                        .cloned()
                    else {
                        continue;
                    };
                    for (_, fid, method_idx, _) in plan.methods {
                        // Synthesized methods already carry their body.
                        let Some(method_idx) = method_idx else {
                            continue;
                        };
                        let body = match &c.body[method_idx] {
                            ast::Stmt::FunctionDef(f) => &f.body,
                            ast::Stmt::AsyncFunctionDef(f) => &f.body,
                            _ => continue,
                        };
                        let body = self.lower_function_body(fid, body);
                        self.functions[fid as usize].body = body;
                    }
                }
                _ => {}
            }
        }

        // 6. Module body. Function definitions are declarations only (their
        //    values are mirrored for late importers, see
        //    `mirror_module_function`); imports were resolved in step 1.
        let mut fs = FuncState {
            fid: 0,
            is_module: true,
            locals: module_locals,
            comp_shadowed: HashMap::new(),
            globals: HashSet::new(),
            loop_depth: 0,
            in_lambda: false,
            is_async: false,
            is_generator: false,
        };
        let mut body = Vec::new();
        for (idx, stmt) in module.body.iter().enumerate() {
            match stmt {
                ast::Stmt::FunctionDef(_) | ast::Stmt::AsyncFunctionDef(_) => {
                    self.mirror_module_function(idx, &def_ids, &mut body);
                    continue;
                }
                ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_) => continue,
                _ => {}
            }
            if let Some(s) = self.lower_stmt(stmt, &mut fs) {
                self.push_lowered_stmt(stmt, s, &fs, &mut body);
            }
        }
        body.splice(0..0, dunder_inits);
        self.functions[0].body = body;

        LHir {
            source_name: self.filename.to_string(),
            functions: std::mem::take(&mut self.functions),
        }
    }

    /// Mirrors a module-level function into the runtime global table.
    ///
    /// Importers normally link an imported function directly by id. That is
    /// impossible for a module the loader marked late — one that is still
    /// initializing when the importer is lowered (an ancestor package, or a
    /// module reached by a deferred import that closes a cycle). Those members
    /// are read by name from the module's global table, whose function entries
    /// this statement creates. Only multi-module programs mirror, because only
    /// they have importers.
    fn mirror_module_function(
        &mut self,
        index: usize,
        def_ids: &HashMap<usize, FunctionId>,
        body: &mut Vec<HStmt>,
    ) {
        if !self.mirror_all_globals {
            return;
        }
        let Some(&fid) = def_ids.get(&index) else {
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
            if !module_references_name(&module.body, name) {
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
                    self.error_at_code(
                        alias.range,
                        codes::UNSUPPORTED,
                        format!("'from {imported} import *' is not supported in Phase 2.10"),
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
                    self.error_at_code(
                        alias.range,
                        codes::UNSUPPORTED,
                        format!("'from {imported} import *' is not supported for user modules"),
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
                        format!("'from {imported} import *' is not supported in Phase 2.10"),
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
                            format!("'from {imported} import *' is not supported for user modules"),
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
    }

    /// Looks up a compiled extension for `name` (possibly dotted).
    fn find_extension(&self, name: &str) -> ExtensionLookup {
        find_extension_for_source(Path::new(self.filename), name)
    }

    /// Reports a shared object whose wheel ABI tag PyX cannot load.
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
    /// itself is checked at runtime by `pyx_extension_call`.
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

    fn module_attr_expr(
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
        self.create_function_with_captures(decl, name, global, &[], decorator_kind)
    }

    /// Lower a function signature with optional leading closure-capture
    /// parameters. Captures are typed `Any` and become locals too, so the
    /// body resolves their names exactly like parameters.
    fn create_function_with_captures(
        &mut self,
        decl: &FnDecl<'_>,
        name: String,
        global: bool,
        captures: &[String],
        decorator_kind: Option<MethodKind>,
    ) -> Option<FunctionId> {
        // Method decorators were classified by `create_method_function`, which
        // also reported the unsupported ones. Other functions only accept the
        // decorators this gate ignores.
        if decorator_kind.is_none() {
            self.reject_unsupported_decorators(decl.decorators);
        }
        if !decl.type_params.is_empty() {
            self.error_at(decl.range, "type parameters are not supported in Phase 1");
        }

        let args = decl.args;
        let mut params = Vec::new();
        let mut locals = Vec::new();
        let mut seen = HashSet::new();

        for capture in captures {
            if !seen.insert(capture.clone()) {
                continue;
            }
            let local = locals.len() as LocalId;
            let ty = Ty::Concrete(Type::Any);
            params.push(HParam {
                local,
                name: capture.clone(),
                ty,
                default: None,
                kind: ParamKind::Positional,
            });
            locals.push(HLocal {
                name: capture.clone(),
                ty,
            });
        }

        if !args.posonlyargs.is_empty() {
            self.error_at(
                args.posonlyargs[0].def.range,
                "positional-only parameters are not supported in Phase 1",
            );
        }
        for arg in args.posonlyargs.iter().chain(args.args.iter()) {
            let default = self.param_default(arg.default.as_deref());
            self.add_param(
                &arg.def,
                default,
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
                ParamKind::VarArg,
                &mut params,
                &mut locals,
                &mut seen,
            );
        }
        for arg in &args.kwonlyargs {
            let default = self.param_default(arg.default.as_deref());
            self.add_param(
                &arg.def,
                default,
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
                ParamKind::KwArg,
                &mut params,
                &mut locals,
                &mut seen,
            );
        }

        // A body containing `yield` makes the function a generator; `async def`
        // plus `yield` would be an async generator, which is not implemented.
        let is_generator = body_has_yield(decl.body);
        if decl.is_async && is_generator {
            self.error_at_code(
                decl.range,
                codes::UNSUPPORTED,
                "async generators are not supported",
            );
        }

        let ret_ty = match decl.returns {
            Some(ann) => self.annotation_ty(ann),
            None => self.infer.fresh(),
        };

        let (line, col) = self.span(decl.range);
        let id = self.functions.len() as FunctionId;
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
        if global {
            self.fn_ids.insert(name, self.fid_base + id);
        }
        Some(id)
    }

    /// Report every decorator that lowering does not ignore.
    fn reject_unsupported_decorators(&mut self, decorators: &[ast::Expr]) {
        for decorator in decorators {
            if !is_ignored_function_decorator(decorator) {
                self.error_at(decorator.range(), "decorators are not supported in Phase 1");
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
        // `@dataclass` is the one supported class decorator. It is never
        // evaluated: the frontend only needs to recognise the imported name to
        // synthesize `__init__`. The no-op decorators accepted for functions
        // stay ignored here too (e.g. `@runtime_checkable`), and `@rich_repr`
        // is ignored as presentation metadata (see
        // [`is_noop_class_decorator`]); everything else is diagnosed.
        let mut is_dataclass = false;
        for decorator in &s.decorator_list {
            if self.is_dataclass_decorator(decorator) {
                is_dataclass = true;
            } else if !is_ignored_function_decorator(decorator)
                && !is_noop_class_decorator(decorator)
            {
                self.error_at(decorator.range(), "class decorators are not supported yet");
            }
        }
        if !s.type_params.is_empty() {
            self.error_at(s.range, "type parameters are not supported yet");
        }
        if !s.keywords.is_empty() {
            self.error_at(
                s.range,
                "class keywords (metaclass, ...) are not supported yet",
            );
        }

        let base = match s.bases.as_slice() {
            [] => None,
            // Any single base expression is accepted; the frontend lowers it
            // like any other value (e.g. `unittest.TestCase` is a module
            // attribute) and the runtime validates it at class creation.
            [_] => Some(0),
            [_, second, ..] => {
                self.error_at(second.range(), "multiple inheritance is not supported yet");
                None
            }
        };
        // `class Point(NamedTuple):` gets a synthesized `__init__` exactly like
        // a dataclass. Only the class form is supported (no functional form).
        let is_namedtuple = s
            .bases
            .first()
            .is_some_and(|base| self.is_typing_namedtuple(base));

        let mut methods: Vec<(String, FunctionId, Option<usize>, MethodKind)> = Vec::new();
        let mut fields: Vec<DataclassField<'s>> = Vec::new();
        let mut attributes: Vec<(String, usize)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (idx, stmt) in s.body.iter().enumerate() {
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
                methods.push((method, id, Some(idx), kind));
            }
        }

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
            self.class_inits.insert(class.clone(), self.fid_base + *id);
        }

        ClassPlan {
            base,
            methods,
            attributes,
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
            comp_shadowed: HashMap::new(),
            globals: HashSet::new(),
            loop_depth: 0,
            in_lambda: false,
            is_async: false,
            is_generator: false,
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

    fn add_param(
        &mut self,
        arg: &ast::Arg,
        default: Option<Const>,
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
            kind,
        });
        locals.push(HLocal { name, ty });
    }

    /// Default value of a parameter (`def f(x=1)`).
    ///
    /// Python evaluates defaults at definition time. Literal defaults are
    /// recorded exactly, so direct calls fill the omitted argument with the
    /// source value. A non-literal default (a class attribute/enum member such
    /// as rich's `color_system=ColorSystem.TRUECOLOR`, or a name) is accepted
    /// rather than rejected, but parameter metadata can only hold a literal:
    /// it is recorded as `None`. Documented limitation: a direct call that
    /// omits such an argument receives `None` instead of evaluating the
    /// source default.
    fn param_default(&mut self, default: Option<&ast::Expr>) -> Option<Const> {
        let default = default?;
        if let Some(constant) = self.const_default(default) {
            return Some(constant);
        }
        Some(Const::None)
    }

    /// Recognise a literal constant usable as a parameter default, including a
    /// unary minus on a numeric literal. Returns `None` after reporting when
    /// the expression is not a literal.
    fn const_default(&mut self, expr: &ast::Expr) -> Option<Const> {
        match expr {
            ast::Expr::Constant(c) => match &c.value {
                ast::Constant::Int(value) => match value.to_str_radix(10).parse::<i64>() {
                    Ok(v) if (MIN_TAGGED_INT..=MAX_TAGGED_INT).contains(&v) => Some(Const::Int(v)),
                    Ok(_) => {
                        self.error_at(expr.range(), "integer literal is out of the tagged range");
                        None
                    }
                    Err(_) => {
                        self.error_at(expr.range(), "integer literal is out of the i64 range");
                        None
                    }
                },
                ast::Constant::Float(v) => Some(Const::Float(*v)),
                ast::Constant::Bool(v) => Some(Const::Bool(*v)),
                ast::Constant::Str(v) => Some(Const::Str(v.clone())),
                ast::Constant::None => Some(Const::None),
                ast::Constant::Bytes(value) => Some(Const::Bytes(value.clone())),
                ast::Constant::Ellipsis => Some(Const::Ellipsis),
                _ => None,
            },
            ast::Expr::UnaryOp(unary) if matches!(unary.op, ast::UnaryOp::USub) => {
                match self.const_default(&unary.operand)? {
                    Const::Int(v) => match v.checked_neg().and_then(fold_int_result) {
                        Some(value) => Some(value),
                        None => {
                            self.error_at(
                                expr.range(),
                                "integer literal is out of the tagged range",
                            );
                            None
                        }
                    },
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

        let mut local_map: HashMap<String, LocalId> = HashMap::new();
        for (idx, local) in self.functions[fid as usize].locals.iter().enumerate() {
            local_map.insert(local.name.clone(), idx as LocalId);
        }
        for name in names {
            // A name declared `global` refers to the runtime global table; it
            // never gets a local slot.
            if local_map.contains_key(&name) || globals.contains(&name) {
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

        let function = &self.functions[fid as usize];
        let mut fs = FuncState {
            fid,
            is_module: false,
            locals: local_map,
            comp_shadowed: HashMap::new(),
            globals,
            loop_depth: 0,
            in_lambda: false,
            is_async: function.is_async,
            is_generator: function.is_generator,
        };
        self.lower_block(body, &mut fs)
    }

    /// Lower a `def`/`async def` nested inside another function body.
    ///
    /// The nested function is appended to the function table with its
    /// captured names as leading `Any` parameters. The enclosing function
    /// stores the closure value in the nested function's local slot, so later
    /// references use the ordinary local/indirect-call path. Captures are
    /// evaluated (copied) at the definition site; later mutation of the
    /// enclosing local does not affect an existing closure.
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
        for loaded_name in &loaded {
            if nested_locals.contains(loaded_name) {
                continue;
            }
            if let Some(local) = fs.lookup(loaded_name) {
                capture_names.push(loaded_name.clone());
                capture_locals.push(local);
            }
        }

        let outer_name = self.functions[fs.fid as usize].name.clone();
        let display = format!("{outer_name}.{name}");
        let fid =
            self.create_function_with_captures(&decl, display, false, &capture_names, None)?;
        let body = self.lower_function_body(fid, decl.body);
        self.functions[fid as usize].body = body;

        Some(HStmt {
            kind: HStmtKind::Closure {
                target,
                function: self.fid_base + fid,
                captures: capture_locals,
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
    /// Every module-level value is mirrored in multi-module programs:
    /// importers read it through the runtime global table, so whether the
    /// value is used cannot be decided while lowering a dependency. Single-
    /// source checks only mirror names their own functions read. The global
    /// store reads the just-assigned local, so the right-hand side is evaluated
    /// exactly once even when it has side effects. Compound module-level blocks
    /// (`if`, `for`, `try`, ...) lower through [`Self::lower_block`], so
    /// conditional assignments are mirrored on the path that actually runs.
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
            HStmtKind::ClassDef { target, name, .. } if fs.is_module && self.mirror_all_globals => {
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
                } else {
                    let Some(local) = fs.lookup(target.id.as_str()) else {
                        self.error_name(
                            target.range,
                            target.id.as_str(),
                            fs,
                            format!("assignment to unknown name '{}'", target.id),
                        );
                        return None;
                    };
                    let value = self.expr(&s.value, fs);
                    HStmtKind::AugAssign {
                        target: local,
                        op,
                        value,
                    }
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
                    body
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
                if fs.is_module {
                    self.error_at(
                        s.range,
                        "function definitions are only supported directly at module level",
                    );
                    return None;
                }
                return self.lower_nested_function(FnDecl::from_def(s), fs, line, col);
            }
            ast::Stmt::AsyncFunctionDef(s) => {
                if fs.is_module {
                    self.error_at(
                        s.range,
                        "function definitions are only supported directly at module level",
                    );
                    return None;
                }
                return self.lower_nested_function(FnDecl::from_async_def(s), fs, line, col);
            }
            ast::Stmt::ClassDef(s) => {
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
                let base = plan
                    .base
                    .and_then(|index| s.bases.get(index))
                    .map(|base| self.expr(base, fs));
                HStmtKind::ClassDef {
                    target,
                    name,
                    base,
                    methods: plan
                        .methods
                        .iter()
                        .map(|(name, function, _, kind)| {
                            (name.clone(), self.fid_base + *function, *kind)
                        })
                        .collect(),
                    attributes: {
                        self.class_attr_scopes.push(HashMap::new());
                        let mut attributes = Vec::with_capacity(plan.attributes.len());
                        for (name, index) in &plan.attributes {
                            // The value expression is evaluated when the class
                            // statement runs, in declaration order, and may
                            // refer to attributes declared before it.
                            let value = match &s.body[*index] {
                                ast::Stmt::Assign(assign) => self.expr(&assign.value, fs),
                                ast::Stmt::AnnAssign(assign) => match assign.value.as_deref() {
                                    Some(value) => self.expr(value, fs),
                                    None => self.error_expr(assign.range),
                                },
                                other => self.error_expr(other.range()),
                            };
                            if let Some(scope) = self.class_attr_scopes.last_mut() {
                                scope.insert(name.clone(), value.clone());
                            }
                            attributes.push((name.clone(), value));
                        }
                        self.class_attr_scopes.pop();
                        attributes
                    },
                }
            }
            // Imports were resolved into the binding tables in step 1; they
            // emit no runtime statement wherever they appear.
            ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_) => return None,
            ast::Stmt::With(s) => return self.lower_with(s, fs, line, col),
            ast::Stmt::AsyncWith(s) => {
                return self.lower_with_items(&s.items, &s.body, s.range, fs)
            }
            ast::Stmt::AsyncFor(s) => {
                return self.lower_for_parts(&s.target, &s.iter, &s.body, &s.orelse, fs, line, col)
            }
            ast::Stmt::Assert(s) => {
                let test = self.expr(&s.test, fs);
                // Python evaluates the message only when the test fails;
                // codegen keeps it inside the failure branch.
                let msg = s.msg.as_deref().map(|msg| self.expr(msg, fs));
                HStmtKind::Assert { test, msg }
            }
            ast::Stmt::Delete(s) => return self.lower_delete(s, fs, line, col),
            ast::Stmt::Global(_) => {
                // Declarations are handled by `collect_scope_globals` before
                // the body is lowered; the statement itself emits nothing.
                return None;
            }
            ast::Stmt::Nonlocal(s) => {
                self.error_at(s.range, "nonlocal is not supported in Phase 1");
                return None;
            }
            ast::Stmt::Match(s) => {
                self.error_at(s.range, "match is not supported in Phase 1");
                return None;
            }
            ast::Stmt::TypeAlias(s) => {
                self.error_at(s.range, "type aliases are not supported in Phase 1");
                return None;
            }
            ast::Stmt::TryStar(s) => {
                self.error_at(s.range, "try/except* is not supported in Phase 1");
                return None;
            }
        };
        Some(HStmt { kind, line, col })
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
                } else if let Some(local) = fs.lookup(name_text) {
                    Some(HStmtKind::Assign {
                        target: local,
                        value,
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
    /// after each target's operands. A name must resolve to a local slot or a
    /// module global; `del` of an unknown (or unbound local) name is a
    /// compile-time diagnostic. In a module body a name also mirrors its
    /// module-level binding, which is removed from the runtime global table
    /// when functions of the unit read that binding.
    fn lower_delete(
        &mut self,
        s: &ast::StmtDelete,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
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
                    let Some(local) = fs.lookup(text) else {
                        self.error_name(
                            name.range,
                            text,
                            fs,
                            format!("cannot delete unknown name '{}'", name.id),
                        );
                        return None;
                    };
                    targets.push(HDeleteTarget::Name(local));
                    // A module-body name is mirrored into the runtime global
                    // table (see `push_lowered_stmt`), so a later read from a
                    // function must miss it too.
                    if fs.is_module && (self.mirror_all_globals || self.used_globals.contains(text))
                    {
                        targets.push(HDeleteTarget::Global {
                            module: self.dunder_name.clone(),
                            name: text.to_string(),
                        });
                    }
                }
                ast::Expr::Attribute(attr) => {
                    let object = self.expr(&attr.value, fs);
                    targets.push(HDeleteTarget::Attr {
                        object,
                        name: attr.attr.to_string(),
                    });
                }
                ast::Expr::Subscript(sub) => {
                    if let ast::Expr::Slice(slice) = &*sub.slice {
                        let object = self.expr(&sub.value, fs);
                        let start = slice.lower.as_ref().map(|e| Box::new(self.expr(e, fs)));
                        let stop = slice.upper.as_ref().map(|e| Box::new(self.expr(e, fs)));
                        let step = slice.step.as_ref().map(|e| Box::new(self.expr(e, fs)));
                        targets.push(HDeleteTarget::Slice {
                            object,
                            start,
                            stop,
                            step,
                        });
                    } else {
                        let object = self.expr(&sub.value, fs);
                        let index = self.expr(&sub.slice, fs);
                        targets.push(HDeleteTarget::Subscript { object, index });
                    }
                }
                other => {
                    self.error_at(
                        other.range(),
                        "delete target must be a name, attribute or subscript",
                    );
                    return None;
                }
            }
        }
        Some(HStmt {
            kind: HStmtKind::Delete { targets },
            line,
            col,
        })
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
                Some(local) => Some(HUnpackTarget::Name(local)),
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

    /// Create the hidden `$withN` local that holds a `with` context object so
    /// its `__enter__()`/`__exit__()` are still reachable from the generated
    /// prelude and `finally` block.
    fn with_temp(&mut self, fs: &mut FuncState, range: TextRange) -> (LocalId, HExpr) {
        let name = format!("$with{}", self.with_counter);
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
            if generator.is_async {
                self.error_at(
                    generator.target.range(),
                    "async comprehensions are not supported yet",
                );
            }
            let iterable = self.expr(&generator.iter, fs);
            let target = self.lower_comp_target(&generator.target, fs, &mut saved);
            let mut conditions = Vec::with_capacity(generator.ifs.len());
            for condition in &generator.ifs {
                conditions.push(self.expr(condition, fs));
            }
            clauses.push(HCompClause {
                target,
                iterable,
                conditions,
            });
        }
        (clauses, saved)
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

    /// Shared `for`/`async for` lowering. `async for` is sequential: it uses
    /// exactly the same synchronous iterator loop.
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
        let unpack_target = matches!(target_expr, ast::Expr::Tuple(_) | ast::Expr::List(_));
        let name_target = match target_expr {
            ast::Expr::Name(n) => Some(n),
            _ => None,
        };
        if !unpack_target && name_target.is_none() {
            self.error_at(
                target_expr.range(),
                "for loop target must be a single name in Phase 2.0",
            );
            return None;
        }
        let var = match name_target {
            Some(target) => match fs.lookup(target.id.as_str()) {
                Some(var) => Some(var),
                None => {
                    self.error_name(
                        target.range,
                        target.id.as_str(),
                        fs,
                        format!("for loop variable '{}' is not defined", target.id),
                    );
                    return None;
                }
            },
            None => None,
        };
        let unpack_targets = if unpack_target {
            let targets = self.unpack_targets(target_expr, fs)?;
            Some(targets)
        } else {
            None
        };

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
            let (var, body) = match (var, unpack_targets) {
                (Some(var), _) => {
                    fs.loop_depth += 1;
                    let body = self.lower_block(loop_body, fs);
                    fs.loop_depth -= 1;
                    (var, body)
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
                    (var, body)
                }
                (None, None) => return None,
            };
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
        let body = self.lower_block(loop_body, fs);
        fs.loop_depth -= 1;
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

    /// Lowers `raise`, `raise X(...)`, `raise ... from cause`.
    ///
    /// Built-in exception kinds keep their tag-based lowering. A call to a
    /// class this unit declares (`class ReprError(Exception)`) has no runtime
    /// tag, because PyX exception objects are native values: it lowers to
    /// `RuntimeError` whose message is `"<ClassName>: <message>"`. Imported
    /// classes (`raise errors.StyleSyntaxError(...)`) use the same fallback
    /// with their dotted source name. This is a documented limitation: the
    /// original class identity is not preserved and every user class shares
    /// the `RuntimeError` tag, so handlers are approximate. The explicit
    /// `from` cause is kept in both paths, and a raise is never dropped.
    /// Whether a name denotes a user-defined or imported exception class
    /// (handled through the documented RuntimeError fallback).
    fn is_user_exception_name(&self, name: &str) -> bool {
        self.class_names.contains(name) || self.imported_user_globals.contains_key(name)
    }

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

    /// Resolves the tags an `except` handler matches.
    ///
    /// Builtin exception names keep their exact tag. Any other exception
    /// expression — a user class declared in this unit (`except ReprError:`),
    /// an imported class (`except ColorParseError:`) or a dotted class
    /// (`except errors.StyleSyntaxError:`) — has no runtime tag, so it is
    /// approximated with [`ExcKind::RuntimeError`], the tag `raise` uses for
    /// user classes. Documented limitation: such a handler also catches any
    /// other user-class raise, and an unknown builtin-looking name is accepted
    /// rather than diagnosed.
    ///
    /// Returns `None` after reporting when the handler expression is not a
    /// name, a dotted name or a non-empty tuple of those.
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
            // Lower the handler body once; tuple handlers share it.
            let handler_body = self.lower_block(&h.body, fs);
            if is_bare {
                handlers.push(HHandler {
                    exc: None,
                    binding,
                    body: handler_body,
                });
            } else {
                for kind in kinds {
                    handlers.push(HHandler {
                        exc: Some(kind),
                        binding,
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

    /// Lower `with a as x, b as y:` into nested `try/finally` blocks.
    ///
    /// Each context expression is evaluated exactly once into a hidden
    /// `$withN` local. Its `__enter__()` result is bound to the `as` target
    /// (the call still runs without a target). The body is then wrapped in
    /// one `try` per item (innermost for the last item) whose `finally` calls
    /// `__exit__(None, None, None)` on the hidden local, so `return`,
    /// `break`, `continue` and exceptions still exit every entered object in
    /// reverse order. The return value of `__exit__` is ignored: exceptions
    /// are never suppressed.
    fn lower_with(
        &mut self,
        s: &ast::StmtWith,
        fs: &mut FuncState,
        _line: u32,
        _col: u32,
    ) -> Option<HStmt> {
        self.lower_with_items(&s.items, &s.body, s.range, fs)
    }

    /// Shared `with`/`async with` lowering. `async with` is sequential: the
    /// synchronous `try/finally` + `__enter__`/`__exit__` protocol is used
    /// unchanged.
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

        // Build nested `try/finally` blocks from the inside out. Each item's
        // context expression, `__enter__()` call and `as` binding are
        // evaluated inside its own try, so a failure while evaluating a later
        // context manager still exits the earlier ones. The finally arm only
        // calls `__exit__()` when the hidden local was actually assigned;
        // locals start as `None`, and `if temp != None` preserves the
        // original exception otherwise.
        let mut body = self.lower_block(with_body, fs);
        for item in items.iter().rev() {
            let (item_line, item_col) = self.span(item.context_expr.range());
            let value = self.expr(&item.context_expr, fs);
            let (temp, temp_ref) = self.with_temp(fs, item.context_expr.range());

            let mut inner_body = Vec::new();
            inner_body.push(HStmt {
                kind: HStmtKind::Assign {
                    target: temp,
                    value,
                },
                line: item_line,
                col: item_col,
            });
            // `with EXPR:` calls `__enter__()` even without an `as` target;
            // the result is bound to the target when one is given.
            let enter = HExpr {
                kind: HExprKind::MethodCall {
                    object: Box::new(temp_ref),
                    method: "__enter__".to_string(),
                    args: Vec::new(),
                    star: None,
                    keywords: Vec::new(),
                },
                ty: self.infer.fresh(),
                line: item_line,
                col: item_col,
            };
            match item.optional_vars.as_deref() {
                None => inner_body.push(HStmt {
                    kind: HStmtKind::Expr(enter),
                    line: item_line,
                    col: item_col,
                }),
                Some(ast::Expr::Name(name)) => match fs.lookup(name.id.as_str()) {
                    Some(local) => inner_body.push(HStmt {
                        kind: HStmtKind::Assign {
                            target: local,
                            value: enter,
                        },
                        line: item_line,
                        col: item_col,
                    }),
                    None => self.error_name(
                        name.range,
                        name.id.as_str(),
                        fs,
                        format!("assignment to unknown name '{}'", name.id),
                    ),
                },
                Some(other) => self.error_at(
                    other.range(),
                    "with ... as only supports a simple name target",
                ),
            }
            inner_body.extend(body);

            let object = HExpr {
                kind: HExprKind::Local(temp),
                ty: self.infer.fresh(),
                line: item_line,
                col: item_col,
            };
            let none = HExpr {
                kind: HExprKind::Const(Const::None),
                ty: Ty::Concrete(Type::Any),
                line: item_line,
                col: item_col,
            };
            let condition = HExpr {
                kind: HExprKind::Bin {
                    op: BinOp::Ne,
                    lhs: Box::new(object.clone()),
                    rhs: Box::new(none.clone()),
                },
                ty: self.infer.fresh(),
                line: item_line,
                col: item_col,
            };
            let exit = HExpr {
                kind: HExprKind::MethodCall {
                    object: Box::new(object),
                    method: "__exit__".to_string(),
                    args: vec![none.clone(), none.clone(), none],
                    star: None,
                    keywords: Vec::new(),
                },
                ty: self.infer.fresh(),
                line: item_line,
                col: item_col,
            };
            let guard = HStmt {
                kind: HStmtKind::If {
                    cond: condition,
                    then_body: vec![HStmt {
                        kind: HStmtKind::Expr(exit),
                        line: item_line,
                        col: item_col,
                    }],
                    else_body: Vec::new(),
                },
                line: item_line,
                col: item_col,
            };
            body = vec![HStmt {
                kind: HStmtKind::Try {
                    body: inner_body,
                    handlers: Vec::new(),
                    else_body: Vec::new(),
                    finally_body: vec![guard],
                },
                line: item_line,
                col: item_col,
            }];
        }

        Some(body.pop().expect("with always wraps into one statement"))
    }

    // ------------------------------------------------------------------
    // Expressions
    // ------------------------------------------------------------------

    fn expr(&mut self, e: &ast::Expr, fs: &mut FuncState) -> HExpr {
        let (line, col) = self.span(e.range());
        match e {
            ast::Expr::Constant(c) => self.const_expr(c, e.range(), line, col),
            ast::Expr::Name(n) => {
                let name = n.id.as_str();
                if let Some(local) = fs.lookup(name) {
                    let ty = self.infer.fresh();
                    HExpr {
                        kind: HExprKind::Local(local),
                        ty,
                        line,
                        col,
                    }
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
                } else if let Some(attribute) = self
                    .class_attr_scopes
                    .last()
                    .and_then(|scope| scope.get(name))
                    .cloned()
                {
                    // A class body may refer to attributes declared earlier
                    // in the same class namespace.
                    attribute
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
                    ast::BoolOp::And => pyx_ir::BoolOp::And,
                    ast::BoolOp::Or => pyx_ir::BoolOp::Or,
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
                            op: pyx_ir::BoolOp::And,
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
                    if let Some(expr) = self.module_attr_expr(module, member, line, col) {
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
            ast::Expr::GeneratorExp(c) => {
                let (clauses, saved) = self.lower_comp_clauses(&c.generators, fs);
                let element = self.expr(&c.elt, fs);
                self.restore_comp_bindings(fs, saved);
                HExpr {
                    kind: HExprKind::GeneratorExp {
                        element: Box::new(element),
                        clauses,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                }
            }
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
                // The value is evaluated before the binding takes effect, so
                // `x := x + 1` reads the existing local; for a fresh name the
                // value reads the outer binding (Python treats the name as
                // local for the whole function and raises `UnboundLocalError`).
                let value = self.expr(&n.value, fs);
                let local = self.walrus_local(fs, name);
                HExpr {
                    kind: HExprKind::NamedExpr {
                        target: local,
                        value: Box::new(value),
                    },
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

        for arg in l.args.posonlyargs.iter().chain(l.args.args.iter()) {
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
            comp_shadowed: HashMap::new(),
            globals: HashSet::new(),
            loop_depth: 0,
            in_lambda: true,
            is_async: false,
            is_generator: false,
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

    /// Lower a list display containing `*` unpacking, e.g.
    /// `["a", *xs, "b"]`.
    ///
    /// The display becomes a chain of list concatenations: each run of plain
    /// elements is a list literal and each starred operand is the right-hand
    /// side of a `+`. The runtime list-concat operator rejects non-list
    /// operands with `TypeError`, and the left-to-right evaluation order of
    /// `+` matches the source order. A leading starred operand starts from an
    /// empty list, which also copies the operand like Python's display does.
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

    /// Lower a dict display containing `**` unpacking, e.g.
    /// `{**h, "k": v}`.
    ///
    /// Literal pairs keep their own `Dict` node and every unpacked mapping is
    /// merged left to right with the internal `DictMerge` builtin, which
    /// preserves insertion order and last-wins semantics. A display that
    /// starts with `**` is merged onto an empty dict so the result is a copy.
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
            ast::Constant::Int(value) => match value.to_str_radix(10).parse::<i64>() {
                Ok(v) => (Const::Int(v), Ty::Concrete(Type::Int)),
                Err(_) => {
                    self.error_at(range, "integer literal is out of the i64 range");
                    (Const::Int(0), Ty::Concrete(Type::Int))
                }
            },
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

    fn call_expr(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        // A starred positional or `**kwargs` operand can only be expanded at
        // runtime. Receiver method calls with a single starred operand keep
        // their dedicated ABI (`pyx_method_star`); the other shapes lower to
        // the general starred-call ABI, or to a deferred call when they
        // cannot be represented (keywords, `**`).
        if call_has_dynamic_arguments(call) {
            // A receiver method keeps its dedicated `pyx_method_star` ABI
            // (unchanged). A module/user-module/extension member is not a
            // method: its attribute lowers to a first-class function value
            // and the call uses the general starred-call ABI below.
            let method_star = match &*call.func {
                ast::Expr::Attribute(attr) => {
                    call.keywords.iter().all(|keyword| keyword.arg.is_some())
                        && self.resolve_module_chain(&attr.value, fs).is_none()
                        && self.resolve_user_module(&attr.value, fs).is_none()
                        && self.resolve_extension_alias(&attr.value, fs).is_none()
                        && self.resolve_deferred_import(&call.func, fs).is_none()
                }
                _ => false,
            };
            if !method_star {
                return self.lower_starred_or_deferred_call(call, fs, line, col);
            }
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
                    if !call.keywords.is_empty() {
                        return self.lower_deferred_call(call, fs, line, col);
                    }
                    let Some(args) = self.plain_call_args(call, fs) else {
                        return self.error_expr(call.range);
                    };
                    let callee = self.deferred_import_expr(&module, member.as_deref(), line, col);
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
                let Some((args, star, keywords)) = self.call_arguments(call, fs) else {
                    return self.error_expr(call.range);
                };
                return HExpr {
                    kind: HExprKind::MethodCall {
                        object: Box::new(object),
                        method: attr.attr.to_string(),
                        args,
                        star: star.map(Box::new),
                        keywords,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
            other => {
                // Any other callee is a function value: Python evaluates the
                // callee before the arguments. Keywords cannot be bound
                // statically, so such calls are deferred.
                if !call.keywords.is_empty() {
                    return self.lower_deferred_call(call, fs, line, col);
                }
                let callee = self.expr(other, fs);
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

        if let Some(local) = fs.lookup(name) {
            // A class of this unit reorders keywords into its `__init__`
            // parameter slots and fills omitted defaults at compile time.
            // Only the module body holds the class value directly; inside a
            // function a local slot with the same name shadows it.
            if fs.is_module {
                if let Some(signature) = self.class_init_signature(name) {
                    let Some(args) =
                        self.class_constructor_arguments(name, &signature, call, fs, line, col)
                    else {
                        return self.error_expr(call.range);
                    };
                    let (callee_line, callee_col) = self.span(name_expr.range);
                    let callee = HExpr {
                        kind: HExprKind::Local(local),
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
            }
            // Keyword binding for a function value is not part of the static
            // ABI; the call still compiles and fails at execution.
            if !call.keywords.is_empty() {
                return self.lower_deferred_call(call, fs, line, col);
            }
            let (callee_line, callee_col) = self.span(name_expr.range);
            let callee = HExpr {
                kind: HExprKind::Local(local),
                ty: self.infer.fresh(),
                line: callee_line,
                col: callee_col,
            };
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

        if let Some(&id) = self.imported_user_functions.get(name) {
            // The signature of an imported function is not part of this
            // unit; the runtime binder handles the positional arguments, so
            // any keyword call is deferred.
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

        if let Some((module, exported)) = self.imported_user_globals.get(name).cloned() {
            if !call.keywords.is_empty() {
                return self.lower_deferred_call(call, fs, line, col);
            }
            let Some(args) = self.plain_call_args(call, fs) else {
                return self.error_expr(call.range);
            };
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

        if let Some((_module, constant)) = self.imported_constants.get(name).copied() {
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
            if !call.keywords.is_empty() {
                return self.lower_deferred_call(call, fs, line, col);
            }
            let Some(args) = self.plain_call_args(call, fs) else {
                return self.error_expr(call.range);
            };
            let (callee_line, callee_col) = self.span(name_expr.range);
            let callee = self.deferred_import_expr(
                &binding.module,
                binding.member.as_deref(),
                callee_line,
                callee_col,
            );
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
        if let Some(builtin) = builtin_by_name(name) {
            let Some(args) = self.builtin_call_arguments(builtin, call, fs, line, col) else {
                return self.error_expr(call.range);
            };
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

        if self.global_names.contains(name) {
            // A same-unit class used as a callee from a function or method:
            // bind its constructor arguments exactly like a module-body call.
            if let Some(signature) = self.class_init_signature(name) {
                let Some(args) =
                    self.class_constructor_arguments(name, &signature, call, fs, line, col)
                else {
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

        // Zero-argument `super()` needs runtime class-context tracking that
        // this runtime does not provide. Keep the expression compilable but
        // make an actual evaluation fail clearly.
        if name == "super" && call.args.is_empty() && call.keywords.is_empty() {
            return self.deferred_import_expr("super()", None, line, col);
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

    /// Lower a general call with a single starred positional operand.
    ///
    /// `callee(args..., *star)` keeps Python's evaluation order: the callee
    /// first, then the positional operands that precede the star, then the
    /// starred iterable. The IR keeps the leading operands and the star
    /// separately and codegen hands them to the runtime's starred-call ABI,
    /// which flattens a list/tuple into positional arguments.
    ///
    /// A second `*`, a positional operand after the star, a `**` keyword and
    /// (for now) any keyword argument cannot be represented by that ABI: the
    /// first two are diagnosed at the call site, the others stay a deferred
    /// call that evaluates its operands and reports the unsupported dynamic
    /// call at runtime.
    fn lower_starred_or_deferred_call(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        let mut star_index = None;
        for (index, arg) in call.args.iter().enumerate() {
            if let ast::Expr::Starred(starred) = arg {
                if star_index.is_some() {
                    self.error_at_code(
                        starred.range,
                        codes::CALL,
                        "only one starred argument is supported",
                    );
                    return self.error_expr(call.range);
                }
                star_index = Some(index);
            }
        }
        let Some(star_index) = star_index else {
            // Only `**kwargs` made the call dynamic.
            return self.lower_deferred_call(call, fs, line, col);
        };
        if !call.keywords.is_empty() {
            // Keyword binding for a starred call is not part of the general
            // call ABI yet; the deferred path keeps the evaluation order and
            // reports it at runtime.
            return self.lower_deferred_call(call, fs, line, col);
        }
        if star_index + 1 != call.args.len() {
            self.error_at_code(
                call.args[star_index + 1].range(),
                codes::CALL,
                "a positional argument cannot follow a starred argument",
            );
            return self.error_expr(call.range);
        }

        let callee = self.starred_callee(&call.func, fs);
        let mut args = Vec::with_capacity(star_index);
        for arg in &call.args[..star_index] {
            args.push(self.expr(arg, fs));
        }
        let ast::Expr::Starred(starred) = &call.args[star_index] else {
            // `star_index` was found above.
            return self.error_expr(call.range);
        };
        let star = self.expr(&starred.value, fs);
        HExpr {
            kind: HExprKind::IndirectCall {
                callee: Box::new(callee),
                args,
                star: Some(Box::new(star)),
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

    /// Lower positional arguments, one optional starred operand and the
    /// keyword pairs of a method call. Source order is preserved: plain
    /// arguments, then the starred operand, then the keywords.
    ///
    /// The method ABI has no separate positional buffer next to the starred
    /// operand, so a call may not mix the two forms.
    fn call_arguments(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
    ) -> Option<LoweredCallArguments> {
        let mut args = Vec::with_capacity(call.args.len());
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
                if !args.is_empty() {
                    self.error_at_code(
                        starred.range,
                        codes::CALL,
                        "a starred argument cannot be combined with positional arguments",
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
        for keyword in &call.keywords {
            let Some(name) = keyword.arg.as_ref() else {
                self.error_at_code(
                    keyword.range,
                    codes::CALL,
                    "**kwargs arguments are not supported",
                );
                return None;
            };
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
        Some((args, star, keywords))
    }

    /// Cloned signature of a function of this unit.
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
        let Some(signature) = self.call_signature(id) else {
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
            for index in capture_count..positional_end {
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

    /// Signature of a same-unit class's `__init__`, or `None` when the class
    /// has no `__init__` or its signature needs the runtime binder
    /// (`*args`, keyword-only parameters or `**kwargs`).
    fn class_init_signature(&self, class: &str) -> Option<CallSignature> {
        let &init = self.class_inits.get(class)?;
        let signature = self.call_signature(init)?;
        if signature.has_dynamic_signature() || signature.capture_count != 0 {
            return None;
        }
        // Slot 0 is `self`; a class constructor supplies the rest.
        (signature.params.len() > 1).then_some(signature)
    }

    /// Bind `Class(args..., name=value...)` against its `__init__` signature.
    ///
    /// The runtime fills `self` when it instantiates the class, so only the
    /// remaining parameters take call arguments. Keywords move into their
    /// parameter slots; omitted parameters use the parameter's literal default
    /// (a `field(...)` default is recorded as `None` and resolved by the
    /// generated body). The caller always passes the full positional arity.
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
            let Some(index) = signature.params[1..]
                .iter()
                .position(|(name, _, _)| name.as_str() == keyword_name.as_str())
            else {
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

    /// Lower a direct builtin call, binding the keywords the builtin supports.
    ///
    /// `max`/`min` accept `key=`: the runtime expects the key slot as the
    /// final argument, so it is appended (with `None` when omitted).
    ///
    /// `sorted` accepts `key=` and `reverse=`: the runtime expects
    /// `(iterable, key, reverse)` with the two optional slots materialised at
    /// their fixed positions.
    fn builtin_call_arguments(
        &mut self,
        builtin: Builtin,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<Vec<HExpr>> {
        let mut args = self.plain_call_args(call, fs)?;
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

    fn un_op(&mut self, op: ast::UnaryOp, _range: TextRange) -> Option<pyx_ir::UnOp> {
        Some(match op {
            ast::UnaryOp::Not => pyx_ir::UnOp::Not,
            ast::UnaryOp::UAdd => pyx_ir::UnOp::Pos,
            ast::UnaryOp::USub => pyx_ir::UnOp::Neg,
            ast::UnaryOp::Invert => pyx_ir::UnOp::Invert,
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

/// Decorators accepted and ignored by lowering.
///
/// Their names come from `functools`, `abc` and `typing`; the decorator
/// expressions are never evaluated, so parametrized forms cost nothing.
fn is_ignored_function_decorator(expr: &ast::Expr) -> bool {
    matches!(
        decorator_final_name(expr).as_deref(),
        Some("lru_cache" | "cache" | "wraps" | "abstractmethod" | "overload" | "runtime_checkable")
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

/// Bounds of the integers the runtime can encode as tagged immediates.
///
/// Arithmetic whose result leaves this range raises `OverflowError` at
/// runtime, so such expressions must not be folded.
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

/// Wraps an integer result, rejecting values the runtime cannot represent.
fn fold_int_result(n: i64) -> Option<Const> {
    (MIN_TAGGED_INT..=MAX_TAGGED_INT)
        .contains(&n)
        .then_some(Const::Int(n))
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

/// Float arithmetic following the runtime's `pyx_binop` semantics. Division
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

fn fold_un(op: pyx_ir::UnOp, operand: &HExpr) -> Option<Const> {
    let HExprKind::Const(constant) = &operand.kind else {
        return None;
    };
    match op {
        pyx_ir::UnOp::Neg => match constant {
            Const::Int(n) => fold_int_result(n.checked_neg()?),
            Const::Float(f) => Some(Const::Float(-f)),
            _ => None,
        },
        pyx_ir::UnOp::Not => match constant {
            Const::Bool(b) => Some(Const::Bool(!b)),
            _ => None,
        },
        pyx_ir::UnOp::Invert => match constant {
            // Python: `~x == -x - 1`. Tagged ints are far inside `i64`, so the
            // complement never overflows.
            Const::Int(n) => fold_int_result(!n),
            Const::Bool(b) => fold_int_result(!i64::from(*b)),
            _ => None,
        },
        pyx_ir::UnOp::Pos => None,
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

/// Names that become locals of the enclosing function.
///
/// `include_defs` controls whether nested function definitions bind their
/// name in this scope. It is `true` for function bodies (a nested `def`
/// stores its closure in a local) and `false` for the module body, where
/// functions live in the function table instead of module locals. Nested
/// function bodies and lambdas are never descended into: they have their own
/// scopes.
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

/// True when a call has a starred positional operand or a `**` keyword. Such
/// operands cannot be expanded at compile time.
fn call_has_dynamic_arguments(call: &ast::ExprCall) -> bool {
    call.args
        .iter()
        .any(|arg| matches!(arg, ast::Expr::Starred(_)))
        || call.keywords.iter().any(|keyword| keyword.arg.is_none())
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

/// The `(name, local)` pair mirrored by a module-body assignment or class
/// definition, if the statement is a simple-name `Assign`/`AnnAssign`/
/// `AugAssign` or a `ClassDef`.
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
        (ast::Stmt::ClassDef(s), HStmtKind::ClassDef { target, .. }) => {
            Some((s.name.to_string(), *target))
        }
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
            ast::Stmt::Assert(s) => {
                walk_free_expr(&s.test, shadowed, out, seen);
                if let Some(msg) = &s.msg {
                    walk_free_expr(msg, shadowed, out, seen);
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
    // `global` names resolve through the module's global table.
    collect_scope_globals(body, &mut locals);

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
        ast::Expr::Name(n) => record_name_load(n.id.as_str(), shadowed, out, seen),
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
