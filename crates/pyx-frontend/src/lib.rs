//! PyX frontend: parse Python source, lower it to typed IR, infer/validate
//! types, and report source-located diagnostics.
//!
//! Public contract (used by `pyx-cli` and tests):
//! [`check_source`] returns either a fully typed [`Program`] or a non-empty list
//! of [`Diagnostic`]s. [`check_file`] additionally resolves imports of user
//! modules and packages (siblings, site-packages and `PYTHONPATH`) and links
//! them into one program. Both must never panic on user input.

pub mod diag;
mod lower;
mod sema;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use pyx_ir::{FunctionId, Program};
use rustpython_parser::{ast, Mode};

pub use diag::Diagnostic;

use diag::codes;
use lower::{HFunction, HParam, LHir, Lowerer, UserModule};
use sema::{Infer, Ty};

/// Parse, lower, type-check and return a fully typed program.
///
/// Errors are collected rather than short-circuited where practical; the
/// returned vector is never empty on failure.
pub fn check_source(source: &str, filename: &str) -> Result<Program, Vec<Diagnostic>> {
    let module = parse_module(source, filename)?;

    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, filename, &mut infer);
    let hir = lowerer.lower_module(&module);
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    if !diags.is_empty() {
        return Err(diags);
    }

    sema::check(&hir, &mut infer, filename)?;
    Ok(sema::emit(&hir, &mut infer))
}

/// Parse `source` into a module AST, reporting syntax errors as diagnostics.
fn parse_module(source: &str, filename: &str) -> Result<ast::ModModule, Vec<Diagnostic>> {
    let parsed = match rustpython_parser::parse(source, Mode::Module, filename) {
        Ok(module) => module,
        Err(error) => {
            let offset = u32::from(error.offset) as usize;
            let (line, col, end_col) = diag::syntax_span(source, offset);
            return Err(vec![Diagnostic::new(
                filename,
                line,
                col,
                format!("invalid syntax: {}", error.error),
            )
            .with_code(codes::SYNTAX)
            .with_span(end_col)
            .with_help("check the Python syntax near this position")]);
        }
    };

    match parsed {
        ast::Mod::Module(module) => Ok(module),
        _ => Err(vec![Diagnostic::new(
            filename,
            1,
            1,
            "unsupported parser mode",
        )]),
    }
}

/// Parse, lower and type-check a file together with the user modules it
/// imports.
///
/// `import foo` / `from foo import f` are resolved as Python modules: first
/// next to the importing file, then through `PYX_SITE_PACKAGES`, the
/// `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages directories and `PYTHONPATH`.
/// Both plain modules (`foo.py`) and packages (`foo/__init__.py`) are
/// supported, including dotted names, submodules and relative imports. Every
/// module is loaded once per canonical path; missing files and incompatible
/// extensions are reported as diagnostics, as are import cycles made only of
/// module-level imports. Imports nested inside functions, classes and compound
/// statements only run when that code does: they are recorded but never
/// loaded, and reading a name they bind raises `RuntimeError` (`dynamic import
/// ... is not supported`) instead of failing compilation. Imported module
/// bodies are compiled into the returned [`Program`] and run before the entry
/// module (see [`Program::initializers`]).
pub fn check_file(path: &Path) -> Result<Program, Vec<Diagnostic>> {
    let mut loader = ModuleLoader::default();
    let display = path.to_path_buf();
    let key = fs::canonicalize(path).unwrap_or_else(|_| display.clone());
    let root = display
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let _ = loader.load_file(key, display, "__main__".to_string(), String::new(), root);

    if !loader.diags.is_empty() {
        return Err(loader.diags);
    }
    if loader.units.is_empty() {
        // Defensive: `load_file` reports diagnostics for every failure path.
        return Err(vec![Diagnostic::new(
            &path.to_string_lossy(),
            1,
            1,
            "cannot read module",
        )]);
    }

    lower_modules(&loader.units)
}

/// A parsed module in dependency order (dependencies before importers).
struct LoadedModule {
    /// Full dotted module name; `__main__` for the entry file.
    name: String,
    /// `__package__`: the package relative imports resolve against.
    package: String,
    /// Display path used for diagnostics and tracebacks.
    display: String,
    source: String,
    module: ast::ModModule,
    /// Units this module imports, in discovery order.
    deps: Vec<usize>,
    /// User modules this unit imports but cannot link directly: they are still
    /// initializing when the unit is loaded (an ancestor package). The lowerer
    /// binds their members late, through the runtime global table. A late
    /// module has no dependency edge, so its initializer runs after this unit:
    /// its members are readable only once the importing module's functions
    /// run.
    late: Vec<String>,
    /// Imports nested in functions, classes or compound bodies, recorded by
    /// the scan as `(module, member)` pairs. They are not linked: the lowerer
    /// binds their names to a runtime `RuntimeError` stub unless the module is
    /// already a visible dependency or native.
    deferred: Vec<(String, Option<String>)>,
}

/// One module's import scan: linked dependencies, failure state and the
/// modules whose members must be bound late.
#[derive(Default)]
struct ImportScan {
    /// Units this module imports, in discovery order.
    deps: Vec<usize>,
    /// A diagnostic was produced for this subtree.
    failed: bool,
    /// Importable modules that must not become dependency edges.
    late: Vec<String>,
    /// Imports nested in functions, classes or compound bodies: the module
    /// (`from x import a` / `import x`) and the imported member, if any.
    /// They are never loaded; the lowerer binds the names to a runtime error
    /// stub (`dynamic import ... is not supported`).
    deferred: Vec<(String, Option<String>)>,
}

impl ImportScan {
    /// Records a module whose members the lowerer resolves through the runtime
    /// global table instead of a direct link.
    fn record_late(&mut self, name: &str) {
        if !self.late.iter().any(|existing| existing == name) {
            self.late.push(name.to_string());
        }
    }

    /// Records an import that runs only when its enclosing code executes.
    fn record_deferred(&mut self, module: &str, member: Option<&str>) {
        let edge = (module.to_string(), member.map(str::to_string));
        if !self.deferred.contains(&edge) {
            self.deferred.push(edge);
        }
    }
}

/// One module file found by the search path.
struct ResolvedModule {
    path: PathBuf,
    /// Full dotted module name.
    name: String,
    /// `__package__` of the module.
    package: String,
    /// Search root the module chain was found in.
    root: PathBuf,
}

/// Outcome of resolving and loading one dotted module name.
enum NamedLoad {
    /// Every loaded unit in the chain (package ancestors first, module last).
    Loaded(Vec<usize>),
    Missing,
    /// A diagnostic was already produced for the module (read/parse error).
    Failed,
    /// The module is already on the load stack; the payload is the import
    /// chain from the cycle start.
    Cyclic {
        chain: Vec<String>,
    },
    /// The module is the importer itself or one of its ancestor packages and
    /// is still initializing: importable, but it must not become a dependency
    /// edge (that would be a cycle).
    InProgress,
}

#[derive(Default)]
struct ModuleLoader {
    /// Finalized modules in post-order (dependencies first, entry last).
    units: Vec<LoadedModule>,
    /// Canonical path -> unit index, so a file is loaded at most once.
    loaded: HashMap<PathBuf, usize>,
    /// Full dotted name -> unit index, so a module keeps one identity.
    by_name: HashMap<String, usize>,
    /// Canonical paths whose load failed; avoids duplicate diagnostics.
    failed: HashSet<PathBuf>,
    /// Modules currently being loaded, for cycle detection. Each frame holds
    /// the canonical path and the dotted name. Only module-level imports are
    /// linked, so every cycle this detects is a real cycle.
    stack: Vec<(PathBuf, String)>,
    diags: Vec<Diagnostic>,
}

impl ModuleLoader {
    /// Loads `display` (keyed by the canonical `key`) and its module-level
    /// user imports. Nested imports are recorded but never loaded: they only
    /// run when their enclosing code executes.
    ///
    /// Returns the unit index of the module, or `None` when any diagnostic was
    /// produced for this subtree.
    fn load_file(
        &mut self,
        key: PathBuf,
        display: PathBuf,
        name: String,
        package: String,
        root: PathBuf,
    ) -> Option<usize> {
        if let Some(&index) = self.loaded.get(&key) {
            return Some(index);
        }
        if self.failed.contains(&key) {
            return None;
        }

        let source = match fs::read_to_string(&display) {
            Ok(source) => source,
            Err(error) => {
                let file = display.to_string_lossy();
                self.diags.push(
                    Diagnostic::new(
                        &file,
                        1,
                        1,
                        format!("cannot read module '{}': {error}", display.display()),
                    )
                    .with_code(codes::UNSUPPORTED)
                    .with_help("check the path and file permissions"),
                );
                self.failed.insert(key);
                return None;
            }
        };
        let file = display.to_string_lossy().into_owned();
        let module = match parse_module(&source, &file) {
            Ok(module) => module,
            Err(mut diagnostics) => {
                self.diags.append(&mut diagnostics);
                self.failed.insert(key);
                return None;
            }
        };

        self.stack.push((key.clone(), name.clone()));
        let mut scan = ImportScan::default();
        let importer_dir = display
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        self.scan_imports(
            &module,
            &source,
            &display,
            &importer_dir,
            &root,
            &name,
            &package,
            &mut scan,
        );
        self.stack.pop();

        if scan.failed {
            self.failed.insert(key);
            return None;
        }

        let index = self.units.len();
        self.units.push(LoadedModule {
            name: name.clone(),
            package,
            display: file,
            source,
            module,
            deps: scan.deps,
            late: scan.late,
            deferred: scan.deferred,
        });
        self.loaded.insert(key, index);
        self.by_name.insert(name, index);
        Some(index)
    }

    /// Resolves `name` through the search path and loads it plus any package
    /// ancestors. Ancestors already being initialized are skipped: they are
    /// importable implicitly and their unit is finalized by the caller.
    fn load_named(&mut self, name: &str, importer_dir: &Path, module_root: &Path) -> NamedLoad {
        let roots = module_search_roots(importer_dir, module_root);
        let Some(chain) = resolve_module(&roots, name) else {
            if let Some(&index) = self.by_name.get(name) {
                return NamedLoad::Loaded(vec![index]);
            }
            return NamedLoad::Missing;
        };

        let mut loaded = Vec::with_capacity(chain.len());
        let total = chain.len();
        for (position, resolved) in chain.into_iter().enumerate() {
            let final_module = position + 1 == total;
            let key = fs::canonicalize(&resolved.path).unwrap_or_else(|_| resolved.path.clone());
            if let Some(&index) = self.by_name.get(&resolved.name) {
                loaded.push(index);
                continue;
            }
            if let Some(start) = self.stack.iter().position(|(path, _)| path == &key) {
                if final_module {
                    // A package that is importing this module right now is
                    // the caller's ancestor and stays partially initialized,
                    // exactly like CPython's parent module during `import
                    // pkg.sub`. It is importable, but linking it as a
                    // dependency would be a cycle.
                    if self.is_initializing_ancestor(&resolved.name) {
                        return NamedLoad::InProgress;
                    }
                    let mut cycle: Vec<String> = self.stack[start..]
                        .iter()
                        .map(|(_, module)| module.clone())
                        .collect();
                    cycle.push(resolved.name);
                    return NamedLoad::Cyclic { chain: cycle };
                }
                // Package `__init__.py` currently initializing: its own unit
                // is the importer, so it must not be linked as a dependency.
                continue;
            }
            if self.failed.contains(&key) {
                return NamedLoad::Failed;
            }
            match self.load_file(
                key,
                resolved.path,
                resolved.name,
                resolved.package,
                resolved.root,
            ) {
                Some(index) => loaded.push(index),
                None => return NamedLoad::Failed,
            }
        }

        if loaded.is_empty() {
            NamedLoad::Missing
        } else {
            NamedLoad::Loaded(loaded)
        }
    }

    /// Loads a module whose absence is not an error (`from pkg import name`
    /// probes `pkg.name` before falling back to package attributes).
    fn probe_named(
        &mut self,
        name: &str,
        importer_dir: &Path,
        module_root: &Path,
    ) -> Option<usize> {
        match self.load_named(name, importer_dir, module_root) {
            NamedLoad::Loaded(mut loaded) => loaded.pop(),
            NamedLoad::Missing
            | NamedLoad::Failed
            | NamedLoad::Cyclic { .. }
            | NamedLoad::InProgress => None,
        }
    }

    /// Whether `name` is the module currently being scanned or one of its
    /// ancestor packages. Both are on the load stack when a module-level
    /// import reaches back to the package that is initializing them.
    fn is_initializing_ancestor(&self, name: &str) -> bool {
        let Some((_, importer)) = self.stack.last() else {
            return false;
        };
        importer == name
            || (importer.len() > name.len()
                && importer.starts_with(name)
                && importer.as_bytes()[name.len()] == b'.')
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_imports(
        &mut self,
        module: &ast::ModModule,
        source: &str,
        display: &Path,
        importer_dir: &Path,
        root: &Path,
        current: &str,
        package: &str,
        scan: &mut ImportScan,
    ) {
        self.scan_stmts(
            &module.body,
            source,
            display,
            importer_dir,
            root,
            current,
            package,
            scan,
            false,
        );
    }

    /// Visits every import in `stmts`.
    ///
    /// `deferred == false` is the module body: its imports are resolved and
    /// linked eagerly, so their initializers keep dependency order
    /// (dependencies before importers) and circular imports are diagnosed.
    /// Everything nested inside a function, class or compound statement is
    /// `deferred == true`: the import only runs when that code executes, so it
    /// is recorded as a `(module, member)` edge and never loaded. This keeps
    /// optional dependency graphs (for example `rich/_extension.py` importing
    /// `rich.pretty` and `rich.traceback` inside a function) out of the
    /// program unless their code is actually needed.
    ///
    /// Branches whose guard is fixed by the import contract are still skipped
    /// — `if TYPE_CHECKING:` never runs (annotations are not evaluated) and
    /// `if __name__ == "__main__":` is false for an imported module — so
    /// type-only and demo imports are not even recorded.
    #[allow(clippy::too_many_arguments)]
    fn scan_stmts(
        &mut self,
        stmts: &[ast::Stmt],
        source: &str,
        display: &Path,
        importer_dir: &Path,
        root: &Path,
        current: &str,
        package: &str,
        scan: &mut ImportScan,
        deferred: bool,
    ) {
        for stmt in stmts {
            match stmt {
                ast::Stmt::Import(s) => {
                    for alias in &s.names {
                        if deferred {
                            // Native modules bind without loading anything;
                            // user modules would need a dynamic import.
                            if pyx_ir::stdlib::module(alias.name.as_str()).is_none() {
                                scan.record_deferred(alias.name.as_str(), None);
                            }
                            continue;
                        }
                        self.resolve_import(
                            source,
                            display,
                            importer_dir,
                            root,
                            alias.name.as_str(),
                            alias.range,
                            scan,
                        );
                    }
                }
                ast::Stmt::ImportFrom(s) => {
                    let level = s.level.as_ref().map(|level| level.to_u32()).unwrap_or(0);
                    let module_name = match lower::resolve_import_name(
                        level,
                        s.module.as_ref().map(|module| module.as_str()),
                        package,
                    ) {
                        Ok(name) => name,
                        Err(message) => {
                            self.push_span(source, display, s.range, codes::UNSUPPORTED, message);
                            scan.failed = true;
                            continue;
                        }
                    };
                    if pyx_ir::stdlib::module(&module_name).is_some() {
                        continue;
                    }
                    if deferred {
                        for alias in &s.names {
                            if alias.name.as_str() != "*" {
                                scan.record_deferred(&module_name, Some(alias.name.as_str()));
                            }
                        }
                        continue;
                    }
                    // `from . import name` inside the package's own
                    // `__init__.py` names this very module; importing it again
                    // would look like a cycle.
                    if module_name != current {
                        self.resolve_import(
                            source,
                            display,
                            importer_dir,
                            root,
                            &module_name,
                            s.range,
                            scan,
                        );
                    }
                    // `from pkg import sub` may name a submodule rather than
                    // an attribute; load it when it exists.
                    for alias in &s.names {
                        if alias.name.as_str() == "*" {
                            continue;
                        }
                        let full = format!("{module_name}.{}", alias.name);
                        if let Some(index) = self.probe_named(&full, importer_dir, root) {
                            if !scan.deps.contains(&index) {
                                scan.deps.push(index);
                            }
                        }
                    }
                }
                ast::Stmt::FunctionDef(s) => self.scan_stmts(
                    &s.body,
                    source,
                    display,
                    importer_dir,
                    root,
                    current,
                    package,
                    scan,
                    true,
                ),
                ast::Stmt::AsyncFunctionDef(s) => self.scan_stmts(
                    &s.body,
                    source,
                    display,
                    importer_dir,
                    root,
                    current,
                    package,
                    scan,
                    true,
                ),
                // A class body executes when the class statement runs, but it
                // can also sit inside a deferred function: keep its imports
                // deferred like every other nested block.
                ast::Stmt::ClassDef(s) => self.scan_stmts(
                    &s.body,
                    source,
                    display,
                    importer_dir,
                    root,
                    current,
                    package,
                    scan,
                    true,
                ),
                ast::Stmt::If(s) => {
                    let branch = lower::static_if_branch(&s.test, current);
                    if branch != Some(false) {
                        self.scan_stmts(
                            &s.body,
                            source,
                            display,
                            importer_dir,
                            root,
                            current,
                            package,
                            scan,
                            true,
                        );
                    }
                    if branch != Some(true) {
                        self.scan_stmts(
                            &s.orelse,
                            source,
                            display,
                            importer_dir,
                            root,
                            current,
                            package,
                            scan,
                            true,
                        );
                    }
                }
                ast::Stmt::For(s) => {
                    self.scan_loop_body(
                        &s.body,
                        &s.orelse,
                        source,
                        display,
                        importer_dir,
                        root,
                        current,
                        package,
                        scan,
                    );
                }
                ast::Stmt::AsyncFor(s) => {
                    self.scan_loop_body(
                        &s.body,
                        &s.orelse,
                        source,
                        display,
                        importer_dir,
                        root,
                        current,
                        package,
                        scan,
                    );
                }
                ast::Stmt::While(s) => {
                    self.scan_loop_body(
                        &s.body,
                        &s.orelse,
                        source,
                        display,
                        importer_dir,
                        root,
                        current,
                        package,
                        scan,
                    );
                }
                ast::Stmt::Try(s) => {
                    self.scan_stmts(
                        &s.body,
                        source,
                        display,
                        importer_dir,
                        root,
                        current,
                        package,
                        scan,
                        true,
                    );
                    for handler in &s.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        self.scan_stmts(
                            &handler.body,
                            source,
                            display,
                            importer_dir,
                            root,
                            current,
                            package,
                            scan,
                            true,
                        );
                    }
                    self.scan_stmts(
                        &s.orelse,
                        source,
                        display,
                        importer_dir,
                        root,
                        current,
                        package,
                        scan,
                        true,
                    );
                    self.scan_stmts(
                        &s.finalbody,
                        source,
                        display,
                        importer_dir,
                        root,
                        current,
                        package,
                        scan,
                        true,
                    );
                }
                ast::Stmt::With(s) => self.scan_stmts(
                    &s.body,
                    source,
                    display,
                    importer_dir,
                    root,
                    current,
                    package,
                    scan,
                    true,
                ),
                ast::Stmt::AsyncWith(s) => self.scan_stmts(
                    &s.body,
                    source,
                    display,
                    importer_dir,
                    root,
                    current,
                    package,
                    scan,
                    true,
                ),
                _ => {}
            }
        }
    }

    /// Scans a loop body and its `else` clause (imports may appear in either).
    #[allow(clippy::too_many_arguments)]
    fn scan_loop_body(
        &mut self,
        body: &[ast::Stmt],
        orelse: &[ast::Stmt],
        source: &str,
        display: &Path,
        importer_dir: &Path,
        root: &Path,
        current: &str,
        package: &str,
        scan: &mut ImportScan,
    ) {
        self.scan_stmts(
            body,
            source,
            display,
            importer_dir,
            root,
            current,
            package,
            scan,
            true,
        );
        self.scan_stmts(
            orelse,
            source,
            display,
            importer_dir,
            root,
            current,
            package,
            scan,
            true,
        );
    }

    /// Resolves one module-level user import and loads it on demand.
    ///
    /// A module that is still initializing (an ancestor package of the
    /// importer) is recorded for late binding; a real cycle is reported.
    #[allow(clippy::too_many_arguments)]
    fn resolve_import(
        &mut self,
        source: &str,
        display: &Path,
        importer_dir: &Path,
        module_root: &Path,
        name: &str,
        range: rustpython_parser::text_size::TextRange,
        scan: &mut ImportScan,
    ) {
        if pyx_ir::stdlib::module(name).is_some() {
            return;
        }

        // A compiled extension is resolved by the lowerer: it binds
        // aliases/members to `Callee::Extension` and needs no linked module
        // unit. Look it up before rejecting dotted names so `pkg.sub_ext`
        // works without package Python code.
        match lower::find_extension_for_source(display, name) {
            lower::ExtensionLookup::Found(_) => return,
            lower::ExtensionLookup::Incompatible { path, reason } => {
                self.push_span(
                    source,
                    display,
                    range,
                    codes::UNSUPPORTED,
                    format!(
                        "compiled extension '{}' is incompatible: {reason}",
                        path.display()
                    ),
                );
                scan.failed = true;
                return;
            }
            lower::ExtensionLookup::Missing => {}
        }

        match self.load_named(name, importer_dir, module_root) {
            NamedLoad::Loaded(loaded) => {
                for index in loaded {
                    if !scan.deps.contains(&index) {
                        scan.deps.push(index);
                    }
                }
            }
            NamedLoad::Failed => scan.failed = true,
            // The module is already initializing (an ancestor package of the
            // importer): no unit to link, and nothing to report here. The
            // lowerer binds its members late, through the runtime global table.
            NamedLoad::InProgress => scan.record_late(name),
            NamedLoad::Cyclic { chain } => {
                self.push_span(
                    source,
                    display,
                    range,
                    codes::UNSUPPORTED,
                    format!("circular import: {}", chain.join(" -> ")),
                );
                scan.failed = true;
            }
            NamedLoad::Missing => {
                self.push_span(
                    source,
                    display,
                    range,
                    codes::UNSUPPORTED,
                    format!("module '{name}' not found: not supported on the import search path"),
                );
                if let Some(last) = self.diags.last_mut() {
                    last.help =
                        Some("expected a module file on the import search path".to_string());
                }
                scan.failed = true;
            }
        }
    }

    fn push_span(
        &mut self,
        source: &str,
        display: &Path,
        range: rustpython_parser::text_size::TextRange,
        code: &'static str,
        message: impl Into<String>,
    ) {
        let start = u32::from(range.start()) as usize;
        let (line, col) = diag::line_col(source, start);
        let end_col = col + u32::from(range.len()).max(1);
        self.diags.push(
            Diagnostic::new(&display.to_string_lossy(), line, col, message)
                .with_code(code)
                .with_span(end_col),
        );
    }
}

/// Directories searched for user modules, in order: the importing file's
/// directory, the root of the current package chain (so `pkg.a` can import
/// `pkg.b`), `PYX_SITE_PACKAGES`, the site-packages directories of
/// `VIRTUAL_ENV`/`CONDA_PREFIX`, then `PYTHONPATH`.
fn module_search_roots(importer_dir: &Path, module_root: &Path) -> Vec<PathBuf> {
    let pyx_site_packages = std::env::var_os("PYX_SITE_PACKAGES");
    let pythonpath = std::env::var_os("PYTHONPATH");
    let virtual_env = std::env::var_os("VIRTUAL_ENV").map(PathBuf::from);
    let conda_prefix = std::env::var_os("CONDA_PREFIX").map(PathBuf::from);
    module_search_roots_from(
        importer_dir,
        module_root,
        pyx_site_packages.as_deref(),
        pythonpath.as_deref(),
        virtual_env.as_deref(),
        conda_prefix.as_deref(),
    )
}

/// Builds the module search path from raw environment values.
///
/// Missing and non-directory entries are dropped and duplicates are searched
/// only once. The importing file's directory always comes first.
fn module_search_roots_from(
    importer_dir: &Path,
    module_root: &Path,
    pyx_site_packages: Option<&std::ffi::OsStr>,
    pythonpath: Option<&std::ffi::OsStr>,
    virtual_env: Option<&Path>,
    conda_prefix: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = vec![importer_dir.to_path_buf(), module_root.to_path_buf()];
    for list in [pyx_site_packages].into_iter().flatten() {
        dirs.extend(std::env::split_paths(list));
    }
    for prefix in [virtual_env, conda_prefix].into_iter().flatten() {
        dirs.extend(lower::prefix_site_packages(prefix));
    }
    if let Some(list) = pythonpath {
        dirs.extend(std::env::split_paths(list));
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

/// Resolves `name` in the first search root that contains it.
///
/// Returns the package ancestors and the module itself in order, so the loader
/// can initialize parents first. Namespace packages (directories without
/// `__init__.py`) are not supported.
fn resolve_module(roots: &[PathBuf], name: &str) -> Option<Vec<ResolvedModule>> {
    if name.is_empty() || name.split('.').any(|part| part.is_empty()) {
        return None;
    }
    let parts: Vec<&str> = name.split('.').collect();
    for root in roots {
        if let Some(mut chain) = resolve_module_in(root, &parts) {
            for module in &mut chain {
                module.root = root.clone();
            }
            return Some(chain);
        }
    }
    None
}

fn resolve_module_in(root: &Path, parts: &[&str]) -> Option<Vec<ResolvedModule>> {
    let mut dir = root.to_path_buf();
    let mut chain = Vec::with_capacity(parts.len());
    let mut name = String::new();
    for (index, part) in parts.iter().enumerate() {
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(part);
        let last = index + 1 == parts.len();
        let package_dir = dir.join(part);
        let init = package_dir.join("__init__.py");
        let module_file = dir.join(format!("{part}.py"));
        if last {
            if init.is_file() {
                chain.push(ResolvedModule {
                    path: init,
                    name: name.clone(),
                    package: name,
                    root: PathBuf::new(),
                });
                return Some(chain);
            }
            if module_file.is_file() {
                let package = match name.rsplit_once('.') {
                    Some((parent, _)) => parent.to_string(),
                    None => String::new(),
                };
                chain.push(ResolvedModule {
                    path: module_file,
                    name,
                    package,
                    root: PathBuf::new(),
                });
                return Some(chain);
            }
            return None;
        }
        if !init.is_file() {
            return None;
        }
        chain.push(ResolvedModule {
            path: init,
            name: name.clone(),
            package: name.clone(),
            root: PathBuf::new(),
        });
        dir = package_dir;
    }
    None
}

/// Lowers every loaded module in dependency order and links the results.
///
/// The module bodies of imported units become [`Program::initializers`] (in
/// dependency order) and run before the entry module.
fn lower_modules(units: &[LoadedModule]) -> Result<Program, Vec<Diagnostic>> {
    let entry_index = units.len() - 1;
    let mut all_functions: Vec<pyx_ir::Function> = Vec::new();
    let mut exports: Vec<HashMap<String, lower::UserExport>> = vec![HashMap::new(); units.len()];
    let mut initializers: Vec<FunctionId> = Vec::new();
    let mut entry_main = 0;

    for (index, unit) in units.iter().enumerate() {
        let base = all_functions.len() as FunctionId;
        let mut infer = Infer::new();
        let mut lowerer = Lowerer::new(&unit.source, &unit.display, &mut infer);
        lowerer.set_fid_base(base);
        lowerer.set_mirror_all_globals(true);
        if index != entry_index {
            lowerer.set_dunder_name(&unit.name);
            lowerer.set_package(&unit.package);
            initializers.push(base);
        }
        let mut user_modules = HashMap::new();
        // Package `__init__.py` files import their submodules, which become
        // attributes of the package. Make the whole transitive dependency set
        // visible so `pkg.sub` resolves after `import pkg`.
        let mut visible: Vec<usize> = Vec::new();
        let mut pending: Vec<usize> = unit.deps.clone();
        while let Some(dep_index) = pending.pop() {
            if visible.contains(&dep_index) {
                continue;
            }
            visible.push(dep_index);
            pending.extend_from_slice(&units[dep_index].deps);
        }
        for dep_index in visible {
            let dep = &units[dep_index];
            user_modules.insert(
                dep.name.clone(),
                UserModule {
                    name: dep.name.clone(),
                    exports: exports[dep_index].clone(),
                },
            );
        }
        lowerer.set_user_modules(user_modules);
        // Modules this unit imports but cannot link (an initializing ancestor
        // package): their members are read late from the runtime global table.
        lowerer.set_late_modules(unit.late.iter().cloned().collect());
        // Modules imported only from nested blocks: the scan recorded them
        // without loading them, so their names bind to the runtime stub.
        lowerer.set_deferred_imports(
            unit.deferred
                .iter()
                .map(|(module, _)| module.clone())
                .collect(),
        );

        let mut hir = lowerer.lower_module(&unit.module);
        let exported = lowerer.exported_names();
        let lower_diags = std::mem::take(&mut lowerer.diags);
        drop(lowerer);
        if !lower_diags.is_empty() {
            return Err(lower_diags);
        }

        exports[index] = exported;
        if index != entry_index {
            for function in hir.functions.iter_mut().skip(1) {
                function.name = format!("{}.{}", unit.name, function.name);
            }
        }

        // Type-check this unit against placeholders for the already emitted
        // dependencies so cross-module calls see their signatures. Placeholder
        // module bodies are empty and therefore produce no diagnostics.
        let mut combined: Vec<HFunction> = all_functions.iter().map(placeholder_function).collect();
        combined.extend(hir.functions.iter().cloned());
        let check_hir = LHir {
            source_name: unit.display.clone(),
            functions: combined,
        };
        sema::check(&check_hir, &mut infer, &unit.display)?;

        let module_hir = LHir {
            source_name: unit.display.clone(),
            functions: std::mem::take(&mut hir.functions),
        };
        let emitted = sema::emit(&module_hir, &mut infer);
        all_functions.extend(emitted.functions);
        if index == entry_index {
            entry_main = base;
        }
    }

    Ok(Program {
        source_name: units[entry_index].display.clone(),
        functions: all_functions,
        main: entry_main,
        initializers,
    })
}

/// Placeholder HIR for an already emitted dependency function: signature only,
/// empty body and module flag set so return checks are skipped.
fn placeholder_function(function: &pyx_ir::Function) -> HFunction {
    HFunction {
        name: function.name.clone(),
        params: function
            .params
            .iter()
            .map(|param| HParam {
                local: param.local,
                name: param.name.clone(),
                ty: Ty::Concrete(param.ty),
                default: param.default.clone(),
                kind: param.kind,
            })
            .collect(),
        capture_count: function.capture_count,
        ret_ty: Ty::Concrete(function.ret_ty),
        locals: Vec::new(),
        body: Vec::new(),
        is_async: function.is_async,
        is_generator: function.is_generator,
        is_module: true,
        line: 1,
        col: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_errors_use_the_syntax_code_span_and_help() {
        let diags = check_source("def broken(:\n    pass\n", "test.py").expect_err("parse error");
        assert_eq!(diags.len(), 1, "{diags:?}");
        let diagnostic = &diags[0];
        assert_eq!(diagnostic.code, codes::SYNTAX);
        assert_eq!(diagnostic.line, 1);
        assert!(
            diagnostic.end_col > diagnostic.col,
            "parse span must cover at least one column: {diagnostic:?}"
        );
        assert_eq!(diagnostic.caret_len(), 1);
        assert_eq!(
            diagnostic.help.as_deref(),
            Some("check the Python syntax near this position")
        );
    }

    #[test]
    fn type_errors_from_the_frontend_use_the_type_code() {
        let diags = check_source("x = 1 + 'a'\n", "test.py").expect_err("type error");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, codes::TYPE);
    }

    #[test]
    fn unknown_names_use_the_name_code_and_suggestion() {
        let diags = check_source("value = 1\nprint(valu)\n", "test.py").expect_err("name error");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, codes::NAME);
        assert_eq!(diags[0].help.as_deref(), Some("did you mean `value`?"));
    }

    // ------------------------------------------------------------------
    // check_file: user modules
    // ------------------------------------------------------------------

    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(test_name: &str) -> TestDir {
            let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pyx-frontend-modules-{}-{test_name}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            TestDir { path }
        }

        fn write(&self, name: &str, source: &str) -> PathBuf {
            let path = self.path.join(name);
            fs::write(&path, source).expect("write source");
            path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Restores an environment variable when the test finishes.
    struct EnvVar {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvVar {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> EnvVar {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            EnvVar { key, old }
        }
    }

    impl Drop for EnvVar {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn program(path: &Path) -> Program {
        check_file(path).unwrap_or_else(|diags| panic!("unexpected diagnostics: {diags:?}"))
    }

    fn function_id(program: &Program, name: &str) -> FunctionId {
        program
            .functions
            .iter()
            .position(|function| function.name == name)
            .unwrap_or_else(|| {
                let names: Vec<&str> = program
                    .functions
                    .iter()
                    .map(|function| function.name.as_str())
                    .collect();
                panic!("function '{name}' not found; functions: {names:?}")
            }) as FunctionId
    }

    /// True when `expr` calls `id`, either linked directly or through the
    /// function value used for imported functions.
    fn calls_function(expr: &pyx_ir::Expr, id: FunctionId) -> bool {
        use pyx_ir::{Callee, ExprKind};
        match &expr.kind {
            ExprKind::Call {
                callee: Callee::Function(f),
                ..
            } => *f == id,
            ExprKind::IndirectCall { callee, .. } => {
                matches!(&callee.kind, ExprKind::FunctionRef(f) if *f == id)
            }
            _ => false,
        }
    }

    /// Returns the callee of a top-level `print(<call>)` statement.
    fn printed_call(program: &Program) -> FunctionId {
        use pyx_ir::{Callee, ExprKind, StmtKind};
        let main = program.main_function();
        for stmt in &main.body {
            if let StmtKind::Expr(expr) = &stmt.kind {
                if let ExprKind::Call { callee, args } = &expr.kind {
                    if matches!(callee, Callee::Builtin(pyx_ir::Builtin::Print)) {
                        if let Some(arg) = args.first() {
                            if let ExprKind::Call {
                                callee: Callee::Function(id),
                                ..
                            } = &arg.kind
                            {
                                return *id;
                            }
                            if let ExprKind::IndirectCall { callee, .. } = &arg.kind {
                                if let ExprKind::FunctionRef(id) = &callee.kind {
                                    return *id;
                                }
                            }
                        }
                    }
                }
            }
        }
        panic!("no print(<direct call>) statement in the entry module");
    }

    #[test]
    fn from_import_binds_the_exported_function() {
        let dir = TestDir::new("from-import");
        dir.write("helper.py", "def add(a, b):\n    return a + b\n");
        let main = dir.write("main.py", "from helper import add\nprint(add(1, 2))\n");
        let program = program(&main);

        let add = function_id(&program, "helper.add");
        assert_eq!(printed_call(&program), add);
        assert_eq!(program.main_function().name, "<module>");
    }

    #[test]
    fn import_alias_and_from_alias_link_direct_calls() {
        let dir = TestDir::new("alias");
        dir.write("helper.py", "def add(a, b):\n    return a + b\n");
        let main = dir.write(
            "main.py",
            "import helper as h\nfrom helper import add as plus\nprint(h.add(1, 2), plus(3, 4))\n",
        );
        let program = program(&main);
        let add = function_id(&program, "helper.add");

        use pyx_ir::{Callee, ExprKind, StmtKind};
        let main_fn = program.main_function();
        let mut calls = Vec::new();
        for stmt in &main_fn.body {
            if let StmtKind::Expr(expr) = &stmt.kind {
                if let ExprKind::Call { callee, args } = &expr.kind {
                    if matches!(callee, Callee::Builtin(pyx_ir::Builtin::Print)) {
                        for arg in args {
                            if calls_function(arg, add) {
                                calls.push(add);
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(calls, vec![add, add]);
    }

    #[test]
    fn nested_imports_are_lowered_in_dependency_order() {
        let dir = TestDir::new("nested");
        dir.write("base.py", "def inc(value):\n    return value + 1\n");
        dir.write(
            "helper.py",
            "from base import inc\n\ndef add(a, b):\n    return inc(a) + b\n",
        );
        let main = dir.write("main.py", "import helper\nprint(helper.add(1, 2))\n");
        let program = program(&main);

        // base was lowered before helper, helper before the entry module.
        let names: Vec<&str> = program
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        let base = names.iter().position(|name| *name == "base.inc").unwrap();
        let helper = names.iter().position(|name| *name == "helper.add").unwrap();
        assert!(base < helper);

        let inc = function_id(&program, "base.inc");
        use pyx_ir::{ExprKind, StmtKind};
        let helper_fn = &program.functions[helper];
        let mut found = false;
        for stmt in &helper_fn.body {
            if let StmtKind::Return(Some(expr)) = &stmt.kind {
                if let ExprKind::Bin { lhs, .. } = &expr.kind {
                    found = calls_function(lhs, inc);
                }
            }
        }
        assert!(found, "helper.add must call base.inc directly");
    }

    #[test]
    fn diamond_dependencies_load_each_file_once() {
        let dir = TestDir::new("diamond");
        dir.write("base.py", "def value():\n    return 1\n");
        dir.write(
            "left.py",
            "import base\n\ndef left():\n    return base.value()\n",
        );
        dir.write(
            "right.py",
            "import base\n\ndef right():\n    return base.value()\n",
        );
        let main = dir.write(
            "main.py",
            "import left\nimport right\nprint(left.left(), right.right())\n",
        );
        let program = program(&main);

        let module_bodies = program
            .functions
            .iter()
            .filter(|function| function.name == "<module>")
            .count();
        assert_eq!(module_bodies, 4, "entry, left, right and the shared base");
        let base_values = program
            .functions
            .iter()
            .filter(|function| function.name == "base.value")
            .count();
        assert_eq!(base_values, 1);
    }

    #[test]
    fn circular_imports_are_diagnosed_with_both_files() {
        let dir = TestDir::new("cycle");
        dir.write("a.py", "from b import g\n\ndef f():\n    return 1\n");
        dir.write("b.py", "from a import f\n\ndef g():\n    return 2\n");
        let main = dir.write("main.py", "import a\nprint(a.f())\n");
        let diags = check_file(&main).expect_err("cycle must fail");
        assert!(
            diags
                .iter()
                .any(|diag| diag.message.contains("circular import")),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|diag| diag.file.ends_with("b.py")),
            "diagnostic must point at the importing file: {diags:?}"
        );
    }

    #[test]
    fn missing_modules_are_diagnosed_at_the_import() {
        let dir = TestDir::new("missing");
        let main = dir.write("main.py", "import nowhere\n");
        let diags = check_file(&main).expect_err("missing module must fail");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("module 'nowhere' not found"));
        assert!(diags[0].file.ends_with("main.py"));
        assert_eq!(diags[0].line, 1);
    }

    #[test]
    fn importing_a_non_function_is_diagnosed() {
        let dir = TestDir::new("non-function");
        dir.write("helper.py", "def add(a, b):\n    return a + b\n");
        let main = dir.write("main.py", "from helper import missing\n");
        let diags = check_file(&main).expect_err("missing export must fail");
        assert!(
            diags.iter().any(|diag| diag
                .message
                .contains("does not export a function named 'missing'")),
            "{diags:?}"
        );
    }

    #[test]
    fn class_and_constant_exports_are_linked_through_the_global_store() {
        let dir = TestDir::new("class-export");
        dir.write("helper.py", "class C:\n    pass\n\nVALUE = 41\n");
        let main = dir.write(
            "main.py",
            "from helper import C, VALUE\n\ndef make():\n    return C()\n\ndef get():\n    return VALUE\n\nprint(VALUE)\n",
        );
        let program = program(&main);

        // The imported module body mirrors both exported names.
        assert_eq!(program.initializers.len(), 1);
        let helper = &program.functions[program.initializers[0] as usize];
        assert_eq!(helper.name, "<module>");
        let mut class_mirrored = false;
        let mut value_mirrored = false;
        for stmt in &helper.body {
            if let pyx_ir::StmtKind::GlobalSet { name, .. } = &stmt.kind {
                class_mirrored |= name == "C";
                value_mirrored |= name == "VALUE";
            }
        }
        assert!(
            class_mirrored,
            "class C must be mirrored into the global store"
        );
        assert!(
            value_mirrored,
            "VALUE must be mirrored into the global store"
        );

        use pyx_ir::{Callee, ExprKind, StmtKind};
        let make = program.function(function_id(&program, "make"));
        let make_return = make
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Return(Some(expr)) => Some(expr),
                _ => None,
            })
            .expect("make must return");
        let ExprKind::IndirectCall { callee, .. } = &make_return.kind else {
            panic!("expected an indirect call, got {:?}", make_return.kind);
        };
        assert!(matches!(
            &callee.kind,
            ExprKind::GlobalGet { module, name } if module == "helper" && name == "C"
        ));

        let get = program.function(function_id(&program, "get"));
        let get_return = get
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Return(Some(expr)) => Some(expr),
                _ => None,
            })
            .expect("get must return");
        assert!(matches!(
            &get_return.kind,
            ExprKind::GlobalGet { module, name } if module == "helper" && name == "VALUE"
        ));

        // `print(VALUE)` in the entry body reads the imported global too.
        let entry = program.main_function();
        let mut found = false;
        for stmt in &entry.body {
            if let StmtKind::Expr(expr) = &stmt.kind {
                if let ExprKind::Call { callee, args } = &expr.kind {
                    if matches!(callee, Callee::Builtin(pyx_ir::Builtin::Print)) {
                        found |= matches!(
                            args.first().map(|arg| &arg.kind),
                            Some(ExprKind::GlobalGet { module, name })
                                if module == "helper" && name == "VALUE"
                        );
                    }
                }
            }
        }
        assert!(found, "the entry module must read helper.VALUE");
    }

    #[test]
    fn imported_module_bodies_run_once_as_initializers() {
        let dir = TestDir::new("side-effect");
        dir.write(
            "helper.py",
            "def add(a, b):\n    return a + b\n\nprint(add(1, 2))\n",
        );
        let main = dir.write("main.py", "import helper\nprint(helper.add(1, 2))\n");
        let program = program(&main);

        assert_eq!(
            program.initializers.len(),
            1,
            "each imported module body runs once"
        );
        let initializer = program.initializers[0];
        assert_ne!(
            initializer, program.main,
            "the entry body is not an initializer"
        );
        assert_eq!(program.functions[initializer as usize].name, "<module>");
        use pyx_ir::{Callee, ExprKind, StmtKind};
        let side_effect = program.functions[initializer as usize]
            .body
            .iter()
            .any(|stmt| {
                matches!(
                    &stmt.kind,
                    StmtKind::Expr(expr)
                        if matches!(&expr.kind, ExprKind::Call {
                            callee: Callee::Builtin(pyx_ir::Builtin::Print),
                            ..
                        })
                )
            });
        assert!(side_effect, "the imported print(...) must be compiled");
    }

    #[test]
    fn package_submodules_and_relative_imports_link() {
        let dir = TestDir::new("package");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(pkg.join("__init__.py"), "VALUE = 7\n").expect("write package init");
        fs::write(
            pkg.join("sub.py"),
            "from . import VALUE\n\ndef value():\n    return VALUE\n",
        )
        .expect("write submodule");
        let main = dir.write(
            "main.py",
            "import pkg.sub\nfrom pkg import sub\nfrom pkg.sub import value\nprint(sub.value(), value(), pkg.sub.value())\n",
        );
        let program = program(&main);

        let value = function_id(&program, "pkg.sub.value");
        use pyx_ir::{ExprKind, StmtKind};
        let function = program.function(value);
        let returned = function
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Return(Some(expr)) => Some(expr),
                _ => None,
            })
            .expect("pkg.sub.value must return");
        assert!(matches!(
            &returned.kind,
            ExprKind::GlobalGet { module, name } if module == "pkg" && name == "VALUE"
        ));

        // Both `sub.value()` and the imported `value()` link directly.
        let mut calls = 0;
        for stmt in &program.main_function().body {
            if let StmtKind::Expr(expr) = &stmt.kind {
                if let ExprKind::Call { callee, args } = &expr.kind {
                    if matches!(callee, pyx_ir::Callee::Builtin(pyx_ir::Builtin::Print)) {
                        for arg in args {
                            if calls_function(arg, value) {
                                calls += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(calls, 3, "all spellings must call pkg.sub.value");

        assert_eq!(
            program.initializers.len(),
            2,
            "pkg and pkg.sub bodies run before the entry"
        );
        assert!(program.initializers[0] < program.initializers[1]);
        assert!(program.initializers[1] < program.main);
    }

    #[test]
    fn relative_module_imports_resolve_against_the_package() {
        let dir = TestDir::new("relative");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(pkg.join("__init__.py"), "").expect("write package init");
        fs::write(pkg.join("sub.py"), "def value():\n    return 1\n").expect("write submodule");
        fs::write(
            pkg.join("helper.py"),
            "from . import sub\nfrom .sub import value\n\ndef call():\n    return value()\n\ndef call_sub():\n    return sub.value()\n",
        )
        .expect("write helper");
        let main = dir.write(
            "main.py",
            "from pkg.helper import call, call_sub\nprint(call(), call_sub())\n",
        );
        let program = program(&main);

        let call = function_id(&program, "pkg.helper.call");
        assert_eq!(printed_call(&program), call);
        let value = function_id(&program, "pkg.sub.value");
        use pyx_ir::StmtKind;
        for name in ["pkg.helper.call", "pkg.helper.call_sub"] {
            let function = program.function(function_id(&program, name));
            let returned = function
                .body
                .iter()
                .find_map(|stmt| match &stmt.kind {
                    StmtKind::Return(Some(expr)) => Some(expr),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name} must return"));
            assert!(
                calls_function(returned, value),
                "{name} must call pkg.sub.value"
            );
        }
    }

    #[test]
    fn site_packages_root_resolves_modules_without_sibling_files() {
        let dir = TestDir::new("site-packages");
        let site = dir.path.join("site-packages");
        let project = dir.path.join("project");
        fs::create_dir_all(site.join("pkg")).expect("create site package");
        fs::create_dir_all(&project).expect("create project");
        fs::write(site.join("pkg").join("__init__.py"), "VALUE = 3\n").expect("write package init");
        fs::write(
            site.join("pkg").join("mod.py"),
            "from . import VALUE\n\ndef get():\n    return VALUE\n",
        )
        .expect("write module");
        let main = project.join("main.py");
        fs::write(&main, "from pkg.mod import get\nprint(get())\n").expect("write entry");

        let roots =
            module_search_roots_from(&project, &project, Some(site.as_os_str()), None, None, None);
        assert_eq!(roots, vec![project.clone(), site.clone()]);

        let _guard = EnvVar::set("PYX_SITE_PACKAGES", site.as_os_str());
        let program = program(&main);
        let get = function_id(&program, "pkg.mod.get");
        assert_eq!(printed_call(&program), get);
        assert_eq!(
            program.initializers.len(),
            2,
            "pkg and pkg.mod bodies run before the entry"
        );
    }

    #[test]
    fn entry_module_binds_dunder_name_to_main() {
        let dir = TestDir::new("dunder-name");
        let main = dir.write("main.py", "print(__name__)\n");
        let program = program(&main);
        use pyx_ir::{Const, ExprKind, StmtKind};
        let entry = program.main_function();
        let mut found = false;
        for stmt in &entry.body {
            if let StmtKind::Assign { target, value } = &stmt.kind {
                if entry.local(*target).name == "__name__" {
                    found = matches!(&value.kind, ExprKind::Const(Const::Str(name)) if name.as_str() == "__main__");
                }
            }
        }
        assert!(found, "__name__ must be initialized to '__main__'");
    }

    // ------------------------------------------------------------------
    // Imports nested in functions and blocks
    // ------------------------------------------------------------------

    /// Returns the expression of the single `return` in `function`.
    fn returned_expression(program: &Program, function: FunctionId) -> &pyx_ir::Expr {
        use pyx_ir::StmtKind;
        program
            .function(function)
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Return(Some(expr)) => Some(expr),
                _ => None,
            })
            .expect("function must return a value")
    }

    #[test]
    fn function_local_import_is_deferred_and_does_not_load_the_module() {
        let dir = TestDir::new("deferred-sibling");
        dir.write("helper.py", "def add(a, b):\n    return a + b\n");
        let main = dir.write(
            "main.py",
            "def run():\n    from helper import add\n    return add(1, 2)\n\nprint(run())\n",
        );
        let program = program(&main);

        let run = function_id(&program, "run");
        use pyx_ir::ExprKind;
        let ExprKind::IndirectCall { callee, .. } = &returned_expression(&program, run).kind else {
            panic!(
                "expected a deferred call, got {:?}",
                returned_expression(&program, run).kind
            );
        };
        assert!(
            matches!(
                &callee.kind,
                ExprKind::DeferredImport { module, member }
                    if module == "helper" && member.as_deref() == Some("add")
            ),
            "the nested import must bind a deferred name: {:?}",
            callee.kind
        );
        assert!(
            program.initializers.is_empty(),
            "the nested import must not load helper eagerly"
        );
    }

    #[test]
    fn function_local_import_binds_a_deferred_package_submodule() {
        let dir = TestDir::new("deferred-submodule");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(pkg.join("__init__.py"), "").expect("write package init");
        fs::write(pkg.join("sub.py"), "def value():\n    return 7\n").expect("write submodule");
        let main = dir.write(
            "main.py",
            "def run():\n    from pkg import sub\n    return sub.value()\n\nprint(run())\n",
        );
        let program = program(&main);

        let run = function_id(&program, "run");
        use pyx_ir::ExprKind;
        let ExprKind::IndirectCall { callee, .. } = &returned_expression(&program, run).kind else {
            panic!(
                "expected a deferred call, got {:?}",
                returned_expression(&program, run).kind
            );
        };
        assert!(
            matches!(
                &callee.kind,
                ExprKind::DeferredImport { module, member }
                    if module == "pkg" && member.as_deref() == Some("sub.value")
            ),
            "the nested submodule import must stay deferred: {:?}",
            callee.kind
        );
        assert!(
            program.initializers.is_empty(),
            "the nested import must not load pkg"
        );
    }

    #[test]
    fn function_local_relative_import_is_deferred() {
        let dir = TestDir::new("deferred-relative");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(pkg.join("__init__.py"), "").expect("write package init");
        fs::write(pkg.join("helper.py"), "def add(a, b):\n    return a + b\n")
            .expect("write helper");
        fs::write(
            pkg.join("sub.py"),
            "def run():\n    from .helper import add\n    return add(1, 2)\n",
        )
        .expect("write submodule");
        let main = dir.write("main.py", "from pkg.sub import run\nprint(run())\n");
        let program = program(&main);

        let run = function_id(&program, "pkg.sub.run");
        use pyx_ir::ExprKind;
        let ExprKind::IndirectCall { callee, .. } = &returned_expression(&program, run).kind else {
            panic!(
                "expected a deferred call, got {:?}",
                returned_expression(&program, run).kind
            );
        };
        assert!(
            matches!(
                &callee.kind,
                ExprKind::DeferredImport { module, member }
                    if module == "pkg.helper" && member.as_deref() == Some("add")
            ),
            "`from .helper import add` must resolve against the package: {:?}",
            callee.kind
        );
        assert!(
            !program
                .functions
                .iter()
                .any(|function| function.name == "pkg.helper.add"),
            "pkg.helper must not be compiled"
        );
    }

    #[test]
    fn function_local_import_of_a_missing_module_is_deferred() {
        let dir = TestDir::new("deferred-missing");
        let main = dir.write(
            "main.py",
            "def run():\n    from nowhere import thing\n    return thing()\n\nprint(run())\n",
        );
        // A nested import is not resolved at compile time, so a module that
        // does not exist only fails when the binding is read.
        let program = program(&main);
        let run = function_id(&program, "run");
        use pyx_ir::ExprKind;
        let ExprKind::IndirectCall { callee, .. } = &returned_expression(&program, run).kind else {
            panic!(
                "expected a deferred call, got {:?}",
                returned_expression(&program, run).kind
            );
        };
        assert!(matches!(
            &callee.kind,
            ExprKind::DeferredImport { module, member }
                if module == "nowhere" && member.as_deref() == Some("thing")
        ));
    }

    #[test]
    fn submodule_can_import_an_initializing_package_member() {
        let dir = TestDir::new("initializing-parent");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "from .child import run\n\ndef greeting():\n    return 1\n",
        )
        .expect("write package init");
        fs::write(
            pkg.join("child.py"),
            "from . import greeting\n\ndef run():\n    return greeting()\n",
        )
        .expect("write child");
        let main = dir.write("main.py", "from pkg.child import run\nprint(run())\n");
        let program = program(&main);

        let run = function_id(&program, "pkg.child.run");
        use pyx_ir::{ExprKind, StmtKind};
        let ExprKind::IndirectCall { callee, .. } = &returned_expression(&program, run).kind else {
            panic!("expected an indirect call to the package member");
        };
        assert!(
            matches!(
                &callee.kind,
                ExprKind::GlobalGet { module, name } if module == "pkg" && name == "greeting"
            ),
            "the initializing package member must be read late: {:?}",
            callee.kind
        );

        // The package initializer mirrors `greeting` for that late read.
        let mirrored = program.initializers.iter().any(|id| {
            program.functions[*id as usize].body.iter().any(
                |stmt| matches!(&stmt.kind, StmtKind::GlobalSet { name, .. } if name == "greeting"),
            )
        });
        assert!(mirrored, "pkg's function must be mirrored as a global");
    }

    #[test]
    fn type_checking_guarded_imports_are_not_resolved() {
        let dir = TestDir::new("type-checking");
        let main = dir.write(
            "main.py",
            "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    import nowhere\n\nprint(1)\n",
        );
        // A missing module inside the guard must not be reported.
        let program = program(&main);
        use pyx_ir::StmtKind;
        assert!(
            program
                .main_function()
                .body
                .iter()
                .all(|stmt| !matches!(stmt.kind, StmtKind::If { .. })),
            "the type-checking branch must not be lowered"
        );
    }

    #[test]
    fn nested_mutual_imports_are_not_cycles() {
        let dir = TestDir::new("deferred-cycle");
        dir.write(
            "a.py",
            "def f():\n    from b import g\n    return g()\n\ndef h():\n    return 1\n",
        );
        dir.write(
            "b.py",
            "def g():\n    from a import f\n    return f()\n\ndef only():\n    return 2\n",
        );
        let main = dir.write("main.py", "from a import f\nprint(f())\n");
        let program = program(&main);

        // a's import of b is inside a function, so b is neither loaded nor
        // linked: there is no cycle, and `f` binds `g` as a deferred name.
        assert!(
            !program
                .functions
                .iter()
                .any(|function| function.name == "b.g"),
            "b must not be compiled"
        );
        let f = function_id(&program, "a.f");
        use pyx_ir::ExprKind;
        let ExprKind::IndirectCall { callee, .. } = &returned_expression(&program, f).kind else {
            panic!(
                "a.f must call a deferred name, got {:?}",
                returned_expression(&program, f).kind
            );
        };
        assert!(
            matches!(
                &callee.kind,
                ExprKind::DeferredImport { module, member }
                    if module == "b" && member.as_deref() == Some("g")
            ),
            "a.f must bind b.g as a deferred call: {:?}",
            callee.kind
        );
        assert_eq!(
            program.initializers.len(),
            1,
            "only a's module body is an initializer"
        );
    }

    #[test]
    fn imported_modules_skip_their_main_blocks() {
        let dir = TestDir::new("main-guard");
        dir.write(
            "helper.py",
            "def add(a, b):\n    return a + b\n\nif __name__ == \"__main__\":\n    import nowhere\n    print(add(1, 2))\n",
        );
        let main = dir.write(
            "main.py",
            "from helper import add\n\nif __name__ == \"__main__\":\n    print(add(1, 2))\n",
        );
        let program = program(&main);

        // The entry's own main block is live and keeps its body.
        use pyx_ir::StmtKind;
        assert!(
            program.main_function().body.iter().any(
                |stmt| matches!(&stmt.kind, StmtKind::If { then_body, .. } if !then_body.is_empty())
            ),
            "the entry's __main__ block must still be lowered"
        );
    }
}
