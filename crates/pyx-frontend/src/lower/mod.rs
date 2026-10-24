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
use pyx_ir::{BinOp, Builtin, Callee, Const, ExcKind, FunctionId, LocalId, Type};
use pyx_ir::{PYTHON_ABI_TAG, PYTHON_VERSION_STRING};
use rustpython_parser::ast::{self, Ranged};
use rustpython_parser::text_size::TextRange;

use crate::diag::{self, codes, line_col, Diagnostic};
use crate::sema::{Infer, Ty};

pub const MODULE_NAME: &str = "<module>";

const BUILTIN_NAMES: [&str; 18] = [
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
}

#[derive(Clone, Debug)]
pub struct HLocal {
    pub name: String,
    pub ty: Ty,
}

#[derive(Clone, Debug)]
pub struct HStmt {
    pub kind: HStmtKind,
    pub line: u32,
    pub col: u32,
}

#[derive(Clone, Debug)]
pub enum HStmtKind {
    Expr(HExpr),
    Assign {
        target: LocalId,
        value: HExpr,
    },
    /// `a, b, c = value`: the value must be a list or tuple of exactly
    /// `targets.len()` elements.
    Unpack {
        targets: Vec<LocalId>,
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
    While {
        cond: HExpr,
        body: Vec<HStmt>,
    },
    ForRange {
        var: LocalId,
        start: HExpr,
        stop: HExpr,
        step: Option<HExpr>,
        body: Vec<HStmt>,
    },
    ForEach {
        var: LocalId,
        iterable: HExpr,
        body: Vec<HStmt>,
    },
    SetSubscript {
        object: HExpr,
        index: HExpr,
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
        methods: Vec<(String, FunctionId)>,
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

/// Target binding inside a comprehension clause. Targets always bind
/// synthetic `$compN` locals; user-visible names are remapped while the
/// comprehension is lowered and restored afterwards.
#[derive(Clone, Debug)]
pub enum HCompTarget {
    Name(LocalId),
    Unpack(Vec<LocalId>),
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

#[derive(Clone, Debug)]
pub enum HExprKind {
    Const(Const),
    Local(LocalId),
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
        /// Keyword arguments in source order; the runtime binds them.
        keywords: Vec<(String, HExpr)>,
    },
    /// Reference to a named function or lambda as a value.
    FunctionRef(FunctionId),
    /// Reference to a builtin function as a first-class value (`f = int`).
    BuiltinRef(Builtin),
    /// `module.function(args...)` resolved from `pyx_ir::stdlib`.
    ModuleCall {
        module: String,
        function: String,
        args: Vec<HExpr>,
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
    },
    Call {
        callee: Callee,
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

/// A class header/body registered before any function body is lowered.
#[derive(Clone, Debug)]
struct ClassPlan {
    /// Index of the single base expression in `ClassDef::bases`, if any.
    base: Option<usize>,
    /// Method name, function id and the index of its `FunctionDef` in the
    /// class body. Methods are lowered after their signatures are registered.
    methods: Vec<(String, FunctionId, usize)>,
}

struct FuncState {
    fid: FunctionId,
    is_module: bool,
    locals: HashMap<String, LocalId>,
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
fn prefix_site_packages(prefix: &Path) -> Vec<PathBuf> {
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

const SUPPORTED_MODULES_HELP: &str = "supported modules: math, io, functools, operator, time, sys, json, os, os.path, _thread, threading, queue, multiprocessing, pickle";

/// A user module (a sibling `.py` file) visible to this unit.
///
/// The loader resolves the file and lowers dependencies first, so exported
/// functions already have final [`FunctionId`]s by the time this unit is
/// lowered.
#[derive(Clone, Debug, Default)]
pub struct UserModule {
    /// Module name as written in the import statement.
    pub name: String,
    /// Top-level functions exported by the module: name -> function id.
    pub exports: HashMap<String, FunctionId>,
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
    /// Base id for this unit's functions. Multi-module programs reserve a
    /// contiguous id range per module; single-source checks use `0`.
    fid_base: FunctionId,
    /// Value bound to the module body's `__name__`.
    dunder_name: String,
    /// Module-level names mirrored into the runtime global table. Only simple
    /// assignment targets are tracked: those are the statements that emit a
    /// [`HStmtKind::GlobalSet`].
    global_names: HashSet<String>,
    /// Global names actually read by a function or method of this unit. The
    /// module body only mirrors those, so units whose functions never read a
    /// module-level value gain no extra statements.
    used_globals: HashSet<String>,
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
    /// Registered class headers, keyed by the class statement's start offset.
    /// Method signatures exist before any body is lowered; the key lets
    /// `lower_stmt` find the plan without threading it through every call.
    class_defs: HashMap<usize, ClassPlan>,
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
            fid_base: 0,
            dunder_name: "__main__".to_string(),
            global_names: HashSet::new(),
            used_globals: HashSet::new(),
            imported_functions: HashMap::new(),
            imported_constants: HashMap::new(),
            unpack_counter: 0,
            comp_counter: 0,
            with_counter: 0,
            aug_counter: 0,
            class_defs: HashMap::new(),
            diags: Vec::new(),
        }
    }

    /// Reserves the [`FunctionId`] range starting at `base` for this unit.
    pub fn set_fid_base(&mut self, base: FunctionId) {
        self.fid_base = base;
    }

    /// Sets the string bound to `__name__` in the module body.
    pub fn set_dunder_name(&mut self, name: &str) {
        self.dunder_name = name.to_string();
    }

    /// Makes resolved user modules (and their exported functions) visible.
    pub fn set_user_modules(&mut self, modules: HashMap<String, UserModule>) {
        self.user_modules = modules;
    }

    /// Top-level function bindings of this unit (name -> already offset id).
    ///
    /// These are exactly the functions a user module exports; methods,
    /// lambdas, nested functions and the module body are not included.
    pub fn exported_functions(&self) -> &HashMap<String, FunctionId> {
        &self.fn_ids
    }

    pub fn lower_module(&mut self, module: &ast::ModModule) -> LHir {
        // 1. Signatures first: forward calls and recursion are both legal.
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

        // 2. Module locals are known before we lower any function body; this
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
        collect_global_names(&module.body, &mut self.global_names);

        // 3. Module-level imports bind global names. They are collected before
        //    any function body is lowered so imported names are visible inside
        //    functions regardless of where the import appears in the file.
        //    Import statements themselves emit no IR.
        self.collect_imports(&module.body);

        // 3b. `__name__` is available as a module-body local. Only inject the
        //     initial assignment when the body actually loads the name, so
        //     modules without it gain no extra statement.
        let mut dunder_init = None;
        if module_refers_to_dunder_name(&module.body) {
            let local = match module_locals.get("__name__") {
                Some(&local) => local,
                None => {
                    let local = self.functions[0].locals.len() as LocalId;
                    self.functions[0].locals.push(HLocal {
                        name: "__name__".to_string(),
                        ty: Ty::Concrete(Type::Str),
                    });
                    module_locals.insert("__name__".to_string(), local);
                    self.module_local_names.insert("__name__".to_string());
                    local
                }
            };
            dunder_init = Some(HStmt {
                kind: HStmtKind::Assign {
                    target: local,
                    value: HExpr {
                        kind: HExprKind::Const(Const::Str(self.dunder_name.clone())),
                        ty: Ty::Concrete(Type::Str),
                        line: 1,
                        col: 1,
                    },
                },
                line: 1,
                col: 1,
            });
        }

        // 4. Function bodies (all signatures are already registered). Class
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
                    for (_, fid, method_idx) in plan.methods {
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

        // 5. Module body. Function definitions and imports are declarations
        //    only; imports were resolved in step 3.
        let mut fs = FuncState {
            fid: 0,
            is_module: true,
            locals: module_locals,
            loop_depth: 0,
            in_lambda: false,
            is_async: false,
            is_generator: false,
        };
        let mut body = Vec::new();
        for stmt in &module.body {
            if matches!(
                stmt,
                ast::Stmt::FunctionDef(_)
                    | ast::Stmt::AsyncFunctionDef(_)
                    | ast::Stmt::Import(_)
                    | ast::Stmt::ImportFrom(_)
            ) {
                continue;
            }
            if let Some(s) = self.lower_stmt(stmt, &mut fs) {
                self.push_lowered_stmt(stmt, s, &fs, &mut body);
            }
        }
        self.functions[0].body = {
            if let Some(init) = dunder_init {
                body.insert(0, init);
            }
            body
        };

        LHir {
            source_name: self.filename.to_string(),
            functions: std::mem::take(&mut self.functions),
        }
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

    /// Resolve module-level imports into global binding tables.
    ///
    /// Import statements emit no IR: they only affect name resolution. Only
    /// direct module-body statements are imports; nested imports are reported
    /// when the enclosing block is lowered. Running this before any function
    /// body makes imported names visible in functions and order-independent.
    fn collect_imports(&mut self, stmts: &[ast::Stmt]) {
        for stmt in stmts {
            match stmt {
                ast::Stmt::Import(s) => {
                    for alias in &s.names {
                        self.collect_import(alias);
                    }
                }
                ast::Stmt::ImportFrom(s) => {
                    self.collect_import_from(s);
                }
                _ => {}
            }
        }
    }

    fn collect_import(&mut self, alias: &ast::Alias) {
        let imported = alias.name.as_str();
        let binding = match &alias.asname {
            Some(name) => name.as_str(),
            None => imported,
        };
        if let Some(module) = stdlib::module(imported) {
            self.module_aliases.insert(binding.to_string(), module);
            return;
        }
        if self.user_modules.contains_key(imported) {
            self.user_module_aliases
                .insert(binding.to_string(), imported.to_string());
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
        self.error_unsupported_module(alias.range, imported);
    }

    fn collect_import_from(&mut self, s: &ast::StmtImportFrom) {
        let Some(module_name) = &s.module else {
            self.error_at_code(
                s.range,
                codes::UNSUPPORTED,
                "relative imports are not supported in Phase 2.10",
            );
            return;
        };
        if s.level.as_ref().is_some_and(|level| level.to_u32() != 0) {
            self.error_at_code(
                s.range,
                codes::UNSUPPORTED,
                "relative imports are not supported in Phase 2.10",
            );
            return;
        }
        let imported = module_name.as_str();
        let module = match stdlib::module(imported) {
            Some(module) => module,
            None => {
                let Some(user) = self.user_modules.get(imported).cloned() else {
                    match self.find_extension(imported) {
                        ExtensionLookup::Found(path) => {
                            let path = path.to_string_lossy().into_owned();
                            self.collect_extension_members(s, imported, &path);
                            return;
                        }
                        ExtensionLookup::Incompatible { path, reason } => {
                            self.error_incompatible_extension(s.range, &path, &reason);
                            return;
                        }
                        ExtensionLookup::Missing => {
                            self.error_unsupported_module(s.range, imported);
                            return;
                        }
                    }
                };
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
                    match user.exports.get(member) {
                        Some(&id) => {
                            self.imported_user_functions.insert(binding.to_string(), id);
                        }
                        None => self.error_at_code(
                            alias.range,
                            codes::ATTRIBUTE,
                            format!(
                                "module '{imported}' does not export a function named '{member}'"
                            ),
                        ),
                    }
                }
                return;
            }
        };
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
    fn collect_extension_members(&mut self, s: &ast::StmtImportFrom, module: &str, path: &str) {
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

    /// Resolves a plain name to an imported user module. Locals shadow
    /// imports, matching [`Self::resolve_module_chain`].
    fn resolve_user_module(&self, expr: &ast::Expr, fs: &FuncState) -> Option<UserModule> {
        let ast::Expr::Name(name) = expr else {
            return None;
        };
        if fs.lookup(name.id.as_str()).is_some() {
            return None;
        }
        let module_name = self.user_module_aliases.get(name.id.as_str())?;
        self.user_modules.get(module_name.as_str()).cloned()
    }

    /// Lowers `module.f(...)` for a function exported by a user module.
    fn user_module_call(
        &mut self,
        module: &UserModule,
        member: &str,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
        let Some(&id) = module.exports.get(member) else {
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
                let Some(args) = self.module_args(call, fs, function, line, col) else {
                    return self.error_expr(call.range);
                };
                HExpr {
                    kind: HExprKind::ModuleCall {
                        module: module.name.to_string(),
                        function: function.name.to_string(),
                        args,
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

        if global {
            if self.fn_ids.contains_key(&name) {
                self.error_at_code(
                    decl.range,
                    codes::DECL,
                    format!("duplicate function definition '{name}'"),
                );
                return None;
            }
            if BUILTIN_NAMES.contains(&name.as_str()) {
                self.error_at_code(
                    decl.range,
                    codes::DECL,
                    format!("function '{name}' shadows a Phase 1 builtin; choose another name"),
                );
                return None;
            }
        }

        self.create_function_named(&decl, name, global)
    }

    /// Register a method as a plain function named `Class.method`.
    ///
    /// Methods never enter [`Lowerer::fn_ids`]: a bare `method` name is not
    /// callable, and different classes may define the same method name.
    fn create_method_function(
        &mut self,
        f: &ast::StmtFunctionDef,
        class: &str,
    ) -> Option<FunctionId> {
        let name = format!("{class}.{}", f.name);
        self.create_function_named(&FnDecl::from_def(f), name, false)
    }

    fn create_async_method_function(
        &mut self,
        f: &ast::StmtAsyncFunctionDef,
        class: &str,
    ) -> Option<FunctionId> {
        let name = format!("{class}.{}", f.name);
        self.create_function_named(&FnDecl::from_async_def(f), name, false)
    }

    /// Shared signature lowering. `global` controls the module-level checks
    /// (duplicate definitions, builtin shadowing) that do not apply to
    /// methods.
    fn create_function_named(
        &mut self,
        decl: &FnDecl<'_>,
        name: String,
        global: bool,
    ) -> Option<FunctionId> {
        self.create_function_with_captures(decl, name, global, &[])
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
    ) -> Option<FunctionId> {
        if !decl.decorators.is_empty() {
            self.error_at(decl.range, "decorators are not supported in Phase 1");
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
            if arg.default.is_some() {
                self.error_at(
                    arg.def.range,
                    "default parameter values are not supported in Phase 1",
                );
            }
            self.add_param(&arg.def, &mut params, &mut locals, &mut seen);
        }
        if let Some(vararg) = &args.vararg {
            self.error_at(
                vararg.range,
                "*args parameters are not supported in Phase 1",
            );
        }
        if let Some(kwarg) = &args.kwarg {
            self.error_at(
                kwarg.range,
                "**kwargs parameters are not supported in Phase 1",
            );
        }
        if let Some(first) = args.kwonlyargs.first() {
            self.error_at(
                first.def.range,
                "keyword-only parameters are not supported in Phase 1",
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

    /// Validate a class header/body and register every method signature.
    ///
    /// The class body may contain method definitions, `pass` and a docstring;
    /// everything else (class attributes, nested classes, `if` blocks, ...) is
    /// rejected with a diagnostic. The returned plan is used to lower the class
    /// statement and its method bodies later.
    fn register_class(&mut self, s: &ast::StmtClassDef) -> ClassPlan {
        let class = s.name.to_string();
        if !s.decorator_list.is_empty() {
            self.error_at(s.range, "class decorators are not supported yet");
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

        let mut methods: Vec<(String, FunctionId, usize)> = Vec::new();
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
            let (method, range) = match stmt {
                ast::Stmt::FunctionDef(f) => (f.name.to_string(), f.range),
                ast::Stmt::AsyncFunctionDef(f) => (f.name.to_string(), f.range),
                other => {
                    self.error_at(
                        other.range(),
                        "class body may only contain method definitions (plus pass or a docstring); class attributes are not supported yet",
                    );
                    continue;
                }
            };
            if !seen.insert(method.clone()) {
                self.error_at_code(
                    range,
                    codes::DECL,
                    format!("duplicate method definition '{class}.{method}'"),
                );
                continue;
            }
            let id = match stmt {
                ast::Stmt::FunctionDef(f) => self.create_method_function(f, &class),
                ast::Stmt::AsyncFunctionDef(f) => self.create_async_method_function(f, &class),
                _ => unreachable!("method statement matched above"),
            };
            if let Some(id) = id {
                methods.push((method, id, idx));
            }
        }

        ClassPlan { base, methods }
    }

    fn add_param(
        &mut self,
        arg: &ast::Arg,
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
        let ty = match &arg.annotation {
            Some(ann) => self.annotation_ty(ann),
            None => self.infer.fresh(),
        };
        let local = locals.len() as LocalId;
        params.push(HParam {
            local,
            name: name.clone(),
            ty,
        });
        locals.push(HLocal { name, ty });
    }

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
                other => {
                    self.error_at_code(
                        ann.range(),
                        codes::DECL,
                        format!("unsupported type annotation '{other}'"),
                    );
                    Ty::Concrete(Type::Any)
                }
            },
            ast::Expr::Constant(c) if matches!(c.value, ast::Constant::None) => {
                Ty::Concrete(Type::None)
            }
            _ => {
                self.error_at_code(ann.range(), codes::DECL, "unsupported type annotation");
                Ty::Concrete(Type::Any)
            }
        }
    }

    /// Lower a function body and set up its [`FuncState`].
    ///
    /// The `async`/generator flags were decided when the signature was
    /// registered, so `await`/`yield` lowering only has to read them.
    fn lower_function_body(&mut self, fid: FunctionId, body: &[ast::Stmt]) -> Vec<HStmt> {
        let mut names = Vec::new();
        collect_names(body, &mut names, true);

        let mut local_map: HashMap<String, LocalId> = HashMap::new();
        for (idx, local) in self.functions[fid as usize].locals.iter().enumerate() {
            local_map.insert(local.name.clone(), idx as LocalId);
        }
        for name in names {
            if local_map.contains_key(&name) {
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
        let fid = self.create_function_with_captures(&decl, display, false, &capture_names)?;
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

    /// Appends one lowered statement and, for module-body assignments, the
    /// matching [`HStmtKind::GlobalSet`].
    ///
    /// The global store reads the just-assigned local, so the right-hand side
    /// is evaluated exactly once even when it has side effects. Compound
    /// module-level blocks (`if`, `for`, `try`, ...) lower through
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
            global_assignment_target(stmt, &h).filter(|(name, _)| self.used_globals.contains(name))
        } else {
            None
        };
        let (line, col) = (h.line, h.col);
        out.push(h);
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
                if s.targets.len() != 1 {
                    self.error_at(s.range, "multiple assignment is not supported in Phase 1");
                    return None;
                }
                let target_expr = &s.targets[0];
                match target_expr {
                    ast::Expr::Name(target) => {
                        let value = self.expr(&s.value, fs);
                        let Some(local) = fs.lookup(target.id.as_str()) else {
                            self.error_name(
                                target.range,
                                target.id.as_str(),
                                fs,
                                format!("assignment to unknown name '{}'", target.id),
                            );
                            return None;
                        };
                        HStmtKind::Assign {
                            target: local,
                            value,
                        }
                    }
                    ast::Expr::Subscript(sub) => {
                        if matches!(&*sub.slice, ast::Expr::Slice(_)) {
                            self.error_at_code(
                                sub.slice.range(),
                                codes::INDEX,
                                "slice assignment is not supported yet",
                            );
                            return None;
                        }
                        // Python evaluates the value expression first, then the
                        // subscript target. The IR preserves that order.
                        let value = self.expr(&s.value, fs);
                        let object = self.expr(&sub.value, fs);
                        let index = self.expr(&sub.slice, fs);
                        HStmtKind::SetSubscript {
                            object,
                            index,
                            value,
                        }
                    }
                    ast::Expr::Attribute(attr) => {
                        // Python evaluates the right-hand side first, then the
                        // target object, then stores the attribute.
                        let value = self.expr(&s.value, fs);
                        let object = self.expr(&attr.value, fs);
                        HStmtKind::SetAttr {
                            object,
                            name: attr.attr.to_string(),
                            value,
                        }
                    }
                    ast::Expr::Tuple(_) | ast::Expr::List(_) => {
                        // Python evaluates the value expression first; the IR
                        // `Unpack` keeps it and checks the length at runtime.
                        let value = self.expr(&s.value, fs);
                        let targets = self.unpack_targets(target_expr, fs)?;
                        HStmtKind::Unpack { targets, value }
                    }
                    _ => {
                        self.error_at(
                            target_expr.range(),
                            "assignment target must be a name, attribute or subscript in Phase 2.0",
                        );
                        return None;
                    }
                }
            }
            ast::Stmt::AugAssign(s) => {
                if let ast::Expr::Subscript(sub) = &*s.target {
                    if matches!(&*sub.slice, ast::Expr::Slice(_)) {
                        self.error_at_code(
                            sub.slice.range(),
                            codes::INDEX,
                            "slice assignment is not supported yet",
                        );
                        return None;
                    }
                    return self.lower_aug_subscript(s, sub, fs, line, col);
                }
                if let ast::Expr::Attribute(attr) = &*s.target {
                    self.error_at(
                        attr.range,
                        "augmented assignment to attributes is not supported yet",
                    );
                    return None;
                }
                let ast::Expr::Name(target) = &*s.target else {
                    self.error_at(
                        s.target.range(),
                        "augmented assignment target must be a single name or a subscript in Phase 2.0",
                    );
                    return None;
                };
                let op = self.bin_op(s.op, s.range)?;
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
            ast::Stmt::If(s) => {
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
                if !s.orelse.is_empty() {
                    self.error_at(s.range, "while-else is not supported in Phase 1");
                }
                let cond = self.expr(&s.test, fs);
                // `while False:` never runs its body; drop the loop. `while
                // True:` stays a real loop so `break` keeps working.
                if matches!(&cond.kind, HExprKind::Const(Const::Bool(false))) {
                    return None;
                }
                fs.loop_depth += 1;
                let body = self.lower_block(&s.body, fs);
                fs.loop_depth -= 1;
                HStmtKind::While { cond, body }
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
                        .into_iter()
                        .map(|(name, function, _)| (name, self.fid_base + function))
                        .collect(),
                }
            }
            ast::Stmt::Import(s) => {
                self.error_at_code(
                    s.range,
                    codes::UNSUPPORTED,
                    "imports are only supported at module level in Phase 2.10",
                );
                return None;
            }
            ast::Stmt::ImportFrom(s) => {
                self.error_at_code(
                    s.range,
                    codes::UNSUPPORTED,
                    "imports are only supported at module level in Phase 2.10",
                );
                return None;
            }
            ast::Stmt::With(s) => return self.lower_with(s, fs, line, col),
            ast::Stmt::AsyncWith(s) => {
                return self.lower_with_items(&s.items, &s.body, s.range, fs)
            }
            ast::Stmt::AsyncFor(s) => {
                return self.lower_for_parts(
                    &s.target, &s.iter, &s.body, &s.orelse, s.range, fs, line, col,
                )
            }
            ast::Stmt::Assert(s) => {
                self.error_at(s.range, "assert is not supported in Phase 1");
                return None;
            }
            ast::Stmt::Delete(s) => {
                self.error_at(s.range, "del is not supported in Phase 1");
                return None;
            }
            ast::Stmt::Global(s) => {
                self.error_at(s.range, "global is not supported in Phase 1");
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
            ast::Stmt::AnnAssign(s) => {
                self.error_at(s.range, "annotated assignment is not supported in Phase 1");
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

    /// Resolve the elements of a tuple/list unpacking target to locals.
    ///
    /// Nested containers and starred targets are rejected with dedicated
    /// diagnostics; anything else must be a plain name.
    fn unpack_targets(&mut self, target: &ast::Expr, fs: &FuncState) -> Option<Vec<LocalId>> {
        let elems = match target {
            ast::Expr::Tuple(t) => &t.elts,
            ast::Expr::List(l) => &l.elts,
            other => {
                self.error_at(
                    other.range(),
                    "unpacking target must be a tuple or list of names",
                );
                return None;
            }
        };
        let mut targets = Vec::with_capacity(elems.len());
        let mut ok = true;
        for elem in elems {
            match elem {
                ast::Expr::Name(n) => match fs.lookup(n.id.as_str()) {
                    Some(local) => targets.push(local),
                    None => {
                        self.error_name(
                            n.range,
                            n.id.as_str(),
                            fs,
                            format!("assignment to unknown name '{}'", n.id),
                        );
                        ok = false;
                    }
                },
                ast::Expr::Tuple(_) | ast::Expr::List(_) => {
                    self.error_at(elem.range(), "nested unpacking is not supported yet");
                    ok = false;
                }
                ast::Expr::Starred(s) => {
                    self.error_at(s.range, "starred unpacking is not supported yet");
                    ok = false;
                }
                other => {
                    self.error_at(other.range(), "unpacking target must be a name");
                    ok = false;
                }
            }
        }
        if ok {
            Some(targets)
        } else {
            None
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

    /// Lower `target[index] op= value`. Python evaluates the target object,
    /// then the index, then the right-hand side, and none of them twice. The
    /// object and index are therefore stored in hidden `$augN` locals; the
    /// read and the write both use those locals. HIR statements cannot be
    /// nested arbitrarily, so the three generated statements are wrapped in an
    /// `if True:` block.
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
            ast::Expr::Tuple(t) => self.lower_comp_unpack(&t.elts, fs, saved),
            ast::Expr::List(l) => self.lower_comp_unpack(&l.elts, fs, saved),
            other => {
                self.error_at(other.range(), "unsupported comprehension target");
                // Keep lowering with a hidden local so later diagnostics still
                // refer to a well-formed clause.
                HCompTarget::Name(self.comp_local(fs))
            }
        }
    }

    fn lower_comp_unpack(
        &mut self,
        elems: &[ast::Expr],
        fs: &mut FuncState,
        saved: &mut Vec<(String, Option<LocalId>)>,
    ) -> HCompTarget {
        // Validate the whole target before binding anything: a rejected target
        // must not shadow names inside the comprehension.
        let mut names = Vec::with_capacity(elems.len());
        let mut ok = true;
        for elem in elems {
            match elem {
                ast::Expr::Name(n) => names.push(n.id.to_string()),
                other => {
                    self.error_at(other.range(), "unsupported comprehension target");
                    ok = false;
                }
            }
        }
        if !ok {
            return HCompTarget::Name(self.comp_local(fs));
        }
        let mut locals = Vec::with_capacity(names.len());
        for name in &names {
            locals.push(self.bind_comp_name(fs, name, saved));
        }
        HCompTarget::Unpack(locals)
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
        self.lower_for_parts(
            &s.target, &s.iter, &s.body, &s.orelse, s.range, fs, line, col,
        )
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
        range: TextRange,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> Option<HStmt> {
        if !orelse.is_empty() {
            self.error_at(range, "for-else is not supported in Phase 1");
        }
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
            return Some(HStmt {
                kind: HStmtKind::ForEach {
                    var,
                    iterable,
                    body,
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
        Some(HStmt {
            kind: HStmtKind::ForRange {
                var,
                start,
                stop,
                step,
                body,
            },
            line,
            col,
        })
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
                    None => {
                        self.error_at_code(
                            n.range,
                            codes::NAME,
                            format!("unknown exception type '{}'", n.id),
                        );
                        return None;
                    }
                },
                ast::Expr::Call(call) => {
                    let ast::Expr::Name(n) = &*call.func else {
                        self.error_at(
                            call.func.range(),
                            "raise only supports exception names in Phase 1",
                        );
                        return None;
                    };
                    let Some(kind) = ExcKind::from_name(n.id.as_str()) else {
                        self.error_at_code(
                            n.range,
                            codes::NAME,
                            format!("unknown exception type '{}'", n.id),
                        );
                        return None;
                    };
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
                    (Some(kind), Some(self.expr(&call.args[0], fs)))
                }
                other => {
                    self.error_at(
                        other.range(),
                        "raise only supports exception names in Phase 1",
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
                Some(type_expr) => match &**type_expr {
                    ast::Expr::Name(n) => match ExcKind::from_name(n.id.as_str()) {
                        Some(kind) => kinds.push(kind),
                        None => {
                            self.error_at_code(
                                n.range,
                                codes::NAME,
                                format!("unknown exception type '{}'", n.id),
                            );
                            continue;
                        }
                    },
                    ast::Expr::Tuple(tuple) => {
                        if tuple.elts.is_empty() {
                            self.error_at_code(
                                tuple.range,
                                codes::CONTROL,
                                "empty exception tuple in except handler",
                            );
                            continue;
                        }
                        let mut ok = true;
                        for elt in &tuple.elts {
                            let ast::Expr::Name(n) = elt else {
                                self.error_at(
                                    elt.range(),
                                    "only simple exception names are supported in exception tuples",
                                );
                                ok = false;
                                continue;
                            };
                            match ExcKind::from_name(n.id.as_str()) {
                                Some(kind) => kinds.push(kind),
                                None => {
                                    self.error_at_code(
                                        n.range,
                                        codes::NAME,
                                        format!("unknown exception type '{}'", n.id),
                                    );
                                    ok = false;
                                }
                            }
                        }
                        if !ok {
                            continue;
                        }
                    }
                    other => {
                        self.error_at(other.range(), "only simple exception names are supported");
                        continue;
                    }
                },
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
                } else if let Some(&id) = self.imported_user_functions.get(name) {
                    HExpr {
                        kind: HExprKind::FunctionRef(id),
                        ty: Ty::Concrete(Type::Function),
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
                } else if fs.in_lambda {
                    self.error_at_code(
                        e.range(),
                        codes::UNSUPPORTED,
                        "closures are not supported yet; lambda can only use its parameters",
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
                if c.ops.len() != 1 || c.comparators.len() != 1 {
                    self.error_at(c.range, "chained comparisons are not supported in Phase 1");
                    return self.error_expr(e.range());
                }
                let Some(op) = self.cmp_op(c.ops[0], c.range) else {
                    return self.error_expr(e.range());
                };
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
                    return match module.exports.get(member) {
                        Some(&id) => HExpr {
                            kind: HExprKind::FunctionRef(id),
                            ty: Ty::Concrete(Type::Function),
                            line,
                            col,
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
                let mut pairs = Vec::with_capacity(d.keys.len());
                for (key, value) in d.keys.iter().zip(d.values.iter()) {
                    let Some(key) = key else {
                        self.error_at(
                            value.range(),
                            "dict unpacking (**) is not supported in Phase 2.0",
                        );
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
                self.error_at(
                    i.range,
                    "conditional expressions are not supported in Phase 1",
                );
                self.error_expr(e.range())
            }
            ast::Expr::NamedExpr(n) => {
                self.error_at(
                    n.range,
                    "assignment expressions (:=) are not supported in Phase 1",
                );
                self.error_expr(e.range())
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
            self.add_param(&arg.def, &mut params, &mut locals, &mut seen);
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
            ast::Constant::Ellipsis => {
                self.error_at(range, "ellipsis is not supported in Phase 1");
                (Const::None, Ty::Concrete(Type::Any))
            }
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
    /// treats as "use the default". User-defined functions, methods and
    /// builtins still require positional arguments in this phase.
    fn module_args(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        spec: &'static FunctionSpec,
        line: u32,
        col: u32,
    ) -> Option<Vec<HExpr>> {
        if call.keywords.is_empty() {
            let mut args = Vec::with_capacity(call.args.len());
            for arg in &call.args {
                args.push(self.expr(arg, fs));
            }
            return Some(args);
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
        Some(
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
        )
    }

    fn call_expr(
        &mut self,
        call: &ast::ExprCall,
        fs: &mut FuncState,
        line: u32,
        col: u32,
    ) -> HExpr {
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
                    return self.user_module_call(&module, attr.attr.as_str(), call, fs, line, col);
                }
                if let Some(path) = self.resolve_extension_alias(&attr.value, fs) {
                    return self.extension_call(&path, attr.attr.as_str(), call, fs, line, col);
                }
                let object = self.expr(&attr.value, fs);
                let mut args = Vec::with_capacity(call.args.len());
                for arg in &call.args {
                    args.push(self.expr(arg, fs));
                }
                let mut keywords = Vec::with_capacity(call.keywords.len());
                for keyword in &call.keywords {
                    let Some(name) = keyword.arg.as_ref() else {
                        self.error_at_code(
                            keyword.range,
                            codes::CALL,
                            "**kwargs arguments are not supported",
                        );
                        return self.error_expr(call.range);
                    };
                    keywords.push((name.to_string(), self.expr(&keyword.value, fs)));
                }
                return HExpr {
                    kind: HExprKind::MethodCall {
                        object: Box::new(object),
                        method: attr.attr.to_string(),
                        args,
                        keywords,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
            other => {
                if !call.keywords.is_empty() {
                    self.error_at_code(
                        call.range,
                        codes::CALL,
                        "keyword arguments are only supported for native module functions",
                    );
                    return self.error_expr(call.range);
                }
                // Any other callee is a function value: Python evaluates the
                // callee before the arguments.
                let callee = self.expr(other, fs);
                let mut args = Vec::with_capacity(call.args.len());
                for arg in &call.args {
                    args.push(self.expr(arg, fs));
                }
                return HExpr {
                    kind: HExprKind::IndirectCall {
                        callee: Box::new(callee),
                        args,
                    },
                    ty: self.infer.fresh(),
                    line,
                    col,
                };
            }
        };
        let name = name_expr.id.as_str();

        if let Some(local) = fs.lookup(name) {
            if !call.keywords.is_empty() {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    "keyword arguments are not supported for function values",
                );
                return self.error_expr(call.range);
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
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if let Some(&id) = self.imported_user_functions.get(name) {
            if !call.keywords.is_empty() {
                self.error_at_code(
                    call.range,
                    codes::CALL,
                    "keyword arguments are not supported for user-defined functions",
                );
                return self.error_expr(call.range);
            }
            let mut args = Vec::with_capacity(call.args.len());
            for arg in &call.args {
                args.push(self.expr(arg, fs));
            }
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

        if let Some((module, function)) = self.imported_functions.get(name).copied() {
            let Some(args) = self.module_args(call, fs, function, line, col) else {
                return self.error_expr(call.range);
            };
            return HExpr {
                kind: HExprKind::ModuleCall {
                    module: module.name.to_string(),
                    function: function.name.to_string(),
                    args,
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
            return self.extension_call(&path, &member, call, fs, line, col);
        }

        if self.module_aliases.contains_key(name)
            || self.user_module_aliases.contains_key(name)
            || self.extension_aliases.contains_key(name)
        {
            self.error_module_value(name_expr.range, name);
            return self.error_expr(call.range);
        }

        if fs.in_lambda {
            self.error_at_code(
                call.range,
                codes::UNSUPPORTED,
                "closures are not supported yet; lambda can only use its parameters",
            );
            return self.error_expr(call.range);
        }

        // Bare `open(...)` is the io.open alias unless the name is shadowed.
        // Locals, imports and user functions were handled above.
        if name == "open" && !self.fn_ids.contains_key(name) {
            let spec =
                stdlib::function("io", "open").expect("io.open is declared in the stdlib table");
            let Some(args) = self.module_args(call, fs, spec, line, col) else {
                return self.error_expr(call.range);
            };
            return HExpr {
                kind: HExprKind::ModuleCall {
                    module: "io".to_string(),
                    function: "open".to_string(),
                    args,
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        }

        if !call.keywords.is_empty() {
            self.error_at_code(
                call.range,
                codes::CALL,
                "keyword arguments are not supported for user-defined functions and builtins",
            );
            return self.error_expr(call.range);
        }
        let mut args = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            args.push(self.expr(arg, fs));
        }

        let callee = if let Some(id) = self.fn_ids.get(name) {
            Callee::Function(*id)
        } else if let Some(builtin) = builtin_by_name(name) {
            Callee::Builtin(builtin)
        } else if self.global_names.contains(name) {
            // A module-level value (class or function) used as a callee.
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
                },
                ty: self.infer.fresh(),
                line,
                col,
            };
        } else {
            if !fs.is_module && self.module_local_names.contains(name) {
                self.error_at_code(
                    call.range,
                    codes::NAME,
                    format!(
                        "name '{name}' is not visible inside a function; pass it as a parameter"
                    ),
                );
            } else {
                self.error_name(
                    name_expr.range,
                    name,
                    fs,
                    format!("name '{name}' is not defined"),
                );
            }
            return self.error_expr(call.range);
        };

        HExpr {
            kind: HExprKind::Call { callee, args },
            ty: self.infer.fresh(),
            line,
            col,
        }
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
            ast::Operator::LShift
            | ast::Operator::RShift
            | ast::Operator::BitOr
            | ast::Operator::BitXor
            | ast::Operator::BitAnd => {
                self.error_at(range, "bitwise operators are not supported in Phase 1");
                return None;
            }
        })
    }

    fn un_op(&mut self, op: ast::UnaryOp, range: TextRange) -> Option<pyx_ir::UnOp> {
        Some(match op {
            ast::UnaryOp::Not => pyx_ir::UnOp::Not,
            ast::UnaryOp::UAdd => pyx_ir::UnOp::Pos,
            ast::UnaryOp::USub => pyx_ir::UnOp::Neg,
            ast::UnaryOp::Invert => {
                self.error_at(range, "bitwise invert (~) is not supported in Phase 1");
                return None;
            }
        })
    }

    fn cmp_op(&mut self, op: ast::CmpOp, range: TextRange) -> Option<BinOp> {
        Some(match op {
            ast::CmpOp::Eq => BinOp::Eq,
            ast::CmpOp::NotEq => BinOp::Ne,
            ast::CmpOp::Lt => BinOp::Lt,
            ast::CmpOp::LtE => BinOp::Le,
            ast::CmpOp::Gt => BinOp::Gt,
            ast::CmpOp::GtE => BinOp::Ge,
            ast::CmpOp::Is | ast::CmpOp::IsNot => {
                self.error_at(
                    range,
                    "identity comparisons ('is'/'is not') are not supported in Phase 1",
                );
                return None;
            }
            ast::CmpOp::In => BinOp::In,
            ast::CmpOp::NotIn => BinOp::NotIn,
        })
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

/// Names bound by an assignment target: a plain name or a flat tuple/list of
/// names. Nested containers are still walked so their names get slots and the
/// dedicated rejection diagnostic is the only error reported.
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

/// Whether the module body itself loads `__name__`.
///
/// Nested function/class bodies have their own scopes and cannot see the
/// module local, so they do not trigger the injected assignment.
fn module_refers_to_dunder_name(stmts: &[ast::Stmt]) -> bool {
    for stmt in stmts {
        if matches!(
            stmt,
            ast::Stmt::FunctionDef(_) | ast::Stmt::AsyncFunctionDef(_) | ast::Stmt::ClassDef(_)
        ) {
            continue;
        }
        let mut names = Vec::new();
        collect_free_name_loads(std::slice::from_ref(stmt), &mut names);
        if names.iter().any(|name| name == "__name__") {
            return true;
        }
    }
    false
}

/// Names that become locals of the enclosing function.
///
/// `include_defs` controls whether nested function definitions bind their
/// name in this scope. It is `true` for function bodies (a nested `def`
/// stores its closure in a local) and `false` for the module body, where
/// functions live in the function table instead of module locals. Nested
/// function bodies and lambdas are never descended into: they have their own
/// scopes.
/// Collects the module-level simple-name assignment targets that are mirrored
/// into the runtime global table. Function and class bodies are not entered;
/// assignments inside module-level compound statements are, because those
/// statements execute at module level.
fn collect_global_names(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Assign(s) => {
                if let [ast::Expr::Name(name)] = s.targets.as_slice() {
                    out.insert(name.id.to_string());
                }
            }
            ast::Stmt::AugAssign(s) => {
                if let ast::Expr::Name(name) = &*s.target {
                    out.insert(name.id.to_string());
                }
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

/// The `(name, local)` pair mirrored by a module-body assignment, if the
/// statement is a simple-name `Assign`/`AugAssign`.
fn global_assignment_target(stmt: &ast::Stmt, h: &HStmt) -> Option<(String, LocalId)> {
    match (stmt, &h.kind) {
        (ast::Stmt::Assign(s), HStmtKind::Assign { target, .. }) => match s.targets.as_slice() {
            [ast::Expr::Name(name)] => Some((name.id.to_string(), *target)),
            _ => None,
        },
        (ast::Stmt::AugAssign(s), HStmtKind::AugAssign { target, .. }) => match &*s.target {
            ast::Expr::Name(name) => Some((name.id.to_string(), *target)),
            _ => None,
        },
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
