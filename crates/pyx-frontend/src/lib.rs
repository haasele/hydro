//! PyX frontend: parse Python source, lower it to typed IR, infer/validate
//! types, and report source-located diagnostics.
//!
//! Public contract (used by `pyx-cli` and tests):
//! [`check_source`] returns either a fully typed [`Program`] or a non-empty list
//! of [`Diagnostic`]s. [`check_file`] additionally resolves imports of sibling
//! user modules (`foo.py`) and links them into one program. Both must never
//! panic on user input.

pub mod diag;
mod lower;
mod sema;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use pyx_ir::{FunctionId, Program};
use rustpython_parser::ast::Ranged;
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
/// `import foo` / `from foo import f` are resolved against `foo.py` next to the
/// importing file (packages and relative imports are not supported). Every
/// imported module is loaded once per canonical path; cycles, missing files and
/// top-level statements other than imports, functions, a docstring and `pass`
/// are reported as diagnostics. All functions of the entry file and its
/// dependencies end up in the returned [`Program`].
pub fn check_file(path: &Path) -> Result<Program, Vec<Diagnostic>> {
    let mut loader = ModuleLoader::default();
    let entry_key = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    loader.load(entry_key, path.to_path_buf(), "__main__".to_string());

    if !loader.diags.is_empty() {
        return Err(loader.diags);
    }
    if loader.units.is_empty() {
        // Defensive: `load` reports diagnostics for every failure path.
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
    /// Module name as first imported; `__main__` for the entry file.
    name: String,
    /// Display path used for diagnostics and tracebacks.
    display: String,
    source: String,
    module: ast::ModModule,
    /// Imports of other user modules: spelling -> unit index.
    deps: Vec<(String, usize)>,
}

#[derive(Default)]
struct ModuleLoader {
    /// Finalized modules in post-order (dependencies first, entry last).
    units: Vec<LoadedModule>,
    /// Canonical path -> unit index, so a file is loaded at most once.
    loaded: HashMap<PathBuf, usize>,
    /// Canonical paths whose load failed; avoids duplicate diagnostics.
    failed: HashSet<PathBuf>,
    /// Modules currently being loaded, for cycle detection.
    stack: Vec<(PathBuf, String)>,
    diags: Vec<Diagnostic>,
}

impl ModuleLoader {
    /// Loads `display` (keyed by the canonical `key`) and its user imports.
    ///
    /// Returns the unit index of the module, or `None` when any diagnostic was
    /// produced for this subtree.
    fn load(&mut self, key: PathBuf, display: PathBuf, name: String) -> Option<usize> {
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
        let mut deps = Vec::new();
        let mut failed = false;
        if name != "__main__" {
            self.check_top_level(&module, &source, &display, &mut failed);
        }
        self.scan_imports(&module, &source, &display, &mut deps, &mut failed);
        self.stack.pop();

        if failed {
            self.failed.insert(key);
            return None;
        }

        let index = self.units.len();
        self.units.push(LoadedModule {
            name,
            display: file,
            source,
            module,
            deps,
        });
        self.loaded.insert(key, index);
        Some(index)
    }

    /// Imported modules may only declare functions/imports (plus docstrings and
    /// `pass`); any other statement would be top-level execution.
    fn check_top_level(
        &mut self,
        module: &ast::ModModule,
        source: &str,
        file: &Path,
        failed: &mut bool,
    ) {
        for stmt in &module.body {
            let allowed = matches!(
                stmt,
                ast::Stmt::FunctionDef(_)
                    | ast::Stmt::AsyncFunctionDef(_)
                    | ast::Stmt::Import(_)
                    | ast::Stmt::ImportFrom(_)
                    | ast::Stmt::Pass(_)
            ) || is_docstring_stmt(stmt);
            if allowed {
                continue;
            }
            self.push_span(
                source,
                file,
                stmt.range(),
                codes::UNSUPPORTED,
                "imported modules may only contain function definitions, imports, a docstring and pass",
            );
            if let Some(last) = self.diags.last_mut() {
                last.help =
                    Some("move executable code into a function or the entry file".to_string());
            }
            *failed = true;
        }
    }

    fn scan_imports(
        &mut self,
        module: &ast::ModModule,
        source: &str,
        display: &Path,
        deps: &mut Vec<(String, usize)>,
        failed: &mut bool,
    ) {
        for stmt in &module.body {
            match stmt {
                ast::Stmt::Import(s) => {
                    for alias in &s.names {
                        self.resolve_import(
                            source,
                            display,
                            alias.name.as_str(),
                            alias.range,
                            deps,
                            failed,
                        );
                    }
                }
                ast::Stmt::ImportFrom(s) => {
                    if s.level.as_ref().is_some_and(|level| level.to_u32() != 0) {
                        self.push_span(
                            source,
                            display,
                            s.range,
                            codes::UNSUPPORTED,
                            "relative imports are not supported",
                        );
                        *failed = true;
                        continue;
                    }
                    let Some(module_name) = &s.module else {
                        self.push_span(
                            source,
                            display,
                            s.range,
                            codes::UNSUPPORTED,
                            "relative imports are not supported",
                        );
                        *failed = true;
                        continue;
                    };
                    self.resolve_import(
                        source,
                        display,
                        module_name.as_str(),
                        s.range,
                        deps,
                        failed,
                    );
                }
                _ => {}
            }
        }
    }

    /// Resolves one user-module import and loads it on demand.
    fn resolve_import(
        &mut self,
        source: &str,
        display: &Path,
        name: &str,
        range: rustpython_parser::text_size::TextRange,
        deps: &mut Vec<(String, usize)>,
        failed: &mut bool,
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
                *failed = true;
                return;
            }
            lower::ExtensionLookup::Missing => {}
        }

        if name.contains('.') {
            self.push_span(
                source,
                display,
                range,
                codes::UNSUPPORTED,
                format!("dotted imports are not supported for user modules: '{name}'"),
            );
            *failed = true;
            return;
        }

        let parent = display.parent().unwrap_or_else(|| Path::new("."));
        let expected = parent.join(format!("{name}.py"));
        if !expected.is_file() {
            self.push_span(
                source,
                display,
                range,
                codes::UNSUPPORTED,
                format!(
                    "module '{name}' not found: not supported unless a sibling {name}.py exists"
                ),
            );
            if let Some(last) = self.diags.last_mut() {
                last.help = Some(format!("expected a sibling file at {}", expected.display()));
            }
            *failed = true;
            return;
        }
        let key = fs::canonicalize(&expected).unwrap_or_else(|_| expected.clone());

        if let Some(position) = self.stack.iter().position(|(path, _)| path == &key) {
            let mut chain: Vec<&str> = self.stack[position..]
                .iter()
                .map(|(_, module_name)| module_name.as_str())
                .collect();
            chain.push(name);
            self.push_span(
                source,
                display,
                range,
                codes::UNSUPPORTED,
                format!("circular import: {}", chain.join(" -> ")),
            );
            *failed = true;
            return;
        }

        if let Some(&index) = self.loaded.get(&key) {
            if !deps.iter().any(|(_, existing)| *existing == index) {
                deps.push((name.to_string(), index));
            }
            return;
        }
        if self.failed.contains(&key) {
            *failed = true;
            return;
        }

        match self.load(key, expected, name.to_string()) {
            Some(index) => {
                if deps.iter().any(|(_, existing)| *existing == index) {
                    return;
                }
                deps.push((name.to_string(), index));
            }
            None => *failed = true,
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

fn is_docstring_stmt(stmt: &ast::Stmt) -> bool {
    matches!(stmt, ast::Stmt::Expr(expr) if matches!(&*expr.value, ast::Expr::Constant(c) if matches!(c.value, ast::Constant::Str(_))))
}

/// Lowers every loaded module in dependency order and links the results.
fn lower_modules(units: &[LoadedModule]) -> Result<Program, Vec<Diagnostic>> {
    let entry_index = units.len() - 1;
    let mut all_functions: Vec<pyx_ir::Function> = Vec::new();
    let mut exports: Vec<HashMap<String, FunctionId>> = vec![HashMap::new(); units.len()];
    let mut entry_main = 0;

    for (index, unit) in units.iter().enumerate() {
        let base = all_functions.len() as FunctionId;
        let mut infer = Infer::new();
        let mut lowerer = Lowerer::new(&unit.source, &unit.display, &mut infer);
        lowerer.set_fid_base(base);
        if index != entry_index {
            lowerer.set_dunder_name(&unit.name);
        }
        let mut user_modules = HashMap::new();
        for (imported_name, dep_index) in &unit.deps {
            user_modules.insert(
                imported_name.clone(),
                UserModule {
                    name: imported_name.clone(),
                    exports: exports[*dep_index].clone(),
                },
            );
        }
        lowerer.set_user_modules(user_modules);

        let mut hir = lowerer.lower_module(&unit.module);
        let exported = lowerer.exported_functions().clone();
        let lower_diags = std::mem::take(&mut lowerer.diags);
        drop(lowerer);
        if !lower_diags.is_empty() {
            return Err(lower_diags);
        }
        // Imported modules must be importable without side effects: only
        // functions (and their bodies) are compiled into the program.
        if index != entry_index {
            check_imported_body(&hir, &unit.display)?;
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
    })
}

/// Rejects executable statements in an imported module's body. Imports are
/// already resolved and function definitions are declarations, so the body
/// must otherwise be empty (the injected `__name__` assignment and a docstring
/// are allowed).
fn check_imported_body(hir: &LHir, file: &str) -> Result<(), Vec<Diagnostic>> {
    let module = &hir.functions[0];
    let mut diagnostics = Vec::new();
    for stmt in &module.body {
        let allowed = match &stmt.kind {
            lower::HStmtKind::Pass => true,
            lower::HStmtKind::Expr(expr) => {
                matches!(&expr.kind, lower::HExprKind::Const(pyx_ir::Const::Str(_)))
            }
            lower::HStmtKind::Assign { target, value } => {
                module
                    .locals
                    .get(*target as usize)
                    .is_some_and(|local| local.name == "__name__")
                    && matches!(&value.kind, lower::HExprKind::Const(pyx_ir::Const::Str(_)))
            }
            _ => false,
        };
        if allowed {
            continue;
        }
        diagnostics.push(
            Diagnostic::new(
                file,
                stmt.line,
                stmt.col,
                "imported modules are not executed: only function definitions, imports, a docstring and pass are allowed",
            )
            .with_code(codes::UNSUPPORTED)
            .with_help("move executable code into a function or the entry file"),
        );
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(diagnostics)
    }
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
                            if let ExprKind::Call {
                                callee: Callee::Function(id),
                                ..
                            } = &arg.kind
                            {
                                calls.push(*id);
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
        use pyx_ir::{Callee, ExprKind, StmtKind};
        let helper_fn = &program.functions[helper];
        let mut found = false;
        for stmt in &helper_fn.body {
            if let StmtKind::Return(Some(expr)) = &stmt.kind {
                if let ExprKind::Bin { lhs, .. } = &expr.kind {
                    if let ExprKind::Call { callee, .. } = &lhs.kind {
                        found = matches!(callee, Callee::Function(id) if *id == inc);
                    }
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
    fn class_exports_are_rejected() {
        let dir = TestDir::new("class-export");
        dir.write("helper.py", "class C:\n    pass\n");
        let main = dir.write("main.py", "from helper import C\n");
        let diags = check_file(&main).expect_err("class exports are not supported");
        assert!(!diags.is_empty());
        assert!(
            diags.iter().any(|diag| diag.file.ends_with("helper.py")),
            "diagnostic must point at the class module: {diags:?}"
        );
    }

    #[test]
    fn executable_statements_in_imported_modules_are_rejected() {
        let dir = TestDir::new("side-effect");
        dir.write(
            "helper.py",
            "def add(a, b):\n    return a + b\n\nprint(add(1, 2))\n",
        );
        let main = dir.write("main.py", "import helper\nprint(helper.add(1, 2))\n");
        let diags = check_file(&main).expect_err("imported side effects must fail");
        assert!(
            diags.iter().any(|diag| diag.file.ends_with("helper.py")
                && diag.message.contains("may only contain")),
            "{diags:?}"
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
}
