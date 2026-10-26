//! Hydro frontend: parse Python source, lower it to typed IR, infer/validate
//! types, and report source-located diagnostics.
//!
//! Public contract (used by the Tea toolchain and tests):
//! [`check_source`] returns either a fully typed [`Program`] or a non-empty list
//! [`Diagnostic`]s. [`check_file`] additionally resolves imports of user
//! modules and packages (siblings, site-packages, `PYTHONPATH` and
//! `HYDRO_STDLIB`) and links them into one program. Both must never panic on
//! user input.
//!
//! [`session::SessionCompiler`] builds incremental REPL/interpreter fragments
//! on top of the same parser, lowerer and type checker, preserving modules,
//! import bindings and session globals between fragments.

pub mod diag;
mod lower;
mod sema;
pub mod session;

pub use diag::Diagnostic;
pub use session::{
    is_incomplete, SessionCompiler, SessionEntry, SessionFragment, SessionImport, SessionOptions,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use hydro_ir::{FunctionId, Program};
use rustpython_parser::{ast, Mode, TemplateString};

use diag::codes;
use lower::{HFunction, HParam, LHir, Lowerer, UserModule};
use sema::{Infer, Ty};

/// Parse, lower, type-check and return a fully typed program.
///
/// Errors are collected rather than short-circuited where practical; the
/// returned vector is never empty on failure.
pub fn check_source(source: &str, filename: &str) -> Result<Program, Vec<Diagnostic>> {
    let (module, templates) = parse_module(source, filename)?;

    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, filename, &mut infer);
    lowerer.set_templates(&templates);
    let hir = lowerer.lower_module(&module);
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    if !diags.is_empty() {
        return Err(diags);
    }

    sema::check(&hir, &mut infer, filename)?;
    Ok(sema::emit(&hir, &mut infer))
}

/// Parse `source` into a module AST plus the PEP 750 template side table,
/// reporting syntax errors (including malformed template strings) as
/// diagnostics.
fn parse_module(
    source: &str,
    filename: &str,
) -> Result<(ast::ModModule, Vec<TemplateString>), Vec<Diagnostic>> {
    let parsed = match rustpython_parser::parse_module_with_templates(source, filename) {
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

    match parsed.module {
        ast::Mod::Module(module) => Ok((module, parsed.templates)),
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
/// next to the importing file, then through `HYDRO_SITE_PACKAGES`, the
/// `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages directories, `PYTHONPATH` and
/// finally `HYDRO_STDLIB` (the CPython `Lib` directory backing the standard
/// library sources). Both plain modules (`foo.py`) and packages
/// (`foo/__init__.py`) are supported, including dotted names, submodules and
/// relative imports. A directory without `__init__.py` is a namespace-package
/// segment: it is importable and acts as a search root for its submodules, but
/// it has no module body and therefore no initializer. Every module is loaded
/// once per canonical path; missing files and incompatible
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
    let entry = loader.load_file(
        key,
        display,
        "__main__".to_string(),
        String::new(),
        root.clone(),
    );
    if let Some(entry) = entry {
        if loader.diags.is_empty() {
            // Nested imports are deferred while the importing module is still
            // being scanned. Once every dependency is initialized, a
            // declaration-only site-packages module can be linked eagerly and
            // its member becomes a normal import target at runtime.
            loader.preload_deferred_site_modules(entry, &root);
        }
    }

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

    lower_modules(&loader.units, &loader.namespaces)
}

/// Parse, lower and type-check a pytest test file together with every
/// ancestor `conftest.py`.
///
/// `conftest.py` units are loaded before the test file and linked as its
/// dependencies (root-most first, so the nearest conftest wins fixture
/// lookups). Their module bodies run once per child, before the test body, and
/// their fixtures are visible to the generated test wrappers by name. Returns
/// the same typed [`Program`] as [`check_file`].
pub fn check_pytest_file(path: &Path) -> Result<Program, Vec<Diagnostic>> {
    let mut loader = ModuleLoader::default();
    let display = path.to_path_buf();
    let key = fs::canonicalize(path).unwrap_or_else(|_| display.clone());
    let root = display
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    // Root-most conftest first. A deeper conftest depends on its parent so the
    // lowerer can reference parent fixtures while generating wrappers.
    let mut directories: Vec<PathBuf> = display
        .parent()
        .map(Path::ancestors)
        .into_iter()
        .flatten()
        .map(Path::to_path_buf)
        .collect();
    directories.reverse();
    directories.dedup();

    let mut conftests: Vec<usize> = Vec::new();
    for (index, directory) in directories.iter().enumerate() {
        let candidate = directory.join("conftest.py");
        if !candidate.is_file() {
            continue;
        }
        let conftest_key = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        let name = format!("conftest{index}");
        if let Some(unit) =
            loader.load_file(conftest_key, candidate, name, String::new(), root.clone())
        {
            if let Some(parent) = conftests.last().copied() {
                loader.units[unit].deps.push(parent);
            }
            conftests.push(unit);
        }
    }

    let entry = loader.load_file(
        key,
        display,
        "__main__".to_string(),
        String::new(),
        root.clone(),
    );
    if let Some(entry) = entry {
        for conftest in &conftests {
            if !loader.units[entry].deps.contains(conftest) {
                loader.units[entry].deps.push(*conftest);
            }
        }
        if loader.diags.is_empty() {
            loader.preload_deferred_site_modules(entry, &root);
        }
    }

    if !loader.diags.is_empty() {
        return Err(loader.diags);
    }
    if loader.units.is_empty() {
        return Err(vec![Diagnostic::new(
            &path.to_string_lossy(),
            1,
            1,
            "cannot read module",
        )]);
    }
    lower_modules(&loader.units, &loader.namespaces)
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
    /// PEP 750 template strings of this module, keyed by source range. The
    /// AST represents each literal as a reserved placeholder `Name`; the
    /// lowerer resolves it here and never treats it as a variable.
    templates: Vec<TemplateString>,
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
    /// Module file, or the package directory for a namespace package.
    path: PathBuf,
    /// Full dotted module name.
    name: String,
    /// `__package__` of the module.
    package: String,
    /// Search root the module chain was found in.
    root: PathBuf,
    /// The segment is a namespace package: a directory without an
    /// `__init__.py`. It has no unit and no initializer; it only contributes
    /// its directory as a search root for submodules.
    namespace: bool,
}

/// Outcome of resolving and loading one dotted module name.
enum NamedLoad {
    /// Every loaded unit in the chain (package ancestors first, module last).
    /// Empty when the name resolves to a namespace package: it exists, but it
    /// has no body to initialize.
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
    /// Namespace packages (directories without `__init__.py`) the search path
    /// resolved while loading this program, in discovery order. They have no
    /// unit; the lowerer exposes them as empty user modules so `import pkg`
    /// and `from pkg import sub` bind like CPython.
    namespaces: Vec<String>,
    diags: Vec<Diagnostic>,
}

impl ModuleLoader {
    /// Loads one source unit that has no file on disk (a REPL fragment,
    /// `<stdin>` or `<tea -c>`) and resolves its module-level imports.
    ///
    /// `virtual_key` is the loader identity of this unit: callers compiling
    /// several fragments must pass a fresh key per fragment so the units stay
    /// distinct. `filename` is the display name used for diagnostics and for
    /// resolving imports: a real path anchors the search at its directory,
    /// while `<stdin>` searches the current directory.
    ///
    /// Diagnostics produced while scanning imports are returned instead of
    /// being left in the loader, so a failed fragment can be rolled back by
    /// the caller.
    #[allow(dead_code)] // Public-to-the-crate session helper; `load_source_at` is used directly.
    pub(crate) fn load_source(
        &mut self,
        virtual_key: PathBuf,
        source: String,
        filename: String,
    ) -> Result<usize, Vec<Diagnostic>> {
        let importer_dir = Path::new(&filename)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        self.load_source_at(virtual_key, source, filename, importer_dir)
    }

    /// [`Self::load_source`] with an explicit import search root.
    pub(crate) fn load_source_at(
        &mut self,
        virtual_key: PathBuf,
        source: String,
        filename: String,
        importer_dir: PathBuf,
    ) -> Result<usize, Vec<Diagnostic>> {
        let diags_before = self.diags.len();
        let file = filename.clone();
        let (module, templates) = parse_module(&source, &file)?;

        let display = PathBuf::from(&filename);
        self.stack
            .push((virtual_key.clone(), "__main__".to_string()));
        let mut scan = ImportScan::default();
        self.scan_imports(
            &module,
            &source,
            &display,
            &importer_dir,
            &importer_dir,
            "__main__",
            "",
            &mut scan,
        );
        self.stack.pop();

        if scan.failed {
            return Err(self.diags.split_off(diags_before));
        }

        let index = self.units.len();
        self.units.push(LoadedModule {
            name: "__main__".to_string(),
            package: String::new(),
            display: file,
            source,
            module,
            templates,
            deps: scan.deps,
            late: scan.late,
            deferred: scan.deferred,
        });
        // The key is not registered in `loaded`: a session deliberately
        // compiles every fragment as a new unit, even when it reuses a
        // virtual filename such as `<stdin>`.
        debug_assert_eq!(self.units.len() - 1, index);
        Ok(index)
    }

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
        let (module, templates) = match parse_module(&source, &file) {
            Ok(parsed) => parsed,
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
            templates,
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
        let final_namespace = chain.last().is_some_and(|module| module.namespace);
        for (position, resolved) in chain.into_iter().enumerate() {
            if resolved.namespace {
                // Namespace package: importable, but it has no body to
                // initialize. Its directory is already part of the search
                // path, so submodules resolve on their own.
                self.record_namespace(&resolved.name);
                continue;
            }
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

        if loaded.is_empty() && !final_namespace {
            NamedLoad::Missing
        } else {
            // A final namespace package exists but contributes no unit, so
            // `Loaded` may be empty: the import succeeds without an
            // initializer.
            NamedLoad::Loaded(loaded)
        }
    }

    /// Records a namespace package resolved by the search path.
    fn record_namespace(&mut self, name: &str) {
        if !self.namespaces.iter().any(|existing| existing == name) {
            self.namespaces.push(name.to_string());
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

    /// Whether `from` (transitively) depends on `to` in the current unit graph.
    fn depends_on(&self, from: usize, to: usize) -> bool {
        let mut pending = vec![from];
        let mut seen = HashSet::new();
        while let Some(index) = pending.pop() {
            if !seen.insert(index) {
                continue;
            }
            if let Some(unit) = self.units.get(index) {
                if unit.deps.contains(&to) {
                    return true;
                }
                pending.extend(unit.deps.iter().copied());
            }
        }
        false
    }

    /// Whether every module-level import of `name` is already linked, native,
    /// or itself a declaration-only site-packages module whose dependencies
    /// are available. Used as a dry run so preloading never pulls a chain
    /// with missing third-party/stdlib dependencies (for example
    /// `rich.pretty` -> `inspect`).
    fn preload_dependencies_available(
        &self,
        name: &str,
        root: &Path,
        visiting: &mut HashSet<String>,
    ) -> bool {
        if hydro_ir::stdlib::module(name).is_some() || self.by_name.contains_key(name) {
            return true;
        }
        if !visiting.insert(name.to_string()) {
            return true;
        }
        let roots = module_search_roots(root, root);
        let Some(chain) = resolve_module(&roots, name) else {
            return false;
        };
        let Some(final_module) = chain.last() else {
            return false;
        };
        // Namespace packages have no file to parse and no body to preload.
        if final_module.namespace {
            return false;
        }
        let Ok(source) = fs::read_to_string(&final_module.path) else {
            return false;
        };
        let Ok(ast::Mod::Module(module)) =
            rustpython_parser::parse(&source, Mode::Module, &final_module.path.to_string_lossy())
        else {
            return false;
        };
        let package = name.rsplit_once('.').map_or("", |(parent, _)| parent);
        for stmt in &module.body {
            let (level, module_name) = match stmt {
                ast::Stmt::Import(s) => {
                    for alias in &s.names {
                        if !self.preload_dependencies_available(alias.name.as_str(), root, visiting)
                        {
                            return false;
                        }
                    }
                    continue;
                }
                ast::Stmt::ImportFrom(s) => (
                    s.level.as_ref().map(|level| level.to_u32()).unwrap_or(0),
                    s.module.as_ref().map(|module| module.as_str()),
                ),
                _ => continue,
            };
            let module_name = if level == 0 {
                module_name.unwrap_or("").to_string()
            } else {
                let Ok(resolved) = lower::resolve_import_name(level, module_name, package) else {
                    return false;
                };
                resolved
            };
            if hydro_ir::stdlib::module(&module_name).is_some()
                || self.by_name.contains_key(&module_name)
            {
                continue;
            }
            if !self.preloadable_nested_module(&module_name, root, root)
                || !self.preload_dependencies_available(&module_name, root, visiting)
            {
                return false;
            }
        }
        visiting.remove(name);
        true
    }

    /// Links declaration-only site-packages modules that are referenced only
    /// from nested blocks.
    ///
    /// They are loaded after the importer graph is complete, so package cycles
    /// (`rich.console` <-> `rich.status`) are no longer initializing. Each
    /// candidate is transactional: a module that triggers diagnostics stays
    /// deferred and leaves the loader state untouched.
    fn preload_deferred_site_modules(&mut self, entry: usize, root: &Path) {
        let old_len = self.units.len();
        let candidates: Vec<String> = self
            .units
            .iter()
            .flat_map(|unit| unit.deferred.iter())
            .map(|(module, _)| module.clone())
            .filter(|module| {
                let basic = !self.by_name.contains_key(module)
                    && hydro_ir::stdlib::module(module).is_none()
                    && self.preloadable_nested_module(module, root, root);
                basic && self.preload_dependencies_available(module, root, &mut HashSet::new())
            })
            .collect();
        for module in candidates {
            let units_before = self.units.len();
            let diags_before = self.diags.len();
            let loaded_before: Vec<PathBuf> = self.loaded.keys().cloned().collect();
            let names_before: Vec<String> = self.by_name.keys().cloned().collect();
            let loaded = self.probe_named(&module, root, root);
            if self.diags.len() > diags_before {
                self.diags.truncate(diags_before);
                self.units.truncate(units_before);
                self.loaded.retain(|path, _| loaded_before.contains(path));
                self.by_name.retain(|name, _| names_before.contains(name));
                continue;
            }
            let Some(index) = loaded else {
                continue;
            };
            let importers: Vec<usize> = self
                .units
                .iter()
                .enumerate()
                .filter(|(_, unit)| {
                    unit.deferred
                        .iter()
                        .any(|(deferred, _)| deferred == &module)
                })
                .map(|(index, _)| index)
                .collect();
            for importer in importers {
                // Adding the edge makes the loaded exports visible to the
                // lowerer. Skip only if it would introduce a dependency cycle
                // (the nested import then keeps its runtime stub).
                if !self.depends_on(index, importer) {
                    if !self.units[importer].deps.contains(&index) {
                        self.units[importer].deps.push(index);
                    }
                    self.units[importer]
                        .deferred
                        .retain(|(deferred, _)| deferred != &module);
                }
            }
        }
        if self.units.len() > old_len {
            self.reorder_preloaded(entry, old_len);
        }
    }

    /// Places modules loaded after the entry module before the earliest unit
    /// that references them, then moves the entry back to the end.
    ///
    /// Lowering is single-pass in unit order: an importer can only see exports
    /// of modules lowered before it. The loaded modules keep their own load
    /// order (dependencies first), and the insertion point is pushed behind
    /// every already-existing dependency.
    fn reorder_preloaded(&mut self, entry: usize, old_len: usize) {
        let old_entry = entry;
        debug_assert_eq!(old_entry, old_len - 1);
        let new_count = self.units.len() - old_len;
        if new_count == 0 {
            return;
        }
        let mut insert = self.units[..old_len]
            .iter()
            .position(|unit| unit.deps.iter().any(|dep| *dep >= old_len))
            .unwrap_or(old_len - 1);
        loop {
            let mut changed = false;
            for index in old_len..self.units.len() {
                for dep in &self.units[index].deps {
                    if *dep < old_len && *dep != old_entry && *dep >= insert {
                        insert = dep + 1;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        let old_units_len = old_len - 1;
        insert = insert.min(old_units_len);

        let mut slots: Vec<Option<LoadedModule>> = self.units.drain(..).map(Some).collect();
        let entry_unit = slots[old_entry].take().expect("entry unit");
        let new_units: Vec<LoadedModule> = slots[old_len..]
            .iter_mut()
            .map(|slot| slot.take().expect("preloaded unit"))
            .collect();
        let mut old_units: Vec<Option<LoadedModule>> = slots[..old_units_len]
            .iter_mut()
            .map(|slot| slot.take())
            .collect();
        let mut ordered: Vec<LoadedModule> = Vec::with_capacity(old_len + new_count);
        for unit in old_units.iter_mut().take(insert) {
            ordered.push(unit.take().expect("old unit"));
        }
        ordered.extend(new_units);
        for unit in old_units.iter_mut().skip(insert) {
            ordered.push(unit.take().expect("old unit"));
        }
        ordered.push(entry_unit);

        let map = |index: usize| -> usize {
            if index == old_entry {
                old_len + new_count - 1
            } else if index >= old_len {
                insert + (index - old_len)
            } else if index >= insert {
                index + new_count
            } else {
                index
            }
        };
        for unit in &mut ordered {
            unit.deps = unit.deps.iter().map(|dep| map(*dep)).collect();
        }
        let by_name = std::mem::take(&mut self.by_name);
        for (name, index) in by_name {
            self.by_name.insert(name, map(index));
        }
        let loaded = std::mem::take(&mut self.loaded);
        for (path, index) in loaded {
            self.loaded.insert(path, map(index));
        }
        self.units = ordered;
    }

    /// Whether the final module file for `name` has no imports of its own.
    /// These data-only modules are safe to preload during scanning, even while
    /// another package module is still initializing.
    fn import_free_module(&self, name: &str, importer_dir: &Path, module_root: &Path) -> bool {
        let roots = module_search_roots(importer_dir, module_root);
        let Some(chain) = resolve_module(&roots, name) else {
            return false;
        };
        let Some(final_module) = chain.last() else {
            return false;
        };
        // Namespace packages have no file to parse and no body to preload.
        if final_module.namespace {
            return false;
        }
        let site_package = final_module.path.components().any(|component| {
            matches!(
                component.as_os_str().to_str(),
                Some("site-packages") | Some("dist-packages")
            )
        });
        if !site_package {
            return false;
        }
        let Ok(source) = fs::read_to_string(&final_module.path) else {
            return false;
        };
        let Ok(ast::Mod::Module(module)) =
            rustpython_parser::parse(&source, Mode::Module, &final_module.path.to_string_lossy())
        else {
            return false;
        };
        module
            .body
            .iter()
            .all(|stmt| !matches!(stmt, ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_)))
    }

    /// Whether a nested import of a site-packages module may be preloaded.
    ///
    /// Only declaration-only modules (imports, functions, classes and a
    /// skipped `__main__` guard) qualify: their compile-time initializer has
    /// no observable side effects. Modules with executable top-level code
    /// stay deferred so optional dependency graphs keep their lazy semantics.
    fn preloadable_nested_module(
        &self,
        name: &str,
        importer_dir: &Path,
        module_root: &Path,
    ) -> bool {
        let roots = module_search_roots(importer_dir, module_root);
        let Some(chain) = resolve_module(&roots, name) else {
            return false;
        };
        let Some(final_module) = chain.last() else {
            return false;
        };
        // Namespace packages have no file to parse and no body to preload.
        if final_module.namespace {
            return false;
        }
        let site_package = final_module.path.components().any(|component| {
            matches!(
                component.as_os_str().to_str(),
                Some("site-packages") | Some("dist-packages")
            )
        });
        if !site_package {
            return false;
        }
        let Ok(source) = fs::read_to_string(&final_module.path) else {
            return false;
        };
        let Ok(ast::Mod::Module(module)) =
            rustpython_parser::parse(&source, Mode::Module, &final_module.path.to_string_lossy())
        else {
            return false;
        };
        module.body.iter().all(|stmt| match stmt {
            ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_) => true,
            ast::Stmt::FunctionDef(_) | ast::Stmt::AsyncFunctionDef(_) | ast::Stmt::ClassDef(_) => {
                true
            }
            ast::Stmt::If(s) => lower::static_if_branch(&s.test, name) == Some(false),
            _ => false,
        })
    }

    /// Links a module imported by a module-level `try` body.
    ///
    /// The import runs in normal execution order, so a module that resolves
    /// and compiles can become a regular dependency; its initializer then runs
    /// once, before the importer, exactly like an eager import. Anything that
    /// cannot be linked (absent module, compile diagnostics, a cycle) leaves
    /// the binding deferred, so the runtime raises `ModuleNotFoundError` and
    /// the enclosing `except ImportError` handles it. The load is transactional:
    /// a failed attempt restores every loader table.
    fn optional_import(
        &mut self,
        name: &str,
        importer_dir: &Path,
        module_root: &Path,
        scan: &mut ImportScan,
    ) -> bool {
        if hydro_ir::stdlib::module(name).is_some() {
            return true;
        }
        if let Some(&index) = self.by_name.get(name) {
            if !scan.deps.contains(&index) {
                scan.deps.push(index);
            }
            return true;
        }
        if !self.optional_preloadable(name, importer_dir, module_root) {
            return false;
        }
        let units_before = self.units.len();
        let diags_before = self.diags.len();
        let loaded_before: Vec<PathBuf> = self.loaded.keys().cloned().collect();
        let names_before: Vec<String> = self.by_name.keys().cloned().collect();
        let namespaces_before = self.namespaces.clone();
        let failed_before = self.failed.clone();
        let loaded = self.load_named(name, importer_dir, module_root);
        let succeeded = matches!(&loaded, NamedLoad::Loaded(_)) && self.diags.len() == diags_before;
        if !succeeded {
            self.diags.truncate(diags_before);
            self.units.truncate(units_before);
            self.loaded.retain(|path, _| loaded_before.contains(path));
            self.by_name.retain(|name, _| names_before.contains(name));
            self.namespaces = namespaces_before;
            self.failed = failed_before;
            return false;
        }
        if let NamedLoad::Loaded(loaded) = loaded {
            for index in loaded {
                if !scan.deps.contains(&index) {
                    scan.deps.push(index);
                }
            }
        }
        true
    }

    /// Whether a module named by a module-level `try` import is safe to link
    /// eagerly.
    ///
    /// Linking runs the module initializer before the importer, so only
    /// modules whose body has no control flow and no extension import are
    /// accepted. The statement whitelist (imports, definitions, assignments,
    /// docstrings and statically dead guards) covers ordinary package helper
    /// modules while keeping optional dependency graphs and extension loaders
    /// lazy; everything else stays deferred and reports `ModuleNotFoundError`
    /// at runtime. An extension import would otherwise move the loader failure
    /// outside the `try`, where the handler could not see it.
    fn optional_preloadable(&self, name: &str, importer_dir: &Path, module_root: &Path) -> bool {
        let roots = module_search_roots(importer_dir, module_root);
        let Some(chain) = resolve_module(&roots, name) else {
            return false;
        };
        let Some(final_module) = chain.last() else {
            return false;
        };
        if final_module.namespace {
            return false;
        }
        let Ok(source) = fs::read_to_string(&final_module.path) else {
            return false;
        };
        let Ok(ast::Mod::Module(module)) =
            rustpython_parser::parse(&source, Mode::Module, &final_module.path.to_string_lossy())
        else {
            return false;
        };
        let package = final_module.package.as_str();
        for stmt in &module.body {
            match stmt {
                ast::Stmt::Import(s) => {
                    for alias in &s.names {
                        let imported = alias.name.as_str();
                        if hydro_ir::stdlib::module(imported).is_some() {
                            continue;
                        }
                        if extension_referenced(&final_module.path, package, imported) {
                            return false;
                        }
                    }
                }
                ast::Stmt::ImportFrom(s) => {
                    let level = s.level.as_ref().map(|level| level.to_u32()).unwrap_or(0);
                    let Ok(module_name) = lower::resolve_import_name(
                        level,
                        s.module.as_ref().map(|module| module.as_str()),
                        package,
                    ) else {
                        return false;
                    };
                    if hydro_ir::stdlib::module(&module_name).is_some() {
                        continue;
                    }
                    if extension_referenced(&final_module.path, package, &module_name) {
                        return false;
                    }
                    for alias in &s.names {
                        if alias.name.as_str() == "*" {
                            // Star imports cannot be linked without knowing
                            // the extension's members.
                            return false;
                        }
                        let full = format!("{module_name}.{}", alias.name);
                        if extension_referenced(&final_module.path, package, &full) {
                            return false;
                        }
                    }
                }
                ast::Stmt::FunctionDef(_)
                | ast::Stmt::AsyncFunctionDef(_)
                | ast::Stmt::ClassDef(_)
                | ast::Stmt::Assign(_)
                | ast::Stmt::AnnAssign(_)
                | ast::Stmt::AugAssign(_)
                | ast::Stmt::Pass(_) => {}
                // Only a docstring is a side-effect-free expression.
                ast::Stmt::Expr(s) => {
                    if !matches!(
                        &*s.value,
                        ast::Expr::Constant(constant)
                            if matches!(constant.value, ast::Constant::Str(_))
                    ) {
                        return false;
                    }
                }
                ast::Stmt::If(s) => {
                    if lower::static_if_branch(&s.test, name) != Some(false) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
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
    ///
    /// `optional` marks the body of a module-level `try`: imports there are
    /// linked when they resolve (so the binding is a normal module reference)
    /// and otherwise recorded as deferred, leaving the runtime
    /// `ModuleNotFoundError` for the handler. Nested functions, classes and
    /// handler bodies are never optional.
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
        optional: bool,
    ) {
        for stmt in stmts {
            match stmt {
                ast::Stmt::Import(s) => {
                    for alias in &s.names {
                        if deferred {
                            // Native modules bind without loading anything.
                            // Preload an existing user module quietly so the
                            // lowerer can link it; an optional module that
                            // fails to resolve stays deferred.
                            if hydro_ir::stdlib::module(alias.name.as_str()).is_none() {
                                if optional {
                                    if !self.optional_import(
                                        alias.name.as_str(),
                                        importer_dir,
                                        root,
                                        scan,
                                    ) {
                                        scan.record_deferred(alias.name.as_str(), None);
                                    }
                                } else {
                                    if self.import_free_module(
                                        alias.name.as_str(),
                                        importer_dir,
                                        root,
                                    ) {
                                        if let Some(index) = self.probe_named(
                                            alias.name.as_str(),
                                            importer_dir,
                                            root,
                                        ) {
                                            if !scan.deps.contains(&index) {
                                                scan.deps.push(index);
                                            }
                                        }
                                    }
                                    scan.record_deferred(alias.name.as_str(), None);
                                }
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
                    if hydro_ir::stdlib::module(&module_name).is_some() {
                        continue;
                    }
                    if deferred {
                        if optional {
                            // A module-level `try` body runs in normal
                            // execution order, so a submodule that resolves can
                            // be linked like an eager import. A submodule that
                            // is absent (or cannot be compiled) stays deferred
                            // and raises `ModuleNotFoundError` at runtime, which
                            // the surrounding `except ImportError` handles.
                            for alias in &s.names {
                                if alias.name.as_str() == "*" {
                                    scan.record_deferred(&module_name, None);
                                    continue;
                                }
                                let full = format!("{module_name}.{}", alias.name);
                                if self.optional_import(&full, importer_dir, root, scan) {
                                    continue;
                                }
                                // `from pkg import name` may bind an attribute
                                // of `pkg`; linking the base module lets the
                                // lowerer resolve exports. If the name is not
                                // an export, the deferred edge below keeps the
                                // runtime import error.
                                self.optional_import(&module_name, importer_dir, root, scan);
                                scan.record_deferred(&module_name, Some(alias.name.as_str()));
                            }
                            continue;
                        }
                        if self.import_free_module(&module_name, importer_dir, root) {
                            if let Some(index) = self.probe_named(&module_name, importer_dir, root)
                            {
                                if !scan.deps.contains(&index) {
                                    scan.deps.push(index);
                                }
                            }
                        }
                        for alias in &s.names {
                            if alias.name.as_str() != "*" {
                                let full = format!("{module_name}.{}", alias.name);
                                if self.import_free_module(&full, importer_dir, root) {
                                    if let Some(index) = self.probe_named(&full, importer_dir, root)
                                    {
                                        if !scan.deps.contains(&index) {
                                            scan.deps.push(index);
                                        }
                                    }
                                }
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
                    false,
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
                    false,
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
                    false,
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
                            optional,
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
                            optional,
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
                        optional,
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
                        optional,
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
                        optional,
                    );
                }
                ast::Stmt::Try(s) => {
                    // The body of a module-level `try` runs in normal
                    // execution order: imports that resolve can be linked
                    // eagerly, while missing ones stay deferred so the handler
                    // catches the runtime `ModuleNotFoundError`.
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
                        optional || !deferred,
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
                            false,
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
                        false,
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
                        false,
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
                    optional,
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
                    optional,
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
        optional: bool,
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
            optional,
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
            optional,
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
        if hydro_ir::stdlib::module(name).is_some() {
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

/// Whether `name` resolves to a compiled extension visible from `source`.
///
/// Both the source-relative and the package-root lookups are consulted: a
/// relative import inside a package names an absolute dotted module, which is
/// rooted at the directory containing the package rather than at the source
/// file's own directory.
fn extension_referenced(source: &Path, package: &str, name: &str) -> bool {
    !matches!(
        lower::find_extension_for_source(source, name),
        lower::ExtensionLookup::Missing
    ) || !matches!(
        lower::extension_lookup_from_package(source, package, name),
        lower::ExtensionLookup::Missing
    )
}

/// Directories searched for user modules, in order: the importing file's
/// directory, the root of the current package chain (so `pkg.a` can import
/// `pkg.b`), `HYDRO_SITE_PACKAGES`, the site-packages directories of
/// `VIRTUAL_ENV`/`CONDA_PREFIX`, `PYTHONPATH`, then `HYDRO_STDLIB`.
///
/// `HYDRO_STDLIB` is the CPython `Lib` directory that backs the standard
/// library sources (`<cpython>/Lib`). It comes last so project modules,
/// site-packages and user `PYTHONPATH` entries always win over the bundled
/// stdlib source tree.
fn module_search_roots(importer_dir: &Path, module_root: &Path) -> Vec<PathBuf> {
    let hydro_site_packages = std::env::var_os("HYDRO_SITE_PACKAGES");
    let pythonpath = std::env::var_os("PYTHONPATH");
    let hydro_stdlib = std::env::var_os("HYDRO_STDLIB");
    let virtual_env = std::env::var_os("VIRTUAL_ENV").map(PathBuf::from);
    let conda_prefix = std::env::var_os("CONDA_PREFIX").map(PathBuf::from);
    module_search_roots_from(
        importer_dir,
        module_root,
        hydro_site_packages.as_deref(),
        pythonpath.as_deref(),
        hydro_stdlib.as_deref(),
        virtual_env.as_deref(),
        conda_prefix.as_deref(),
    )
}

/// Builds the module search path from raw environment values.
///
/// Order: the importing directory and the current package root, then
/// `HYDRO_SITE_PACKAGES`, the site-packages directories of `VIRTUAL_ENV` and
/// `CONDA_PREFIX`, `PYTHONPATH`, and finally `HYDRO_STDLIB`. Every explicit
/// user location therefore takes precedence over the standard library source
/// tree, matching CPython's `sys.path` layout.
///
/// Missing and non-directory entries are dropped and duplicates are searched
/// only once. The importing file's directory always comes first.
fn module_search_roots_from(
    importer_dir: &Path,
    module_root: &Path,
    hydro_site_packages: Option<&std::ffi::OsStr>,
    pythonpath: Option<&std::ffi::OsStr>,
    hydro_stdlib: Option<&std::ffi::OsStr>,
    virtual_env: Option<&Path>,
    conda_prefix: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = vec![importer_dir.to_path_buf(), module_root.to_path_buf()];
    for list in [hydro_site_packages].into_iter().flatten() {
        dirs.extend(std::env::split_paths(list));
    }
    for prefix in [virtual_env, conda_prefix].into_iter().flatten() {
        dirs.extend(lower::prefix_site_packages(prefix));
    }
    for list in [pythonpath, hydro_stdlib].into_iter().flatten() {
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

/// Resolves `name` on the module search path.
///
/// Returns the package ancestors and the module itself in order, so the loader
/// can initialize parents first. Namespace packages (directories without
/// `__init__.py`) are valid package segments: a real module or package found
/// later on the search path wins over a namespace portion, matching CPython's
/// `PathFinder`.
fn resolve_module(roots: &[PathBuf], name: &str) -> Option<Vec<ResolvedModule>> {
    if name.is_empty() || name.split('.').any(|part| part.is_empty()) {
        return None;
    }
    let parts: Vec<&str> = name.split('.').collect();
    let mut namespace = None;
    for root in roots {
        if let Some(mut chain) = resolve_module_in(root, &parts) {
            for module in &mut chain {
                module.root = root.clone();
            }
            if chain.last().is_some_and(|module| module.namespace) {
                // Keep looking for a real package/module in a later root.
                namespace.get_or_insert(chain);
            } else {
                return Some(chain);
            }
        }
    }
    namespace
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
                    namespace: false,
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
                    namespace: false,
                });
                return Some(chain);
            }
            if package_dir.is_dir() {
                // A namespace package: the directory is importable but has no
                // `__init__.py` and therefore no module body.
                chain.push(ResolvedModule {
                    path: package_dir,
                    name: name.clone(),
                    package: name,
                    root: PathBuf::new(),
                    namespace: true,
                });
                return Some(chain);
            }
            return None;
        }
        if init.is_file() {
            chain.push(ResolvedModule {
                path: init,
                name: name.clone(),
                package: name.clone(),
                root: PathBuf::new(),
                namespace: false,
            });
            dir = package_dir;
            continue;
        }
        // A plain module cannot contain submodules: `pkg.sub` is unresolved
        // even when `pkg.py` exists.
        if module_file.is_file() {
            return None;
        }
        if !package_dir.is_dir() {
            return None;
        }
        // Intermediate namespace segment: continue the dotted chain inside
        // the directory.
        chain.push(ResolvedModule {
            path: package_dir.clone(),
            name: name.clone(),
            package: name.clone(),
            root: PathBuf::new(),
            namespace: true,
        });
        dir = package_dir;
    }
    None
}

/// Lowers every loaded module in dependency order and links the results.
///
/// The module bodies of imported units become [`Program::initializers`] (in
/// dependency order) and run before the entry module. Namespace packages in
/// `namespaces` have no unit: they are exposed as empty user modules and never
/// produce an initializer.
fn lower_modules(
    units: &[LoadedModule],
    namespaces: &[String],
) -> Result<Program, Vec<Diagnostic>> {
    let entry_index = units.len() - 1;
    let mut all_functions: Vec<hydro_ir::Function> = Vec::new();
    let mut exports: Vec<HashMap<String, lower::UserExport>> = vec![HashMap::new(); units.len()];
    let mut initializers: Vec<FunctionId> = Vec::new();
    let mut entry_main = 0;
    let mut class_registry: HashMap<(String, String), (FunctionId, lower::CallSignature)> =
        HashMap::new();
    let mut function_registry: HashMap<FunctionId, lower::CallSignature> = HashMap::new();

    for (index, unit) in units.iter().enumerate() {
        let base = all_functions.len() as FunctionId;
        let mut infer = Infer::new();
        let mut lowerer = Lowerer::new(&unit.source, &unit.display, &mut infer);
        lowerer.set_templates(&unit.templates);
        lowerer.set_fid_base(base);
        // Imported units may be read by name from their global table while
        // they are still initializing, so they mirror every module-level
        // binding. The entry module has no importers: mirroring every
        // assignment would add a global store to every loop iteration, so it
        // mirrors only the names its own functions read (see
        // `Lowerer::collect_function_read_globals`).
        lowerer.set_mirror_all_globals(index != entry_index);
        lowerer.set_imported_classes(class_registry.clone(), function_registry.clone());
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
                    all: lower::module_all_names(&dep.module.body),
                },
            );
        }
        // Namespace packages resolved anywhere in this program are importable
        // names with no body and no exports.
        for namespace in namespaces {
            user_modules
                .entry(namespace.clone())
                .or_insert_with(|| UserModule {
                    name: namespace.clone(),
                    exports: HashMap::new(),
                    all: None,
                });
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
        let exported_classes = lowerer.exported_class_signatures();
        let exported_functions = lowerer.exported_function_signatures();
        let lower_diags = std::mem::take(&mut lowerer.diags);
        drop(lowerer);
        for (class, init, mut signature) in exported_classes {
            signature.default_exprs = vec![None; signature.default_exprs.len()];
            class_registry.insert((unit.name.clone(), class), (init, signature));
        }
        for (id, mut signature) in exported_functions {
            signature.default_exprs = vec![None; signature.default_exprs.len()];
            function_registry.insert(id, signature);
        }
        if !lower_diags.is_empty() {
            return Err(lower_diags);
        }

        exports[index] = exported;
        if index != entry_index {
            // Imported units keep one identity per unit: the module body and
            // every nested function are namespaced by the dotted module name,
            // so the runtime can materialize `sys.modules` entries exactly and
            // two modules with the same function name never collide.
            for function in &mut hir.functions {
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
fn placeholder_function(function: &hydro_ir::Function) -> HFunction {
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
                default_expr: None,
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
                "hydro-frontend-modules-{}-{test_name}-{unique}",
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

        /// Creates an empty shared-object stub at `name` (directories
        /// included). The frontend only resolves the path; loading happens at
        /// runtime.
        fn add_extension(&self, name: &str) -> PathBuf {
            let path = self.path.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create extension dir");
            }
            fs::write(&path, b"").expect("write extension stub");
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

        fn unset(key: &'static str) -> EnvVar {
            let old = std::env::var_os(key);
            std::env::remove_var(key);
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

    /// Serializes tests that mutate process-wide import environment variables.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
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
    fn calls_function(expr: &hydro_ir::Expr, id: FunctionId) -> bool {
        use hydro_ir::{Callee, ExprKind};
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

    /// Names stored into the runtime global table by `stmts`, nested blocks
    /// included.
    fn global_set_names(stmts: &[hydro_ir::Stmt]) -> Vec<String> {
        fn walk(stmts: &[hydro_ir::Stmt], out: &mut Vec<String>) {
            use hydro_ir::StmtKind;
            for stmt in stmts {
                match &stmt.kind {
                    StmtKind::GlobalSet { name, .. } => out.push(name.clone()),
                    StmtKind::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        walk(then_body, out);
                        walk(else_body, out);
                    }
                    StmtKind::While {
                        body, else_body, ..
                    }
                    | StmtKind::ForRange {
                        body, else_body, ..
                    }
                    | StmtKind::ForEach {
                        body, else_body, ..
                    } => {
                        walk(body, out);
                        walk(else_body, out);
                    }
                    StmtKind::Try {
                        body,
                        handlers,
                        else_body,
                        finally_body,
                    } => {
                        walk(body, out);
                        for handler in handlers {
                            walk(&handler.body, out);
                        }
                        walk(else_body, out);
                        walk(finally_body, out);
                    }
                    _ => {}
                }
            }
        }

        let mut names = Vec::new();
        walk(stmts, &mut names);
        names
    }

    /// Returns the callee of a top-level `print(<call>)` statement.
    fn printed_call(program: &Program) -> FunctionId {
        use hydro_ir::{Callee, ExprKind, StmtKind};
        let main = program.main_function();
        for stmt in &main.body {
            if let StmtKind::Expr(expr) = &stmt.kind {
                if let ExprKind::Call { callee, args } = &expr.kind {
                    if matches!(callee, Callee::Builtin(hydro_ir::Builtin::Print)) {
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

        use hydro_ir::{Callee, ExprKind, StmtKind};
        let main_fn = program.main_function();
        let mut calls = Vec::new();
        for stmt in &main_fn.body {
            if let StmtKind::Expr(expr) = &stmt.kind {
                if let ExprKind::Call { callee, args } = &expr.kind {
                    if matches!(callee, Callee::Builtin(hydro_ir::Builtin::Print)) {
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
        use hydro_ir::{ExprKind, StmtKind};
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
        assert_eq!(module_bodies, 1, "only the entry body keeps MODULE_NAME");
        let imported_bodies = program
            .functions
            .iter()
            .filter(|function| function.name.ends_with(".<module>"))
            .count();
        assert_eq!(
            imported_bodies, 3,
            "left, right and the shared base bodies are module-qualified"
        );
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
        assert_eq!(helper.name, "helper.<module>");
        let mut class_mirrored = false;
        let mut value_mirrored = false;
        for stmt in &helper.body {
            if let hydro_ir::StmtKind::GlobalSet { name, .. } = &stmt.kind {
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

        use hydro_ir::{Callee, ExprKind, StmtKind};
        let make = program.function(function_id(&program, "make"));
        let make_return = make
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Return(Some(expr)) => Some(expr),
                _ => None,
            })
            .expect("make must return");
        let callee = match &make_return.kind {
            ExprKind::IndirectCall { callee, .. } | ExprKind::CallKw { callee, .. } => callee,
            other => panic!("expected a class call, got {other:?}"),
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
                    if matches!(callee, Callee::Builtin(hydro_ir::Builtin::Print)) {
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
        assert_eq!(
            program.functions[initializer as usize].name,
            "helper.<module>"
        );
        use hydro_ir::{Callee, ExprKind, StmtKind};
        let side_effect = program.functions[initializer as usize]
            .body
            .iter()
            .any(|stmt| {
                matches!(
                    &stmt.kind,
                    StmtKind::Expr(expr)
                        if matches!(&expr.kind, ExprKind::Call {
                            callee: Callee::Builtin(hydro_ir::Builtin::Print),
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
        use hydro_ir::{ExprKind, StmtKind};
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
                    if matches!(callee, hydro_ir::Callee::Builtin(hydro_ir::Builtin::Print)) {
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
        use hydro_ir::StmtKind;
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
        let _env = env_lock();
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

        let roots = module_search_roots_from(
            &project,
            &project,
            Some(site.as_os_str()),
            None,
            None,
            None,
            None,
        );
        assert_eq!(roots, vec![project.clone(), site.clone()]);

        let _guard = EnvVar::set("HYDRO_SITE_PACKAGES", site.as_os_str());
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
    fn hydro_stdlib_search_root_follows_pythonpath_and_resolves_test_support() {
        let _env = env_lock();
        let dir = TestDir::new("hydro-stdlib");
        let lib = dir.path.join("Lib");
        let project = dir.path.join("project");
        let pythonpath = dir.path.join("pythonpath");
        fs::create_dir_all(lib.join("test").join("support")).expect("create test.support");
        fs::create_dir_all(&project).expect("create project");
        fs::create_dir_all(&pythonpath).expect("create pythonpath");
        fs::write(lib.join("test").join("__init__.py"), "").expect("write test init");
        fs::write(
            lib.join("test").join("support").join("__init__.py"),
            "VALUE = 41\n\ndef get():\n    return VALUE + 1\n",
        )
        .expect("write support init");
        fs::write(
            lib.join("test").join("support").join("helper.py"),
            "def inc(value):\n    return value + 1\n",
        )
        .expect("write support helper");

        let main = project.join("main.py");
        fs::write(
            &main,
            "import test.support\nfrom test.support import get, VALUE\nfrom test.support.helper import inc\nprint(get(), VALUE, inc(1))\n",
        )
        .expect("write entry");

        let roots = module_search_roots_from(
            &project,
            &project,
            None,
            Some(pythonpath.as_os_str()),
            Some(lib.as_os_str()),
            None,
            None,
        );
        assert_eq!(
            roots,
            vec![project.clone(), pythonpath.clone(), lib.clone()],
            "site/user modules keep precedence over HYDRO_STDLIB"
        );

        let _stdlib = EnvVar::set("HYDRO_STDLIB", lib.as_os_str());
        let _pythonpath = EnvVar::unset("PYTHONPATH");
        let program = program(&main);
        let get = function_id(&program, "test.support.get");
        let inc = function_id(&program, "test.support.helper.inc");
        use hydro_ir::{Callee, ExprKind, StmtKind};
        let entry = program.main_function();
        let mut get_called = false;
        let mut inc_called = false;
        for stmt in &entry.body {
            if let StmtKind::Expr(expr) = &stmt.kind {
                if let ExprKind::Call { callee, args } = &expr.kind {
                    if matches!(callee, Callee::Builtin(hydro_ir::Builtin::Print)) {
                        for arg in args {
                            get_called |= calls_function(arg, get);
                            inc_called |= calls_function(arg, inc);
                        }
                    }
                }
            }
        }
        assert!(get_called, "`get()` must link to test.support.get");
        assert!(inc_called, "`inc(1)` must link to test.support.helper.inc");
        // `test` and `test.support` are real packages: both bodies run once,
        // in dependency order, before the entry.
        assert_eq!(program.initializers.len(), 3, "{program:?}");
        let body_names: Vec<&str> = program
            .initializers
            .iter()
            .map(|id| program.functions[*id as usize].name.as_str())
            .collect();
        assert_eq!(
            body_names,
            vec![
                "test.<module>",
                "test.support.<module>",
                "test.support.helper.<module>"
            ]
        );
    }

    #[test]
    fn hydro_stdlib_missing_modules_keep_the_same_diagnostic_when_unset() {
        let _env = env_lock();
        let dir = TestDir::new("hydro-stdlib-missing");
        let main = dir.write("main.py", "import nowhere\n");
        let _stdlib = EnvVar::unset("HYDRO_STDLIB");
        let _pythonpath = EnvVar::unset("PYTHONPATH");
        let diags = check_file(&main).expect_err("missing module must fail");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(
            diags[0].message,
            "module 'nowhere' not found: not supported on the import search path"
        );
        assert_eq!(
            diags[0].help.as_deref(),
            Some("expected a module file on the import search path")
        );
    }

    #[test]
    fn namespace_package_segments_resolve_without_an_initializer() {
        let dir = TestDir::new("namespace");
        fs::create_dir_all(dir.path.join("ns_pkg")).expect("create namespace package");
        fs::write(
            dir.path.join("ns_pkg").join("mod.py"),
            "def value():\n    return 3\n",
        )
        .expect("write module");
        let main = dir.write("main.py", "from ns_pkg.mod import value\nprint(value())\n");
        let namespace_program = program(&main);
        let value = function_id(&namespace_program, "ns_pkg.mod.value");
        assert_eq!(printed_call(&namespace_program), value);
        assert_eq!(
            namespace_program.initializers.len(),
            1,
            "only the real module body runs; the namespace package has none"
        );
        assert_eq!(
            namespace_program.functions[namespace_program.initializers[0] as usize].name,
            "ns_pkg.mod.<module>"
        );
        assert!(
            !namespace_program
                .functions
                .iter()
                .any(|function| function.name == "ns_pkg.<module>"),
            "a namespace package must not emit an initializer"
        );

        // Importing the final namespace package itself succeeds and links
        // nothing.
        let main = dir.write("main_ns.py", "import ns_pkg\nprint(1)\n");
        let namespace_program = program(&main);
        assert!(
            namespace_program.initializers.is_empty(),
            "namespace packages have no body"
        );
    }

    #[test]
    fn native_star_import_binds_spec_functions_and_constants() {
        let dir = TestDir::new("star-native");
        let main = dir.write(
            "main.py",
            "from math import *\nx = sqrt(9)\ny = pi\nprint(x)\n",
        );
        let program = program(&main);
        use hydro_ir::{ExprKind, StmtKind};
        let entry = program.main_function();
        let mut sqrt_bound = false;
        let mut pi_bound = false;
        for stmt in &entry.body {
            let StmtKind::Assign { target, value } = &stmt.kind else {
                continue;
            };
            match entry.local(*target).name.as_str() {
                "x" => {
                    sqrt_bound = matches!(
                        &value.kind,
                        ExprKind::ModuleCall { module, function, .. }
                            if module == "math" && function == "sqrt"
                    );
                }
                "y" => {
                    pi_bound = matches!(
                        &value.kind,
                        ExprKind::ModuleAttr { module, name }
                            if module == "math" && name == "pi"
                    );
                }
                _ => {}
            }
        }
        assert!(sqrt_bound, "`sqrt` must bind to math.sqrt");
        assert!(pi_bound, "`pi` must bind to math.pi");
    }

    #[test]
    fn user_star_import_binds_public_exports_only() {
        let dir = TestDir::new("star-user");
        dir.write(
            "helper.py",
            "VALUE = 7\n\ndef add(a, b):\n    return a + b\n\n_private = 1\n",
        );
        let main = dir.write("main.py", "from helper import *\nprint(add(1, 2), VALUE)\n");
        let program = program(&main);
        let add = function_id(&program, "helper.add");
        use hydro_ir::{ExprKind, StmtKind};
        let entry = program.main_function();
        let mut add_called = false;
        let mut reads_value = false;
        for stmt in &entry.body {
            let StmtKind::Expr(expr) = &stmt.kind else {
                continue;
            };
            let ExprKind::Call {
                callee: hydro_ir::Callee::Builtin(hydro_ir::Builtin::Print),
                args,
            } = &expr.kind
            else {
                continue;
            };
            for arg in args {
                add_called |= calls_function(arg, add);
                reads_value |= matches!(
                    &arg.kind,
                    ExprKind::GlobalGet { module, name }
                        if module == "helper" && name == "VALUE"
                );
            }
        }
        assert!(add_called, "`add` must link to helper.add");
        assert!(reads_value, "`VALUE` must read helper.VALUE");

        // `_private` is not exported by a star import without `__all__`.
        let bad = dir.write("bad.py", "from helper import *\nprint(_private)\n");
        let diags = check_file(&bad).expect_err("private name must stay unbound");
        assert!(
            diags.iter().any(|diag| diag.message.contains("'_private'")),
            "{diags:?}"
        );
    }

    #[test]
    fn user_star_import_honors_a_literal_all_in_order() {
        let dir = TestDir::new("star-all");
        dir.write(
            "helper.py",
            "__all__ = (\"keep\",)\n\ndef keep():\n    return 1\n\ndef drop():\n    return 2\n",
        );
        let good = dir.write("good.py", "from helper import *\nprint(keep())\n");
        let program = program(&good);
        assert_eq!(printed_call(&program), function_id(&program, "helper.keep"));

        let bad = dir.write("bad.py", "from helper import *\nprint(drop())\n");
        let diags = check_file(&bad).expect_err("names outside __all__ are filtered");
        assert!(
            diags.iter().any(|diag| diag.message.contains("'drop'")),
            "{diags:?}"
        );
    }

    #[test]
    fn entry_module_binds_dunder_name_to_main() {
        let dir = TestDir::new("dunder-name");
        let main = dir.write("main.py", "print(__name__)\n");
        let program = program(&main);
        use hydro_ir::{Const, ExprKind, StmtKind};
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
    fn returned_expression(program: &Program, function: FunctionId) -> &hydro_ir::Expr {
        use hydro_ir::StmtKind;
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
        use hydro_ir::ExprKind;
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
        use hydro_ir::ExprKind;
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
        use hydro_ir::ExprKind;
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
        use hydro_ir::ExprKind;
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
    fn module_level_try_import_links_a_sibling_source_module() {
        let dir = TestDir::new("try-source");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "try:\n    from . import sub\nexcept ImportError:\n    pass\n\ndef get():\n    return sub.value()\n",
        )
        .expect("write package init");
        fs::write(pkg.join("sub.py"), "def value():\n    return 7\n").expect("write submodule");
        let main = dir.write("main.py", "from pkg import get\nprint(get())\n");
        let program = program(&main);

        // The try body runs in normal order, so `sub` is compiled and
        // initialized before the package body, exactly like an eager import.
        let sub_value = function_id(&program, "pkg.sub.value");
        let get = function_id(&program, "pkg.get");
        use hydro_ir::{Callee, ExprKind, StmtKind};
        let returned = returned_expression(&program, get);
        assert!(
            matches!(
                &returned.kind,
                ExprKind::Call {
                    callee: Callee::Function(id),
                    ..
                } if *id == sub_value
            ),
            "pkg.get must call the linked submodule function: {:?}",
            returned.kind
        );

        assert_eq!(program.initializers.len(), 2, "{:?}", program.initializers);
        let names: Vec<&str> = program
            .initializers
            .iter()
            .map(|id| program.functions[*id as usize].name.as_str())
            .collect();
        assert_eq!(names, vec!["pkg.sub.<module>", "pkg.<module>"]);

        // The try body binds the preloaded submodule through a real module
        // reference; it does not execute the import twice.
        let pkg_init = &program.functions[program.initializers[1] as usize].body;
        let stored = pkg_init
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::GlobalSet { name, value, .. } if name == "sub" => Some(value),
                StmtKind::Try { body, .. } => body.iter().find_map(|inner| match &inner.kind {
                    StmtKind::GlobalSet { name, value, .. } if name == "sub" => Some(value),
                    _ => None,
                }),
                _ => None,
            })
            .expect("the try body must bind `sub`");
        assert!(
            matches!(&stored.kind, ExprKind::ModuleRef { module } if module == "pkg.sub"),
            "the binding must be a module reference: {:?}",
            stored.kind
        );
    }

    #[test]
    fn module_level_try_import_of_a_missing_submodule_stays_deferred() {
        let dir = TestDir::new("try-missing");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "try:\n    from . import missing\nexcept ImportError:\n    pass\n\ndef get():\n    return missing\n",
        )
        .expect("write package init");
        let main = dir.write("main.py", "from pkg import get\nprint(get())\n");
        let program = program(&main);

        // Nothing was preloaded: the handler owns the missing import and the
        // binding is resolved at runtime, where `ModuleNotFoundError` runs the
        // `except ImportError` handler.
        assert_eq!(program.initializers.len(), 1, "{:?}", program.initializers);
        assert_eq!(
            program.functions[program.initializers[0] as usize].name,
            "pkg.<module>"
        );
        let get = function_id(&program, "pkg.get");
        use hydro_ir::ExprKind;
        let returned = returned_expression(&program, get);
        assert!(
            matches!(
                &returned.kind,
                ExprKind::DeferredImport { module, member }
                    if module == "pkg" && member.as_deref() == Some("missing")
            ),
            "the missing submodule must stay deferred: {:?}",
            returned.kind
        );
    }

    #[test]
    fn module_level_try_import_of_an_extension_submodule_uses_the_loader() {
        let dir = TestDir::new("try-extension");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "try:\n    from . import ext\nexcept ImportError:\n    pass\n\ndef get():\n    return ext.value()\n",
        )
        .expect("write package init");
        let so = dir.add_extension("pkg/ext.abi3.so");
        let expected = fs::canonicalize(&so)
            .expect("canonical extension path")
            .to_string_lossy()
            .into_owned();
        let main = dir.write("main.py", "from pkg import get\nprint(get())\n");
        let program = program(&main);

        // The extension is not a compiled unit: it binds to the shared object
        // and the runtime loader stays inside the `try`, so a load failure
        // runs the handler.
        assert_eq!(program.initializers.len(), 1, "{:?}", program.initializers);
        let get = function_id(&program, "pkg.get");
        use hydro_ir::{Callee, ExprKind};
        let returned = returned_expression(&program, get);
        match &returned.kind {
            ExprKind::Call {
                callee: Callee::Extension { path, member },
                ..
            } => {
                assert_eq!(path, &expected);
                assert_eq!(member, "value");
            }
            other => panic!("expected an extension call, got {other:?}"),
        }
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
        use hydro_ir::{ExprKind, StmtKind};
        let callee = match &returned_expression(&program, run).kind {
            ExprKind::IndirectCall { callee, .. } | ExprKind::CallKw { callee, .. } => callee,
            other => panic!("expected a call to the package member, got {other:?}"),
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
        use hydro_ir::StmtKind;
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
        use hydro_ir::ExprKind;
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
    fn entry_module_does_not_mirror_unread_loop_locals_but_imported_modules_do() {
        let dir = TestDir::new("entry-mirror");
        dir.write(
            "helper.py",
            "total = 0\nfor i in range(3):\n    total = total + i\n",
        );
        let main = dir.write(
            "main.py",
            "from helper import total\n\nn = 0\nacc = 0\nwhile n < 3:\n    acc = acc + n\n    n = n + 1\nprint(total + acc)\n",
        );
        let program = program(&main);

        // The entry module has no importers, so its top-level loop locals stay
        // local: storing them on every iteration is pure overhead.
        let entry_names = global_set_names(&program.main_function().body);
        assert!(
            !entry_names.iter().any(|name| name == "n" || name == "acc"),
            "entry loop locals must not be mirrored: {entry_names:?}"
        );

        // Imported units stay fully mirrored because an importer may read any
        // exported name.
        assert_eq!(program.initializers.len(), 1);
        let helper_names =
            global_set_names(&program.functions[program.initializers[0] as usize].body);
        assert!(
            helper_names.iter().any(|name| name == "total"),
            "the imported module must still mirror its globals: {helper_names:?}"
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
        use hydro_ir::StmtKind;
        assert!(
            program.main_function().body.iter().any(
                |stmt| matches!(&stmt.kind, StmtKind::If { then_body, .. } if !then_body.is_empty())
            ),
            "the entry's __main__ block must still be lowered"
        );
    }
}
