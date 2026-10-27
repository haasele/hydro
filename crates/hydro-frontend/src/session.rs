//! Incremental compilation for the IR interpreter and REPL.
//!
//! A [`SessionCompiler`] compiles one fragment of Python source at a time
//! against a persistent module loader and one shared runtime namespace. The
//! returned [`SessionFragment`] contains the fragment's [`Program`] and only
//! the module initializers that this fragment introduced: modules imported by
//! an earlier fragment are linked, but their bodies are not run again.
//!
//! Session semantics differ from [`check_source`](crate::check_source) and
//! [`check_file`](crate::check_file):
//!
//! - names bound by earlier fragments live in the runtime global table, so an
//!   unbound read resolves to `GlobalGet` instead of failing compilation;
//! - a session global shadows a builtin, function or exception class;
//! - every module-level binding is mirrored into the global table, and
//!   [`SessionFragment::bound_names`] reports the source-level bindings the
//!   interpreter must flush when the fragment exits (normally or through an
//!   exception);
//! - import statements mirror their bindings into the global table and are
//!   not re-initialized on a later import.
//!
//! Standalone module compilation is unchanged; session mode is opt-in.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hydro_ir::{FunctionId, Program};
use rustpython_parser::ast;
use rustpython_parser::lexer::LexicalErrorType;
use rustpython_parser::{Mode, ParseErrorType};

use crate::diag::codes;
use crate::lower::{self, CallSignature, LHir, Lowerer, UserExport, UserModule};
use crate::sema::{self, Infer};
use crate::{placeholder_function, Diagnostic, ModuleLoader};

/// Where a session fragment comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEntry {
    /// Interactive input read from standard input (`<stdin>`).
    Stdin,
    /// Source passed with `tea -c` (`<tea -c>`).
    Command,
    /// A real script file. Its directory anchors user imports.
    File(PathBuf),
}

impl SessionEntry {
    /// Display name used in diagnostics and function tables.
    pub fn filename(&self) -> String {
        match self {
            SessionEntry::Stdin => "<stdin>".to_string(),
            SessionEntry::Command => "<tea -c>".to_string(),
            SessionEntry::File(path) => path.to_string_lossy().into_owned(),
        }
    }
}

/// Configuration for a [`SessionCompiler`].
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Source of every fragment compiled by this session.
    pub entry: SessionEntry,
    /// Module key used by `GlobalGet`/`GlobalSet` for session names. It must
    /// match the key the interpreter uses for its global table.
    pub module_name: String,
    /// Overrides the directory used to resolve user imports. Defaults to the
    /// entry file's directory, or the current directory for virtual entries.
    pub search_root: Option<PathBuf>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            entry: SessionEntry::Stdin,
            module_name: "__main__".to_string(),
            search_root: None,
        }
    }
}

impl SessionOptions {
    /// A session reading from standard input.
    pub fn stdin() -> Self {
        SessionOptions::default()
    }

    /// A session compiling `tea -c` source fragments.
    pub fn command() -> Self {
        SessionOptions {
            entry: SessionEntry::Command,
            ..SessionOptions::default()
        }
    }

    /// A session compiling fragments of `path`.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        SessionOptions {
            entry: SessionEntry::File(path.into()),
            ..SessionOptions::default()
        }
    }

    /// Sets the runtime module key for session globals.
    pub fn with_module_name(mut self, name: impl Into<String>) -> Self {
        self.module_name = name.into();
        self
    }

    /// Overrides the import search root.
    pub fn with_search_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.search_root = Some(root.into());
        self
    }
}

/// One import name bound by a fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionImport {
    /// Name bound in the session namespace.
    pub binding: String,
    /// Module named by the import.
    pub module: String,
    /// Imported member (`from module import member`), or `None` for a plain
    /// `import module`.
    pub member: Option<String>,
}

/// The result of compiling one session fragment.
#[derive(Clone, Debug)]
pub struct SessionFragment {
    /// The fragment's program. Function ids are stable for the whole session:
    /// `functions` contains every function emitted so far, and `main` is this
    /// fragment's module body.
    pub program: Arc<Program>,
    /// Module bodies introduced by this fragment, in dependency order. These
    /// must run before [`Program::main`]; modules from earlier fragments do
    /// not appear here.
    pub initializers: Vec<FunctionId>,
    /// Every source-level module binding of this fragment, in source order
    /// (imports included). The interpreter flushes these names from module
    /// locals into the global table when the fragment exits.
    pub bound_names: Vec<String>,
    /// Import bindings resolved by this fragment.
    pub imports: Vec<SessionImport>,
}

/// Compiles REPL/`-c`/script fragments against one persistent namespace.
pub struct SessionCompiler {
    options: SessionOptions,
    loader: ModuleLoader,
    /// Exports of every loader unit, indexed like `loader.units`.
    exports: Vec<HashMap<String, UserExport>>,
    /// Whether each loader unit writes its own namespace at runtime, indexed
    /// like `loader.units` (see [`UserModule::dynamic_globals`]).
    dynamic_units: Vec<bool>,
    /// Compiled-extension paths each loader unit star-imports, indexed like
    /// `loader.units` (see [`UserModule::extension_stars`]).
    extension_star_units: Vec<Vec<String>>,
    /// Class constructors exported by earlier units, keyed by module and
    /// class name, so a later fragment can bind an imported class call.
    class_registry: HashMap<(String, String), (FunctionId, CallSignature)>,
    /// Function signatures exported by earlier units, keyed by absolute id.
    function_registry: HashMap<FunctionId, CallSignature>,
    /// Every emitted function of the session, in id order.
    functions: Vec<hydro_ir::Function>,
    /// Number of loader units already lowered.
    lowered_units: usize,
    /// Names bound by earlier fragments, in first-binding order.
    global_names: Vec<String>,
    global_name_set: HashSet<String>,
    /// Monotonic fragment counter; keeps virtual loader keys distinct.
    fragment: u64,
}

impl Default for SessionCompiler {
    fn default() -> Self {
        SessionCompiler::new(SessionOptions::default())
    }
}

/// Loader and emission state saved before a fragment, so a failed fragment
/// leaves the session usable.
struct Snapshot {
    units: usize,
    diags: usize,
    loaded: HashMap<PathBuf, usize>,
    by_name: HashMap<String, usize>,
    failed: HashSet<PathBuf>,
    stack: usize,
    namespaces: usize,
    exports: usize,
    lowered_units: usize,
    functions: usize,
    class_registry: HashMap<(String, String), (FunctionId, CallSignature)>,
    function_registry: HashMap<FunctionId, CallSignature>,
}

impl SessionCompiler {
    /// Creates a session with no bound names and no loaded modules.
    pub fn new(options: SessionOptions) -> Self {
        SessionCompiler {
            options,
            loader: ModuleLoader::default(),
            exports: Vec::new(),
            dynamic_units: Vec::new(),
            extension_star_units: Vec::new(),
            class_registry: HashMap::new(),
            function_registry: HashMap::new(),
            functions: Vec::new(),
            lowered_units: 0,
            global_names: Vec::new(),
            global_name_set: HashSet::new(),
            fragment: 0,
        }
    }

    /// The session configuration.
    pub fn options(&self) -> &SessionOptions {
        &self.options
    }

    /// Names bound by earlier fragments, in first-binding order.
    pub fn globals(&self) -> &[String] {
        &self.global_names
    }

    /// Compiles one fragment of `source` for the configured entry.
    pub fn compile(&mut self, source: &str) -> Result<SessionFragment, Vec<Diagnostic>> {
        let filename = self.options.entry.filename();
        let import_root = self.options.search_root.clone();
        self.compile_source(source, &filename, import_root)
    }

    /// Reads and compiles `path` as one fragment.
    ///
    /// Imports resolve from the file's directory. The file is compiled as a
    /// fresh unit even if an earlier fragment already used the same path.
    pub fn compile_file(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<SessionFragment, Vec<Diagnostic>> {
        let path = path.as_ref();
        let filename = path.to_string_lossy().into_owned();
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                return Err(vec![Diagnostic::new(
                    &filename,
                    1,
                    1,
                    format!("cannot read module '{}': {error}", path.display()),
                )
                .with_code(codes::UNSUPPORTED)
                .with_help("check the path and file permissions")]);
            }
        };
        self.compile_source(&source, &filename, None)
    }

    fn compile_source(
        &mut self,
        source: &str,
        filename: &str,
        import_root: Option<PathBuf>,
    ) -> Result<SessionFragment, Vec<Diagnostic>> {
        let snapshot = self.snapshot();
        let result = self.compile_inner(source, filename, import_root);
        if result.is_err() {
            self.restore(snapshot);
        }
        result
    }

    fn compile_inner(
        &mut self,
        source: &str,
        filename: &str,
        import_root: Option<PathBuf>,
    ) -> Result<SessionFragment, Vec<Diagnostic>> {
        self.fragment += 1;
        let key = PathBuf::from(format!("<session-entry-{}>", self.fragment));
        let entry_index = match import_root {
            Some(root) => {
                self.loader
                    .load_source_at(key, source.to_string(), filename.to_string(), root)?
            }
            None => self
                .loader
                .load_source(key, source.to_string(), filename.to_string())?,
        };

        let unit_count = self.loader.units.len();
        self.exports.resize(unit_count, HashMap::new());
        self.dynamic_units.resize(unit_count, false);
        self.extension_star_units.resize(unit_count, Vec::new());
        let first_new_unit = self.lowered_units;
        let mut initializers: Vec<FunctionId> = Vec::new();
        let mut entry_main: Option<FunctionId> = None;

        let parsed = &self.loader.units[entry_index];
        let bound_names = source_bound_names(&parsed.module);
        let imports = source_imports(&parsed.module);

        for index in first_new_unit..unit_count {
            let unit = &self.loader.units[index];
            let is_entry = index == entry_index;
            let base = self.functions.len() as FunctionId;

            let mut infer = Infer::new();
            let mut lowerer = Lowerer::new(&unit.source, &unit.display, &mut infer);
            lowerer.set_templates(&unit.templates);
            lowerer.set_fid_base(base);
            // A session fragment is a compilation unit of its own: every
            // module-level binding must survive into the shared global table.
            lowerer.set_mirror_all_globals(true);
            lowerer
                .set_imported_classes(self.class_registry.clone(), self.function_registry.clone());
            if is_entry {
                lowerer.set_session_mode(true);
                lowerer.set_session_globals(self.global_names.iter().cloned());
                lowerer.set_dunder_name(&self.options.module_name);
            } else {
                lowerer.set_dunder_name(&unit.name);
                lowerer.set_package(&unit.package);
                initializers.push(base);
            }

            let mut user_modules = HashMap::new();
            let mut visible: Vec<usize> = Vec::new();
            let mut pending: Vec<usize> = unit.deps.clone();
            while let Some(dep_index) = pending.pop() {
                if visible.contains(&dep_index) {
                    continue;
                }
                visible.push(dep_index);
                pending.extend_from_slice(&self.loader.units[dep_index].deps);
            }
            for dep_index in visible {
                let dep = &self.loader.units[dep_index];
                user_modules.insert(
                    dep.name.clone(),
                    UserModule {
                        name: dep.name.clone(),
                        exports: self.exports[dep_index].clone(),
                        all: lower::module_all_names(&dep.module.body),
                        dynamic_globals: self.dynamic_units[dep_index],
                        extension_stars: self.extension_star_units[dep_index].clone(),
                    },
                );
            }
            for namespace in &self.loader.namespaces {
                user_modules
                    .entry(namespace.clone())
                    .or_insert_with(|| UserModule {
                        name: namespace.clone(),
                        exports: HashMap::new(),
                        all: None,
                        dynamic_globals: false,
                        extension_stars: Vec::new(),
                    });
            }
            lowerer.set_user_modules(user_modules);
            lowerer.set_late_modules(unit.late.iter().cloned().collect());
            lowerer.set_deferred_imports(
                unit.deferred
                    .iter()
                    .map(|(module, _)| module.clone())
                    .collect(),
            );

            let mut hir = lowerer.lower_module(&unit.module);
            let exported = lowerer.exported_names();
            let dynamic_globals = lowerer.exports_dynamic_globals();
            let extension_stars = lowerer.exported_extension_stars();
            let exported_classes = lowerer.exported_class_signatures();
            let exported_functions = lowerer.exported_function_signatures();
            let lower_diags = std::mem::take(&mut lowerer.diags);
            drop(lowerer);
            if !lower_diags.is_empty() {
                return Err(lower_diags);
            }

            for (class, init, mut signature) in exported_classes {
                signature.default_exprs = vec![None; signature.default_exprs.len()];
                self.class_registry
                    .insert((unit.name.clone(), class), (init, signature));
            }
            for (id, mut signature) in exported_functions {
                signature.default_exprs = vec![None; signature.default_exprs.len()];
                self.function_registry.insert(id, signature);
            }
            self.exports[index] = exported;
            self.dynamic_units[index] = dynamic_globals;
            self.extension_star_units[index] = extension_stars;

            if !is_entry {
                // Same stable identity scheme as a whole-file check: imported
                // module bodies and their functions are prefixed with the
                // dotted module name.
                for function in &mut hir.functions {
                    function.name = format!("{}.{}", unit.name, function.name);
                }
            }

            // Type-check this unit with placeholders for every function
            // emitted so far, so cross-fragment calls see their signatures.
            let mut combined: Vec<crate::lower::HFunction> = self
                .functions
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
            self.functions.extend(emitted.functions);
            if is_entry {
                entry_main = Some(base);
            }
        }

        self.lowered_units = unit_count;
        for name in &bound_names {
            if self.global_name_set.insert(name.clone()) {
                self.global_names.push(name.clone());
            }
        }

        let entry_main = entry_main.expect("session fragment always lowers its entry unit");
        let program = Arc::new(Program {
            source_name: filename.to_string(),
            functions: self.functions.clone(),
            main: entry_main,
            initializers: initializers.clone(),
        });
        Ok(SessionFragment {
            program,
            initializers,
            bound_names,
            imports,
        })
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            units: self.loader.units.len(),
            diags: self.loader.diags.len(),
            loaded: self.loader.loaded.clone(),
            by_name: self.loader.by_name.clone(),
            failed: self.loader.failed.clone(),
            stack: self.loader.stack.len(),
            namespaces: self.loader.namespaces.len(),
            exports: self.exports.len(),
            lowered_units: self.lowered_units,
            functions: self.functions.len(),
            class_registry: self.class_registry.clone(),
            function_registry: self.function_registry.clone(),
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.loader.units.truncate(snapshot.units);
        self.loader.diags.truncate(snapshot.diags);
        self.loader.loaded = snapshot.loaded;
        self.loader.by_name = snapshot.by_name;
        self.loader.failed = snapshot.failed;
        self.loader.stack.truncate(snapshot.stack);
        self.loader.namespaces.truncate(snapshot.namespaces);
        self.exports.truncate(snapshot.exports);
        self.dynamic_units.truncate(snapshot.exports);
        self.extension_star_units.truncate(snapshot.exports);
        self.lowered_units = snapshot.lowered_units;
        self.functions.truncate(snapshot.functions);
        self.class_registry = snapshot.class_registry;
        self.function_registry = snapshot.function_registry;
    }
}

/// Whether `source` is an incomplete fragment that a REPL should buffer.
///
/// True for rustpython-parser EOF errors (unfinished blocks and statements)
/// and for unbalanced brackets or unfinished string literals, which a
/// multi-line prompt continues. A genuine syntax error returns false so it is
/// reported immediately.
pub fn is_incomplete(source: &str, filename: &str) -> bool {
    match rustpython_parser::parse(source, Mode::Module, filename) {
        Ok(_) => false,
        Err(error) => {
            // `UnrecognizedEof` with only `Indent` expected is converted by
            // rustpython-parser into an `IndentationError` holding the EOF
            // position. An open block (`def f():\n`) ends exactly there; a
            // real indentation error points inside the source.
            let at_eof = u32::from(error.offset) as usize >= source.trim_end().len();
            match error.error {
                ParseErrorType::Eof => true,
                // Unclosed brackets and unfinished triple-quoted strings
                // surface as a lexical EOF. An unmatched *closing* bracket is
                // a `NestingError` and stays a genuine syntax error.
                ParseErrorType::Lexical(LexicalErrorType::Eof) => true,
                ParseErrorType::Lexical(LexicalErrorType::IndentationError) => at_eof,
                _ => false,
            }
        }
    }
}

/// Ordered, de-duplicated source-level bindings of a fragment's module body.
fn source_bound_names(module: &ast::ModModule) -> Vec<String> {
    let mut set = HashSet::new();
    lower::collect_source_bound_names(&module.body, &mut set, true);
    let mut names: Vec<String> = set
        .into_iter()
        .filter(|name| !name.starts_with('$'))
        .collect();
    names.sort();
    names
}

/// Imports of a fragment's module body, including imports in module-level
/// compound statements but not those inside functions or classes.
fn source_imports(module: &ast::ModModule) -> Vec<SessionImport> {
    fn walk(stmts: &[ast::Stmt], out: &mut Vec<SessionImport>) {
        for stmt in stmts {
            match stmt {
                ast::Stmt::Import(s) => {
                    for alias in &s.names {
                        out.push(SessionImport {
                            binding: import_binding(alias),
                            module: alias.name.to_string(),
                            member: None,
                        });
                    }
                }
                ast::Stmt::ImportFrom(s) => {
                    if s.module
                        .as_ref()
                        .is_some_and(|module| module.as_str() == "__future__")
                    {
                        continue;
                    }
                    let module = s
                        .module
                        .as_ref()
                        .map(|module| module.to_string())
                        .unwrap_or_default();
                    for alias in &s.names {
                        if alias.name.as_str() == "*" {
                            continue;
                        }
                        out.push(SessionImport {
                            binding: import_binding(alias),
                            module: module.clone(),
                            member: Some(alias.name.to_string()),
                        });
                    }
                }
                ast::Stmt::If(s) => {
                    walk(&s.body, out);
                    walk(&s.orelse, out);
                }
                ast::Stmt::For(s) => {
                    walk(&s.body, out);
                    walk(&s.orelse, out);
                }
                ast::Stmt::AsyncFor(s) => {
                    walk(&s.body, out);
                    walk(&s.orelse, out);
                }
                ast::Stmt::While(s) => {
                    walk(&s.body, out);
                    walk(&s.orelse, out);
                }
                ast::Stmt::With(s) => walk(&s.body, out),
                ast::Stmt::AsyncWith(s) => walk(&s.body, out),
                ast::Stmt::Try(s) => {
                    walk(&s.body, out);
                    for handler in &s.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        walk(&handler.body, out);
                    }
                    walk(&s.orelse, out);
                    walk(&s.finalbody, out);
                }
                ast::Stmt::TryStar(s) => {
                    walk(&s.body, out);
                    for handler in &s.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        walk(&handler.body, out);
                    }
                    walk(&s.orelse, out);
                    walk(&s.finalbody, out);
                }
                _ => {}
            }
        }
    }

    let mut imports = Vec::new();
    walk(&module.body, &mut imports);
    imports
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use hydro_ir::{Builtin, ExprKind, StmtKind};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn compile(compiler: &mut SessionCompiler, source: &str) -> SessionFragment {
        compiler
            .compile(source)
            .unwrap_or_else(|diags| panic!("unexpected diagnostics: {diags:?}"))
    }

    fn global_get(expr: &hydro_ir::Expr) -> Option<(&str, &str)> {
        match &expr.kind {
            ExprKind::GlobalGet { module, name } => Some((module.as_str(), name.as_str())),
            _ => None,
        }
    }

    fn expr_of_first_assign(program: &Program) -> &hydro_ir::Expr {
        let function = program.main_function();
        function
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                // Imported units mirror `__name__`; a session fragment may
                // carry that binding before the user assignment, so look for
                // the first assignment that is not the module dunder.
                StmtKind::Assign { target, value }
                    if function.local(*target).name != "__name__" =>
                {
                    Some(value)
                }
                _ => None,
            })
            .expect("fragment must assign a value")
    }

    #[test]
    fn session_name_shadows_builtin() {
        let mut compiler = SessionCompiler::new(SessionOptions::stdin());
        compile(&mut compiler, "list = 41\n");
        let fragment = compile(&mut compiler, "print(list)\n");

        let print = fragment
            .program
            .main_function()
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Expr(expr) => match &expr.kind {
                    ExprKind::Call { callee, args }
                        if *callee == hydro_ir::Callee::Builtin(Builtin::Print) =>
                    {
                        args.first()
                    }
                    _ => None,
                },
                _ => None,
            })
            .expect("print call");
        assert_eq!(global_get(print), Some(("__main__", "list")));
    }

    #[test]
    fn earlier_name_is_read_without_a_local() {
        let mut compiler = SessionCompiler::new(SessionOptions::stdin());
        compile(&mut compiler, "x = 1\n");
        let fragment = compile(&mut compiler, "y = x + 1\n");
        let value = expr_of_first_assign(&fragment.program);
        match &value.kind {
            ExprKind::Bin { lhs, .. } => {
                assert_eq!(global_get(lhs), Some(("__main__", "x")));
            }
            other => panic!("expected binop, got {other:?}"),
        }
    }

    #[test]
    fn function_and_class_definitions_persist() {
        let mut compiler = SessionCompiler::new(SessionOptions::stdin());
        compile(
            &mut compiler,
            "def add(a, b):\n    return a + b\n\nclass Point:\n    pass\n",
        );
        let fragment = compile(&mut compiler, "add(1, 2)\nPoint()\n");

        let mut callees = Vec::new();
        for stmt in &fragment.program.main_function().body {
            if let StmtKind::Expr(expr) = &stmt.kind {
                if let ExprKind::IndirectCall { callee, .. } = &expr.kind {
                    if let Some((_, name)) = global_get(callee) {
                        callees.push(name.to_string());
                    }
                }
            }
        }
        assert!(callees.iter().any(|name| name == "add"), "{callees:?}");
        assert!(callees.iter().any(|name| name == "Point"), "{callees:?}");
        // Functions of the first fragment stay addressable from the second
        // fragment's program.
        assert!(fragment
            .program
            .functions
            .iter()
            .any(|function| function.name == "add"));
    }

    #[test]
    fn failed_fragment_does_not_change_the_session() {
        let mut compiler = SessionCompiler::new(SessionOptions::stdin());
        compile(&mut compiler, "x = 1\n");
        let diags = compiler
            .compile("y = 1 + 'a'\n")
            .expect_err("type error must fail");
        assert!(!diags.is_empty());
        assert!(!compiler.globals().iter().any(|name| name == "y"));
        let fragment = compile(&mut compiler, "print(x)\n");
        let print = fragment
            .program
            .main_function()
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Expr(expr) => match &expr.kind {
                    ExprKind::Call { callee, args }
                        if *callee == hydro_ir::Callee::Builtin(Builtin::Print) =>
                    {
                        Some(args)
                    }
                    _ => None,
                },
                _ => None,
            })
            .expect("print call");
        assert_eq!(global_get(&print[0]), Some(("__main__", "x")));
    }

    #[test]
    fn augmented_assignment_of_an_earlier_name_uses_the_global_table() {
        let mut compiler = SessionCompiler::new(SessionOptions::stdin());
        compile(&mut compiler, "counter = 1\n");
        let fragment = compile(&mut compiler, "counter += 1\n");
        let main = fragment.program.main_function();
        let stores: Vec<&str> = main
            .body
            .iter()
            .filter_map(|stmt| match &stmt.kind {
                StmtKind::GlobalSet { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(stores.contains(&"counter"), "{stores:?}");
    }

    // ------------------------------------------------------------------
    // Imports
    // ------------------------------------------------------------------

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(test_name: &str) -> TestDir {
            let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hydro-session-{}-{test_name}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            TestDir { path }
        }

        fn write(&self, name: &str, source: &str) {
            fs::write(self.path.join(name), source).expect("write source");
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn session_in(dir: &TestDir) -> SessionCompiler {
        SessionCompiler::new(SessionOptions {
            entry: SessionEntry::Stdin,
            module_name: "__main__".to_string(),
            search_root: Some(dir.path.clone()),
        })
    }

    #[test]
    fn imports_bind_and_initialize_once() {
        let dir = TestDir::new("imports");
        dir.write("helper.py", "def add(a, b):\n    return a + b\n");
        let mut compiler = session_in(&dir);

        let first = compile(&mut compiler, "import helper\nfrom helper import add\n");
        assert_eq!(
            first.initializers.len(),
            1,
            "helper body must be initialized"
        );
        let helper_body = first.initializers[0] as usize;
        assert_eq!(first.program.functions[helper_body].name, "helper.<module>");
        let main_names: Vec<&str> = first
            .program
            .main_function()
            .body
            .iter()
            .filter_map(|stmt| match &stmt.kind {
                StmtKind::GlobalSet { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(main_names.contains(&"helper"), "{main_names:?}");
        assert!(main_names.contains(&"add"), "{main_names:?}");
        assert!(first.bound_names.iter().any(|name| name == "helper"));
        assert!(first.bound_names.iter().any(|name| name == "add"));

        let second = compile(&mut compiler, "import helper\nfrom helper import add\n");
        assert!(
            second.initializers.is_empty(),
            "an already initialized module must not run again"
        );
        assert_eq!(second.imports.len(), 2);
        let module_bodies = second
            .program
            .functions
            .iter()
            .filter(|function| function.name == "<module>")
            .count();
        assert_eq!(
            module_bodies, 2,
            "each fragment entry body keeps MODULE_NAME"
        );
        assert!(
            second
                .program
                .functions
                .iter()
                .any(|function| function.name == "helper.<module>"),
            "the imported helper body keeps its module-qualified name"
        );
    }

    #[test]
    fn session_import_name_resolves_in_a_later_fragment() {
        let dir = TestDir::new("import-call");
        dir.write("helper.py", "def add(a, b):\n    return a + b\n");
        let mut compiler = session_in(&dir);
        compile(&mut compiler, "from helper import add\n");
        let fragment = compile(&mut compiler, "add(1, 2)\n");
        let callee = fragment
            .program
            .main_function()
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                StmtKind::Expr(expr) => match &expr.kind {
                    ExprKind::IndirectCall { callee, .. } => Some(callee),
                    _ => None,
                },
                _ => None,
            })
            .expect("indirect call");
        assert_eq!(global_get(callee), Some(("__main__", "add")));
    }

    // ------------------------------------------------------------------
    // Bound names
    // ------------------------------------------------------------------

    #[test]
    fn bound_names_cover_every_module_binding_form() {
        let mut compiler = SessionCompiler::new(SessionOptions::stdin());
        let fragment = compile(
            &mut compiler,
            "a = 1\nb: int = 2\nc = 0\nc += 1\n\
             def f():\n    hidden = 1\n\n\
             class C:\n    attr = 1\n\n\
             for i in [1, 2]:\n    pass\n\
             with open('x') as w:\n    pass\n\
             try:\n    pass\nexcept Exception as err:\n    x = 1\n\
             p, *rest = [1, 2, 3]\n\
             import os\nfrom os import getcwd as os_path\n",
        );
        for expected in [
            "a", "b", "c", "f", "C", "i", "w", "err", "x", "p", "rest", "os", "os_path",
        ] {
            assert!(
                fragment.bound_names.iter().any(|name| name == expected),
                "missing binding {expected}: {:?}",
                fragment.bound_names
            );
        }
        assert!(!fragment.bound_names.iter().any(|name| name == "hidden"));
        assert!(!fragment.bound_names.iter().any(|name| name == "attr"));
        assert!(!fragment
            .bound_names
            .iter()
            .any(|name| name.starts_with('$')));
    }

    #[test]
    fn normal_compilation_keeps_its_semantics() {
        // The session API does not change standalone checks: an unknown name
        // is still a compile error, and no global read is synthesized.
        let diags = crate::check_source("value = 1\nprint(valu)\n", "test.py")
            .expect_err("unknown name must fail");
        assert!(diags.iter().any(|diag| diag.code == codes::NAME));
    }

    // ------------------------------------------------------------------
    // Incomplete input
    // ------------------------------------------------------------------

    #[test]
    fn incomplete_input_is_detected() {
        assert!(is_incomplete("def f():\n", "<stdin>"));
        assert!(is_incomplete("if True:\n", "<stdin>"));
        assert!(!is_incomplete("if True:\n    x = 1\n", "<stdin>"));
        assert!(is_incomplete("x = [1, 2\n", "<stdin>"));
        assert!(is_incomplete("x = (1,\n", "<stdin>"));
        assert!(is_incomplete("x = '''abc\n", "<stdin>"));
        assert!(!is_incomplete("x = 1\n", "<stdin>"));
        assert!(!is_incomplete("def f():\n    pass\n", "<stdin>"));
        assert!(!is_incomplete("def broken(:\n    pass\n", "<stdin>"));
        assert!(!is_incomplete("x = )\n", "<stdin>"));
    }
}
