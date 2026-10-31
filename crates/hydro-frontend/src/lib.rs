//! Hydro frontend: parse Python source, lower it to typed IR, infer/validate
//! types, and report source-located diagnostics.
//!
//! Public contract (used by the Tea toolchain and tests):
//! [`check_source`] returns either a fully typed [`Program`] or a non-empty list
//! [`Diagnostic`]s. [`check_file`] additionally resolves imports of user
//! modules and packages (siblings, site-packages, `PYTHONPATH`, `HYDRO_STDLIB`
//! and the vendored `stdlib/` tree) and links them into one program. Both must
//! never panic on user input.
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

use hydro_ir::{FunctionId, ParamKind, Program};
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
/// `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages directories and `PYTHONPATH`,
/// then the pure-Python modules vendored in Hydro's own `stdlib/` tree, and
/// finally `HYDRO_STDLIB` (a CPython `Lib` directory backing every standard
/// library module Hydro does not vendor). User and project roots therefore win
/// over the standard library, while a vendored adaptation (for example the
/// patched `enum.py`) wins over the generic `Lib` source even for a module
/// whose own directory is inside that `Lib` tree. Both plain modules
/// (`foo.py`) and packages
/// (`foo/__init__.py`) are supported, including dotted names, submodules and
/// relative imports. A directory without `__init__.py` is a namespace-package
/// segment: it is importable and acts as a search root for its submodules, but
/// it has no module body and therefore no initializer. Every module is loaded
/// once per canonical path; missing files and incompatible
/// extensions are reported as diagnostics. Import cycles bind the edge that
/// reaches a module still being initialized late, through its runtime global
/// table, so each unit is compiled and initialized exactly once. Imports
/// nested inside functions and classes only run when that code does: they are
/// recorded but never loaded, and reading a name they bind raises the runtime
/// import error instead of failing compilation. Imports inside module-level
/// control flow (`if`/`for`/`while`/`with`) run while the module initializes,
/// so a module found on the search path is linked like a top-level import
/// while an absent one stays deferred; a module-level `try` body links only
/// modules whose initializer cannot fail inside the handler. Imported module
/// bodies are compiled into the returned [`Program`] and run before the entry
/// module (see [`Program::initializers`]). A module that reads a partially
/// initialized ancestor package (NumPy's `_typing` helpers read `numpy.dtype`)
/// is initialized after that package body when the import chain has a
/// function-only edge that can be relaxed; direct submodule imports keep the
/// existing dependency order.
///
/// Packages that expose members through PEP 562 `__getattr__` and
/// `importlib.import_module` (pydantic's `_dynamic_imports`) are handled by a
/// bounded discovery policy, not by compiling at runtime: relative module-name
/// string literals carried by dispatch containers or call arguments of a
/// **top-level** package initializer are resolved against that package and
/// loaded like ordinary modules, so their initializers run before the program
/// (see `dynamic_import_candidates`). Nested package initializers and plain
/// modules keep their tables lazy, because CPython only reaches them after the
/// enclosing package graph initialized. Missing, absolute, native and
/// syntactically unrelated strings are ignored.
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
    if entry.is_some() && loader.diags.is_empty() {
        // Nested imports are deferred while the importing module is still
        // being scanned. Once every dependency is initialized, a
        // declaration-only site-packages module can be linked eagerly and
        // its member becomes a normal import target at runtime.
        loader.preload_deferred_site_modules(&root);
        // A compiled extension imports Python modules from its own
        // package while it initializes; link those support modules and put
        // them before the unit that loads the extension.
        loader.preload_extension_support_modules(&root);
        // Honour the dependency edges added after the scan (extension
        // support and deferred preloads) in the initializer order. The
        // preload passes keep the entry unit last, so its index is the
        // last unit — the index `load_file` returned may have moved.
        if loader.diags.is_empty() {
            let entry = loader.units.len() - 1;
            loader.sort_units_by_dependencies(entry);
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
            loader.preload_deferred_site_modules(&root);
            loader.preload_extension_support_modules(&root);
            if loader.diags.is_empty() {
                // The preload passes keep the entry unit last, so its index
                // is the last unit even after they insert modules.
                let entry = loader.units.len() - 1;
                loader.sort_units_by_dependencies(entry);
            }
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
pub(crate) struct LoadedModule {
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
    /// Dependencies linked only because a nested import names them (see
    /// [`ModuleLoader::preload_deferred_site_modules`]). The importer's body
    /// does not execute those imports, so the ordering pass may relax the edge
    /// to break an initializer cycle; a module-level import of the same unit is
    /// never recorded here.
    deferred_deps: Vec<usize>,
    /// User modules this unit imports but cannot link directly: they are still
    /// initializing when the unit is loaded (an ancestor package, or a module
    /// that is importing this unit back). The lowerer binds their members late,
    /// through the runtime global table, and binds the module itself — not a
    /// package attribute — for `from . import sub`. A late module has no
    /// dependency edge, so its initializer normally runs after this unit: its
    /// members are readable only once the importing module's functions run.
    /// The exception is a late ancestor package that this unit does not depend
    /// on directly: `unit_initializer_order` runs the package body first so a
    /// helper can read the names it published before importing the helper.
    late: Vec<String>,
    /// Imports nested in functions, classes or compound bodies, recorded by
    /// the scan as `(module, member)` pairs. They are not linked: the lowerer
    /// binds their names to a runtime `RuntimeError` stub unless the module is
    /// already a visible dependency or native.
    deferred: Vec<(String, Option<String>)>,
    /// Module-level `from X import *` targets of this unit (see
    /// [`ImportScan::star_imports`]).
    star_imports: Vec<String>,
    /// Compiled extensions this unit imports, as `(import name, shared
    /// object path)`. A C extension's initializer may import Python modules
    /// that no source statement names (`numpy._core._exceptions`), so the
    /// loader links the declaration-only siblings of each referenced
    /// extension (see [`ModuleLoader::preload_extension_support_modules`]).
    extensions: Vec<(String, String)>,
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
    /// User modules this unit star-imports at module level (`from X import *`).
    /// Their literal `__all__` feeds the unit's effective `__all__`, mirroring
    /// the runtime `extend_all(module)` pattern NumPy uses to re-export whole
    /// dependency surfaces.
    star_imports: Vec<String>,
    /// Compiled extensions imported at module level, as `(import name, shared
    /// object path)`.
    extensions: Vec<(String, String)>,
}

impl ImportScan {
    /// Records a module whose members the lowerer resolves through the runtime
    /// global table instead of a direct link, or — for a submodule named by
    /// `from pkg import name` — the module value itself from the registry.
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

    /// Records a compiled extension imported by this unit.
    fn record_extension(&mut self, module: &str, path: &str) {
        let edge = (module.to_string(), path.to_string());
        if !self.extensions.contains(&edge) {
            self.extensions.push(edge);
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
    /// The module is already initializing (it is on the load stack, either the
    /// importer's ancestor package or a module reached through an import
    /// cycle): importable, but it must not become a dependency edge. Its
    /// members are bound late through the runtime global table, and its body
    /// still becomes exactly one initializer.
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
            deferred_deps: Vec::new(),
            late: scan.late,
            deferred: scan.deferred,
            star_imports: scan.star_imports,
            extensions: scan.extensions,
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

        // A top-level package may name its submodules only through a
        // dynamic-import dispatch table (`pydantic._dynamic_imports`): the
        // literals are collected before the unit joins the program and loaded
        // immediately afterwards, so the submodules initialise after the
        // package body — exactly like `importlib.import_module` at
        // attribute-access time. Nested packages keep their tables lazy.
        let dynamic = dynamic_import_candidates(&module, &name, &package);

        let index = self.units.len();
        self.units.push(LoadedModule {
            name: name.clone(),
            package,
            display: file,
            source,
            module,
            templates,
            deps: scan.deps,
            deferred_deps: Vec::new(),
            late: scan.late,
            deferred: scan.deferred,
            star_imports: scan.star_imports,
            extensions: scan.extensions,
        });
        self.loaded.insert(key, index);
        self.by_name.insert(name, index);
        self.load_dynamic_import_modules(index, &dynamic, &importer_dir, &root);
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
            if let Some(_start) = self.stack.iter().position(|(path, _)| path == &key) {
                if final_module {
                    // The module is initializing right now (an ancestor
                    // package or a genuine import cycle). CPython exposes the
                    // partially initialized module and binds members when the
                    // code that reads them runs; Hydro has the same observable
                    // contract by recording the name as *late*: the importer
                    // reads the target's global table at use time, the target
                    // is not linked as a dependency, and its single unit —
                    // loaded by the outer frame on the stack — provides the
                    // one-time initializer. Re-loading the file here would
                    // compile it twice and run its initializer twice.
                    return NamedLoad::InProgress;
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

    /// Loads the submodules a top-level package names only through
    /// dynamic-import dispatch.
    ///
    /// [`dynamic_import_candidates`] collects the relative names a top-level
    /// package initializer spells out in dispatch tables and call arguments
    /// (nested packages keep theirs lazy). Each one is resolved and loaded
    /// *after* the naming unit is registered: CPython runs
    /// `importlib.import_module` from `__getattr__` only once the package body
    /// has finished, so the discovered units must initialize after their
    /// package and before the program entry.
    ///
    /// The load is transactional per name: a module the frontend cannot read
    /// or parse is rolled back and stays a runtime `ModuleNotFoundError`,
    /// exactly like a module that was never discovered. A name that already
    /// has a unit — a static import or another dispatch table already linked
    /// it — is skipped, so it never compiles twice.
    ///
    /// Every newly loaded unit also depends on `importing`: the dispatch
    /// literal is read by code that runs after the naming module body, so the
    /// naming unit must initialize first even when the submodule does not
    /// import its package explicitly. The submodule's own static imports are
    /// ordinary dependency edges, and the topological pass keeps all of them.
    fn load_dynamic_import_modules(
        &mut self,
        importing: usize,
        candidates: &[String],
        importer_dir: &Path,
        module_root: &Path,
    ) {
        for name in candidates {
            if self.by_name.contains_key(name) {
                continue;
            }
            let units_before = self.units.len();
            let diags_before = self.diags.len();
            let loaded_before: Vec<PathBuf> = self.loaded.keys().cloned().collect();
            let names_before: Vec<String> = self.by_name.keys().cloned().collect();
            let namespaces_before = self.namespaces.clone();
            let failed_before = self.failed.clone();
            let loaded = self.load_named(name, importer_dir, module_root);
            let succeeded =
                matches!(loaded, NamedLoad::Loaded(_)) && self.diags.len() == diags_before;
            if !succeeded {
                self.diags.truncate(diags_before);
                self.units.truncate(units_before);
                self.loaded.retain(|path, _| loaded_before.contains(path));
                self.by_name
                    .retain(|existing, _| names_before.contains(existing));
                self.namespaces = namespaces_before;
                self.failed = failed_before;
                continue;
            }
            for unit in self.units[units_before..].iter_mut() {
                if !unit.deps.contains(&importing) {
                    unit.deps.push(importing);
                }
            }
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
            NamedLoad::Missing | NamedLoad::Failed | NamedLoad::InProgress => None,
        }
    }

    /// Probes the `pkg.name` submodule of a module-level `from pkg import name`.
    ///
    /// The import machinery tries `pkg.name` as a module file/package before
    /// reading `name` as an attribute of `pkg`. When that file is initializing
    /// right now (an import cycle: `pkg.name` imported this unit, which is
    /// importing `pkg.name` back), no second unit is created but the name *is*
    /// a submodule, so it is recorded for late binding: the lowerer binds the
    /// module value itself — read through the registry once initialized —
    /// instead of falling back to a `pkg` attribute that CPython would never
    /// consult.
    fn probe_submodule(
        &mut self,
        name: &str,
        importer_dir: &Path,
        module_root: &Path,
        scan: &mut ImportScan,
    ) -> Option<usize> {
        match self.load_named(name, importer_dir, module_root) {
            NamedLoad::Loaded(mut loaded) => loaded.pop(),
            NamedLoad::InProgress => {
                scan.record_late(name);
                None
            }
            NamedLoad::Missing | NamedLoad::Failed => None,
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

    /// Links site-packages modules that are referenced only from nested
    /// blocks.
    ///
    /// They are loaded after the importer graph is complete, so package cycles
    /// (`rich.console` <-> `rich.status`) are no longer initializing. Each
    /// candidate is transactional: a module that triggers diagnostics stays
    /// deferred and leaves the loader state untouched.
    ///
    /// A deferred module may already be linked through another path (a static
    /// import, another package's preload, a dynamic-import candidate). Those
    /// importers still carry a runtime stub, so their edges are added and the
    /// stubs dropped first: the function body must bind the linked module, not
    /// look it up at call time, when its initializer may not have run.
    fn preload_deferred_site_modules(&mut self, root: &Path) {
        let old_len = self.units.len();
        self.link_already_linked_deferred_modules(root);
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
                        self.units[importer].deferred_deps.push(index);
                    }
                    self.units[importer]
                        .deferred
                        .retain(|(deferred, _)| deferred != &module);
                }
            }
        }
        if self.units.len() > old_len {
            self.reorder_preloaded(old_len);
        }
    }

    /// Turns a deferred stub into a dependency edge when its module is already
    /// linked.
    ///
    /// The module value may still be unregistered when the importing function
    /// runs (`pydantic.plugin._schema_validator` imports `._loader` from
    /// `create_schema_validator`, which the package can call before `_loader`
    /// initializes), so the stub must become a compile-time binding. Only
    /// third-party site modules are rewired: project modules keep the deferred
    /// semantics of conditional module-level imports. A cycle keeps the stub:
    /// the module is on the same initialization path and the runtime lookup is
    /// the only safe form.
    ///
    /// A `from <base> import <member>` pair whose member names a submodule that
    /// is not linked is left deferred: the base edge alone would make the
    /// lowerer treat the member as a package attribute and reject the program
    /// (`pydantic/main.py` -> `from .deprecated import parse`). The submodule's
    /// own deferred record is what the preload and the lowerer fallback use.
    fn link_already_linked_deferred_modules(&mut self, root: &Path) {
        let mut links: Vec<(usize, usize, String, Option<String>)> = Vec::new();
        for (importer, unit) in self.units.iter().enumerate() {
            for (module, member) in &unit.deferred {
                let Some(&index) = self.by_name.get(module) else {
                    continue;
                };
                if index == importer
                    || !is_site_package_path(Path::new(&self.units[index].display))
                    || self.depends_on(index, importer)
                {
                    continue;
                }
                if let Some(member) = member {
                    if self.deferred_pair_names_unlinked_submodule(module, member, root) {
                        continue;
                    }
                }
                links.push((index, importer, module.clone(), member.clone()));
            }
        }
        for (index, importer, module, member) in links {
            // The edge makes the module's exports visible to the lowerer;
            // dropping the stub then binds the name statically.
            if !self.units[importer].deps.contains(&index) {
                self.units[importer].deps.push(index);
                self.units[importer].deferred_deps.push(index);
            }
            self.units[importer]
                .deferred
                .retain(|(deferred, existing)| {
                    deferred != &module || existing.as_deref() != member.as_deref()
                });
        }
    }

    /// Whether a deferred `from <module> import <member>` pair names a
    /// submodule that exists on the search path but is not linked yet.
    ///
    /// Such a pair must keep its runtime permission: the submodule is the real
    /// import target, and only its own deferred record can be consumed by a
    /// preload.
    fn deferred_pair_names_unlinked_submodule(
        &self,
        module: &str,
        member: &str,
        root: &Path,
    ) -> bool {
        let full = format!("{module}.{member}");
        !self.by_name.contains_key(&full)
            && resolve_module(&module_search_roots(root, root), &full).is_some()
    }

    /// Links Python modules that a compiled extension's initializer imports.
    ///
    /// A C extension's `PyInit_*` runs `IMPORT_GLOBAL`-style imports that no
    /// Python source statement names: NumPy's `_multiarray_umath` imports
    /// `numpy.exceptions`, `numpy._globals`, `numpy._core._exceptions` and
    /// `numpy._core.printoptions` while it initializes. Hydro has no runtime
    /// Python compiler, so those modules must already be linked.
    ///
    /// For every extension a unit imports, the loader walks the extension's
    /// package chain (the directory holding the shared object, then each
    /// ancestor that has an `__init__.py`) and adds a dependency edge from the
    /// importing unit to every sibling module in those directories. Siblings
    /// that are not linked yet are preloaded when they have no imports of their
    /// own (a declaration-only module, safe to run early); a preload that
    /// produces diagnostics is rolled back transactionally. Cycles are skipped
    /// (for example `numpy._core.numeric` imports `umath`, so it cannot be
    /// ordered before it).
    fn preload_extension_support_modules(&mut self, root: &Path) {
        let jobs: Vec<(usize, String, PathBuf)> = self
            .units
            .iter()
            .enumerate()
            .flat_map(|(index, unit)| {
                unit.extensions.iter().filter_map(move |(module, path)| {
                    let dir = Path::new(path).parent()?.to_path_buf();
                    Some((index, module.clone(), dir))
                })
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        let old_len = self.units.len();
        // Directory scans are shared: several units often import different
        // extensions of the same package (`numpy._core`), and the ancestor
        // package directory repeats for every job.
        let mut scanned: std::collections::HashMap<(PathBuf, String), Vec<usize>> =
            std::collections::HashMap::new();
        for (importer, module, directory) in jobs {
            // Dotted package of the extension (`numpy._core` for
            // `numpy._core._multiarray_umath`).
            let mut prefix = match module.rsplit_once('.') {
                Some((prefix, _)) => prefix.to_string(),
                None => continue,
            };
            let mut directory = directory;
            loop {
                let key = (directory.clone(), prefix.clone());
                let support: Vec<usize> = if let Some(cached) = scanned.get(&key).cloned() {
                    cached
                } else {
                    let support = self.scan_extension_support(&directory, &prefix, root);
                    scanned.insert(key, support.clone());
                    support
                };
                for index in support {
                    if index == importer || self.units[importer].deps.contains(&index) {
                        continue;
                    }
                    // A cycle means the support module already depends on the
                    // importer (or on a unit ordered after it); keep the
                    // original order then, exactly like the deferred preload.
                    if self.depends_on(index, importer) {
                        continue;
                    }
                    self.units[importer].deps.push(index);
                }
                if !directory.join("__init__.py").is_file() {
                    break;
                }
                let Some(parent) = directory.parent() else {
                    break;
                };
                let Some((outer, _)) = prefix.rsplit_once('.') else {
                    break;
                };
                directory = parent.to_path_buf();
                prefix = outer.to_string();
            }
        }
        if self.units.len() > old_len {
            self.reorder_preloaded(old_len);
        }
    }

    /// Support modules of one package directory: the plain `.py` siblings,
    /// preloading the ones that are not linked yet and have no imports of
    /// their own.
    fn scan_extension_support(
        &mut self,
        directory: &Path,
        prefix: &str,
        root: &Path,
    ) -> Vec<usize> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("py") {
                    return None;
                }
                let stem = path.file_stem()?.to_str()?;
                // Package initializers are units already (or the importer
                // itself); only plain submodules are support candidates.
                if stem == "__init__" {
                    return None;
                }
                Some(format!("{prefix}.{stem}"))
            })
            .collect();
        names.sort();
        let mut support = Vec::with_capacity(names.len());
        for name in names {
            if let Some(&index) = self.by_name.get(&name) {
                support.push(index);
                continue;
            }
            if !self.import_free_module_with(&name, directory, root, false) {
                continue;
            }
            // Preload transactionally: a module that produces a diagnostic
            // keeps its runtime stub instead of failing the program.
            let units_before = self.units.len();
            let diags_before = self.diags.len();
            let loaded_before: Vec<PathBuf> = self.loaded.keys().cloned().collect();
            let names_before: Vec<String> = self.by_name.keys().cloned().collect();
            let loaded = self.probe_named(&name, directory, root);
            if self.diags.len() > diags_before {
                self.diags.truncate(diags_before);
                self.units.truncate(units_before);
                self.loaded.retain(|path, _| loaded_before.contains(path));
                self.by_name.retain(|name, _| names_before.contains(name));
                continue;
            }
            if let Some(index) = loaded {
                support.push(index);
            }
        }
        support
    }

    /// Stable topological order of the units, dependencies first.
    ///
    /// Load order already places dependencies before their importers, so an
    /// existing program keeps its exact order. Edges added after the scan
    /// (extension support modules, deferred preloads) may point backwards;
    /// this pass honours them while keeping the entry module last. A cycle
    /// never traps units: those it cannot place keep their current order.
    fn sort_units_by_dependencies(&mut self, entry: usize) {
        let count = self.units.len();
        if count < 2 || entry >= count {
            return;
        }
        let mut indegree = vec![0usize; count];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (index, unit) in self.units.iter().enumerate() {
            let mut seen: Vec<usize> = Vec::new();
            for &dep in &unit.deps {
                if dep >= count || dep == index || seen.contains(&dep) {
                    continue;
                }
                seen.push(dep);
                indegree[index] += 1;
                dependents[dep].push(index);
            }
        }
        let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> = (0..count)
            .filter(|&index| index != entry && indegree[index] == 0)
            .map(std::cmp::Reverse)
            .collect();
        let mut order: Vec<usize> = Vec::with_capacity(count);
        let mut placed = vec![false; count];
        while let Some(std::cmp::Reverse(index)) = ready.pop() {
            order.push(index);
            placed[index] = true;
            for &dependent in &dependents[index] {
                indegree[dependent] -= 1;
                if indegree[dependent] == 0 && dependent != entry {
                    ready.push(std::cmp::Reverse(dependent));
                }
            }
        }
        for (index, is_placed) in placed.iter().enumerate() {
            if index != entry && !*is_placed {
                order.push(index);
            }
        }
        order.push(entry);
        if order
            .iter()
            .enumerate()
            .all(|(position, &index)| position == index)
        {
            return;
        }
        let mut new_index = vec![0usize; count];
        for (position, &index) in order.iter().enumerate() {
            new_index[index] = position;
        }
        let mut slots: Vec<Option<LoadedModule>> = self.units.drain(..).map(Some).collect();
        let mut reordered: Vec<LoadedModule> = Vec::with_capacity(count);
        for &index in &order {
            reordered.push(slots[index].take().expect("unit"));
        }
        for unit in &mut reordered {
            unit.deps = unit.deps.iter().map(|dep| new_index[*dep]).collect();
            unit.deps.sort_unstable();
            unit.deps.dedup();
            unit.deferred_deps = unit
                .deferred_deps
                .iter()
                .map(|dep| new_index[*dep])
                .collect();
            unit.deferred_deps.sort_unstable();
            unit.deferred_deps.dedup();
        }
        let by_name = std::mem::take(&mut self.by_name);
        for (name, index) in by_name {
            self.by_name.insert(name, new_index[index]);
        }
        let loaded = std::mem::take(&mut self.loaded);
        for (path, index) in loaded {
            self.loaded.insert(path, new_index[index]);
        }
        self.units = reordered;
    }

    /// Places modules loaded after the entry module before the earliest unit
    /// that references them, then moves the entry back to the end.
    ///
    /// Lowering is single-pass in unit order: an importer can only see exports
    /// of modules lowered before it. The loaded modules keep their own load
    /// order (dependencies first), and the insertion point is pushed behind
    /// every already-existing dependency.
    ///
    /// Every preload pass starts with the entry unit last (module loading
    /// leaves it there and this function returns it there), so the entry of the
    /// pass is `old_len - 1`. That index is derived here instead of being
    /// passed in: a caller's entry index goes stale as soon as an earlier pass
    /// reordered the units, and a stale index would overlap the old and new
    /// ranges and drop a unit (`old unit` panic). The three consumed ranges —
    /// `..old_len - 1`, `old_len - 1` and `old_len..` — partition every slot,
    /// so each unit is taken exactly once.
    fn reorder_preloaded(&mut self, old_len: usize) {
        let old_entry = old_len - 1;
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
            unit.deferred_deps = unit.deferred_deps.iter().map(|dep| map(*dep)).collect();
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
        self.import_free_module_with(name, importer_dir, module_root, true)
    }

    /// [`Self::import_free_module`] with the site-tree gate made explicit.
    ///
    /// The scan only preloads modules from third-party site trees, but a
    /// module sitting next to a compiled extension is part of that extension's
    /// package even when the tree has another name (a vendored dependency, or
    /// the project itself): the extension's initializer may import it, so the
    /// support scan accepts it regardless.
    fn import_free_module_with(
        &self,
        name: &str,
        importer_dir: &Path,
        module_root: &Path,
        require_site: bool,
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
        if require_site && !is_site_package_path(&final_module.path) {
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
    /// A module whose body only declares names, definitions and data
    /// bindings (see [`preloadable_body_stmt`]) has an initializer the
    /// compiler can run early without changing observable behavior; its
    /// imports are checked separately by
    /// [`Self::preload_dependencies_available`], and a load that produces
    /// diagnostics is rolled back transactionally. Modules with dynamic
    /// control flow or call statements stay deferred so optional dependency
    /// graphs keep their lazy semantics.
    ///
    /// Data bindings are included generically: `pydantic.plugin._loader`
    /// binds its plugin cache and entry-point group with annotated
    /// assignments before `get_plugins` is imported from a function body, and
    /// Hydro has no runtime compiler, so leaving such a module deferred turns
    /// the function-level import into a guaranteed `ModuleNotFoundError`.
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
        if !is_site_package_path(&final_module.path) {
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
            .all(|stmt| preloadable_body_stmt(stmt, name))
    }

    /// Links a module imported by a module-level `try` body.
    ///
    /// The import runs in normal execution order, so a module that resolves
    /// and compiles can become a regular dependency; its initializer then runs
    /// once, before the importer, exactly like an eager import. Anything that
    /// cannot be linked (absent module, compile diagnostics, a cycle) leaves
    /// the binding to the runtime, which raises `ModuleNotFoundError` for a
    /// missing module and `ImportError` for a missing member, and the enclosing
    /// `except ImportError` handles it. The load is transactional: a failed
    /// attempt restores every loader table.
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
                other => {
                    if !declaration_only_stmt(other, name) {
                        return false;
                    }
                }
            }
        }
        true
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
            false,
        );
    }

    /// Visits every import in `stmts`.
    ///
    /// `deferred == false` is the module body: its imports are resolved and
    /// linked eagerly, so their initializers keep dependency order
    /// (dependencies before importers) and circular imports are diagnosed.
    /// `deferred == true` means the import runs only when the enclosing
    /// function or class body executes, so it is recorded as a
    /// `(module, member)` edge and never loaded. This keeps optional
    /// dependency graphs (for example `rich/_extension.py` importing
    /// `rich.pretty` and `rich.traceback` inside a function) out of the
    /// program unless their code is actually needed.
    ///
    /// Module-level control flow (`if`/`for`/`while`/`with` in the module body)
    /// is *conditional*: its body runs while the module initializes, so an
    /// import there may not be dropped — NumPy imports `_core` inside an
    /// `if`/`else`. A module found on the search path is linked like a
    /// top-level import, while one that is absent stays deferred (a platform
    /// branch that does not run on this host must still compile).
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
        conditional: bool,
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
                            package,
                            importer_dir,
                            root,
                            alias.name.as_str(),
                            alias.range,
                            scan,
                            conditional,
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
                                    // A wildcard import needs the module's
                                    // exports. A package that compiles is
                                    // linked like an eager import, so the
                                    // lowerer can bind its public names; an
                                    // absent or unloadable one stays deferred
                                    // and reports its runtime import error.
                                    // `from . import *` names this package
                                    // itself, which the lowerer expands from
                                    // the unit's own signatures.
                                    if module_name != current
                                        && self.optional_import(
                                            &module_name,
                                            importer_dir,
                                            root,
                                            scan,
                                        )
                                    {
                                        if !scan.star_imports.contains(&module_name) {
                                            scan.star_imports.push(module_name.clone());
                                        }
                                    } else {
                                        scan.record_deferred(&module_name, None);
                                    }
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
                                // `from pkg import sub` names the submodule
                                // `pkg.sub` whenever it exists on the search
                                // path, exactly like CPython's import
                                // machinery. Record it separately so the
                                // deferred-site preload targets the module
                                // itself instead of the package attribute:
                                // Hydro compiles everything ahead of time, so
                                // an unlinked submodule would otherwise turn
                                // the call site into a `ModuleNotFoundError`.
                                if self.by_name.contains_key(&full)
                                    || resolve_module(
                                        &module_search_roots(importer_dir, root),
                                        &full,
                                    )
                                    .is_some()
                                {
                                    scan.record_deferred(&full, None);
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
                            package,
                            importer_dir,
                            root,
                            &module_name,
                            s.range,
                            scan,
                            conditional,
                        );
                    }
                    // `from pkg import sub` may name a submodule rather than
                    // an attribute; load it when it exists. A submodule that is
                    // initializing right now stays a submodule binding (see
                    // [`Self::probe_submodule`]), never a package attribute.
                    for alias in &s.names {
                        if alias.name.as_str() == "*" {
                            continue;
                        }
                        let full = format!("{module_name}.{}", alias.name);
                        if let Some(index) = self.probe_submodule(&full, importer_dir, root, scan) {
                            if !scan.deps.contains(&index) {
                                scan.deps.push(index);
                            }
                            continue;
                        }
                        // Inside module-level control flow an absent submodule
                        // may simply be a branch that does not run here. Record
                        // the name so the lowerer binds it late instead of
                        // rejecting the program; `X.member` attribute imports
                        // from a module that *was* linked never take this path.
                        if conditional
                            && (module_name == current || !self.by_name.contains_key(&module_name))
                        {
                            scan.record_deferred(&full, Some(alias.name.as_str()));
                        }
                    }
                    if module_name != current
                        && s.names.iter().any(|alias| alias.name.as_str() == "*")
                    {
                        scan.star_imports.push(module_name.clone());
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
                    false,
                ),
                // A class body executes when the class statement runs. At
                // module level that is during module initialization, so its
                // imports are ordinary dependencies ordered like every other
                // module-level import (CPython's `email.message` imports
                // `email.iterators` from its `Message` class body). A class
                // inside a deferred function keeps its imports deferred. The
                // conditional flag keeps a module that is absent on this
                // platform a runtime error instead of a compile diagnostic,
                // matching the previous deferred behaviour for such branches.
                ast::Stmt::ClassDef(s) => self.scan_stmts(
                    &s.body,
                    source,
                    display,
                    importer_dir,
                    root,
                    current,
                    package,
                    scan,
                    deferred,
                    false,
                    conditional || !deferred,
                ),
                ast::Stmt::If(s) => {
                    // Module-level branches execute while the module
                    // initializes, so imports inside them are linked (or
                    // recorded as deferred when the module is absent — a
                    // platform branch that does not run here). Inside a
                    // function or class body the whole block stays deferred.
                    let conditional = conditional || !deferred;
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
                            deferred,
                            optional,
                            conditional,
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
                            deferred,
                            optional,
                            conditional,
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
                        deferred,
                        optional,
                        conditional,
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
                        deferred,
                        optional,
                        conditional,
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
                        deferred,
                        optional,
                        conditional,
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
                        conditional,
                    );
                    // Handler, `else` and `finally` bodies execute at import
                    // time too (a handler only on an exception), so their
                    // imports are conditional rather than dropped. Nested in a
                    // function they stay deferred like every other block.
                    let conditional = conditional || !deferred;
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
                            deferred,
                            false,
                            conditional,
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
                        deferred,
                        false,
                        conditional,
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
                        deferred,
                        false,
                        conditional,
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
                    deferred,
                    optional,
                    conditional || !deferred,
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
                    deferred,
                    optional,
                    conditional || !deferred,
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
        deferred: bool,
        optional: bool,
        conditional: bool,
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
            deferred,
            optional,
            conditional || !deferred,
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
            deferred,
            optional,
            conditional || !deferred,
        );
    }

    /// Resolves one module-level user import and loads it on demand.
    ///
    /// A module that is still initializing (an ancestor package of the
    /// importer) is recorded for late binding.
    ///
    /// `conditional` marks an import that runs only when a module-level
    /// control-flow branch executes (`if`/loop/`with`, or a `try` handler).
    /// The loader still links the module when the search path finds it — NumPy
    /// imports its `_core` package inside `if __NUMPY_SETUP__: ... else:` — but
    /// an absent module stays deferred instead of failing compilation: a
    /// platform branch that never runs must not require the platform's
    /// modules, and the runtime raises `ModuleNotFoundError` only if the branch
    /// actually executes.
    #[allow(clippy::too_many_arguments)]
    fn resolve_import(
        &mut self,
        source: &str,
        display: &Path,
        package: &str,
        importer_dir: &Path,
        module_root: &Path,
        name: &str,
        range: rustpython_parser::text_size::TextRange,
        scan: &mut ImportScan,
        conditional: bool,
    ) {
        if hydro_ir::stdlib::module(name).is_some() {
            return;
        }
        if name == "__main__" {
            // The entry module is never loaded as a dependency: the runtime
            // publishes its module value under the canonical name, and the
            // lowerer binds `import __main__` / `from __main__ import x`
            // against the entry global namespace.
            return;
        }

        // A compiled extension is resolved by the lowerer: it binds
        // aliases/members to `Callee::Extension` and needs no linked module
        // unit. Look it up before rejecting dotted names so `pkg.sub_ext`
        // works without package Python code. A relative import inside a
        // subpackage spells the extension as an absolute dotted name, so a
        // source-relative miss is retried from the package root (`pkg/sub.py`
        // importing `.sub_ext` searches `<root>/pkg/sub_ext`, not
        // `<root>/pkg/pkg/sub_ext`), exactly like `Lowerer::find_extension`.
        // Without the retry the scan reports `module '...' not found` for an
        // extension the lowerer would have bound.
        let extension = match lower::find_extension_for_source(display, name) {
            lower::ExtensionLookup::Missing => {
                lower::extension_lookup_from_package(display, package, name)
            }
            found => found,
        };
        match extension {
            lower::ExtensionLookup::Found { path, .. } => {
                // The extension's initializer can import Python modules that
                // no source statement names; remember it so the loader can
                // link those support modules (see
                // `preload_extension_support_modules`).
                scan.record_extension(name, &path.to_string_lossy());
                return;
            }
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
            lower::ExtensionLookup::Ambiguous { paths } => {
                self.push_span(
                    source,
                    display,
                    range,
                    codes::UNSUPPORTED,
                    lower::ambiguous_artifacts_message(&paths),
                );
                scan.failed = true;
                return;
            }
            lower::ExtensionLookup::PackageInit { path, reason } => {
                self.push_span(
                    source,
                    display,
                    range,
                    codes::UNSUPPORTED,
                    lower::package_init_artifact_message(&path, &reason),
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
            // The module is initializing right now: an ancestor package, or a
            // module reached through a true import cycle. No second unit is
            // created; members are read late from its global table once the
            // outer frame finishes its one-time initializer.
            NamedLoad::InProgress => scan.record_late(name),
            NamedLoad::Missing => {
                if conditional {
                    // The branch may not run on this platform/interpreter;
                    // link nothing and let the runtime importer report the
                    // missing module only if the branch executes.
                    scan.record_deferred(name, None);
                    return;
                }
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

/// Whether `path` lives inside a configured third-party site tree.
///
/// The pinned ecosystem tree is often named `site` (`HYDRO_SITE_PACKAGES` may
/// point at any directory name), and venv layouts differ, so the check follows
/// the configured roots instead of matching the literal `site-packages`/
/// `dist-packages` directory names. `PYTHONPATH` entries and `HYDRO_STDLIB`
/// are not site roots: those trees keep their eager import order.
///
/// Both sides are compared after canonicalization, so a symlinked site tree
/// resolves exactly like the importer resolves it.
fn is_site_package_path(path: &Path) -> bool {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if canonical.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("site-packages") | Some("dist-packages")
        )
    }) {
        return true;
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(list) = std::env::var_os("HYDRO_SITE_PACKAGES") {
        roots.extend(std::env::split_paths(&list));
    }
    for prefix in [
        std::env::var_os("VIRTUAL_ENV").map(PathBuf::from),
        std::env::var_os("CONDA_PREFIX").map(PathBuf::from),
    ]
    .into_iter()
    .flatten()
    {
        roots.extend(lower::prefix_site_packages(&prefix));
    }
    roots.iter().any(|root| {
        let root = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        canonical.starts_with(&root)
    })
}

/// Directories searched for user modules, in order: the importing file's
/// directory, the root of the current package chain (so `pkg.a` can import
/// `pkg.b`), `HYDRO_SITE_PACKAGES`, the site-packages directories of
/// `VIRTUAL_ENV`/`CONDA_PREFIX`, `PYTHONPATH`, the vendored `stdlib/` tree
/// shipped with Hydro, `HYDRO_STDLIB` and the `lib-dynload` directories
/// derived from it ([`stdlib_extension_dirs`]).
///
/// `HYDRO_STDLIB` is a CPython `Lib` directory that backs the standard library
/// sources (`<cpython>/Lib`). Project modules, site-packages and `PYTHONPATH`
/// entries precede it, but the vendored modules follow them and precede
/// `HYDRO_STDLIB`: they are Hydro's compatibility adaptations, so an override
/// installed in `stdlib/` stays observable even for an importer that lives
/// inside the `Lib` tree. Modules Hydro does not vendor fall through to
/// `HYDRO_STDLIB` and, for compiled extensions, to its `lib-dynload` siblings.
fn module_search_roots(importer_dir: &Path, module_root: &Path) -> Vec<PathBuf> {
    let hydro_site_packages = std::env::var_os("HYDRO_SITE_PACKAGES");
    let pythonpath = std::env::var_os("PYTHONPATH");
    let hydro_stdlib = std::env::var_os("HYDRO_STDLIB");
    let virtual_env = std::env::var_os("VIRTUAL_ENV").map(PathBuf::from);
    let conda_prefix = std::env::var_os("CONDA_PREFIX").map(PathBuf::from);
    let bundled_stdlib = bundled_stdlib_root();
    module_search_roots_from(
        importer_dir,
        module_root,
        hydro_site_packages.as_deref(),
        pythonpath.as_deref(),
        hydro_stdlib.as_deref(),
        virtual_env.as_deref(),
        conda_prefix.as_deref(),
        bundled_stdlib,
    )
}

/// Directory of the pure-Python standard library modules vendored with Hydro
/// (`stdlib/` in the repository).
///
/// A development build finds it through the crate's compile-time location
/// (`CARGO_MANIFEST_DIR`); an installed layout ships it with the executable and
/// is found through `<exe dir>/stdlib` or `<prefix>/stdlib` when the executable
/// lives in `<prefix>/bin`. Executable-relative candidates come first so a
/// shipped tree is never shadowed by the checkout of the machine that built
/// it. `None` when no candidate exists. Do not confuse this root with
/// `HYDRO_STDLIB`: that variable names a CPython `Lib` tree, and the vendored
/// modules here are the compatibility adaptations that must win over it
/// (see [`module_search_roots_from`]).
fn bundled_stdlib_root() -> Option<&'static Path> {
    static ROOT: std::sync::LazyLock<Option<PathBuf>> = std::sync::LazyLock::new(|| {
        let mut candidates = Vec::new();
        if let Some(exe_dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            candidates.push(exe_dir.join("stdlib"));
            candidates.push(exe_dir.join("..").join("stdlib"));
        }
        candidates.push(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("stdlib"),
        );
        candidates
            .into_iter()
            .find(|dir| dir.is_dir())
            .and_then(|dir| fs::canonicalize(dir).ok())
    });
    ROOT.as_deref()
}

/// `lib-dynload` directories that hold the C extension modules of the CPython
/// standard library named by `HYDRO_STDLIB`.
///
/// CPython keeps extension modules next to the standard library, not inside
/// `Lib`, so a `HYDRO_STDLIB` installation must not need a `PYTHONPATH` entry
/// to import them. `HYDRO_STDLIB` names either a `Lib` tree
/// (`<cpython>/Lib`, whose sibling `<cpython>/lib-dynload` holds the extension
/// modules) or an installed version root such as `/usr/lib/python3.14` (whose
/// child `<root>/lib-dynload` does). Both layouts are tried in that order
/// (`<stdlib>/lib-dynload`, then `<stdlib>/../lib-dynload`); only existing
/// directories are returned and duplicates are kept once, so the order is
/// deterministic. The `..` of the sibling layout is resolved lexically, so no
/// returned path carries a `..` component.
///
/// This is the single frontend definition of the list: the module loader seeds
/// it as a search root and the lowerer's extension lookup appends it, so a
/// compiled `import _elementtree` and its runtime resolution agree. The
/// runtime derives the same directories for `sys.path` (see
/// `hydro_runtime::modules::search_path`); keep the two in sync.
pub(crate) fn stdlib_extension_dirs(hydro_stdlib: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let Some(list) = hydro_stdlib else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    for stdlib in std::env::split_paths(list) {
        if stdlib.as_os_str().is_empty() {
            continue;
        }
        for candidate in [
            stdlib.join("lib-dynload"),
            stdlib.join("..").join("lib-dynload"),
        ] {
            if !candidate.is_dir() {
                continue;
            }
            // The sibling layout is built as `<stdlib>/../lib-dynload`; report
            // the lexically normalised form so search roots carry no `..`.
            let dir = lexical_normalize(&candidate);
            let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            if seen.insert(key) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// Lexically normalises a path: drops `.` components and resolves `..`
/// without touching the filesystem (like `os.path.normpath`).
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Appends [`stdlib_extension_dirs`] to `dirs`, skipping entries already
/// present (compared after canonicalization).
///
/// Used by the lowerer's extension search, which has no final deduplication
/// pass of its own; the module loader and the runtime fall back to their
/// existing filter.
pub(crate) fn push_stdlib_extension_dirs(
    dirs: &mut Vec<PathBuf>,
    hydro_stdlib: Option<&std::ffi::OsStr>,
) {
    let mut seen: HashSet<PathBuf> = dirs
        .iter()
        .map(|dir| fs::canonicalize(dir).unwrap_or_else(|_| dir.clone()))
        .collect();
    for dir in stdlib_extension_dirs(hydro_stdlib) {
        let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.insert(key) {
            dirs.push(dir);
        }
    }
}

/// Builds the module search path from raw environment values.
///
/// Order: the importing directory and the current package root, then
/// `HYDRO_SITE_PACKAGES`, the site-packages directories of `VIRTUAL_ENV` and
/// `CONDA_PREFIX`, `PYTHONPATH`, the vendored `stdlib/` tree
/// ([`bundled_stdlib_root`]), `HYDRO_STDLIB` and the `lib-dynload` directories
/// derived from it ([`stdlib_extension_dirs`]). Every user location therefore
/// takes precedence over the standard library, while a vendored adaptation
/// wins over the generic `Lib` source, which in turn wins over an extension of
/// the same name in `lib-dynload`.
///
/// The importing directory and the current package root normally come first so
/// relative and sibling imports resolve like CPython's. When the importer
/// itself lives inside the `HYDRO_STDLIB` tree those two roots *are* the
/// sysroot: a standard-library module importing `enum` would otherwise resolve
/// its own `Lib/enum.py` and bypass the vendored adaptation. They are moved
/// after the vendored tree (and before `HYDRO_STDLIB`, whose entries still
/// cover everything Hydro does not vendor).
///
/// Missing and non-directory entries are dropped and duplicates are searched
/// only once. The importing file's directory comes first unless it is part of
/// the `HYDRO_STDLIB` sysroot.
#[allow(clippy::too_many_arguments)]
fn module_search_roots_from(
    importer_dir: &Path,
    module_root: &Path,
    hydro_site_packages: Option<&std::ffi::OsStr>,
    pythonpath: Option<&std::ffi::OsStr>,
    hydro_stdlib: Option<&std::ffi::OsStr>,
    virtual_env: Option<&Path>,
    conda_prefix: Option<&Path>,
    bundled_stdlib: Option<&Path>,
) -> Vec<PathBuf> {
    let mut early = vec![importer_dir.to_path_buf(), module_root.to_path_buf()];
    for list in [hydro_site_packages].into_iter().flatten() {
        early.extend(std::env::split_paths(list));
    }
    for prefix in [virtual_env, conda_prefix].into_iter().flatten() {
        early.extend(lower::prefix_site_packages(prefix));
    }
    for list in [pythonpath].into_iter().flatten() {
        early.extend(std::env::split_paths(list));
    }
    let mut sysroot: Vec<PathBuf> = Vec::new();
    if let Some(list) = hydro_stdlib {
        sysroot.extend(std::env::split_paths(list));
    }
    // Roots inside `HYDRO_STDLIB` are the sysroot seen through the importer's
    // location, not user overrides: they must not shadow a vendored module.
    let mut sysroot_scoped: Vec<PathBuf> = Vec::new();
    if !sysroot.is_empty() {
        let canonical: Vec<PathBuf> = sysroot
            .iter()
            .map(|dir| fs::canonicalize(dir).unwrap_or_else(|_| dir.clone()))
            .collect();
        early.retain(|dir| {
            let resolved = fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
            let inside = canonical.iter().any(|root| resolved.starts_with(root));
            if inside {
                sysroot_scoped.push(dir.clone());
            }
            !inside
        });
    }
    // Vendored modules are Hydro's own compatibility shims (for example the
    // patched `enum.py`); they must win over a generic CPython `Lib` sysroot
    // while every module Hydro does not vendor still falls through to it.
    let mut dirs = early;
    dirs.extend(bundled_stdlib.map(Path::to_path_buf));
    dirs.extend(sysroot_scoped);
    dirs.extend(sysroot);
    // CPython's C extension modules live in `lib-dynload` next to the standard
    // library. Deriving them from `HYDRO_STDLIB` keeps `import _elementtree`
    // linkable without a `PYTHONPATH` entry. They follow the sysroot so a
    // `Lib/*.py` module wins over an extension of the same name, and the
    // deduplication below drops repeats of an explicit `PYTHONPATH` entry.
    dirs.extend(stdlib_extension_dirs(hydro_stdlib));

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

/// Whether one module-level statement only declares names or data.
///
/// Imports, definitions, assignments, docstrings and statically dead guards
/// carry no control flow of their own, so an eager preload can run them early.
/// Assignments are admitted generically — helper modules routinely bind caches
/// and constants at module scope (`pydantic.plugin._loader` is the motivating
/// example), and module-level `try` imports already link the same shape. The
/// preload paths separately require every imported module to resolve and roll
/// the load back on any diagnostic. Statements that run control flow of their
/// own (`for`, `while`, `with`, `try`) and non-docstring expression statements
/// stay deferred: they may perform observable work or depend on state the
/// early initializer cannot see.
fn declaration_only_stmt(stmt: &ast::Stmt, name: &str) -> bool {
    match stmt {
        ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_) => true,
        ast::Stmt::FunctionDef(_)
        | ast::Stmt::AsyncFunctionDef(_)
        | ast::Stmt::ClassDef(_)
        | ast::Stmt::Assign(_)
        | ast::Stmt::AnnAssign(_)
        | ast::Stmt::AugAssign(_)
        | ast::Stmt::Pass(_) => true,
        // Only a docstring is a side-effect-free expression.
        ast::Stmt::Expr(s) => matches!(
            &*s.value,
            ast::Expr::Constant(constant) if matches!(constant.value, ast::Constant::Str(_))
        ),
        // A statically dead branch never runs at import time; a taken one may
        // contain control flow the preload must not move earlier.
        ast::Stmt::If(s) => lower::static_if_branch(&s.test, name) == Some(false),
        _ => false,
    }
}

/// Whether one module-level statement of a deferred-preload candidate only
/// declares names or data.
///
/// Like [`declaration_only_stmt`], with statically resolved branches entered:
/// a *taken* branch runs at import time, so its body must satisfy the same
/// rule, and the dead `else` is skipped. `pydantic/deprecated/parse.py` only
/// binds a warning alias under `if not TYPE_CHECKING:`, and that shape must
/// stay linkable. A dynamic branch may run, so it remains ineligible. The
/// optional-import path keeps [`declaration_only_stmt`] because it additionally
/// inspects imports inside taken branches for extension loaders.
fn preloadable_body_stmt(stmt: &ast::Stmt, name: &str) -> bool {
    match stmt {
        ast::Stmt::If(s) => {
            // `if not TYPE_CHECKING:` is taken at runtime even though the
            // compiler keeps the branch dynamic; `parse.py` binds its warning
            // alias there.
            if lower::negated_type_checking(&s.test) {
                return s.body.iter().all(|stmt| preloadable_body_stmt(stmt, name));
            }
            match lower::static_if_branch(&s.test, name) {
                Some(true) => s.body.iter().all(|stmt| preloadable_body_stmt(stmt, name)),
                Some(false) => s
                    .orelse
                    .iter()
                    .all(|stmt| preloadable_body_stmt(stmt, name)),
                None => false,
            }
        }
        other => declaration_only_stmt(other, name),
    }
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

/// Absolute module names a top-level package spells out for dynamic dispatch.
///
/// pydantic 2.x exposes its public API through PEP 562 `__getattr__` and a
/// `_dynamic_imports` table whose values are relative module names
/// (`'.main'`, `'.type_adapter'`, `'.deprecated.tools'`): the table is read at
/// attribute-access time and `importlib.import_module` runs then, so no
/// `import` statement names those modules and the loader would never compile
/// them. This is the bounded static half of that pattern — module discovery for
/// the AOT compiler, not runtime compilation:
///
/// * only a **top-level package initializer** is scanned: `pkg/__init__.py`
///   whose package has no dot. Its table is the package's public API surface.
///   A nested package (`numpy/_typing/__init__.py`, `pkg/sub/__init__.py`) or a
///   plain module keeps its table lazy: CPython reaches those tables only after
///   the enclosing package graph finished initializing, and eagerly compiling
///   the targets would run internal annotation helpers before the parent
///   publishes its re-exports (NumPy's `_typing._dtype_like` reads
///   `numpy.dtype`, which `numpy._core` installs only when its own body runs);
/// * only string constants that are elements or values of a `dict`, `list`,
///   `tuple` or `set` display, or arguments of a call, are considered. A
///   docstring, a bare `X = '.name'` assignment and every other string stay
///   ordinary data;
/// * the constant must be relative and name a real module: it is resolved
///   against the unit's `__package__` exactly like a relative import (`pkg` for
///   `pkg/__init__.py`), so absolute external names (`'pydantic_core'`),
///   sentinel spellings (`'__module__'`) and literals with no file on the
///   search path are dropped. A resolved target may itself be nested
///   (`'.deprecated.tools'` -> `pkg.deprecated.tools`); the restriction is on
///   the *naming* unit, not on the target;
/// * the scan stops at function and class bodies: a literal there runs only
///   when that code does, so it stays as lazy as a nested import;
/// * a name already provided by a static import or by the native module table
///   is dropped, so a module keeps one unit and one initializer.
///
/// Returns absolute names (`pkg.main`) in discovery order.
fn dynamic_import_candidates(module: &ast::ModModule, current: &str, package: &str) -> Vec<String> {
    // A top-level package initializer is the only dynamic-dispatch surface
    // preloaded. Everything else (entry script, conftest, REPL fragment,
    // nested package, plain module) has no package or no public table: its
    // literals stay lazy so annotation and internal helpers cannot initialize
    // before the enclosing package graph finishes.
    if package.is_empty() || package.contains('.') || package != current {
        return Vec::new();
    }
    let mut literals: Vec<String> = Vec::new();
    collect_dynamic_dispatch_stmts(&module.body, current, &mut literals);
    let mut candidates: Vec<String> = Vec::new();
    for literal in literals {
        let Some(name) = resolve_dynamic_module_literal(&literal, package) else {
            continue;
        };
        if name == current
            || hydro_ir::stdlib::module(&name).is_some()
            || candidates.contains(&name)
        {
            continue;
        }
        candidates.push(name);
    }
    candidates
}

/// Resolves one relative dispatch literal against the naming unit's package.
///
/// Returns `None` for an absolute name, a literal that is only dots (the
/// package itself), a malformed relative import, or a name that is not a valid
/// dotted identifier. The caller decides whether the module exists.
fn resolve_dynamic_module_literal(literal: &str, package: &str) -> Option<String> {
    if package.is_empty() {
        return None;
    }
    let dots = literal.bytes().take_while(|byte| *byte == b'.').count();
    if dots == 0 || dots == literal.len() {
        return None;
    }
    let level = u32::try_from(dots).ok()?;
    let name = lower::resolve_import_name(level, Some(&literal[dots..]), package).ok()?;
    for part in name.split('.') {
        if part.is_empty() || !part.chars().all(|c| c == '_' || c.is_alphanumeric()) {
            return None;
        }
    }
    Some(name)
}

/// Collects dispatch literals from the module-level statements of a unit.
///
/// Module-level control flow executes while the module initialises, so a
/// dispatch table inside it counts; function and class bodies run later and are
/// not entered. `current` makes the same statically fixed branches as the
/// import scan (`if TYPE_CHECKING:` never runs, `if __name__ == "__main__":`
/// is false for an imported module).
fn collect_dynamic_dispatch_stmts(stmts: &[ast::Stmt], current: &str, out: &mut Vec<String>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Assign(s) => collect_dynamic_dispatch_expr(&s.value, out),
            ast::Stmt::AnnAssign(s) => {
                if let Some(value) = &s.value {
                    collect_dynamic_dispatch_expr(value, out);
                }
            }
            ast::Stmt::AugAssign(s) => collect_dynamic_dispatch_expr(&s.value, out),
            ast::Stmt::Expr(s) => collect_dynamic_dispatch_expr(&s.value, out),
            ast::Stmt::If(s) => {
                let branch = lower::static_if_branch(&s.test, current);
                if branch != Some(false) {
                    collect_dynamic_dispatch_stmts(&s.body, current, out);
                }
                if branch != Some(true) {
                    collect_dynamic_dispatch_stmts(&s.orelse, current, out);
                }
            }
            ast::Stmt::For(s) => {
                collect_dynamic_dispatch_stmts(&s.body, current, out);
                collect_dynamic_dispatch_stmts(&s.orelse, current, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_dynamic_dispatch_stmts(&s.body, current, out);
                collect_dynamic_dispatch_stmts(&s.orelse, current, out);
            }
            ast::Stmt::While(s) => {
                collect_dynamic_dispatch_stmts(&s.body, current, out);
                collect_dynamic_dispatch_stmts(&s.orelse, current, out);
            }
            ast::Stmt::With(s) => collect_dynamic_dispatch_stmts(&s.body, current, out),
            ast::Stmt::AsyncWith(s) => collect_dynamic_dispatch_stmts(&s.body, current, out),
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    collect_dynamic_dispatch_stmts(&case.body, current, out);
                }
            }
            ast::Stmt::Try(s) => {
                collect_dynamic_dispatch_stmts(&s.body, current, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_dynamic_dispatch_stmts(&handler.body, current, out);
                }
                collect_dynamic_dispatch_stmts(&s.orelse, current, out);
                collect_dynamic_dispatch_stmts(&s.finalbody, current, out);
            }
            ast::Stmt::TryStar(s) => {
                collect_dynamic_dispatch_stmts(&s.body, current, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_dynamic_dispatch_stmts(&handler.body, current, out);
                }
                collect_dynamic_dispatch_stmts(&s.orelse, current, out);
                collect_dynamic_dispatch_stmts(&s.finalbody, current, out);
            }
            // Function and class bodies run later: their literals stay lazy,
            // like nested imports. Imports and the remaining statements carry
            // no dispatch table.
            _ => {}
        }
    }
}

/// Collects dispatch literals from one module-level expression.
///
/// Only container displays and call arguments carry module names; a bare string
/// expression (a docstring) or an unrelated constant is never inspected.
fn collect_dynamic_dispatch_expr(expr: &ast::Expr, out: &mut Vec<String>) {
    // A string expression statement is a docstring, not a dispatch table.
    if matches!(expr, ast::Expr::Constant(_)) {
        return;
    }
    collect_nested_dispatch_literals(expr, out);
}

/// Walks one dispatch container or call and records every relative string.
///
/// Dict keys are attribute names (`'Thing'`), so only values name modules. A
/// nested container stays inside the dispatch context; anything else (a name,
/// an f-string, an operation) is dynamic and cannot be resolved statically.
fn collect_nested_dispatch_literals(expr: &ast::Expr, out: &mut Vec<String>) {
    match expr {
        ast::Expr::Dict(dict) => {
            for value in &dict.values {
                collect_nested_dispatch_literals(value, out);
            }
        }
        ast::Expr::List(list) => {
            for element in &list.elts {
                collect_nested_dispatch_literals(element, out);
            }
        }
        ast::Expr::Tuple(tuple) => {
            for element in &tuple.elts {
                collect_nested_dispatch_literals(element, out);
            }
        }
        ast::Expr::Set(set) => {
            for element in &set.elts {
                collect_nested_dispatch_literals(element, out);
            }
        }
        ast::Expr::Call(call) => {
            for argument in &call.args {
                collect_nested_dispatch_literals(argument, out);
            }
            for keyword in &call.keywords {
                collect_nested_dispatch_literals(&keyword.value, out);
            }
        }
        ast::Expr::Constant(constant) => {
            if let ast::Constant::Str(value) = &constant.value {
                if value.starts_with('.') {
                    out.push(value.clone());
                }
            }
        }
        _ => {}
    }
}

/// Effective `__all__` of a loaded unit, including names a module re-exports
/// through module-level `from dependency import *`.
///
/// NumPy builds its public surface with `from .umath import *` followed by
/// `extend_all(umath)`, which appends the dependency's `__all__` to its own at
/// runtime. A star import of that module must therefore see the union, not the
/// literal list only; otherwise whole dependency surfaces (`absolute`, every
/// ufunc, ...) silently disappear from `numpy._core` and `numpy`.
///
/// A dependency without a literal `__all__` contributes its public export
/// names (the same fallback CPython uses). `visiting` breaks cycles: a module
/// reached again keeps its literal list.
fn effective_module_all(
    units: &[LoadedModule],
    exports: &[HashMap<String, lower::UserExport>],
    index: usize,
    visiting: &mut HashSet<usize>,
) -> Option<Vec<String>> {
    let literal = lower::module_all_names(&units[index].module.body);
    if !visiting.insert(index) {
        return literal;
    }
    let Some(mut names) = literal else {
        visiting.remove(&index);
        return None;
    };
    for dependency in &units[index].star_imports {
        let Some(dep) = units
            .iter()
            .position(|unit| unit.name.as_str() == dependency.as_str())
        else {
            continue;
        };
        let extra = match effective_module_all(units, exports, dep, visiting) {
            Some(dep_names) => dep_names,
            None => {
                let mut public: Vec<String> = exports[dep]
                    .keys()
                    .filter(|name| !name.starts_with('_'))
                    .cloned()
                    .collect();
                public.sort();
                public
            }
        };
        for name in extra {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    visiting.remove(&index);
    Some(names)
}

/// One module-level import of a unit, in the form the initializer-order pass
/// needs: a whole module (a module alias) or named members.
enum ModuleImportKind {
    /// `import a.b` / `from x import sub`: binding the module value does not
    /// need the target's body to have run.
    ModuleAlias,
    /// `from a import name, other`: named members, some of which are direct
    /// function handles.
    Members(Vec<String>),
}

/// Collects the imports that execute while a unit's module body initializes.
///
/// Nested function and class bodies run later and carry no ordering
/// requirement; module-level control flow does execute, so its imports count.
/// Branches whose guard is statically fixed are resolved like the import scan
/// (`if TYPE_CHECKING:` never runs).
fn collect_initializer_imports(
    stmts: &[ast::Stmt],
    current: &str,
    package: &str,
    out: &mut Vec<(String, ModuleImportKind)>,
) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Import(s) => {
                for alias in &s.names {
                    out.push((alias.name.to_string(), ModuleImportKind::ModuleAlias));
                }
            }
            ast::Stmt::ImportFrom(s) => {
                let level = s.level.as_ref().map(|level| level.to_u32()).unwrap_or(0);
                let Ok(module) = lower::resolve_import_name(
                    level,
                    s.module.as_ref().map(|module| module.as_str()),
                    package,
                ) else {
                    continue;
                };
                out.push((
                    module,
                    ModuleImportKind::Members(
                        s.names.iter().map(|alias| alias.name.to_string()).collect(),
                    ),
                ));
            }
            // Definitions only execute their own nested imports when called.
            ast::Stmt::FunctionDef(_) | ast::Stmt::AsyncFunctionDef(_) | ast::Stmt::ClassDef(_) => {
            }
            ast::Stmt::If(s) => {
                let branch = lower::static_if_branch(&s.test, current);
                if branch != Some(false) {
                    collect_initializer_imports(&s.body, current, package, out);
                }
                if branch != Some(true) {
                    collect_initializer_imports(&s.orelse, current, package, out);
                }
            }
            ast::Stmt::For(s) => {
                collect_initializer_imports(&s.body, current, package, out);
                collect_initializer_imports(&s.orelse, current, package, out);
            }
            ast::Stmt::AsyncFor(s) => {
                collect_initializer_imports(&s.body, current, package, out);
                collect_initializer_imports(&s.orelse, current, package, out);
            }
            ast::Stmt::While(s) => {
                collect_initializer_imports(&s.body, current, package, out);
                collect_initializer_imports(&s.orelse, current, package, out);
            }
            ast::Stmt::With(s) => collect_initializer_imports(&s.body, current, package, out),
            ast::Stmt::AsyncWith(s) => collect_initializer_imports(&s.body, current, package, out),
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    collect_initializer_imports(&case.body, current, package, out);
                }
            }
            ast::Stmt::Try(s) => {
                collect_initializer_imports(&s.body, current, package, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_initializer_imports(&handler.body, current, package, out);
                }
                collect_initializer_imports(&s.orelse, current, package, out);
                collect_initializer_imports(&s.finalbody, current, package, out);
            }
            ast::Stmt::TryStar(s) => {
                collect_initializer_imports(&s.body, current, package, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_initializer_imports(&handler.body, current, package, out);
                }
                collect_initializer_imports(&s.orelse, current, package, out);
                collect_initializer_imports(&s.finalbody, current, package, out);
            }
            _ => {}
        }
    }
}

/// Names a module body binds with a `def` statement.
///
/// Such exports are compiled into callable handles that exist before the
/// defining module's body runs; importing one does not require the defining
/// module to have initialized. Every other export (a class, an assignment, a
/// type alias, a re-export) is a value stored by the body and does.
fn module_callable_names(module: &ast::ModModule, current: &str) -> HashSet<String> {
    // Only definitions that always execute are callable handles before the
    // body runs. A definition inside a loop, a `try` or an open branch may
    // never execute, so it keeps the edge hard.
    fn walk(stmts: &[ast::Stmt], current: &str, out: &mut HashSet<String>) {
        for stmt in stmts {
            match stmt {
                ast::Stmt::FunctionDef(s) => {
                    out.insert(s.name.to_string());
                }
                ast::Stmt::AsyncFunctionDef(s) => {
                    out.insert(s.name.to_string());
                }
                ast::Stmt::If(s) => {
                    let branch = lower::static_if_branch(&s.test, current);
                    match branch {
                        Some(true) => walk(&s.body, current, out),
                        Some(false) => walk(&s.orelse, current, out),
                        None => {}
                    }
                }
                _ => {}
            }
        }
    }

    let mut names = HashSet::new();
    walk(&module.body, current, &mut names);
    names
}

/// Callable exports of every unit, following explicit and star re-exports.
///
/// A package initializer often only re-exports its implementation module
/// (`numpy/linalg/__init__.py` does `from ._linalg import *`). Importing
/// `matrix_power` from that package binds the same compiled function handle as
/// importing it from `_linalg`, so the re-export must be recognized before an
/// edge can be called soft. Only `def` names are propagated: classes and plain
/// assignments stay values that need the defining body to have run.
fn initializer_callable_names(
    units: &[LoadedModule],
    by_name: &HashMap<String, usize>,
) -> HashMap<usize, HashSet<String>> {
    fn collect(
        index: usize,
        units: &[LoadedModule],
        by_name: &HashMap<String, usize>,
        cache: &mut HashMap<usize, HashSet<String>>,
        visiting: &mut HashSet<usize>,
    ) -> HashSet<String> {
        if let Some(cached) = cache.get(&index) {
            return cached.clone();
        }
        if !visiting.insert(index) {
            return HashSet::new();
        }
        let unit = &units[index];
        let mut names = module_callable_names(&unit.module, &unit.name);
        let mut imports = Vec::new();
        collect_initializer_imports(&unit.module.body, &unit.name, &unit.package, &mut imports);
        for (module, kind) in imports {
            let Some(&dep) = by_name.get(&module) else {
                continue;
            };
            if dep == index {
                continue;
            }
            let dep_names = collect(dep, units, by_name, cache, visiting);
            if let ModuleImportKind::Members(members) = kind {
                for name in members {
                    if name == "*" {
                        names.extend(dep_names.iter().cloned());
                    } else if !by_name.contains_key(&format!("{module}.{name}"))
                        && dep_names.contains(&name)
                    {
                        names.insert(name);
                    }
                }
            }
        }
        visiting.remove(&index);
        cache.insert(index, names.clone());
        names
    }

    let mut cache = HashMap::new();
    for index in 0..units.len() {
        let mut visiting = HashSet::new();
        collect(index, units, by_name, &mut cache, &mut visiting);
    }
    cache
}

/// Whether the importer's module body can initialize before `dep`'s.
///
/// Only a `from dep import name` where every imported `name` is a `def` in
/// `dep` qualifies: the lowerer binds those to direct handles, so
/// running the importer's body first cannot observe a missing value. A module
/// alias, a star import, a submodule binding, an assignment/value export and
/// every edge added by preload (extension support, deferred discovery) are
/// conservative: the importer may read the target's namespace and must stay
/// after it. The exception is an edge linked from a *nested* import: its
/// statement runs only when the enclosing function does, never while the
/// importer's body initializes, so it may be relaxed to break a cycle.
fn initializer_edge_is_soft(
    units: &[LoadedModule],
    by_name: &HashMap<String, usize>,
    callables: &HashMap<usize, HashSet<String>>,
    importer: usize,
    dep: usize,
) -> bool {
    if units[importer].deferred_deps.contains(&dep) {
        return true;
    }
    let dep_unit = &units[dep];
    let callable = callables.get(&dep).cloned().unwrap_or_default();
    let mut imports = Vec::new();
    collect_initializer_imports(
        &units[importer].module.body,
        &units[importer].name,
        &units[importer].package,
        &mut imports,
    );
    let mut saw_edge = false;
    for (module, kind) in imports {
        if module == dep_unit.name {
            match &kind {
                ModuleImportKind::ModuleAlias => return false,
                ModuleImportKind::Members(names) => {
                    if names.is_empty() {
                        // `from dep import ()` cannot occur; be conservative.
                        return false;
                    }
                    for name in names {
                        if name.as_str() == "*"
                            || by_name.contains_key(&format!("{module}.{name}"))
                            || !callable.contains(name)
                        {
                            return false;
                        }
                    }
                    saw_edge = true;
                }
            }
            continue;
        }
        // `from base import name` where `base.name` is `dep`: the scan linked
        // the submodule, not a package attribute, so the importer needs the
        // submodule's body (module alias).
        if let Some((base, member)) = dep_unit.name.rsplit_once('.') {
            if module == base {
                if let ModuleImportKind::Members(names) = &kind {
                    if names.iter().any(|name| name.as_str() == member) {
                        return false;
                    }
                }
            }
        }
    }
    saw_edge
}

/// One ordering edge between module bodies.
///
/// `dep -> importer` means the dependency usually initializes first. A soft
/// edge only binds compiled function/class handles and may be dropped to break
/// a cycle; constraint edges encode "the package body must initialize before
/// this late helper".
struct InitializerEdge {
    dep: usize,
    importer: usize,
    soft: bool,
    dropped: bool,
}

/// Forward reachability over the currently active ordering edges.
///
/// The direction is `dependency -> importer`, so a path from a late unit to
/// its ancestor package closes that constraint's cycle.
fn initializer_reachable(
    start: usize,
    outgoing: &[Vec<usize>],
    edges: &[InitializerEdge],
    placed: &[bool],
) -> HashSet<usize> {
    let mut seen = HashSet::new();
    let mut pending = vec![start];
    while let Some(index) = pending.pop() {
        if !seen.insert(index) {
            continue;
        }
        for &edge in &outgoing[index] {
            if edges[edge].dropped {
                continue;
            }
            let importer = edges[edge].importer;
            if !placed[importer] {
                pending.push(importer);
            }
        }
    }
    seen
}

/// Imports of `module` in `stmts` that execute while the owning module body
/// initializes.
///
/// Direct imports, module-level control flow and class bodies run during
/// module initialization; function bodies do not and are skipped. Used to
/// decide whether a reversed package/submodule cycle is safe: every import of
/// the delayed submodule must be a top-level `from ... import ...` so its
/// binding stores can move to an initializer segment.
fn imports_module_during_init(stmts: &[ast::Stmt], package: &str, module: &str) -> bool {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Import(s) => {
                if s.names.iter().any(|alias| alias.name.as_str() == module) {
                    return true;
                }
            }
            ast::Stmt::ImportFrom(s) => {
                let level = s.level.as_ref().map(|level| level.to_u32()).unwrap_or(0);
                if let Ok(resolved) = lower::resolve_import_name(
                    level,
                    s.module.as_ref().map(|module| module.as_str()),
                    package,
                ) {
                    if resolved == module {
                        return true;
                    }
                }
            }
            ast::Stmt::FunctionDef(_) | ast::Stmt::AsyncFunctionDef(_) => {}
            ast::Stmt::ClassDef(s) => {
                if imports_module_during_init(&s.body, package, module) {
                    return true;
                }
            }
            ast::Stmt::If(s) => {
                if imports_module_during_init(&s.body, package, module)
                    || imports_module_during_init(&s.orelse, package, module)
                {
                    return true;
                }
            }
            ast::Stmt::For(s) => {
                if imports_module_during_init(&s.body, package, module)
                    || imports_module_during_init(&s.orelse, package, module)
                {
                    return true;
                }
            }
            ast::Stmt::AsyncFor(s) => {
                if imports_module_during_init(&s.body, package, module)
                    || imports_module_during_init(&s.orelse, package, module)
                {
                    return true;
                }
            }
            ast::Stmt::While(s) => {
                if imports_module_during_init(&s.body, package, module)
                    || imports_module_during_init(&s.orelse, package, module)
                {
                    return true;
                }
            }
            ast::Stmt::With(s) => {
                if imports_module_during_init(&s.body, package, module) {
                    return true;
                }
            }
            ast::Stmt::AsyncWith(s) => {
                if imports_module_during_init(&s.body, package, module) {
                    return true;
                }
            }
            ast::Stmt::Try(s) => {
                if imports_module_during_init(&s.body, package, module)
                    || imports_module_during_init(&s.orelse, package, module)
                    || imports_module_during_init(&s.finalbody, package, module)
                {
                    return true;
                }
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if imports_module_during_init(&handler.body, package, module) {
                        return true;
                    }
                }
            }
            ast::Stmt::TryStar(s) => {
                if imports_module_during_init(&s.body, package, module)
                    || imports_module_during_init(&s.orelse, package, module)
                    || imports_module_during_init(&s.finalbody, package, module)
                {
                    return true;
                }
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if imports_module_during_init(&handler.body, package, module) {
                        return true;
                    }
                }
            }
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    if imports_module_during_init(&case.body, package, module) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether a top-level compound statement imports `module` from a block that
/// executes during module initialization.
fn compound_imports_module(stmt: &ast::Stmt, package: &str, module: &str) -> bool {
    match stmt {
        ast::Stmt::ClassDef(s) => imports_module_during_init(&s.body, package, module),
        ast::Stmt::If(s) => {
            imports_module_during_init(&s.body, package, module)
                || imports_module_during_init(&s.orelse, package, module)
        }
        ast::Stmt::For(s) => {
            imports_module_during_init(&s.body, package, module)
                || imports_module_during_init(&s.orelse, package, module)
        }
        ast::Stmt::AsyncFor(s) => {
            imports_module_during_init(&s.body, package, module)
                || imports_module_during_init(&s.orelse, package, module)
        }
        ast::Stmt::While(s) => {
            imports_module_during_init(&s.body, package, module)
                || imports_module_during_init(&s.orelse, package, module)
        }
        ast::Stmt::With(s) => imports_module_during_init(&s.body, package, module),
        ast::Stmt::AsyncWith(s) => imports_module_during_init(&s.body, package, module),
        ast::Stmt::Try(s) => {
            imports_module_during_init(&s.body, package, module)
                || imports_module_during_init(&s.orelse, package, module)
                || imports_module_during_init(&s.finalbody, package, module)
                || s.handlers.iter().any(|handler| {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    imports_module_during_init(&handler.body, package, module)
                })
        }
        ast::Stmt::TryStar(s) => {
            imports_module_during_init(&s.body, package, module)
                || imports_module_during_init(&s.orelse, package, module)
                || imports_module_during_init(&s.finalbody, package, module)
                || s.handlers.iter().any(|handler| {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    imports_module_during_init(&handler.body, package, module)
                })
        }
        ast::Stmt::Match(s) => s
            .cases
            .iter()
            .any(|case| imports_module_during_init(&case.body, package, module)),
        _ => false,
    }
}

/// Whether `package` may initialize before the submodule `unit` it imports,
/// even though `unit` names `package` as a late (cycle) import.
///
/// `zipfile/__init__.py` defines everything and only then does
/// `from ._path import Path, CompleteDirs`; `zipfile/_path/__init__.py`
/// subclasses `zipfile.ZipFile` while initializing. CPython therefore runs the
/// package body's prefix first, then the submodule, then the package's last
/// import statement. Hydro runs a whole module body as one initializer, so the
/// order is package first, and the package's import bindings are published by
/// an extra initializer segment.
///
/// The reversal is only safe when every import of the submodule is a direct
/// top-level `from <submodule> import ...`, the package body never reads the
/// imported names while it initializes, and those imports are the *tail* of
/// the module body (the `zipfile` shape). A package that defines more names
/// after the import keeps the established submodule-first order: its later
/// definitions must stay in the package body, not move to the deferred
/// segment (`pkg/__init__.py` imports `run` first and defines `greeting`
/// afterwards, and `pkg.child` resolves `greeting` late from a function).
fn deferrable_cycle_import(units: &[LoadedModule], package: usize, unit: usize) -> bool {
    let p = &units[package];
    let dep = units[unit].name.as_str();
    let mut saw = false;
    let mut first_import: Option<usize> = None;
    for (position, stmt) in p.module.body.iter().enumerate() {
        match stmt {
            ast::Stmt::Import(s) => {
                if s.names.iter().any(|alias| alias.name.as_str() == dep) {
                    return false;
                }
            }
            ast::Stmt::ImportFrom(s) => {
                let level = s.level.as_ref().map(|level| level.to_u32()).unwrap_or(0);
                let Ok(module) = lower::resolve_import_name(
                    level,
                    s.module.as_ref().map(|module| module.as_str()),
                    &p.package,
                ) else {
                    continue;
                };
                if module != dep {
                    continue;
                }
                if s.names.iter().any(|alias| alias.name.as_str() == "*") {
                    return false;
                }
                for alias in &s.names {
                    let binding = alias
                        .asname
                        .as_ref()
                        .map(|name| name.as_str())
                        .unwrap_or(alias.name.as_str());
                    if lower::module_references_name(&p.module.body, binding) {
                        return false;
                    }
                }
                first_import.get_or_insert(position);
                saw = true;
            }
            ast::Stmt::FunctionDef(_) | ast::Stmt::AsyncFunctionDef(_) => {}
            other => {
                if compound_imports_module(other, &p.package, dep) {
                    return false;
                }
            }
        }
    }
    if !saw {
        return false;
    }
    // The package must run all of its own definitions before the submodule:
    // every statement after the first import of the submodule must itself be
    // such an import (the `zipfile` tail). A package that defines more names
    // after the import keeps the established submodule-first order, and those
    // definitions must not move into the deferred segment (`pkg/__init__.py`:
    // `from .child import run` followed by `def greeting`).
    let Some(first) = first_import else {
        return false;
    };
    for stmt in &p.module.body[first..] {
        let ast::Stmt::ImportFrom(s) = stmt else {
            return false;
        };
        let level = s.level.as_ref().map(|level| level.to_u32()).unwrap_or(0);
        let Ok(module) = lower::resolve_import_name(
            level,
            s.module.as_ref().map(|module| module.as_str()),
            &p.package,
        ) else {
            return false;
        };
        if module != dep {
            return false;
        }
    }
    true
}

/// Package/submodule cycles whose package must initialize before the
/// submodule: `(package index, submodules)`. See
/// [`deferrable_cycle_import`].
pub(crate) fn reversed_cycle_imports(units: &[LoadedModule]) -> HashMap<usize, Vec<usize>> {
    let by_name: HashMap<&str, usize> = units
        .iter()
        .enumerate()
        .map(|(index, unit)| (unit.name.as_str(), index))
        .collect();
    let mut reversed: HashMap<usize, Vec<usize>> = HashMap::new();
    for (index, unit) in units.iter().enumerate() {
        for late in &unit.late {
            let Some(&package) = by_name.get(late.as_str()) else {
                continue;
            };
            if package == index {
                continue;
            }
            let ancestor = unit.name.len() > late.len()
                && unit.name.starts_with(late.as_str())
                && unit.name.as_bytes().get(late.len()) == Some(&b'.');
            if !ancestor {
                continue;
            }
            if !units[package].deps.contains(&index) {
                continue;
            }
            if !deferrable_cycle_import(units, package, index) {
                continue;
            }
            let entries = reversed.entry(package).or_default();
            if !entries.contains(&index) {
                entries.push(index);
            }
        }
    }
    reversed
}

/// Orders the module bodies so a unit that reads a partially initialized
/// ancestor package initializes after that package.
///
/// CPython runs a package body incrementally: when `pkg/__init__.py` imports
/// `pkg.lib`, the package's already-executed prefix is visible to every module
/// reached from there. Hydro runs a whole module body as one initializer, and
/// the scan normally places every imported unit before its importer, so a
/// helper reached through `pkg.lib` (`numpy._typing._dtype_like`, which reads
/// `numpy.dtype`) would run before `pkg/__init__.py` had published anything.
///
/// A unit whose `late` set names an ancestor package it does not depend on
/// directly is therefore constrained to run after that package. The dependency
/// graph can then contain a cycle (`pkg` -> `lib` -> ... -> helper -> `pkg`);
/// an edge that only binds compiled `def`/`class` handles is soft and may be
/// dropped to break it. A module that imports functions from the delayed
/// subtree still binds them directly, so it can initialize before the subtree
/// body; a value/alias/module-alias import keeps the edge and the cycle falls
/// back to the previous order. The entry unit always stays last.
///
/// A package that imports the submodule only at its end, after its own
/// definitions (`zipfile`), is the mirror image: the submodule's class bases
/// need the package's already-executed prefix, so the package is constrained
/// first and its import edge is soft. `reversed` holds those pairs; the
/// lowerer moves the package's binding stores to an extra initializer segment
/// that runs after the submodule.
pub(crate) fn unit_initializer_order(
    units: &[LoadedModule],
    entry: usize,
    reversed: &HashMap<usize, Vec<usize>>,
) -> Vec<usize> {
    let count = units.len();
    let debug = std::env::var_os("HYDRO_DEBUG_INIT_ORDER").is_some();
    if debug {
        for (index, unit) in units.iter().enumerate() {
            eprintln!(
                "init-unit {index} {} deps={:?} late={:?}",
                unit.name, unit.deps, unit.late
            );
        }
    }
    let identity: Vec<usize> = (0..count).collect();
    let by_name: HashMap<String, usize> = units
        .iter()
        .enumerate()
        .map(|(index, unit)| (unit.name.clone(), index))
        .collect();

    // Constraint edges `package -> unit`: the package body must run first.
    let mut constraints: Vec<(usize, usize)> = Vec::new();
    for (index, unit) in units.iter().enumerate() {
        for late in &unit.late {
            let Some(&package) = by_name.get(late) else {
                continue;
            };
            if package == index || package == entry {
                continue;
            }
            let ancestor = unit.name.len() > late.len()
                && unit.name.starts_with(late.as_str())
                && unit.name.as_bytes().get(late.len()) == Some(&b'.');
            if !ancestor {
                continue;
            }
            // A submodule the package body imports directly is part of the
            // existing "run it before the body" contract; moving it after the
            // body would leave the package's own import reads uninitialized.
            if units[package].deps.contains(&index) {
                continue;
            }
            if !constraints.contains(&(package, index)) {
                constraints.push((package, index));
            }
        }
    }
    // Deferred-cycle packages: the package body runs first, its import of the
    // submodule is published by a later segment, so the dependency edge is
    // relaxed instead of forcing the submodule first.
    for (&package, submodules) in reversed {
        if package == entry {
            continue;
        }
        for &unit in submodules {
            if unit != package && !constraints.contains(&(package, unit)) {
                constraints.push((package, unit));
            }
        }
    }
    if constraints.is_empty() {
        if debug {
            eprintln!("init-constraints: none");
        }
        return identity;
    }
    if debug {
        eprintln!("init-constraints: {constraints:?}");
    }

    let callable = initializer_callable_names(units, &by_name);
    let mut edges: Vec<InitializerEdge> = Vec::new();
    for (importer, unit) in units.iter().enumerate() {
        for &dep in &unit.deps {
            if dep >= count || dep == importer {
                continue;
            }
            edges.push(InitializerEdge {
                dep,
                importer,
                soft: initializer_edge_is_soft(units, &by_name, &callable, importer, dep)
                    || reversed
                        .get(&importer)
                        .is_some_and(|submodules| submodules.contains(&dep)),
                dropped: false,
            });
        }
    }
    // A "package first" constraint is only useful when some soft edge on the
    // cycle can be relaxed. If the late helper can reach its package through
    // hard edges alone (a package and its `_core` subpackage import each other
    // through module aliases), no reordering can satisfy both; keep the
    // dependency-scan order for that helper and drop the constraint before it
    // can move unrelated modules.
    let unplaced = vec![false; count];
    let mut hard_outgoing: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (index, edge) in edges.iter().enumerate() {
        if !edge.soft {
            hard_outgoing[edge.dep].push(index);
        }
    }
    constraints.retain(|&(package, unit)| {
        !initializer_reachable(unit, &hard_outgoing, &edges, &unplaced).contains(&package)
    });
    if debug {
        eprintln!("init-constraints-hard: {constraints:?}");
        for (index, edge) in edges.iter().enumerate() {
            eprintln!(
                "init-edge {index} {} -> {} soft={}",
                units[edge.dep].name, units[edge.importer].name, edge.soft
            );
        }
    }
    if constraints.is_empty() {
        if debug {
            eprintln!("init-constraints: none after hard filter");
        }
        return identity;
    }

    let mut constraint_edges: Vec<usize> = Vec::with_capacity(constraints.len());
    for &(dep, importer) in &constraints {
        if dep >= count || importer >= count || dep == importer {
            continue;
        }
        constraint_edges.push(edges.len());
        edges.push(InitializerEdge {
            dep,
            importer,
            soft: false,
            dropped: false,
        });
    }

    let mut degrees = vec![0usize; count];
    let mut outgoing: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (index, edge) in edges.iter().enumerate() {
        if edge.dropped {
            continue;
        }
        degrees[edge.importer] += 1;
        outgoing[edge.dep].push(index);
    }

    let mut placed = vec![false; count];
    let mut order: Vec<usize> = Vec::with_capacity(count);
    let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> = (0..count)
        .filter(|&index| index != entry && degrees[index] == 0)
        .map(std::cmp::Reverse)
        .collect();
    loop {
        while let Some(std::cmp::Reverse(index)) = ready.pop() {
            if placed[index] || index == entry {
                continue;
            }
            placed[index] = true;
            order.push(index);
            for &edge in &outgoing[index] {
                if edges[edge].dropped {
                    continue;
                }
                let importer = edges[edge].importer;
                if placed[importer] || importer == entry {
                    continue;
                }
                degrees[importer] -= 1;
                if degrees[importer] == 0 {
                    ready.push(std::cmp::Reverse(importer));
                }
            }
        }
        if order.len() + 1 >= count {
            break;
        }
        // A soft edge is droppable only when it lies on a path from a
        // constrained (late) unit back to its ancestor package: dropping an
        // unrelated edge would reorder modules that are not part of this
        // approximation.
        let mut candidate = None;
        'edges: for (index, edge) in edges.iter().enumerate() {
            if edge.dropped || !edge.soft || placed[edge.dep] || placed[edge.importer] {
                continue;
            }
            for &constraint in &constraint_edges {
                let constrained = &edges[constraint];
                if constrained.dropped {
                    continue;
                }
                let (package, unit) = (constrained.dep, constrained.importer);
                if package == entry || unit == entry || placed[package] || placed[unit] {
                    continue;
                }
                let from_unit = initializer_reachable(unit, &outgoing, &edges, &placed);
                if !from_unit.contains(&edge.dep) {
                    continue;
                }
                let from_importer =
                    initializer_reachable(edge.importer, &outgoing, &edges, &placed);
                if from_importer.contains(&package) || edge.importer == package {
                    candidate = Some(index);
                    break 'edges;
                }
            }
        }
        let index = if let Some(index) = candidate {
            index
        } else {
            // No soft boundary exists for a cycle (for example a package and
            // its `_core` subpackage that import each other through module
            // aliases). Give up that particular "package first" constraint
            // instead of blocking the package itself: the directly imported
            // subtree keeps the previous, dependency-scan order.
            let fallback = constraint_edges.iter().copied().find(|&constraint| {
                let constrained = &edges[constraint];
                !constrained.dropped
                    && !placed[constrained.dep]
                    && !placed[constrained.importer]
                    && initializer_reachable(constrained.importer, &outgoing, &edges, &placed)
                        .contains(&constrained.dep)
            });
            let Some(index) = fallback else {
                break;
            };
            index
        };
        edges[index].dropped = true;
        let importer = edges[index].importer;
        degrees[importer] -= 1;
        if degrees[importer] == 0 && importer != entry {
            ready.push(std::cmp::Reverse(importer));
        }
    }
    for (index, is_placed) in placed.iter().enumerate() {
        if index != entry && !*is_placed {
            order.push(index);
        }
    }
    order.push(entry);
    if debug {
        eprintln!(
            "init-order: {:?}",
            order
                .iter()
                .map(|&index| units[index].name.as_str())
                .collect::<Vec<_>>()
        );
        eprintln!(
            "init-dropped: {:?}",
            edges
                .iter()
                .enumerate()
                .filter(|(_, edge)| edge.dropped)
                .map(|(index, edge)| (
                    index,
                    units[edge.dep].name.as_str(),
                    units[edge.importer].name.as_str()
                ))
                .collect::<Vec<_>>()
        );
    }
    order
}

/// Lowers every loaded module in dependency order and links the results.
///
/// The module bodies of imported units become [`Program::initializers`] and run
/// before the entry module. They follow the dependency order, except that a
/// unit that reads a partially initialized ancestor package is placed after
/// that package ([`unit_initializer_order`]). Namespace packages in
/// `namespaces` have no unit: they are exposed as empty user modules and never
/// produce an initializer.
fn lower_modules(
    units: &[LoadedModule],
    namespaces: &[String],
) -> Result<Program, Vec<Diagnostic>> {
    let entry_index = units.len() - 1;
    // Reversed package/submodule cycles and the initializer order they imply.
    // Both are derived from the scan (deps/late/AST), so they are known before
    // any unit is lowered; the lowerer needs the deferred module names.
    let reversed = reversed_cycle_imports(units);
    let initializer_order = unit_initializer_order(units, entry_index, &reversed);
    let unit_by_name: HashMap<String, usize> = units
        .iter()
        .enumerate()
        .map(|(index, unit)| (unit.name.clone(), index))
        .collect();
    let mut all_functions: Vec<hydro_ir::Function> = Vec::new();
    let mut exports: Vec<HashMap<String, lower::UserExport>> = vec![HashMap::new(); units.len()];
    // Whether each unit installs names into its own namespace at runtime;
    // importers of such a module bind unknown members late.
    let mut dynamic_units: Vec<bool> = vec![false; units.len()];
    // PEP 562 hooks each unit defines, so importers bind a missing member to a
    // runtime module-attribute fetch. `dir(module)` reads the `__dir__` global
    // at runtime, so it needs no compile-time metadata.
    let mut getattr_units: Vec<bool> = vec![false; units.len()];
    // Compiled-extension paths each unit star-imports (directly or through a
    // star-imported module); importers inherit them so unknown members bind to
    // lazy extension reads.
    let mut extension_star_units: Vec<Vec<String>> = vec![Vec::new(); units.len()];
    // Module body of each unit, in unit (lowering) order. The initializer list
    // is derived separately because a package body can need a module that must
    // itself run after the package (see `unit_initializer_order`).
    let mut initializer_by_unit: Vec<Option<FunctionId>> = vec![None; units.len()];
    // Extra deferred-import initializer segments of each unit, in source
    // order, with the unit indices each must run after.
    let mut segments_by_unit: Vec<Vec<(FunctionId, Vec<usize>)>> = vec![Vec::new(); units.len()];
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
            initializer_by_unit[index] = Some(base);
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
                    all: effective_module_all(units, &exports, dep_index, &mut HashSet::new()),
                    dynamic_globals: dynamic_units[dep_index],
                    extension_stars: extension_star_units[dep_index].clone(),
                    module_getattr: getattr_units[dep_index],
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
                    dynamic_globals: false,
                    extension_stars: Vec::new(),
                    module_getattr: false,
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
        // Modules whose body the loader ordered after this unit (a reversed
        // package/submodule cycle): their module-level import bindings move to
        // extra initializer segments.
        let deferred_initializer_modules: HashSet<String> = reversed
            .get(&index)
            .map(|submodules| {
                submodules
                    .iter()
                    .map(|&dep| units[dep].name.clone())
                    .collect()
            })
            .unwrap_or_default();
        lowerer.set_initializer_deferred_modules(deferred_initializer_modules);

        let mut hir = lowerer.lower_module(&unit.module);
        let segments = lowerer.take_module_segments();
        let exported = lowerer.exported_names();
        let dynamic_globals = lowerer.exports_dynamic_globals();
        let module_getattr = lowerer.exports_module_getattr();
        let extension_stars = lowerer.exported_extension_stars();
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
        segments_by_unit[index] = segments
            .into_iter()
            .map(|(id, module)| {
                let deps = unit_by_name.get(&module).copied().into_iter().collect();
                (id, deps)
            })
            .collect();

        exports[index] = exported;
        dynamic_units[index] = dynamic_globals;
        getattr_units[index] = module_getattr;
        extension_star_units[index] = extension_stars;
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
        let mut combined: Vec<HFunction> = all_functions
            .iter()
            .map(|function| placeholder_function(function, &mut infer))
            .collect();
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

    let initializers = build_initializers(
        units,
        entry_index,
        &initializer_order,
        &initializer_by_unit,
        &segments_by_unit,
        &reversed,
        0,
    );

    Ok(Program {
        source_name: units[entry_index].display.clone(),
        functions: all_functions,
        main: entry_main,
        initializers,
    })
}

/// Whether `index` may initialize now: every dependency body and every
/// deferred import segment of its dependencies already ran.
///
/// A reversed-cycle dependency is exempt from the body check (it is ordered
/// later on purpose), but its own segments are not: the importer must wait
/// until the dependency's binding segments completed.
fn initializer_unit_ready(
    units: &[LoadedModule],
    reversed: &HashMap<usize, Vec<usize>>,
    placed: &[bool],
    pushed: &[Vec<bool>],
    index: usize,
) -> bool {
    for &dep in &units[index].deps {
        if dep == index || dep >= units.len() {
            continue;
        }
        let relaxed = reversed
            .get(&index)
            .is_some_and(|submodules| submodules.contains(&dep));
        if !relaxed && !placed[dep] {
            return false;
        }
        if pushed
            .get(dep)
            .is_some_and(|segments| segments.iter().any(|&done| !done))
        {
            return false;
        }
    }
    true
}

/// Pushes every deferred import segment whose unit and dependencies already
/// initialized, in source order per unit.
fn push_initializer_segments(
    segments: &[Vec<(FunctionId, Vec<usize>)>],
    placed: &[bool],
    pushed: &mut [Vec<bool>],
    initializers: &mut Vec<FunctionId>,
) {
    loop {
        let mut progress = false;
        for unit in 0..segments.len() {
            for segment in 0..segments[unit].len() {
                if pushed[unit][segment] {
                    continue;
                }
                if !placed[unit] {
                    break;
                }
                if segment > 0 && !pushed[unit][segment - 1] {
                    break;
                }
                let (id, deps) = &segments[unit][segment];
                let ready = deps.iter().all(|&dep| {
                    placed.get(dep).copied().unwrap_or(false)
                        && pushed
                            .get(dep)
                            .is_none_or(|done| done.iter().all(|&done| done))
                });
                if !ready {
                    break;
                }
                initializers.push(*id);
                pushed[unit][segment] = true;
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
}

/// Interleaves module bodies and deferred import segments.
///
/// Without a reversed cycle the list is exactly the units in
/// [`unit_initializer_order`] order, which keeps the established behaviour.
/// Otherwise a unit's extra segments run as soon as every module they import
/// initialized; an importer of a reversed-cycle package waits for those
/// segments, exactly like CPython waits for the package body to finish its
/// last import statement before another module sees it.
pub(crate) fn build_initializers(
    units: &[LoadedModule],
    entry: usize,
    order: &[usize],
    mains: &[Option<FunctionId>],
    segments: &[Vec<(FunctionId, Vec<usize>)>],
    reversed: &HashMap<usize, Vec<usize>>,
    initialized: usize,
) -> Vec<FunctionId> {
    if reversed.is_empty() {
        return order
            .iter()
            .copied()
            .filter(|&index| index >= initialized)
            .filter_map(|index| mains.get(index).copied().flatten())
            .collect();
    }
    let count = units.len();
    let initialized = initialized.min(count);
    let mut placed = vec![false; count];
    let mut emitted = vec![false; count];
    // A session fragment reuses the units of earlier fragments: they already
    // ran and must not be emitted again.
    for index in 0..initialized {
        placed[index] = true;
        emitted[index] = true;
    }
    let mut pushed: Vec<Vec<bool>> = segments
        .iter()
        .map(|list| vec![false; list.len()])
        .collect();
    let mut initializers = Vec::new();
    let mut pending: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&index| index != entry && index >= initialized)
        .collect();
    loop {
        let mut progress = false;
        let mut next = Vec::new();
        for index in pending {
            if emitted[index] {
                continue;
            }
            if !initializer_unit_ready(units, reversed, &placed, &pushed, index) {
                next.push(index);
                continue;
            }
            if let Some(id) = mains.get(index).copied().flatten() {
                initializers.push(id);
            }
            placed[index] = true;
            emitted[index] = true;
            push_initializer_segments(segments, &placed, &mut pushed, &mut initializers);
            progress = true;
        }
        pending = next;
        if pending.is_empty() || !progress {
            break;
        }
    }
    // A cycle that the constraints could not satisfy keeps the loader order;
    // emit whatever is left, then any remaining segment.
    for index in pending {
        if !emitted[index] {
            if let Some(id) = mains.get(index).copied().flatten() {
                initializers.push(id);
            }
            placed[index] = true;
            emitted[index] = true;
        }
    }
    push_initializer_segments(segments, &placed, &mut pushed, &mut initializers);
    // Defensive: a segment whose unit was skipped above still runs at the end.
    for unit in 0..segments.len() {
        for segment in 0..segments[unit].len() {
            if !pushed[unit][segment] {
                initializers.push(segments[unit][segment].0);
                pushed[unit][segment] = true;
            }
        }
    }
    initializers
}

/// Placeholder HIR for an already emitted dependency function: signature only,
/// empty body and module flag set so return checks are skipped.
///
/// An unannotated parameter was emitted with its inferred (or widened) type,
/// but it has no source contract: the placeholder gives the slot fresh
/// gradual provenance so an importer's call site can still widen a conflict.
/// Variadic parameters keep their list/dict container type.
fn placeholder_function(function: &hydro_ir::Function, inf: &mut Infer) -> HFunction {
    HFunction {
        name: function.name.clone(),
        params: function
            .params
            .iter()
            .map(|param| {
                let ty = if param.declared
                    || matches!(param.kind, ParamKind::VarArg | ParamKind::KwArg)
                {
                    Ty::Concrete(param.ty)
                } else {
                    let ty = inf.fresh();
                    inf.mark_gradual(ty);
                    ty
                };
                HParam {
                    local: param.local,
                    name: param.name.clone(),
                    ty,
                    declared: param.declared,
                    default: param.default.clone(),
                    default_expr: None,
                    kind: param.kind,
                }
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
    // HYDRO_STDLIB extension directories
    // ------------------------------------------------------------------

    #[test]
    fn stdlib_extension_dirs_follow_the_stdlib_layout() {
        let dir = TestDir::new("stdlib-extension-dirs");
        let stdlib = dir.path.join("Lib");
        let nested = stdlib.join("lib-dynload");
        let sibling = dir.path.join("lib-dynload");
        fs::create_dir_all(&nested).expect("create nested lib-dynload");
        fs::create_dir_all(&sibling).expect("create sibling lib-dynload");

        // `HYDRO_STDLIB` may be a `Lib` tree or a version root; both layouts
        // are tried, `<stdlib>/lib-dynload` first.
        assert_eq!(
            stdlib_extension_dirs(Some(stdlib.as_os_str())),
            vec![nested.clone(), sibling.clone()]
        );

        // A root without extension modules contributes nothing, and an unset
        // variable derives no directory at all. The bare root gets its own
        // parent so the shared sibling `lib-dynload` above is not found.
        let bare = dir.path.join("bare-parent").join("bare");
        fs::create_dir_all(&bare).expect("create bare root");
        assert!(stdlib_extension_dirs(Some(bare.as_os_str())).is_empty());
        assert!(stdlib_extension_dirs(None).is_empty());

        // `HYDRO_STDLIB` is path-split, so a repeated root keeps each derived
        // directory once.
        let repeated =
            std::env::join_paths([stdlib.as_path(), stdlib.as_path()]).expect("join stdlib roots");
        assert_eq!(
            stdlib_extension_dirs(Some(repeated.as_os_str())),
            vec![nested, sibling]
        );
    }

    #[test]
    fn module_search_roots_place_lib_dynload_after_the_sysroot() {
        let dir = TestDir::new("roots-lib-dynload");
        let stdlib = dir.path.join("Lib");
        let nested = stdlib.join("lib-dynload");
        let sibling = dir.path.join("lib-dynload");
        fs::create_dir_all(&nested).expect("create nested lib-dynload");
        fs::create_dir_all(&sibling).expect("create sibling lib-dynload");
        let importer = dir.path.join("project");
        fs::create_dir_all(&importer).expect("create importer root");

        let roots = module_search_roots_from(
            &importer,
            &importer,
            None,
            None,
            Some(stdlib.as_os_str()),
            None,
            None,
            None,
        );
        assert_eq!(roots, vec![importer, stdlib, nested, sibling]);
    }

    #[test]
    fn push_stdlib_extension_dirs_keeps_existing_entries_once() {
        let dir = TestDir::new("push-lib-dynload");
        let stdlib = dir.path.join("Lib");
        let nested = stdlib.join("lib-dynload");
        let sibling = dir.path.join("lib-dynload");
        fs::create_dir_all(&nested).expect("create nested lib-dynload");
        fs::create_dir_all(&sibling).expect("create sibling lib-dynload");

        // An explicit search root that already names a derived directory (a
        // `PYTHONPATH` entry, for example) is not repeated.
        let mut roots = vec![sibling.clone()];
        push_stdlib_extension_dirs(&mut roots, Some(stdlib.as_os_str()));
        assert_eq!(roots, vec![sibling, nested]);
    }

    #[test]
    fn hydro_stdlib_lib_dynload_makes_a_standard_extension_linkable() {
        // Acceptance shape: only `HYDRO_STDLIB` is set, with no `PYTHONPATH`
        // entry naming the extension directory. The import scan and the
        // lowerer resolve shared objects through the same extension search
        // path, so a stub artifact is enough: the frontend only needs its
        // path, loading happens at runtime.
        let _env = env_lock();
        let dir = TestDir::new("stdlib-lib-dynload-import");
        let stdlib = dir.path.join("python3.14");
        let dynload = stdlib.join("lib-dynload");
        fs::create_dir_all(&dynload).expect("create lib-dynload");
        fs::write(dynload.join("_elementtree.abi3.so"), b"").expect("write extension stub");
        let main = dir.write("main.py", "import _elementtree\n");

        let _stdlib = EnvVar::set("HYDRO_STDLIB", stdlib.as_os_str());
        let _pythonpath = EnvVar::unset("PYTHONPATH");
        let _site = EnvVar::unset("HYDRO_SITE_PACKAGES");
        let _extensions = EnvVar::unset("HYDRO_EXTENSION_PATH");

        let program =
            check_file(&main).expect("the extension must resolve from HYDRO_STDLIB alone");
        assert!(
            !program.functions.is_empty(),
            "the entry module must compile"
        );
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

    /// Names of the compiled module bodies in initializer order.
    fn initializer_names(program: &Program) -> Vec<&str> {
        program
            .initializers
            .iter()
            .map(|id| program.functions[*id as usize].name.as_str())
            .collect()
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
            // The keyword-capable call path wraps the same function value.
            ExprKind::CallKw { callee, .. } => {
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
    fn imports_in_module_level_branches_are_linked() {
        // NumPy imports its `_core` package from `if __NUMPY_SETUP__: ...
        // else:`. The branch runs while the module initializes, so the package
        // is a real dependency whose initializer runs before the importer;
        // dropping it produced `ModuleNotFoundError: No module named
        // 'numpy._core'` at runtime. A module absent on this host (a platform
        // branch) stays a deferred runtime import instead of a compile error.
        let dir = TestDir::new("conditional-import");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "SETUP = False\nif SETUP:\n    from . import absent\nelse:\n    from . import core\n    from .core import value\n\ndef exported():\n    return value()\n",
        )
        .expect("write package init");
        fs::write(pkg.join("core.py"), "def value():\n    return 7\n").expect("write core");
        let main = dir.write("main.py", "import pkg\nprint(pkg.exported())\n");
        let program = program(&main);

        // The `else` branch executes at import time: `core` is compiled once
        // and initialized before the package body that imports it.
        let initializer_names: Vec<&str> = program
            .initializers
            .iter()
            .map(|id| program.functions[*id as usize].name.as_str())
            .collect();
        assert_eq!(
            initializer_names,
            ["pkg.core.<module>", "pkg.<module>"],
            "the conditional dependency initializes before its importer"
        );

        // `pkg.exported` calls the linked `core.value` directly.
        let exported = function_id(&program, "pkg.exported");
        assert!(
            calls_function(
                returned_expression(&program, exported),
                function_id(&program, "pkg.core.value"),
            ),
            "the else branch must link core.value: {:?}",
            returned_expression(&program, exported).kind
        );

        // The absent branch module never becomes a unit.
        assert!(
            program
                .functions
                .iter()
                .all(|function| !function.name.contains("absent")),
            "a deferred branch module must not be compiled"
        );
    }

    #[test]
    fn star_import_propagates_dynamic_namespace_metadata_between_units() {
        // `dynamicmod` appends nothing to its literal `__all__` but installs
        // names with `globals()[...] = ...`. `middle` star-imports it and
        // `consumer` star-imports `middle`, so the dynamic metadata must
        // propagate across both hops; `consumer` then lowers an unresolved
        // star-imported name to a read of its own global table instead of a
        // compile error.
        let dir = TestDir::new("dynamic-star-metadata");
        dir.write(
            "dynamicmod.py",
            "namespace = globals()\nnamespace['INJECTED'] = 1\n__all__ = []\n",
        );
        dir.write("middle.py", "from dynamicmod import *\n");
        dir.write(
            "consumer.py",
            "from middle import *\n\ndef read():\n    return INJECTED\n",
        );
        let main = dir.write("main.py", "import consumer\nprint(consumer.read())\n");
        let program = program(&main);

        use hydro_ir::ExprKind;
        let read = function_id(&program, "consumer.read");
        assert!(
            matches!(
                &returned_expression(&program, read).kind,
                ExprKind::GlobalGet { module, name }
                    if module == "consumer" && name == "INJECTED"
            ),
            "expected the importer's dynamic global read, got {:?}",
            returned_expression(&program, read).kind
        );
    }

    #[test]
    fn star_imported_extension_metadata_reaches_importers_between_units() {
        // `umath.py` is `from ._multiarray_umath import *`; the extension
        // path is not in its static export table. `numeric.py` imports a
        // missing umath member directly and star-imports umath; `main.py`
        // then imports `NAN` from numeric, which numeric only has through the
        // inherited extension star. Every hop requires the lowerer metadata to
        // travel through the loader-sized UserModule table (a missing fallback
        // is a compile diagnostic).
        let dir = TestDir::new("extension-star-metadata");
        dir.add_extension("_multiarray_umath.abi3.so");
        dir.write("umath.py", "from _multiarray_umath import *\n");
        dir.write(
            "numeric.py",
            "from umath import PINF\nfrom umath import *\nvalue = NAN\n",
        );
        let main = dir.write("main.py", "from numeric import NAN\nprint(NAN)\n");
        let program = program(&main);
        assert!(
            program
                .functions
                .iter()
                .any(|function| function.name == "numeric.<module>"),
            "numeric must compile against the propagated extension namespace"
        );
    }

    #[test]
    fn circular_imports_bind_late_and_initialize_each_unit_once() {
        // `a` imports `b` eagerly and `b` imports back from `a`. CPython
        // exposes the partially initialized `a` to `b`; Hydro records that
        // back edge as late binding, compiles each file once and runs exactly
        // one initializer per module, `b` before `a` because `b` finishes
        // loading first.
        let dir = TestDir::new("cycle");
        dir.write("a.py", "from b import g\n\ndef f():\n    return g()\n");
        dir.write("b.py", "from a import f\n\ndef g():\n    return f()\n");
        let main = dir.write("main.py", "import a\nprint(a.f())\n");
        let program = program(&main);

        // Each unit is compiled exactly once.
        for name in ["a.f", "b.g"] {
            assert_eq!(
                program
                    .functions
                    .iter()
                    .filter(|function| function.name == name)
                    .count(),
                1,
                "{name} must be compiled once"
            );
        }
        let initializer_names: Vec<&str> = program
            .initializers
            .iter()
            .map(|id| program.functions[*id as usize].name.as_str())
            .collect();
        assert_eq!(
            initializer_names,
            ["b.<module>", "a.<module>"],
            "one initializer per module, dependency side first"
        );

        // `b` reads the still-initializing `a.f` through the global table.
        use hydro_ir::ExprKind;
        let g = function_id(&program, "b.g");
        let (ExprKind::IndirectCall { callee, .. } | ExprKind::CallKw { callee, .. }) =
            &returned_expression(&program, g).kind
        else {
            panic!(
                "b.g must call the late-bound a.f, got {:?}",
                returned_expression(&program, g).kind
            );
        };
        assert!(
            matches!(
                &callee.kind,
                ExprKind::GlobalGet { module, name } if module == "a" && name == "f"
            ),
            "the cycle back edge must read a.f late: {:?}",
            callee.kind
        );

        // The eager edge `a` -> `b` keeps its direct function link.
        let f = function_id(&program, "a.f");
        let g_id = function_id(&program, "b.g");
        assert!(
            calls_function(returned_expression(&program, f), g_id),
            "a.f must call b.g directly"
        );
    }

    #[test]
    fn star_reexports_extend_the_effective_all() {
        // `base` defines `value`; `middle` star-imports it and declares a
        // literal `__all__` of its own. NumPy extends `__all__` at runtime
        // with `extend_all(...)`; the loader must mirror that union, or every
        // re-exported name disappears from the importer's namespace.
        let dir = TestDir::new("effective-all");
        dir.write(
            "base.py",
            "__all__ = ['value']\n\ndef value():\n    return 7\n",
        );
        dir.write(
            "middle.py",
            "from base import *\n\n__all__ = ['local']\n\ndef local():\n    return 1\n",
        );
        let main = dir.write("main.py", "from middle import *\nvalue()\nlocal()\n");
        let program = program(&main);

        let value = function_id(&program, "base.value");
        assert!(
            program.main_function().body.iter().any(|stmt| {
                matches!(
                    &stmt.kind,
                    hydro_ir::StmtKind::Expr(expr) if calls_function(expr, value)
                )
            }),
            "middle must re-export base.value through its effective __all__"
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

    /// Value stored into the runtime global table by the initializer of
    /// `module` for `name`.
    fn initializer_global(program: &Program, module: &str, name: &str) -> hydro_ir::Expr {
        use hydro_ir::StmtKind;
        let function = function_id(program, &format!("{module}.<module>"));
        program.functions[function as usize]
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::GlobalSet {
                    name: stored,
                    value,
                    ..
                } if stored == name => Some(value.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{module} must publish the `{name}` global"))
    }

    /// Follows a mirrored global's value when it merely reads the local that
    /// produced it, returning the expression assigned to that local.
    fn resolved_initializer(
        program: &Program,
        module: &str,
        expr: hydro_ir::Expr,
    ) -> hydro_ir::Expr {
        use hydro_ir::{ExprKind, StmtKind};
        let local = match &expr.kind {
            ExprKind::Local(local) => Some(*local),
            _ => None,
        };
        let Some(local) = local else {
            return expr;
        };
        let function = function_id(program, &format!("{module}.<module>"));
        program.functions[function as usize]
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Assign { target, value } if *target == local => Some(value.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{module} must assign its mirrored local {local:?}"))
    }

    /// The `(name, origin, is_package)` arguments of a unit's `__spec__`
    /// binding.
    fn module_spec_arguments(program: &Program, module: &str) -> (String, String, bool) {
        use hydro_ir::{Const, ExprKind};
        let spec = resolved_initializer(
            program,
            module,
            initializer_global(program, module, "__spec__"),
        );
        let ExprKind::ModuleCall {
            module: native,
            function,
            args,
            ..
        } = &spec.kind
        else {
            panic!(
                "{module}.__spec__ must be built by ModuleSpec machinery: {:?}",
                spec.kind
            );
        };
        assert_eq!(native, "importlib.machinery");
        assert_eq!(function, "_module_spec");
        let constant = |index: usize| match &args[index].kind {
            ExprKind::Const(value) => value,
            other => panic!("spec argument {index} must be constant: {other:?}"),
        };
        let name = match constant(0) {
            Const::Str(name) => name.clone(),
            other => panic!("spec name must be a str: {other:?}"),
        };
        let origin = match constant(1) {
            Const::Str(origin) => origin.clone(),
            other => panic!("spec origin must be a str: {other:?}"),
        };
        let is_package = match constant(2) {
            Const::Bool(value) => *value,
            other => panic!("spec package flag must be a bool: {other:?}"),
        };
        (name, origin, is_package)
    }

    /// Asserts `module.__loader__` is read from the same `__spec__` object, so
    /// the CPython identity `__loader__ is __spec__.loader` holds.
    fn assert_loader_is_spec_loader(program: &Program, module: &str) {
        use hydro_ir::ExprKind;
        let loader = resolved_initializer(
            program,
            module,
            initializer_global(program, module, "__loader__"),
        );
        let ExprKind::GetAttr { object, name } = &loader.kind else {
            panic!(
                "{module}.__loader__ must come from the spec: {:?}",
                loader.kind
            );
        };
        assert_eq!(name, "loader");
        let ExprKind::Local(local) = &object.kind else {
            panic!(
                "__loader__ must read the `__spec__` local: {:?}",
                object.kind
            );
        };
        let function = function_id(program, &format!("{module}.<module>"));
        assert_eq!(
            program.functions[function as usize].local(*local).name,
            "__spec__"
        );
    }

    #[test]
    fn package_init_publishes_spec_metadata() {
        let dir = TestDir::new("spec-package");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(pkg.join("__init__.py"), "VALUE = 1\n").expect("write package init");
        let main = dir.write("main.py", "import pkg\nprint(pkg.VALUE)\n");
        let program = program(&main);

        let (name, origin, is_package) = module_spec_arguments(&program, "pkg");
        assert_eq!(name, "pkg");
        assert!(
            Path::new(&origin).ends_with(Path::new("pkg").join("__init__.py")),
            "unexpected origin: {origin}"
        );
        assert!(is_package, "a package __init__.py is its own package");

        // `__package__` is the spec's parent (the package itself) and
        // `__file__` is the spec's origin.
        let package = resolved_initializer(
            &program,
            "pkg",
            initializer_global(&program, "pkg", "__package__"),
        );
        assert!(
            matches!(
                &package.kind,
                hydro_ir::ExprKind::Const(hydro_ir::Const::Str(value)) if value.as_str() == "pkg"
            ),
            "{:?}",
            package.kind
        );
        let file = resolved_initializer(
            &program,
            "pkg",
            initializer_global(&program, "pkg", "__file__"),
        );
        assert!(
            matches!(
                &file.kind,
                hydro_ir::ExprKind::Const(hydro_ir::Const::Str(value)) if value == &origin
            ),
            "{:?}",
            file.kind
        );
        assert_loader_is_spec_loader(&program, "pkg");
    }

    #[test]
    fn submodule_publishes_parent_package_metadata() {
        let dir = TestDir::new("spec-submodule");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(pkg.join("__init__.py"), "").expect("write package init");
        fs::write(pkg.join("mod.py"), "VALUE = 2\n").expect("write submodule");
        let main = dir.write("main.py", "import pkg.mod\nprint(pkg.mod.VALUE)\n");
        let program = program(&main);

        let (name, origin, is_package) = module_spec_arguments(&program, "pkg.mod");
        assert_eq!(name, "pkg.mod");
        assert!(
            Path::new(&origin).ends_with(Path::new("pkg").join("mod.py")),
            "unexpected origin: {origin}"
        );
        assert!(!is_package, "a plain module is not a package");

        let package = resolved_initializer(
            &program,
            "pkg.mod",
            initializer_global(&program, "pkg.mod", "__package__"),
        );
        assert!(
            matches!(
                &package.kind,
                hydro_ir::ExprKind::Const(hydro_ir::Const::Str(value)) if value.as_str() == "pkg"
            ),
            "{:?}",
            package.kind
        );
        assert_loader_is_spec_loader(&program, "pkg.mod");
    }

    #[test]
    fn top_level_module_publishes_empty_package() {
        let dir = TestDir::new("spec-top-level");
        dir.write("top.py", "VALUE = 3\n");
        let main = dir.write("main.py", "import top\nprint(top.VALUE)\n");
        let program = program(&main);

        let (name, origin, is_package) = module_spec_arguments(&program, "top");
        assert_eq!(name, "top");
        assert!(
            Path::new(&origin).ends_with("top.py"),
            "unexpected origin: {origin}"
        );
        assert!(!is_package, "a top-level module is not a package");

        // CPython gives a top-level module an empty `__package__` (its
        // `ModuleSpec.parent` is the empty string).
        let package = resolved_initializer(
            &program,
            "top",
            initializer_global(&program, "top", "__package__"),
        );
        assert!(
            matches!(
                &package.kind,
                hydro_ir::ExprKind::Const(hydro_ir::Const::Str(value)) if value.is_empty()
            ),
            "{:?}",
            package.kind
        );
        assert_loader_is_spec_loader(&program, "top");
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
    fn entry_module_can_be_imported_by_a_dependency() {
        // `rlcompleter` imports `__main__`; the entry module is a runtime
        // value, so the loader must not look for a `__main__.py` on the
        // import search path.
        let dir = TestDir::new("entry-module-import");
        dir.write(
            "helper.py",
            "import __main__\nnamespace = __main__.__dict__\nfrom __main__ import value\n",
        );
        let main = dir.write("main.py", "import helper\nvalue = 1\n");
        let program = check_file(&main).expect("__main__ is a known runtime module");
        let names: Vec<&str> = program
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        assert!(names.contains(&"helper.<module>"), "{names:?}");
    }

    #[test]
    fn bundled_stdlib_resolves_enum_without_hydro_stdlib() {
        let _env = env_lock();
        let _stdlib = EnvVar::unset("HYDRO_STDLIB");
        let _pythonpath = EnvVar::unset("PYTHONPATH");
        let bundled = bundled_stdlib_root().expect("the vendored stdlib root is discoverable");
        assert!(
            bundled.join("enum.py").is_file(),
            "enum.py must be vendored in {bundled:?}"
        );

        let dir = TestDir::new("bundled-enum-resolver");
        let project = dir.path.join("project");
        fs::create_dir_all(&project).expect("create project");
        let roots = module_search_roots_from(
            &project,
            &project,
            None,
            None,
            None,
            None,
            None,
            Some(bundled),
        );
        assert_eq!(roots.last().map(PathBuf::as_path), Some(bundled));
        let chain = resolve_module(&roots, "enum").expect("enum must resolve");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].path, bundled.join("enum.py"));
        assert!(!chain[0].namespace, "enum must be a real module");
    }

    #[test]
    fn vendored_stdlib_wins_over_hydro_stdlib_inside_the_sysroot() {
        // A standard-library module that imports `enum` lives in `HYDRO_STDLIB`
        // itself, so its own directory is the sysroot: without an explicit
        // ordering rule it resolves `Lib/enum.py` and bypasses the vendored
        // adaptation. The vendored tree must come first for those importers
        // too, while a module Hydro does not vendor still falls through to
        // `HYDRO_STDLIB`, and user/project roots keep their precedence.
        let _env = env_lock();
        let dir = TestDir::new("vendored-precedence");
        let lib = dir.path.join("Lib");
        let project = dir.path.join("project");
        let support = lib.join("test").join("support");
        fs::create_dir_all(&support).expect("create test.support");
        fs::create_dir_all(&project).expect("create project");
        fs::write(lib.join("test").join("__init__.py"), "").expect("write test init");
        // A sysroot `enum` that cannot compile: choosing it over the vendored
        // module fails the whole check, so the entry program is the assertion.
        fs::write(
            lib.join("enum.py"),
            "import definitely_missing_sysroot_module\n",
        )
        .expect("write decoy enum");
        // Not vendored by Hydro: it must still resolve from the sysroot.
        fs::write(lib.join("copy.py"), "VALUE = 7\n").expect("write sysroot copy");
        fs::write(
            support.join("__init__.py"),
            "import copy\nimport enum\n\nMARK = copy.VALUE + len(enum.__all__)\n",
        )
        .expect("write support init");
        let main = project.join("main.py");
        fs::write(&main, "import test.support\nprint(test.support.MARK)\n").expect("write entry");

        let bundled = bundled_stdlib_root().expect("the vendored stdlib root is discoverable");
        let roots = module_search_roots_from(
            &support,
            &lib,
            None,
            None,
            Some(lib.as_os_str()),
            None,
            None,
            Some(bundled),
        );
        assert_eq!(
            roots,
            vec![bundled.to_path_buf(), support.clone(), lib.clone()],
            "a sysroot importer must see the vendored overrides first"
        );
        let chain = resolve_module(&roots, "enum").expect("enum must resolve");
        assert_eq!(chain[0].path, bundled.join("enum.py"));
        let chain = resolve_module(&roots, "copy").expect("copy is supplied by HYDRO_STDLIB");
        assert_eq!(chain[0].path, lib.join("copy.py"));

        // A root outside the sysroot keeps its precedence over both.
        let outside = module_search_roots_from(
            &project,
            &project,
            None,
            None,
            Some(lib.as_os_str()),
            None,
            None,
            Some(bundled),
        );
        assert_eq!(
            outside,
            vec![project.clone(), bundled.to_path_buf(), lib.clone()],
            "project modules still win over the standard library"
        );

        let _stdlib = EnvVar::set("HYDRO_STDLIB", lib.as_os_str());
        let _pythonpath = EnvVar::unset("PYTHONPATH");
        let program = program(&main);
        let names = initializer_names(&program);
        assert!(names.contains(&"enum.<module>"), "{names:?}");
        assert!(names.contains(&"copy.<module>"), "{names:?}");
        assert!(names.contains(&"test.support.<module>"), "{names:?}");
    }

    #[test]
    fn bundled_enum_compiles_a_subclass_and_member_access() {
        let _env = env_lock();
        let _stdlib = EnvVar::unset("HYDRO_STDLIB");
        let _pythonpath = EnvVar::unset("PYTHONPATH");
        let dir = TestDir::new("bundled-enum-compile");
        let main = dir.write(
            "main.py",
            "from enum import Enum, auto\n\
             \n\
             class DisplayModes(Enum):\n    stdout = auto()\n    stderr = auto()\n\
             \n\
             def mode():\n    return DisplayModes.stdout\n\
             \n\
             RESULT = mode()\n",
        );
        let program = program(&main);

        // The vendored module body is linked as an initializer, so `Enum`,
        // `auto` and the metaclass machinery exist at runtime.
        assert_eq!(program.initializers.len(), 1, "{program:?}");
        let initializer = program.functions[program.initializers[0] as usize]
            .name
            .as_str();
        assert_eq!(initializer, "enum.<module>");

        use hydro_ir::{ExprKind, StmtKind};
        let mode = program.function(function_id(&program, "mode"));
        let returned = mode
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Return(Some(expr)) => Some(expr),
                _ => None,
            })
            .expect("mode must return the member");
        assert!(
            matches!(&returned.kind, ExprKind::GetAttr { name, .. } if name == "stdout"),
            "member access must stay a class attribute read: {returned:?}"
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
    fn pep562_getattr_module_resolves_missing_members_at_runtime() {
        // Pydantic 2.x exposes `BaseModel` through a `_dynamic_imports` map
        // behind module `__getattr__`; the static export table cannot see it.
        let dir = TestDir::new("pep562-dynamic");
        dir.write(
            "dynamic.py",
            r#"_dynamic_imports = {'BaseModel': 'dynamic_models'}

def __getattr__(name):
    if name not in _dynamic_imports:
        raise AttributeError(f"module 'dynamic' has no attribute {name!r}")

    from importlib import import_module
    return import_module(_dynamic_imports[name])
"#,
        );
        dir.write("dynamic_models.py", "class BaseModel:\n    pass\n");
        let main = dir.write(
            "main.py",
            "from dynamic import BaseModel\nvalue = BaseModel\n",
        );
        let program = program(&main);

        use hydro_ir::{ExprKind, StmtKind};
        let entry = program.main_function();
        let value = entry
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Assign { target, value } if entry.local(*target).name == "value" => {
                    Some(value)
                }
                _ => None,
            })
            .expect("entry assigns value");
        assert!(
            matches!(
                &value.kind,
                ExprKind::ModuleAttr { module, name }
                    if module == "dynamic" && name == "BaseModel"
            ),
            "the missing member must be read from the module value, got {:?}",
            value.kind
        );
    }

    #[test]
    fn pep562_getattr_nested_relative_import_loads_the_submodule() {
        // `__getattr__` implementations import their helper lazily
        // (`from . import _impl`). A declaration-only site-packages submodule
        // is preloaded, so the hook body can link the submodule by name.
        let _env = env_lock();
        let dir = TestDir::new("pep562-nested");
        let site = dir.path.join("site-packages");
        fs::create_dir_all(site.join("pkg")).expect("create package");
        fs::write(
            site.join("pkg").join("__init__.py"),
            "\
def __getattr__(name):
    from . import _impl
    return getattr(_impl, name)
",
        )
        .expect("write package init");
        fs::write(
            site.join("pkg").join("_impl.py"),
            "class BaseModel:\n    pass\n",
        )
        .expect("write submodule");
        let main = dir.write("main.py", "from pkg import BaseModel\nvalue = BaseModel\n");

        let _guard = EnvVar::set("HYDRO_SITE_PACKAGES", site.as_os_str());
        let program = program(&main);
        assert!(
            program
                .functions
                .iter()
                .any(|function| function.name == "pkg._impl.<module>"),
            "the nested relative import must load `pkg._impl`"
        );

        use hydro_ir::{ExprKind, StmtKind};
        let entry = program.main_function();
        let value = entry
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Assign { target, value } if entry.local(*target).name == "value" => {
                    Some(value)
                }
                _ => None,
            })
            .expect("entry assigns value");
        assert!(
            matches!(
                &value.kind,
                ExprKind::ModuleAttr { module, name } if module == "pkg" && name == "BaseModel"
            ),
            "the missing member must be read from the package value, got {:?}",
            value.kind
        );
    }

    #[test]
    fn dynamic_import_dispatch_compiles_relative_submodules() {
        // pydantic 2.x exposes its public API through PEP 562 `__getattr__`
        // and a `_dynamic_imports` table of relative module names. No `import`
        // statement names `.main`, so the loader itself must compile it for the
        // runtime `importlib.import_module` call to find the module.
        let dir = TestDir::new("dynamic-dispatch");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            r#"from importlib import import_module

_dynamic_imports = {'Thing': (__spec__.parent, '.main')}

def __getattr__(name):
    if name not in _dynamic_imports:
        raise AttributeError(name)
    package, module_name = _dynamic_imports[name]
    module = import_module(module_name, package=package)
    return getattr(module, name)
"#,
        )
        .expect("write package init");
        fs::write(pkg.join("main.py"), "class Thing:\n    pass\n").expect("write submodule");
        let main = dir.write("main.py", "from pkg import Thing\nvalue = Thing\n");
        let program = program(&main);

        let initializers = initializer_names(&program);
        assert!(
            initializers.contains(&"pkg.main.<module>"),
            "the dispatched submodule must be compiled: {initializers:?}"
        );
        // The package body runs before the module it dispatches to, exactly
        // like `import_module` at attribute-access time in CPython.
        let package_at = initializers
            .iter()
            .position(|name| *name == "pkg.<module>")
            .expect("the package must initialize");
        let submodule_at = initializers
            .iter()
            .position(|name| *name == "pkg.main.<module>")
            .expect("the submodule must initialize");
        assert!(
            package_at < submodule_at,
            "the package must initialize first: {initializers:?}"
        );

        // `Thing` stays a runtime attribute read from the package value, so
        // the PEP 562 hook resolves it when the program reads it.
        use hydro_ir::{ExprKind, StmtKind};
        let entry = program.main_function();
        let value = entry
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Assign { target, value } if entry.local(*target).name == "value" => {
                    Some(value)
                }
                _ => None,
            })
            .expect("entry assigns value");
        assert!(
            matches!(
                &value.kind,
                ExprKind::ModuleAttr { module, name } if module == "pkg" && name == "Thing"
            ),
            "the missing member must be read from the package value, got {:?}",
            value.kind
        );
    }

    #[test]
    fn dynamic_import_dispatch_skips_unresolvable_and_external_names() {
        // Only relative literals that name a real module count as dispatch
        // entries: a missing module (`.not_a_module`), a literal whose table
        // entry names an external package base rather than this package
        // (`('pydantic_core', '.core_schema')`), an absolute import name
        // (`os`), a bare package reference (`.`) and unrelated strings are all
        // ignored.
        let dir = TestDir::new("dynamic-skip");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            r#""""Not a dispatch table: '.not_a_module'."""
from importlib import import_module

_suffixes = ['.py', '.not_a_module']
_UNRELATED = '.also_not_a_module'
_dynamic_imports = {
    'Missing': (__spec__.parent, '.not_a_module'),
    'External': ('pydantic_core', '.core_schema'),
    'Absolute': (__spec__.parent, 'os'),
    'Package': ('pydantic_core', '.'),
}

def _load(name):
    # A table inside a function body runs only when that function is called,
    # so it stays as lazy as a nested import.
    hidden = {'Hidden': (__spec__.parent, '.hidden')}
    return hidden[name]

def __getattr__(name):
    if name not in _dynamic_imports:
        raise AttributeError(name)
    package, module_name = _dynamic_imports[name]
    return getattr(import_module(module_name, package=package), name)
"#,
        )
        .expect("write package init");
        fs::write(pkg.join("hidden.py"), "class Hidden:\n    pass\n").expect("write hidden");
        let main = dir.write("main.py", "import pkg\nvalue = pkg\n");
        let program = program(&main);

        let initializers = initializer_names(&program);
        assert_eq!(
            initializers,
            vec!["pkg.<module>"],
            "only the package itself may initialize: {initializers:?}"
        );
        assert!(
            !program.functions.iter().any(|function| {
                function.name.contains("not_a_module")
                    || function.name.contains("also_not_a_module")
                    || function.name.contains("core_schema")
                    || function.name.starts_with("pkg.hidden")
                    || function.name.starts_with("os.")
            }),
            "unresolvable, external and function-local names must not pull modules"
        );
    }

    #[test]
    fn dynamic_import_dispatch_does_not_duplicate_a_static_submodule() {
        // `from . import helper` already links `pkg.helper`; the dispatch
        // table naming the same module must not compile a second unit.
        let dir = TestDir::new("dynamic-duplicate");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            r#"from importlib import import_module

from . import helper

_dynamic_imports = {'Thing': (__spec__.parent, '.helper')}

def __getattr__(name):
    if name not in _dynamic_imports:
        raise AttributeError(name)
    package, module_name = _dynamic_imports[name]
    return getattr(import_module(module_name, package=package), name)
"#,
        )
        .expect("write package init");
        fs::write(pkg.join("helper.py"), "Thing = 1\n").expect("write helper");
        let main = dir.write("main.py", "from pkg import Thing\nvalue = Thing\n");
        let program = program(&main);

        let helpers = program
            .functions
            .iter()
            .filter(|function| function.name == "pkg.helper.<module>")
            .count();
        assert_eq!(helpers, 1, "the submodule must stay one unit");
        let helper_initializers = initializer_names(&program)
            .iter()
            .filter(|name| **name == "pkg.helper.<module>")
            .count();
        assert_eq!(helper_initializers, 1, "one initializer per unit");
    }

    #[test]
    fn dynamic_import_dispatch_matches_the_pydantic_table_shape() {
        // The pinned pydantic 2.14 `__init__` names its public API through an
        // *annotated* `_dynamic_imports` table whose values are
        // `(__spec__.parent, '.module')` pairs, repeats one module under many
        // attribute names, keeps an `'__module__'` sentinel for the
        // `dataclasses` entry, and calls `import_module` from `__getattr__`.
        // `pkg.main` imports the package back (`from . import SUPPORT`), which
        // the real `pydantic.main` does too. Every relative target links once,
        // after the package body; the sentinel and the function-local f-string
        // name nothing.
        let dir = TestDir::new("dynamic-pydantic-shape");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            r#"from importlib import import_module

SUPPORT = 'support'

_dynamic_imports: 'dict[str, tuple[str, str]]' = {
    'dataclasses': (__spec__.parent, '__module__'),
    'Model': (__spec__.parent, '.main'),
    'create_model': (__spec__.parent, '.main'),
    'Field': (__spec__.parent, '.fields'),
}

def __getattr__(name):
    dynamic_attr = _dynamic_imports.get(name)
    if dynamic_attr is None:
        raise AttributeError(name)
    package, module_name = dynamic_attr
    if module_name == '__module__':
        return import_module(f'.{name}', package=package)
    module = import_module(module_name, package=package)
    return getattr(module, name)
"#,
        )
        .expect("write package init");
        fs::write(
            pkg.join("main.py"),
            "from . import SUPPORT\n\nclass Model:\n    support = SUPPORT\n",
        )
        .expect("write main");
        fs::write(
            pkg.join("fields.py"),
            "Field = 'field'\ncreate_model = None\n",
        )
        .expect("write fields");
        let main = dir.write(
            "main.py",
            "from pkg import Model, Field\nvalue = (Model, Field)\n",
        );
        let program = program(&main);

        let initializers = initializer_names(&program);
        for target in ["pkg.main.<module>", "pkg.fields.<module>"] {
            assert_eq!(
                initializers.iter().filter(|name| **name == target).count(),
                1,
                "each dispatched module must own exactly one unit: {initializers:?}"
            );
        }
        let package_at = initializers
            .iter()
            .position(|name| *name == "pkg.<module>")
            .expect("the package must initialize");
        for target in ["pkg.main.<module>", "pkg.fields.<module>"] {
            let at = initializers
                .iter()
                .position(|name| *name == target)
                .expect(target);
            assert!(
                package_at < at,
                "the package body must run before {target}: {initializers:?}"
            );
        }
        // The `__module__` sentinel (the `dataclasses` entry) names no module.
        assert!(
            !initializers
                .iter()
                .any(|name| name.starts_with("pkg.dataclasses")),
            "{initializers:?}"
        );
    }

    #[test]
    fn nested_package_dynamic_literals_stay_lazy() {
        // A top-level package's table is preloaded (`pkg` -> `.sub`), but a
        // nested package's table is not (`pkg.sub` -> `.leaf`). CPython only
        // reaches a nested package's `__getattr__`/`import_module` after the
        // enclosing package graph finished initializing, and eagerly compiling
        // such targets runs internal helpers before their parent publishes the
        // names they read (NumPy: `numpy._typing._dtype_like` reads
        // `numpy.dtype`, which `numpy._core` installs when its own body runs).
        let dir = TestDir::new("dynamic-nested");
        let pkg = dir.path.join("pkg");
        let sub = pkg.join("sub");
        fs::create_dir_all(&sub).expect("create subpackage");
        fs::write(
            pkg.join("__init__.py"),
            r#"from importlib import import_module

_dynamic_imports = {'Thing': (__spec__.parent, '.sub')}

def __getattr__(name):
    package, module_name = _dynamic_imports[name]
    return getattr(import_module(module_name, package=package), name)
"#,
        )
        .expect("write package init");
        fs::write(
            sub.join("__init__.py"),
            r#"from importlib import import_module

_dynamic_imports = {'Leaf': (__spec__.parent, '.leaf')}

def __getattr__(name):
    package, module_name = _dynamic_imports[name]
    return getattr(import_module(module_name, package=package), name)
"#,
        )
        .expect("write subpackage init");
        fs::write(sub.join("leaf.py"), "Leaf = 1\n").expect("write leaf");
        let main = dir.write("main.py", "from pkg import Thing\nvalue = Thing\n");
        let program = program(&main);

        let initializers = initializer_names(&program);
        assert!(
            initializers.contains(&"pkg.sub.<module>"),
            "the top-level package may dispatch to a nested package: {initializers:?}"
        );
        assert!(
            !initializers.contains(&"pkg.sub.leaf.<module>"),
            "a nested package's table stays lazy: {initializers:?}"
        );
    }

    #[test]
    fn optional_try_star_import_links_a_present_module() {
        // A module-level `try` body runs in normal execution order: when the
        // star-imported module compiles, its exports are linked eagerly, so a
        // `from helper import *` inside `try` is a real import rather than a
        // runtime error.
        let dir = TestDir::new("star-optional");
        dir.write("helper.py", "def add(a, b):\n    return a + b\n");
        let main = dir.write(
            "main.py",
            "try:\n    from helper import *\nexcept ImportError:\n    pass\n\nprint(add(1, 2))\n",
        );
        let program = program(&main);
        assert_eq!(printed_call(&program), function_id(&program, "helper.add"));
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

    #[test]
    fn imported_module_docstring_is_importable() {
        // `from module import __doc__` is the NumPy re-export idiom
        // (`numpy/lib/array_utils.py`): the imported module publishes its
        // docstring as a module global.
        let dir = TestDir::new("dunder-doc");
        dir.write("helper.py", "\"\"\"Helper documentation.\"\"\"\n");
        let main = dir.write("main.py", "from helper import __doc__\nprint(__doc__)\n");
        let program = program(&main);

        use hydro_ir::{Const, ExprKind, StmtKind};
        assert_eq!(program.initializers.len(), 1);
        let helper = &program.functions[program.initializers[0] as usize];
        assert_eq!(helper.name, "helper.<module>");
        let mut published = None;
        for stmt in &helper.body {
            if let StmtKind::Assign { target, value } = &stmt.kind {
                if helper.local(*target).name == "__doc__" {
                    published = match &value.kind {
                        ExprKind::Const(Const::Str(text)) => Some(text.clone()),
                        _ => None,
                    };
                }
            }
        }
        assert_eq!(
            published.as_deref(),
            Some("Helper documentation."),
            "the docstring must be the module's __doc__"
        );

        // The importer reads helper's global table, not an entry-local copy.
        let entry = program.main_function();
        let mut imported = false;
        for stmt in &entry.body {
            if let StmtKind::Expr(expr) = &stmt.kind {
                if let ExprKind::Call { args, .. } = &expr.kind {
                    imported |= args.iter().any(|arg| {
                        matches!(
                            &arg.kind,
                            ExprKind::GlobalGet { module, name }
                                if module == "helper" && name == "__doc__"
                        )
                    });
                }
            }
        }
        assert!(imported, "print(__doc__) must read helper.__doc__");
    }

    #[test]
    fn undocumented_imported_module_docstring_is_none() {
        let dir = TestDir::new("dunder-doc-none");
        dir.write("helper.py", "VALUE = 1\n");
        let main = dir.write("main.py", "from helper import __doc__\nprint(__doc__)\n");
        let program = program(&main);

        use hydro_ir::{Const, ExprKind, StmtKind};
        let helper = &program.functions[program.initializers[0] as usize];
        assert!(
            helper.body.iter().any(|stmt| matches!(
                &stmt.kind,
                StmtKind::Assign { target, value }
                    if helper.local(*target).name == "__doc__"
                        && matches!(&value.kind, ExprKind::Const(Const::None))
            )),
            "an undocumented module binds __doc__ to None: {:?}",
            helper.body
        );

        let entry = program.main_function();
        assert!(
            entry.body.iter().any(|stmt| matches!(
                &stmt.kind,
                StmtKind::Expr(expr)
                    if matches!(
                        &expr.kind,
                        ExprKind::Call { args, .. }
                            if args.iter().any(|arg| matches!(
                                &arg.kind,
                                ExprKind::GlobalGet { module, name }
                                    if module == "helper" && name == "__doc__"
                            ))
                    )
            )),
            "print(__doc__) must read helper.__doc__"
        );
    }

    #[test]
    fn explicit_doc_assignment_replaces_the_docstring() {
        // CPython sets the module docstring first, then executes the body, so
        // a module-level `__doc__ = ...` replaces it.
        let dir = TestDir::new("dunder-doc-replace");
        dir.write(
            "helper.py",
            "\"\"\"Original documentation.\"\"\"\n\n__doc__ = \"Replaced documentation.\"\n",
        );
        let main = dir.write("main.py", "from helper import __doc__\nprint(__doc__)\n");
        let program = program(&main);

        use hydro_ir::{Const, ExprKind, StmtKind};
        let helper = &program.functions[program.initializers[0] as usize];
        let mut assigned = Vec::new();
        for stmt in &helper.body {
            if let StmtKind::Assign { target, value } = &stmt.kind {
                if helper.local(*target).name == "__doc__" {
                    if let ExprKind::Const(Const::Str(text)) = &value.kind {
                        assigned.push(text.to_string());
                    }
                }
            }
        }
        assert_eq!(
            assigned,
            vec![
                "Original documentation.".to_string(),
                "Replaced documentation.".to_string()
            ],
            "the docstring is bound first and the explicit assignment replaces it"
        );
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
                ExprKind::DeferredImport { module, member, .. }
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
                ExprKind::DeferredImport { module, member, .. }
                    if module == "pkg.sub" && member.as_deref() == Some("value")
            ),
            "the nested submodule import must stay deferred and target the \
             submodule itself: {:?}",
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
                ExprKind::DeferredImport { module, member, .. }
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
            ExprKind::DeferredImport { module, member, .. }
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
    fn module_try_import_initializes_the_local_a_handler_shadows() {
        // `uuid.py` imports the native `_uuid` extension in a module-level
        // `try` whose `except ImportError:` handler rebinds `_uuid = None`.
        // That handler assignment claims a module-local slot, which shadows
        // the compile-time import binding for the reads that follow, so the
        // import statement must initialize the slot itself. Before this, an
        // infallible (native) import emitted no binding and the try body read
        // an uninitialized local — `_uuid` was `None` at the
        // `_uuid.has_stable_extractable_node` read.
        let dir = TestDir::new("try-import-shadowed-local");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "try:\n    import _uuid\n    STABLE = _uuid.has_stable_extractable_node\nexcept ImportError:\n    _uuid = None\n    STABLE = False\n",
        )
        .expect("write package init");
        let main = dir.write("main.py", "import pkg\nprint(pkg.STABLE)\n");
        let program = program(&main);

        use hydro_ir::StmtKind;
        let init = &program.functions[program.initializers[0] as usize];
        assert_eq!(init.name, "pkg.<module>");
        let mut local_binding = false;
        let mut global_binding = false;
        // A multi-binding import statement is wrapped in `if True:` so every
        // store runs where the statement runs.
        fn scan_local_import(
            body: &[hydro_ir::Stmt],
            init: &hydro_ir::Function,
            local_binding: &mut bool,
            global_binding: &mut bool,
        ) {
            use hydro_ir::{ExprKind, StmtKind};
            for stmt in body {
                match &stmt.kind {
                    StmtKind::Assign { target, value } if init.local(*target).name == "_uuid" => {
                        *local_binding = matches!(
                            &value.kind,
                            ExprKind::ModuleRef { module } if module == "_uuid"
                        );
                    }
                    StmtKind::GlobalSet { name, value, .. } if name == "_uuid" => {
                        *global_binding = matches!(&value.kind, ExprKind::Local(_));
                    }
                    StmtKind::If { then_body, .. } => {
                        scan_local_import(then_body, init, local_binding, global_binding);
                    }
                    _ => {}
                }
            }
        }
        for stmt in &init.body {
            let StmtKind::Try { body, .. } = &stmt.kind else {
                continue;
            };
            scan_local_import(body, init, &mut local_binding, &mut global_binding);
        }
        assert!(
            local_binding,
            "the import must initialize the `_uuid` local the handler shadows"
        );
        assert!(
            global_binding,
            "the module value must be published from the same local"
        );
    }

    #[test]
    fn a_prior_units_builtin_binding_does_not_shadow_it_in_the_next_unit() {
        // `pkg/__init__.py` binds `type`; `pkg/child.py` still calls the
        // builtin. Each unit is lowered against its own module bindings, so the
        // child's `type` must stay a builtin call and never become a runtime
        // global read of the package's table (where `type` is a string; a
        // leaked lookup is what made pydantic's preloaded `copy` see a foreign
        // global).
        let dir = TestDir::new("builtin-unit-isolation");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "type = 'shadowed in the package'\n",
        )
        .expect("write package init");
        fs::write(pkg.join("child.py"), "value = type(1)\n").expect("write child");
        let main = dir.write("main.py", "from pkg.child import value\nprint(value)\n");
        let program = program(&main);

        use hydro_ir::{Callee, ExprKind, StmtKind};
        let child = program.function(function_id(&program, "pkg.child.<module>"));
        let mut builtin_call = false;
        let mut leaked_global = false;
        for stmt in &child.body {
            let StmtKind::Assign { target: _, value } = &stmt.kind else {
                continue;
            };
            match &value.kind {
                ExprKind::Call { callee, .. } => {
                    builtin_call = matches!(callee, Callee::Builtin(hydro_ir::Builtin::Type));
                }
                ExprKind::GlobalGet { name, .. } if name == "type" => leaked_global = true,
                _ => {}
            }
        }
        assert!(
            !leaked_global,
            "the child's `type` must not read another unit's global"
        );
        assert!(
            builtin_call,
            "the child must call the builtin `type`: {:?}",
            child.body
        );
    }

    #[test]
    fn module_level_try_import_of_a_missing_submodule_is_a_runtime_import() {
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
        use hydro_ir::ExprKind;

        // Nothing was preloaded: the handler owns the missing import, so the
        // statement performs it at runtime, where the failed from-import
        // (`cannot import name 'missing' from 'pkg'`) runs the `except
        // ImportError` handler, and reads go through the module global table so
        // the handler's fallback is visible.
        assert_eq!(program.initializers.len(), 1, "{:?}", program.initializers);
        assert_eq!(
            program.functions[program.initializers[0] as usize].name,
            "pkg.<module>"
        );
        let init = &program.functions[program.initializers[0] as usize].body;
        let stored = stored_global(init, "missing").expect("the try body must perform the import");
        assert!(
            matches!(
                &stored.kind,
                ExprKind::DeferredImport { module, member, .. }
                    if module == "pkg" && member.as_deref() == Some("missing")
            ),
            "the missing submodule must be imported at runtime: {:?}",
            stored.kind
        );

        let get = function_id(&program, "pkg.get");
        let returned = returned_expression(&program, get);
        assert!(
            matches!(
                &returned.kind,
                ExprKind::GlobalGet { module, name }
                    if module == "pkg" && name == "missing"
            ),
            "the binding must be read from the module globals: {:?}",
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
        use hydro_ir::ExprKind;

        // The extension is not a compiled unit: the `try` body imports it at
        // runtime, keeping the shared object, so a load failure runs the
        // handler and reads go through the module global table.
        assert_eq!(program.initializers.len(), 1, "{:?}", program.initializers);
        let init = &program.functions[program.initializers[0] as usize].body;
        let stored = stored_global(init, "ext").expect("the try body must perform the import");
        assert!(
            matches!(
                &stored.kind,
                ExprKind::DeferredImport { module, member, path }
                    if module == "pkg.ext"
                        && member.is_none()
                        && path.as_deref() == Some(expected.as_str())
            ),
            "the extension must be imported at runtime: {:?}",
            stored.kind
        );

        let get = function_id(&program, "pkg.get");
        let returned = returned_expression(&program, get);
        let ExprKind::MethodCall {
            object,
            method,
            args,
            ..
        } = &returned.kind
        else {
            panic!("expected an extension method call, got {:?}", returned.kind);
        };
        assert_eq!(method, "value");
        assert!(args.is_empty(), "{args:?}");
        assert!(
            matches!(
                &object.kind,
                ExprKind::GlobalGet { module, name }
                    if module == "pkg" && name == "ext"
            ),
            "the member must be read from the module global: {:?}",
            object.kind
        );
    }

    /// The expression behind a value published into the global table.
    ///
    /// An optional import stores the deferred import in a module local
    /// (`Assign { value: DeferredImport }`) and then publishes that local, so a
    /// `Local` store resolves to the expression assigned to it in the same
    /// block. Every other store publishes its value directly.
    fn published_value<'a>(
        stmts: &'a [hydro_ir::Stmt],
        value: &'a hydro_ir::Expr,
    ) -> &'a hydro_ir::Expr {
        let hydro_ir::ExprKind::Local(local) = &value.kind else {
            return value;
        };
        stmts
            .iter()
            .find_map(|stmt| match &stmt.kind {
                hydro_ir::StmtKind::Assign { target, value } if target == local => Some(value),
                _ => None,
            })
            .unwrap_or(value)
    }

    /// Value stored into module global `name` by the entries of `stmts`.
    ///
    /// A store of a module local resolves through the assignment that produced
    /// the local, so an optional import is observed as the import it performs.
    fn stored_global<'a>(stmts: &'a [hydro_ir::Stmt], name: &str) -> Option<&'a hydro_ir::Expr> {
        use hydro_ir::StmtKind;
        stmts.iter().find_map(|stmt| match &stmt.kind {
            StmtKind::GlobalSet {
                name: bound, value, ..
            } if bound == name => Some(published_value(stmts, value)),
            StmtKind::Try { body, handlers, .. } => stored_global(body, name).or_else(|| {
                handlers
                    .iter()
                    .find_map(|handler| stored_global(&handler.body, name))
            }),
            StmtKind::If {
                then_body,
                else_body,
                ..
            } => stored_global(then_body, name).or_else(|| stored_global(else_body, name)),
            _ => None,
        })
    }

    #[test]
    fn optional_import_of_a_missing_native_member_runs_its_handler() {
        // `from _socket import NOPE` names no native member. Inside a `try`
        // whose handler catches `ImportError` the statement is deferred
        // through the extension shared object, so the handler binds the
        // fallback instead of the compiler rejecting the program. The fixture
        // makes the resolved shared object independent of the host's Python
        // installation.
        let dir = TestDir::new("optional-native-member");
        let so = dir.add_extension("_socket.so");
        let expected = fs::canonicalize(&so)
            .expect("canonical extension path")
            .to_string_lossy()
            .into_owned();
        let main = dir.write(
            "main.py",
            "try:\n    from _socket import NOPE\nexcept ImportError:\n    NOPE = 42\n\nprint(NOPE)\n",
        );
        let program = program(&main);
        use hydro_ir::ExprKind;

        let entry = program.main_function();
        let imported = stored_global(&entry.body, "NOPE").expect("the try body must import NOPE");
        assert!(
            matches!(
                &imported.kind,
                ExprKind::DeferredImport { module, member, path }
                    if module == "_socket"
                        && member.as_deref() == Some("NOPE")
                        && path.as_deref() == Some(expected.as_str())
            ),
            "expected a deferred extension import, got {:?}",
            imported.kind
        );

        // The handler publishes its fallback in the module global table.
        let handlers = entry
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                hydro_ir::StmtKind::Try { handlers, .. } => Some(handlers),
                _ => None,
            })
            .expect("the entry module must contain the `try`");
        let fallback = handlers[0]
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                hydro_ir::StmtKind::GlobalSet {
                    name, value: bound, ..
                } if name == "NOPE" => Some(published_value(&handlers[0].body, bound)),
                _ => None,
            })
            .expect("the handler must bind the fallback");
        assert!(
            matches!(&fallback.kind, ExprKind::Const(hydro_ir::Const::Int(42))),
            "expected the fallback value, got {:?}",
            fallback.kind
        );
    }

    #[test]
    fn optional_import_of_a_missing_user_member_is_a_runtime_import() {
        // The sibling module compiles, but it does not provide the imported
        // name: only the handler can supply it, so the import runs at runtime.
        let dir = TestDir::new("optional-user-member");
        dir.write("helper.py", "def add(a, b):\n    return a + b\n");
        let main = dir.write(
            "main.py",
            "try:\n    from helper import nope\nexcept ImportError:\n    nope = 0\n\nprint(nope)\n",
        );
        let program = program(&main);
        use hydro_ir::ExprKind;

        let entry = program.main_function();
        let imported = stored_global(&entry.body, "nope").expect("the try body must import nope");
        assert!(
            matches!(
                &imported.kind,
                ExprKind::DeferredImport { module, member, path }
                    if module == "helper"
                        && member.as_deref() == Some("nope")
                        && path.is_none()
            ),
            "expected a runtime import, got {:?}",
            imported.kind
        );
    }

    #[test]
    fn optional_import_of_a_linked_member_keeps_the_static_binding() {
        // The named function exists in a linked module, so the import cannot
        // fail and stays a compile-time link.
        let dir = TestDir::new("optional-linked-member");
        dir.write("helper.py", "def add(a, b):\n    return a + b\n");
        let main = dir.write(
            "main.py",
            "try:\n    from helper import add\nexcept ImportError:\n    add = None\n\nprint(add(1, 2))\n",
        );
        let program = program(&main);
        assert_eq!(printed_call(&program), function_id(&program, "helper.add"));
        let entry = program.main_function();
        assert!(
            stored_global(&entry.body, "add").is_none(),
            "a guaranteed import must not run at runtime: {:?}",
            entry.body
        );
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
    fn initializing_package_function_is_not_read_before_the_package_body() {
        // Package cycle: `pkg` imports `pkg.child` before defining
        // `greeting`, and `pkg.child` imports `greeting` back from `pkg`.
        // The child initializer runs first, so it must not eagerly read
        // `pkg.greeting`; the compile-time binding resolves the name when
        // `run()` executes, after the package body mirrored the function.
        let dir = TestDir::new("initializing-parent-order");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "from .child import run\n\ndef greeting():\n    return 'hi'\n",
        )
        .expect("write package init");
        fs::write(
            pkg.join("child.py"),
            "from . import greeting\n\ndef run():\n    return greeting()\n",
        )
        .expect("write child");
        let main = dir.write("main.py", "from pkg.child import run\nprint(run())\n");
        let program = program(&main);

        use hydro_ir::{ExprKind, StmtKind};
        let initializer = |name: &str| {
            program
                .initializers
                .iter()
                .copied()
                .find(|id| program.functions[*id as usize].name == name)
                .unwrap_or_else(|| panic!("missing initializer {name}"))
        };
        let child = initializer("pkg.child.<module>");
        let package = initializer("pkg.<module>");
        assert!(
            child < package,
            "the package dependency initializes before the package body"
        );

        let eager_read =
            program.functions[child as usize]
                .body
                .iter()
                .any(|stmt| match &stmt.kind {
                    StmtKind::GlobalSet { value, .. } => matches!(
                        &value.kind,
                        ExprKind::GlobalGet { module, name }
                            if module == "pkg" && name == "greeting"
                    ),
                    _ => false,
                });
        assert!(
            !eager_read,
            "the child initializer must not read pkg.greeting before the package body runs"
        );

        let mirrored =
            program.functions[package as usize]
                .body
                .iter()
                .any(|stmt| match &stmt.kind {
                    StmtKind::GlobalSet { name, value, .. } if name == "greeting" => {
                        matches!(&value.kind, ExprKind::FunctionRef(_))
                    }
                    _ => false,
                });
        assert!(
            mirrored,
            "the package body must mirror the function value for the late read"
        );
    }

    #[test]
    fn relative_from_import_binds_an_initializing_sibling_submodule() {
        // `pkg.numeric` imports `pkg.shape`, which imports `pkg.numeric` back.
        // CPython resolves `from . import numeric` through the import system,
        // so the binding is the (partially initialized) module object, never a
        // `pkg` attribute. NumPy's `numpy._core/shape_base.py` imports
        // `numeric` exactly like this, while `numeric` imports `shape_base`.
        let dir = TestDir::new("initializing-sibling");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(pkg.join("__init__.py"), "from . import numeric\n").expect("write package init");
        fs::write(
            pkg.join("numeric.py"),
            "from . import shape\n\ndef value():\n    return 1\n",
        )
        .expect("write numeric");
        fs::write(
            pkg.join("shape.py"),
            "from . import numeric\n\ndef get():\n    return numeric.value()\n",
        )
        .expect("write shape");
        let main = dir.write("main.py", "from pkg.shape import get\nprint(get())\n");
        let program = program(&main);

        // `shape` initializes first (it is `numeric`'s dependency) and binds
        // the still-initializing sibling by identity, not through `pkg`.
        let shape = function_id(&program, "pkg.shape.<module>");
        use hydro_ir::{ExprKind, StmtKind};
        let stored = program.functions[shape as usize]
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::GlobalSet { name, value, .. } if name == "numeric" => Some(value),
                _ => None,
            })
            .expect("shape must bind `numeric`");
        assert!(
            matches!(&stored.kind, ExprKind::ModuleRef { module } if module == "pkg.numeric"),
            "the binding must be the module itself: {:?}",
            stored.kind
        );
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
                ExprKind::DeferredImport { module, member, .. }
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

    /// Initializer slot of the module body `name` (`<dotted module>.<module>`).
    fn initializer_slot(program: &Program, name: &str) -> Option<usize> {
        program
            .initializers
            .iter()
            .position(|id| program.functions[*id as usize].name == name)
    }

    #[test]
    fn nested_site_package_import_links_when_the_site_directory_has_any_name() {
        // The pinned ecosystem tree is called `site`, not `site-packages`.
        // A submodule referenced only from a nested block must still be
        // preloaded from that tree; before the fix the literal directory-name
        // check skipped it and the runtime reported `No module named`.
        let _env = env_lock();
        let dir = TestDir::new("site-any-name");
        let site = dir.path.join("site");
        let project = dir.path.join("project");
        fs::create_dir_all(site.join("pkg")).expect("create site package");
        fs::create_dir_all(&project).expect("create project");
        fs::write(
            site.join("pkg").join("__init__.py"),
            "def __getattr__(name):\n    import pkg.sub as sub\n    return sub\n",
        )
        .expect("write package init");
        fs::write(site.join("pkg").join("sub.py"), "VALUE = 7\n").expect("write submodule");
        let main = project.join("main.py");
        fs::write(&main, "import pkg\nprint(pkg.sub.VALUE)\n").expect("write entry");

        let _guard = EnvVar::set("HYDRO_SITE_PACKAGES", site.as_os_str());
        let program = program(&main);
        assert!(
            initializer_slot(&program, "pkg.sub.<module>").is_some(),
            "the nested submodule must be linked from a site tree with any name: {:?}",
            program
                .initializers
                .iter()
                .map(|id| &program.functions[*id as usize].name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn deferred_import_in_a_nested_package_preloads_a_data_module() {
        // `pydantic/plugin/_schema_validator.py` imports `._loader` inside a
        // function body, and `_loader.py` binds its caches with annotated
        // assignments before defining `get_plugins`. Declaration-only
        // preloading must accept those data bindings (they perform no
        // observable work), because Hydro has no runtime compiler: leaving the
        // module deferred turns the function-level import into a guaranteed
        // `ModuleNotFoundError: No module named 'pkg._loader'`.
        let _env = env_lock();
        let dir = TestDir::new("nested-deferred-data");
        let site = dir.path.join("site");
        let project = dir.path.join("project");
        fs::create_dir_all(site.join("pkg")).expect("create site package");
        fs::create_dir_all(&project).expect("create project");
        fs::write(
            site.join("pkg").join("__init__.py"),
            "from ._schema import make\n",
        )
        .expect("write package init");
        fs::write(
            site.join("pkg").join("_schema.py"),
            "def make():\n    from ._loader import get_plugins\n    return get_plugins()\n",
        )
        .expect("write schema module");
        fs::write(
            site.join("pkg").join("_loader.py"),
            "from __future__ import annotations\n\n\
             import os\n\
             from typing import TYPE_CHECKING, Final\n\n\
             if TYPE_CHECKING:\n    from pkg import Protocol\n\n\
             GROUP: Final[str] = 'pkg'\n\
             _plugins: dict[str, object] | None = None\n\
             _loading: bool = False\n\n\
             def get_plugins():\n    return ()\n",
        )
        .expect("write loader module");
        let main = project.join("main.py");
        fs::write(&main, "from pkg import make\nprint(make())\n").expect("write entry");

        let _guard = EnvVar::set("HYDRO_SITE_PACKAGES", site.as_os_str());
        let program = program(&main);
        assert!(
            initializer_slot(&program, "pkg._loader.<module>").is_some(),
            "the deferred loader module must be preloaded: {:?}",
            initializer_names(&program)
        );
    }

    #[test]
    fn already_linked_deferred_module_initializes_before_its_importer() {
        // The same shape, but the entry links `pkg._loader` statically. The
        // deferred stub inside `pkg._schema.make` must still be replaced by a
        // dependency edge: a runtime lookup would run while `_loader`'s
        // initializer has not registered the module yet, which is exactly the
        // `No module named 'pydantic.plugin._loader'` failure.
        let _env = env_lock();
        let dir = TestDir::new("deferred-already-linked");
        let site = dir.path.join("site");
        let project = dir.path.join("project");
        fs::create_dir_all(site.join("pkg")).expect("create site package");
        fs::create_dir_all(&project).expect("create project");
        fs::write(
            site.join("pkg").join("__init__.py"),
            "from ._schema import make\n",
        )
        .expect("write package init");
        fs::write(
            site.join("pkg").join("_schema.py"),
            "def make():\n    from ._loader import get_plugins\n    return get_plugins()\n",
        )
        .expect("write schema module");
        fs::write(
            site.join("pkg").join("_loader.py"),
            "from __future__ import annotations\n\n\
             GROUP: str = 'pkg'\n\
             _plugins: dict[str, object] | None = None\n\n\
             def get_plugins():\n    return ()\n",
        )
        .expect("write loader module");
        let main = project.join("main.py");
        fs::write(
            &main,
            "import pkg._loader\nfrom pkg import make\nprint(make())\n",
        )
        .expect("write entry");

        let _guard = EnvVar::set("HYDRO_SITE_PACKAGES", site.as_os_str());
        let program = program(&main);
        let loader = initializer_slot(&program, "pkg._loader.<module>").unwrap_or_else(|| {
            panic!(
                "pkg._loader must be linked: {:?}",
                initializer_names(&program)
            )
        });
        let schema = initializer_slot(&program, "pkg._schema.<module>").unwrap_or_else(|| {
            panic!(
                "pkg._schema must be linked: {:?}",
                initializer_names(&program)
            )
        });
        assert!(
            loader < schema,
            "the already-linked module must initialize first: {:?}",
            initializer_names(&program)
        );

        use hydro_ir::{Callee, ExprKind};
        let make = function_id(&program, "pkg._schema.make");
        let returned = returned_expression(&program, make);
        assert!(
            matches!(
                &returned.kind,
                ExprKind::Call {
                    callee: Callee::Function(_),
                    ..
                }
            ),
            "the deferred import must bind the linked function: {:?}",
            returned.kind
        );
    }

    #[test]
    fn nested_from_import_of_a_submodule_alias_preloads_the_submodule() {
        // `pydantic/version.py` does `from ._internal import _git as git`
        // inside `version_info()`. The deferred scan records the *base*
        // package (`pkg._internal`), but the import target is the submodule
        // `pkg._internal._git`: the preload must link the submodule, or the
        // lowerer treats `_git` as a missing package attribute and rejects the
        // program (`module 'pkg._internal' does not export ...`). A second
        // nested import keeps the deferred set non-empty, which is what made
        // the fallback hide the bug in the empty case.
        let _env = env_lock();
        let dir = TestDir::new("nested-submodule-alias");
        let site = dir.path.join("site");
        let project = dir.path.join("project");
        fs::create_dir_all(site.join("pkg").join("_internal")).expect("create site package");
        fs::create_dir_all(&project).expect("create project");
        fs::write(
            site.join("pkg").join("__init__.py"),
            "from .version import VERSION\n",
        )
        .expect("write package init");
        fs::write(
            site.join("pkg").join("version.py"),
            r#"VERSION = '1.0'


def version_info() -> str:
    import definitely_missing_mod
    from ._internal import _git as git

    return git.git_revision()
"#,
        )
        .expect("write version module");
        fs::write(site.join("pkg").join("_internal").join("__init__.py"), "")
            .expect("write internal package init");
        fs::write(
            site.join("pkg").join("_internal").join("_git.py"),
            r#""""Git helpers."""

from __future__ import annotations

import subprocess
from pathlib import Path


def git_revision() -> str:
    return 'rev'
"#,
        )
        .expect("write _git module");
        let main = project.join("main.py");
        fs::write(
            &main,
            "import pkg.version\nprint(pkg.version.version_info())\n",
        )
        .expect("write entry");

        let _guard = EnvVar::set("HYDRO_SITE_PACKAGES", site.as_os_str());
        let program = program(&main);
        let git = initializer_slot(&program, "pkg._internal._git.<module>").unwrap_or_else(|| {
            panic!(
                "the aliased submodule must be linked: {:?}",
                initializer_names(&program)
            )
        });
        let version = initializer_slot(&program, "pkg.version.<module>").unwrap_or_else(|| {
            panic!(
                "pkg.version must be linked: {:?}",
                initializer_names(&program)
            )
        });
        assert!(
            git < version,
            "the submodule must initialize before its importer: {:?}",
            initializer_names(&program)
        );

        // The call binds through the linked submodule, never the runtime
        // import stub.
        use hydro_ir::{Callee, ExprKind};
        let info = function_id(&program, "pkg.version.version_info");
        let returned = returned_expression(&program, info);
        let linked = match &returned.kind {
            ExprKind::Call {
                callee: Callee::Function(_),
                ..
            } => true,
            ExprKind::Call {
                callee: Callee::Extension { .. },
                ..
            } => false,
            ExprKind::IndirectCall { callee, .. } => matches!(
                &callee.kind,
                ExprKind::ModuleAttr { module, name }
                    if module == "pkg._internal._git" && name == "git_revision"
            ),
            _ => false,
        };
        assert!(
            linked,
            "`git.git_revision()` must bind the linked submodule: {:?}",
            returned.kind
        );
    }

    #[test]
    fn nested_from_import_preloads_a_taken_static_branch_module() {
        // `pydantic/deprecated/parse.py` binds a warning alias under
        // `if not TYPE_CHECKING:` before defining its API. The branch is
        // statically taken and the body is declaration-only, so the deferred
        // preload must enter it and link the module; otherwise
        // `from .deprecated import parse` in `pydantic/main.py` has no target.
        let _env = env_lock();
        let dir = TestDir::new("taken-branch-preload");
        let site = dir.path.join("site");
        let project = dir.path.join("project");
        fs::create_dir_all(site.join("pkg").join("deprecated")).expect("create site package");
        fs::create_dir_all(&project).expect("create project");
        fs::write(
            site.join("pkg").join("__init__.py"),
            "from .main import run\n",
        )
        .expect("write package init");
        fs::write(
            site.join("pkg").join("main.py"),
            "def run():\n\
             \x20   import definitely_missing_mod\n\
             \x20   from .deprecated import parse\n\
             \x20   return parse.load_file()\n",
        )
        .expect("write main module");
        fs::write(site.join("pkg").join("deprecated").join("__init__.py"), "")
            .expect("write deprecated package init");
        fs::write(
            site.join("pkg").join("deprecated").join("parse.py"),
            r#""""Deprecated parse helpers."""

from __future__ import annotations

import os
from typing import TYPE_CHECKING

if not TYPE_CHECKING:
    FLAG = 1


def load_file():
    return os.getpid()
"#,
        )
        .expect("write parse module");
        let main = project.join("main.py");
        fs::write(&main, "import pkg\nprint(pkg.run())\n").expect("write entry");

        let _guard = EnvVar::set("HYDRO_SITE_PACKAGES", site.as_os_str());
        let program = program(&main);
        let parse =
            initializer_slot(&program, "pkg.deprecated.parse.<module>").unwrap_or_else(|| {
                panic!(
                    "the taken-branch module must be preloaded: {:?}",
                    initializer_names(&program)
                )
            });
        let run = initializer_slot(&program, "pkg.main.<module>").unwrap_or_else(|| {
            panic!("pkg.main must be linked: {:?}", initializer_names(&program))
        });
        assert!(
            parse < run,
            "the submodule must initialize before its importer: {:?}",
            initializer_names(&program)
        );
    }

    #[test]
    fn nested_from_import_of_an_unloadable_submodule_stays_deferred() {
        // When the member names a real submodule that the preload cannot link
        // (executable module-level code), the binding must stay a deferred
        // submodule stub instead of becoming an "does not export" error. The
        // base package is linked, so only the submodule's own deferred record
        // can authorise the runtime binding.
        let _env = env_lock();
        let dir = TestDir::new("deferred-submodule-fallback");
        let site = dir.path.join("site");
        let project = dir.path.join("project");
        fs::create_dir_all(site.join("pkg").join("deprecated")).expect("create site package");
        fs::create_dir_all(&project).expect("create project");
        fs::write(
            site.join("pkg").join("__init__.py"),
            "from .main import run\n",
        )
        .expect("write package init");
        fs::write(
            site.join("pkg").join("main.py"),
            "def run():\n\
             \x20   import definitely_missing_mod\n\
             \x20   from .deprecated import parse\n\
             \x20   return parse.load_file()\n",
        )
        .expect("write main module");
        fs::write(site.join("pkg").join("deprecated").join("__init__.py"), "")
            .expect("write deprecated package init");
        fs::write(
            site.join("pkg").join("deprecated").join("parse.py"),
            "import os\n\n\
             print('module side effect')\n\n\
             def load_file():\n    return os.getpid()\n",
        )
        .expect("write unloadable parse module");
        let main = project.join("main.py");
        fs::write(&main, "import pkg\nprint(pkg.run())\n").expect("write entry");

        let _guard = EnvVar::set("HYDRO_SITE_PACKAGES", site.as_os_str());
        // The program must compile: the alias binds the deferred submodule
        // stub, not a missing package attribute.
        let program = program(&main);
        assert!(
            initializer_slot(&program, "pkg.deprecated.parse.<module>").is_none(),
            "an executable module body must stay deferred: {:?}",
            initializer_names(&program)
        );
    }

    #[test]
    fn subpackage_relative_extension_import_resolves_from_the_package_root() {
        // `from ._native import value` inside `pkg/sub/mod.py` names the
        // absolute module `pkg.sub._native`, and the artifact lives next to
        // the source (`pkg/sub/_native.abi3.so`). The source-relative scan
        // would search `pkg/sub/pkg/sub/_native`, so the lookup must be
        // retried from the package root exactly like the lowerer does;
        // otherwise compilation fails with
        // `module 'pkg.sub._native' not found` before the extension is bound
        // and the extension-support pass never sees the declaration.
        let dir = TestDir::new("subpackage-extension");
        dir.add_extension("pkg/sub/_native.abi3.so");
        dir.write("pkg/__init__.py", "");
        dir.write("pkg/sub/__init__.py", "");
        dir.write("pkg/sub/mod.py", "from ._native import value\n");
        let main = dir.write("main.py", "import pkg.sub.mod\nprint('ok')\n");
        let program = program(&main);

        let extension_units = program
            .functions
            .iter()
            .filter(|function| function.name.starts_with("pkg.sub._native"))
            .count();
        assert_eq!(
            extension_units, 0,
            "a compiled extension is bound by the lowerer, not compiled into a unit"
        );
        assert!(
            initializer_slot(&program, "pkg.sub.mod.<module>").is_some(),
            "the subpackage module must compile"
        );
    }

    #[test]
    fn extension_support_modules_initialize_before_the_extension_importer() {
        // A compiled extension imports Python modules from its own package
        // while it initializes, and no source statement names them. They must
        // be linked and initialized before the unit that loads the extension.
        let dir = TestDir::new("extension-support");
        dir.add_extension("pkg/_core/_multiarray_umath.abi3.so");
        dir.write("pkg/__init__.py", "from . import _core\n");
        dir.write(
            "pkg/exceptions.py",
            "__all__ = [\"AxisError\"]\n\nclass AxisError(Exception):\n    pass\n",
        );
        dir.write("pkg/_core/__init__.py", "from . import umath\n");
        dir.write("pkg/_core/umath.py", "from ._multiarray_umath import *\n");
        dir.write(
            "pkg/_core/_exceptions.py",
            "__all__ = [\"AxisError\"]\n\nclass AxisError(Exception):\n    pass\n",
        );
        let main = dir.write("main.py", "import pkg\nprint('ok')\n");
        let program = program(&main);

        let names: Vec<&str> = program
            .initializers
            .iter()
            .map(|id| program.functions[*id as usize].name.as_str())
            .collect();
        let umath = initializer_slot(&program, "pkg._core.umath.<module>")
            .unwrap_or_else(|| panic!("umath must be an initializer: {names:?}"));
        let support = initializer_slot(&program, "pkg._core._exceptions.<module>")
            .unwrap_or_else(|| panic!("extension support module must be linked: {names:?}"));
        let sibling = initializer_slot(&program, "pkg.exceptions.<module>")
            .unwrap_or_else(|| panic!("package sibling must be linked: {names:?}"));
        assert!(
            support < umath,
            "support module must initialize before the extension importer: {names:?}"
        );
        assert!(
            sibling < umath,
            "package sibling must initialize before the extension importer: {names:?}"
        );
    }

    #[test]
    fn dynamic_preload_and_extension_support_keep_unit_indices_disjoint() {
        // Regression: the deferred-site preload runs first and moves the entry
        // unit to the end. The extension-support pass then used to receive the
        // entry's stale index, take an already-consumed slot and panic with
        // `old unit` inside `reorder_preloaded` (reproduced by NumPy). One
        // package carries all three triggers: a dynamic-import literal, a
        // compiled extension with a support sibling, and a nested import of a
        // site module, so both preload passes run and reorder.
        let _env = env_lock();
        let dir = TestDir::new("preload-reorder");
        let site = dir.path.join("site");
        fs::create_dir_all(site.join("pkg")).expect("create site package");
        fs::write(
            site.join("pkg").join("__init__.py"),
            r#"from importlib import import_module

from ._native import compute

_dynamic_imports = {'Thing': (__spec__.parent, '.main')}

def helper():
    import lazy_helper
    return lazy_helper.value()

def __getattr__(name):
    if name not in _dynamic_imports:
        raise AttributeError(name)
    package, module_name = _dynamic_imports[name]
    return getattr(import_module(module_name, package=package), name)
"#,
        )
        .expect("write package init");
        fs::write(site.join("pkg").join("main.py"), "class Thing:\n    pass\n")
            .expect("write dynamic submodule");
        // A declaration-only sibling of the shared object: the
        // extension-support pass links it before the importer.
        fs::write(site.join("pkg").join("_support.py"), "SUPPORT = 1\n")
            .expect("write extension support module");
        // A nested import of a declaration-only site module: the
        // deferred-site pass links it once the import graph is complete.
        fs::write(
            site.join("lazy_helper.py"),
            "import math\n\ndef value():\n    return math.pi\n",
        )
        .expect("write nested module");
        dir.add_extension("site/pkg/_native.abi3.so");

        let main = dir.write("main.py", "from pkg import Thing\nvalue = Thing\n");
        let _guard = EnvVar::set("HYDRO_SITE_PACKAGES", site.as_os_str());
        let program = program(&main);

        let names: Vec<&str> = program
            .initializers
            .iter()
            .map(|id| program.functions[*id as usize].name.as_str())
            .collect();
        // Both preload passes insert a unit (each reordering once) and the
        // dynamic literal compiles a third. Old and new unit indices partition
        // disjointly, and the order stays deterministic: the deferred module
        // before its importer, the extension support before the extension
        // importer, and the package before the dynamically dispatched module.
        assert_eq!(
            names,
            vec![
                "lazy_helper.<module>",
                "pkg._support.<module>",
                "pkg.<module>",
                "pkg.main.<module>",
            ]
        );
    }

    #[test]
    fn lazy_typing_helpers_initialize_after_the_package() {
        // NumPy's `numpy.lib` eagerly imports a helper (`api`) that imports
        // functions from a subpackage (`_linalg`), which reaches the private
        // typing helpers. Those helpers read `numpy.dtype`/`numpy.value` from
        // the package prefix at module scope, so running them before
        // `numpy/__init__.py` publishes its names raises AttributeError.
        // `_linalg` re-exports the implementation with `import *`, exactly
        // like `numpy/linalg/__init__.py`. The function-only import
        // `from ._linalg import compute` is the soft boundary: the helper can
        // still initialize before `_linalg`, while the typing chain is placed
        // after the package. Recognizing the star re-export as a callable is
        // what makes that edge soft.
        let dir = TestDir::new("late-typing-order");
        let pkg = dir.path.join("pkg");
        let lib = pkg.join("lib");
        let linalg = lib.join("_linalg");
        let typing = lib.join("_typing");
        fs::create_dir_all(&linalg).expect("create linalg package");
        fs::create_dir_all(&typing).expect("create typing package");
        fs::write(
            pkg.join("__init__.py"),
            "value = 1\nfrom .lib import HOLDER\n",
        )
        .expect("write package init");
        fs::write(lib.join("__init__.py"), "from .api import HOLDER\n").expect("write lib init");
        fs::write(
            lib.join("api.py"),
            "from ._linalg import compute\n\nHOLDER = 1\n\ndef run(value):\n    return compute(value)\n",
        )
        .expect("write api");
        fs::write(linalg.join("__init__.py"), "from ._impl import *\n").expect("write linalg init");
        fs::write(
            linalg.join("_impl.py"),
            "from pkg.lib._typing import ALIAS\n\ndef compute(value):\n    return ALIAS if value is None else value\n",
        )
        .expect("write linalg impl");
        fs::write(
            typing.join("__init__.py"),
            "from ._dtype_like import ALIAS\n",
        )
        .expect("write typing init");
        fs::write(
            typing.join("_dtype_like.py"),
            "import pkg\n\nALIAS = pkg.value\n",
        )
        .expect("write typing helper");
        let main = dir.write("main.py", "import pkg\nprint(pkg.HOLDER)\n");
        let program = program(&main);

        let names = initializer_names(&program);
        let position = |name: &str| {
            names
                .iter()
                .position(|current| *current == name)
                .unwrap_or_else(|| panic!("missing initializer '{name}': {names:?}"))
        };
        let pkg_body = position("pkg.<module>");
        let lib_body = position("pkg.lib.<module>");
        let helper = position("pkg.lib._typing._dtype_like.<module>");
        let typing_body = position("pkg.lib._typing.<module>");
        let impl_body = position("pkg.lib._linalg._impl.<module>");
        let linalg_body = position("pkg.lib._linalg.<module>");
        assert!(
            lib_body < pkg_body,
            "the package body reads a value from lib: {names:?}"
        );
        assert!(
            pkg_body < helper,
            "the typing helper reads the package prefix: {names:?}"
        );
        assert!(
            helper < typing_body && typing_body < impl_body && impl_body < linalg_body,
            "the typing chain keeps its own dependency order: {names:?}"
        );
    }

    #[test]
    fn direct_submodule_cycle_keeps_the_scan_order() {
        // A submodule imported directly by its package body keeps the existing
        // contract: it initializes before the body, because the body may read
        // the module's exports. Only a *transitive* helper of a partially
        // initialized ancestor is moved after the package.
        let dir = TestDir::new("direct-submodule-cycle");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(pkg.join("__init__.py"), "from . import sub\nvalue = 1\n")
            .expect("write package init");
        fs::write(
            pkg.join("sub.py"),
            "import pkg\n\ndef value():\n    return pkg.value\n",
        )
        .expect("write submodule");
        let main = dir.write("main.py", "import pkg\nprint(pkg.value)\n");
        let program = program(&main);

        assert_eq!(
            initializer_names(&program),
            vec!["pkg.sub.<module>", "pkg.<module>"],
            "a directly imported submodule stays before its package body"
        );
    }

    #[test]
    fn package_initializes_before_a_late_submodule_cycle() {
        // `pkg/__init__.py` defines `ZipFile` and only then imports `._path`;
        // `pkg/_path.py` subclasses `pkg.ZipFile` while initializing. CPython
        // runs the package prefix, then the submodule, then the package's last
        // import statement. Hydro runs the package body first and publishes the
        // import binding from an extra initializer segment.
        let dir = TestDir::new("deferred-cycle-order");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "class ZipFile:\n    pass\n\nfrom ._path import Path\n",
        )
        .expect("write package init");
        fs::write(
            pkg.join("_path.py"),
            "import pkg\n\nclass CompleteDirs(pkg.ZipFile):\n    pass\n\nPath = CompleteDirs\n",
        )
        .expect("write submodule");
        let main = dir.write("main.py", "import pkg\nprint(pkg.Path)\n");
        let program = program(&main);

        let names = initializer_names(&program);
        let package = names
            .iter()
            .position(|name| *name == "pkg.<module>")
            .unwrap_or_else(|| panic!("package initializer missing: {names:?}"));
        let submodule = names
            .iter()
            .position(|name| *name == "pkg._path.<module>")
            .unwrap_or_else(|| panic!("submodule initializer missing: {names:?}"));
        let imports = names
            .iter()
            .position(|name| *name == "pkg.<imports>")
            .unwrap_or_else(|| panic!("deferred binding segment missing: {names:?}"));
        assert!(
            package < submodule,
            "the package prefix runs first: {names:?}"
        );
        assert!(
            submodule < imports,
            "the deferred binding runs after _path initialized: {names:?}"
        );
    }

    #[test]
    fn a_package_reading_the_imported_value_keeps_the_submodule_first() {
        // `Path` is read while the package initializes, so its binding cannot
        // move after the submodule: keep the established dependency order.
        let dir = TestDir::new("deferred-cycle-read");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "from ._path import Path\nVALUE = Path\n\nclass ZipFile:\n    pass\n",
        )
        .expect("write package init");
        fs::write(
            pkg.join("_path.py"),
            "import pkg\n\nclass CompleteDirs(pkg.ZipFile):\n    pass\n\nPath = CompleteDirs\n",
        )
        .expect("write submodule");
        let main = dir.write("main.py", "import pkg\nprint(pkg.VALUE)\n");
        let program = program(&main);

        assert_eq!(
            initializer_names(&program),
            vec!["pkg._path.<module>", "pkg.<module>"],
            "a module-body read of the binding keeps the dependency order"
        );
    }

    #[test]
    fn class_body_import_links_a_user_module() {
        // A class body executes during module initialization, so its import is
        // an ordinary dependency (`email.message` imports `email.iterators` in
        // its `Message` class body).
        let dir = TestDir::new("class-body-import-link");
        let pkg = dir.path.join("pkg");
        fs::create_dir_all(&pkg).expect("create package");
        fs::write(
            pkg.join("__init__.py"),
            "class Message:\n    from .helper import walk\n",
        )
        .expect("write package init");
        fs::write(pkg.join("helper.py"), "def walk(self):\n    return 1\n").expect("write helper");
        let main = dir.write("main.py", "import pkg\nprint(pkg.Message.walk)\n");
        let program = program(&main);

        assert!(
            initializer_names(&program).contains(&"pkg.helper.<module>"),
            "a class-body import must link its module: {:?}",
            initializer_names(&program)
        );
    }
}
