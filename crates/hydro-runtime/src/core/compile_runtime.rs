//! Runtime Python compiler and evaluator behind `compile`, `exec` and `eval`.
//!
//! Hydro normally consumes Python source ahead of time: the frontend parses,
//! type-checks and lowers it, and codegen emits machine code. The standard
//! library nevertheless compiles snippets at run time (`test.support`,
//! `typing_inspection`, `six`, dataclass factories, doctest-style probes), so
//! the runtime needs a real path that parses and executes source *now*.
//!
//! # Architecture
//!
//! This module is deliberately self-contained: it depends on the workspace
//! parser (`hydro-python-parser`) but not on `hydro-frontend` or
//! `hydro-interp`. The dependency direction forbids the latter
//! (`hydro-interp` already depends on this crate) and the typed frontend
//! rejects the dynamically typed, unbound-name programs `exec`/`eval` must
//! accept. The evaluator therefore walks the RustPython AST directly and
//! expresses every value as a runtime tagged `i64`, using the same operators,
//! containers, call binder, exception machinery and import loader the
//! generated code uses.
//!
//! * [`builtin_compile`] parses source (str/bytes, PEP 263 encoded) into a
//!   code object. The compiled unit is kept in a process-wide table keyed by
//!   the code object's identity; the object is rooted for the process
//!   lifetime so its identity cannot be recycled.
//! * [`builtin_exec`] / [`builtin_eval`] accept source text, bytes or such a
//!   code object, resolve the namespace arguments, and run the unit.
//!
//! # Functions defined by dynamic source
//!
//! A `def` executed here becomes an ordinary runtime function value. Since
//! this crate cannot add a `FunctionBody` variant (the value model is owned
//! elsewhere), the function is materialised as a
//! [`crate::value::FunctionBody::Wrapper`] whose entry point is the shared
//! [`dyn_full_entry`] trampoline. The compiled definition is identified by a
//! tagged integer in the function's signature, alongside the defining
//! namespace and closure environments, so the entry point can recover the
//! body from the registry without embedding a Rust pointer per function. The
//! runtime keyword binder fills defaults, `*args`, keyword-only slots and
//! `**kwargs` before the trampoline runs, exactly as it does for generated
//! code; `FUNCTION_FLAG_VARARGS | FUNCTION_FLAG_DYNAMIC_DEFAULTS` routes every
//! call through that binder.
//!
//! # Rooting
//!
//! Every value the evaluator holds across a runtime call is registered on a
//! growable shadow root frame ([`Roots`]) owned by the active [`Interp`].
//! Values are returned to the caller unrooted and the caller roots them
//! immediately, which is allocation-free. Statement boundaries truncate the
//! frame, so loops do not grow the root stack without bound.
//!
//! # Scope model
//!
//! Module/`exec` namespaces are real dicts. A function body gets a fresh
//! locals dict plus a chain of captured environment dicts (innermost first),
//! then the globals dict, then `builtins`. `nonlocal` assignment is not
//! supported by the evaluator (reads through the captured chain are), and any
//! construct the evaluator cannot execute raises the real tagged error
//! (`NotImplementedError` for a valid-but-unsupported form, `SyntaxError` for
//! invalid source) instead of silently doing nothing.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use hydro_ir::{BinOp, ExcKind, ParamKind, UnOp};
use num_bigint::BigInt;
use rustpython_parser::{ast, parse, Mode};

use crate::core::builtins as runtime_builtins;
use crate::core::globals;
use crate::{abi, container, exc, heap, iter, methods, modules, ops, value};

/// Hidden signature-slot names. They cannot collide with a Python identifier,
/// so user keyword arguments never bind them.
const HIDDEN_ID: &str = "\u{0}hydro-id";
const HIDDEN_GLOBALS: &str = "\u{0}hydro-globals";
const HIDDEN_ENV: &str = "\u{0}hydro-env";

/// Source mode of one compiled unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnitMode {
    Exec,
    Eval,
    Single,
}

/// One compiled source unit. The AST is owned here for the process lifetime
/// while a code object produced by [`builtin_compile`] refers to it.
struct Unit {
    mode: UnitMode,
    filename: String,
    first_line: u32,
    /// Bit set of `compile()` flags; kept for introspection and `__future__`
    /// compatibility. Execution does not depend on it.
    flags: i64,
    module: ast::Mod,
}

/// Process-wide code-object table.
struct CodeUnits {
    units: Vec<Arc<Unit>>,
    by_code: HashMap<i64, usize>,
    /// Root tokens keeping every registered code object alive.
    roots: Vec<usize>,
}

static UNITS: LazyLock<Mutex<CodeUnits>> = LazyLock::new(|| {
    Mutex::new(CodeUnits {
        units: Vec::new(),
        by_code: HashMap::new(),
        roots: Vec::new(),
    })
});

fn units_lock() -> std::sync::MutexGuard<'static, CodeUnits> {
    UNITS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Body of a dynamically compiled function.
#[derive(Clone)]
enum DynDef {
    Function {
        args: ast::Arguments,
        body: Vec<ast::Stmt>,
    },
    Lambda {
        args: ast::Arguments,
        body: Box<ast::Expr>,
    },
}

/// A registered dynamic function body plus its call-frame layout.
struct RegisteredDef {
    def: DynDef,
    /// User-visible parameter slots in binder order (positional, `*args`,
    /// keyword-only, `**kwargs`). The binder produces exactly one value per
    /// entry, after the hidden prefix slots.
    slot_names: Vec<String>,
    /// Number of leading slots the binder fills from signature defaults
    /// (identity, globals and the captured environments).
    hidden: usize,
}

static DEFS: LazyLock<Mutex<Vec<Arc<RegisteredDef>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn defs_lock() -> std::sync::MutexGuard<'static, Vec<Arc<RegisteredDef>>> {
    DEFS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Control transfer out of a statement list.
#[derive(Clone, Copy, Debug)]
enum Flow {
    Return(i64),
    Break,
    Continue,
    /// An exception is pending in the runtime; the handler stack has not been
    /// inspected yet.
    Raise,
}

type ExecResult = Result<(), Flow>;
type EvalResult = Result<i64, Flow>;

/// Sets the pending exception and returns the matching control signal.
fn raise_tag(kind: ExcKind, message: &str) -> Flow {
    exc::raise_with_message(kind.tag(), message);
    Flow::Raise
}

fn raise_syntax(message: &str) -> Flow {
    raise_tag(ExcKind::SyntaxError, message)
}

fn type_error(message: &str) -> Flow {
    raise_tag(ExcKind::TypeError, message)
}

/// Reports a `SourceError` as the pending exception and returns `None`.
fn report_source_error(error: SourceError) -> i64 {
    let (tag, message) = match error {
        SourceError::Type(message) => (ExcKind::TypeError.tag(), message),
        SourceError::Syntax(message) => (ExcKind::SyntaxError.tag(), message),
    };
    exc::raise_with_message(tag, &message)
}

/// Failure to turn a Python value into source text.
enum SourceError {
    Type(String),
    Syntax(String),
}

// ---------------------------------------------------------------------------
// Root stack
// ---------------------------------------------------------------------------

/// Growable precise-root stack for one evaluator activation.
///
/// Mirrors the interpreter's scratch frame: the buffer is registered with the
/// collector and the used prefix is rescanned. Growing detaches the old
/// registration, allocates a new buffer (Rust allocation never triggers a
/// Hydro collection) and re-registers it.
struct Roots {
    slots: Vec<i64>,
    token: usize,
    used: usize,
}

impl Roots {
    const INITIAL: usize = 32;

    fn new() -> Roots {
        let mut slots = vec![value::NONE; Self::INITIAL];
        let token = abi::hydro_gc_frame_push(slots.as_mut_ptr(), slots.len());
        abi::hydro_gc_frame_used(token, 0);
        Roots {
            slots,
            token,
            used: 0,
        }
    }

    fn push(&mut self, item: i64) {
        if self.used == self.slots.len() {
            self.grow();
        }
        self.slots[self.used] = item;
        self.used += 1;
        abi::hydro_gc_frame_used(self.token, self.used);
    }

    fn mark(&self) -> usize {
        self.used
    }

    fn truncate(&mut self, mark: usize) {
        if mark < self.used {
            self.used = mark;
            abi::hydro_gc_frame_used(self.token, self.used);
        }
    }

    fn grow(&mut self) {
        let capacity = self.slots.len() * 2;
        abi::hydro_gc_frame_pop(self.token);
        let mut slots = vec![value::NONE; capacity];
        slots[..self.used].copy_from_slice(&self.slots[..self.used]);
        self.token = abi::hydro_gc_frame_push(slots.as_mut_ptr(), capacity);
        self.slots = slots;
        abi::hydro_gc_frame_used(self.token, self.used);
    }
}

impl Drop for Roots {
    fn drop(&mut self) {
        if self.token != 0 {
            abi::hydro_gc_frame_pop(self.token);
        }
    }
}

// ---------------------------------------------------------------------------
// Namespace helpers
// ---------------------------------------------------------------------------

fn dict_lookup(dict: i64, name: &str) -> Option<i64> {
    value::as_dict(dict)?
        .iter()
        .find_map(|(key, item)| (value::as_str(*key) == Some(name)).then_some(*item))
}

fn dict_set_str(dict: i64, roots: &mut Roots, name: &str, item: i64) -> i64 {
    let key = value::str_alloc(name.to_string());
    roots.push(key);
    roots.push(item);
    container::dict_set(dict, key, item)
}

/// Encodes a small Rust integer as a tagged runtime integer.
fn int_value(n: i64) -> i64 {
    value::encode_int(n).unwrap_or_else(|| value::bigint_new(BigInt::from(n)))
}

/// Runtime integer for a decimal literal of arbitrary size.
fn integer_literal(text: &str) -> i64 {
    match text.parse::<i64>() {
        Ok(n) => ops::int_result(n),
        Err(_) => match BigInt::parse_bytes(text.as_bytes(), 10) {
            Some(n) => value::bigint_new(n),
            None => value::NONE,
        },
    }
}

fn line_of_offset(source: &str, offset: rustpython_parser::text_size::TextSize) -> usize {
    let mut offset = (u32::from(offset) as usize).min(source.len());
    while offset > 0 && !source.is_char_boundary(offset) {
        offset -= 1;
    }
    1 + source[..offset].matches('\n').count()
}

fn module_label(globals: i64) -> String {
    dict_lookup(globals, "__name__")
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "__main__".to_string())
}

// ---------------------------------------------------------------------------
// Source decoding (PEP 263)
// ---------------------------------------------------------------------------

fn source_text(source: i64) -> Result<String, SourceError> {
    if let Some(text) = value::as_str(source) {
        return Ok(text.to_string());
    }
    if let Some(bytes) = value::as_bytes(source) {
        return decode_source_bytes(bytes).map_err(SourceError::Syntax);
    }
    Err(SourceError::Type(format!(
        "source must be a string or bytes-like object, not '{}'",
        value::type_name(source)
    )))
}

fn decode_source_bytes(bytes: &[u8]) -> Result<String, String> {
    let mut data = bytes;
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        data = &data[3..];
    }
    let mut encoding = "utf-8".to_string();
    for line in String::from_utf8_lossy(data).lines().take(2) {
        if let Some(cookie) = coding_cookie(line) {
            encoding = cookie;
            break;
        }
    }
    match normalize_encoding(&encoding).ok_or_else(|| format!("unknown encoding: {encoding}"))? {
        "utf-8" => {
            String::from_utf8(data.to_vec()).map_err(|_| "invalid utf-8 in source code".to_string())
        }
        "latin-1" => Ok(data.iter().map(|byte| *byte as char).collect()),
        _ => {
            if data.iter().all(|byte| *byte < 0x80) {
                Ok(data.iter().map(|byte| *byte as char).collect())
            } else {
                Err("non-ASCII byte in ASCII source".to_string())
            }
        }
    }
}

fn coding_cookie(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let index = lower.find("coding")?;
    let rest = trimmed[index + "coding".len()..].trim_start();
    let rest = rest.strip_prefix(':').or_else(|| rest.strip_prefix('='))?;
    let rest = rest.trim_start();
    let end = rest
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.'))
        .unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_string())
}

fn normalize_encoding(name: &str) -> Option<&'static str> {
    let lower = name.trim().to_ascii_lowercase();
    match lower.as_str() {
        "utf-8" | "utf8" | "utf_8" | "utf-8-sig" | "u8" => Some("utf-8"),
        "latin-1" | "latin1" | "latin_1" | "iso-8859-1" | "iso8859-1" | "iso_8859_1" | "8859"
        | "cp819" | "l1" => Some("latin-1"),
        "ascii" | "us-ascii" | "646" => Some("ascii"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Compiled unit registry and code objects
// ---------------------------------------------------------------------------

fn store_unit(unit: Unit) -> i64 {
    let code = value::code_new(value::CodeState {
        name: "<module>".to_string(),
        qualname: "<module>".to_string(),
        filename: unit.filename.clone(),
        first_line: unit.first_line,
        argcount: 0,
        posonlyargcount: 0,
        kwonlyargcount: 0,
        // CO_NEWLOCALS: a module body receives its own namespace.
        flags: 0x0002,
        nlocals: 0,
        varnames: Vec::new(),
    });
    let root = heap::root_values(std::slice::from_ref(&code));
    let mut units = units_lock();
    let id = units.units.len();
    units.units.push(Arc::new(unit));
    units.by_code.insert(code, id);
    units.roots.push(root);
    code
}

fn unit_for_code(code: i64) -> Option<Arc<Unit>> {
    let units = units_lock();
    let id = *units.by_code.get(&code)?;
    units.units.get(id).cloned()
}

// ---------------------------------------------------------------------------
// Function body registry
// ---------------------------------------------------------------------------

fn register_def(def: DynDef, slot_names: Vec<String>, hidden: usize) -> usize {
    let mut defs = defs_lock();
    defs.push(Arc::new(RegisteredDef {
        def,
        slot_names,
        hidden,
    }));
    defs.len() - 1
}

/// Shared entry point of every runtime-compiled function.
///
/// The runtime keyword binder passes the hidden prefix (tagged body id,
/// globals, captured environments) followed by one bound value per declared
/// parameter. The body id recovers the AST from [`DEFS`]; the remaining slots
/// become the function's locals.
extern "C" fn dyn_full_entry(argv: *const i64) -> i64 {
    if argv.is_null() {
        return value::NONE;
    }
    // SAFETY: the binder always passes the full slot vector for the signature
    // attached to the function value; slot 0 is the tagged body id.
    let id_value = unsafe { *argv };
    let Some(id) = value::as_integer(id_value) else {
        return exc::raise_with_message(
            ExcKind::RuntimeError.tag(),
            "runtime-compiled function has a corrupt identity slot",
        );
    };
    let entry = {
        let defs = defs_lock();
        defs.get(id as usize).cloned()
    };
    let Some(entry) = entry else {
        return exc::raise_with_message(
            ExcKind::RuntimeError.tag(),
            "runtime-compiled function body is no longer registered",
        );
    };
    let slots = entry.hidden + entry.slot_names.len();
    // SAFETY: the binder produced exactly `slots` values (signature length).
    let all = unsafe { std::slice::from_raw_parts(argv, slots) };
    run_registered(&entry, all)
}

fn run_registered(entry: &RegisteredDef, all: &[i64]) -> i64 {
    let globals_ns = all[1];
    let env_count = entry.hidden.saturating_sub(2);
    let envs = all[2..2 + env_count].to_vec();
    let params = &all[entry.hidden..];
    let locals = value::dict_new(Vec::new());
    let mut interp = Interp::new(globals_ns, locals, None, envs, String::new(), false);
    for (name, item) in entry.slot_names.iter().zip(params) {
        interp.set_local(name, *item);
    }
    match &entry.def {
        DynDef::Function { body, .. } => match interp.exec_stmts(body) {
            Ok(()) => value::NONE,
            Err(Flow::Return(result)) => result,
            Err(_) => value::NONE,
        },
        DynDef::Lambda { body, .. } => match interp.eval(body) {
            Ok(result) | Err(Flow::Return(result)) => result,
            Err(_) => value::NONE,
        },
    }
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

/// Comprehension output shape.
enum CompKind {
    List(ast::Expr),
    Set(ast::Expr),
    Dict(ast::Expr, ast::Expr),
}

/// One evaluator activation over a namespace.
struct Interp {
    globals: i64,
    locals: i64,
    /// Module key mirrored into the global store and its `globals()` view when
    /// the caller supplied no explicit namespace. `None` for explicit dicts and
    /// for function bodies.
    module: Option<String>,
    /// Captured lexical environments, innermost first.
    captures: Vec<i64>,
    handled: Vec<i64>,
    #[allow(dead_code)]
    filename: String,
    single: bool,
    last_value: i64,
    global_names: Vec<String>,
    nonlocal_names: Vec<String>,
    roots: Roots,
}

impl Interp {
    fn new(
        globals_ns: i64,
        locals_ns: i64,
        module: Option<String>,
        captures: Vec<i64>,
        filename: String,
        single: bool,
    ) -> Interp {
        let mut roots = Roots::new();
        roots.push(globals_ns);
        roots.push(locals_ns);
        for env in &captures {
            roots.push(*env);
        }
        Interp {
            globals: globals_ns,
            locals: locals_ns,
            module,
            captures,
            handled: Vec::new(),
            filename,
            single,
            last_value: value::NONE,
            global_names: Vec::new(),
            nonlocal_names: Vec::new(),
            roots,
        }
    }

    fn environment(&self) -> Vec<i64> {
        let mut envs = Vec::with_capacity(self.captures.len() + 1);
        envs.push(self.locals);
        envs.extend_from_slice(&self.captures);
        envs
    }

    fn lookup(&mut self, name: &str) -> Option<i64> {
        if let Some(item) = dict_lookup(self.locals, name) {
            return Some(item);
        }
        for env in &self.captures {
            if let Some(item) = dict_lookup(*env, name) {
                return Some(item);
            }
        }
        if let Some(item) = dict_lookup(self.globals, name) {
            return Some(item);
        }
        let item = modules::module_get_by_name("builtins", name);
        if exc::pending() {
            // A miss in the builtins module is an `AttributeError`; anything
            // else is a real failure while resolving the builtin name.
            exc::clear();
            return None;
        }
        Some(item)
    }

    fn bind(&mut self, name: &str, item: i64) {
        self.roots.push(item);
        if self.global_names.iter().any(|candidate| candidate == name) {
            let _ = dict_set_str(self.globals, &mut self.roots, name, item);
            self.mirror(name, item);
        } else if self
            .nonlocal_names
            .iter()
            .any(|candidate| candidate == name)
            && !self.captures.is_empty()
        {
            let target = self.captures[0];
            let _ = dict_set_str(target, &mut self.roots, name, item);
        } else {
            let _ = dict_set_str(self.locals, &mut self.roots, name, item);
            self.mirror(name, item);
        }
    }

    /// Publishes a binding into the caller's module store and its `globals()`
    /// view when this activation mirrors the module namespace.
    fn mirror(&mut self, name: &str, item: i64) {
        if let Some(module) = &self.module {
            globals::set(module, name, item);
            globals::set_in_view(module, name, item);
        }
    }

    /// Binds a parameter slot without global mirroring.
    fn set_local(&mut self, name: &str, item: i64) {
        self.roots.push(item);
        let _ = dict_set_str(self.locals, &mut self.roots, name, item);
    }

    // -- statements -------------------------------------------------------

    fn exec_stmts(&mut self, body: &[ast::Stmt]) -> ExecResult {
        for stmt in body {
            let mark = self.roots.mark();
            let result = self.exec_stmt(stmt);
            self.roots.truncate(mark);
            result?;
        }
        Ok(())
    }

    fn exec_stmt(&mut self, stmt: &ast::Stmt) -> ExecResult {
        match stmt {
            ast::Stmt::Expr(node) => {
                let item = self.eval(&node.value)?;
                if self.single {
                    self.roots.push(item);
                    if item != value::NONE {
                        let text = value::str_alloc(value::repr(item));
                        self.roots.push(text);
                        let _ = runtime_builtins::print(1, &text);
                        if exc::pending() {
                            return Err(Flow::Raise);
                        }
                    }
                    self.last_value = item;
                }
                Ok(())
            }
            ast::Stmt::Assign(node) => {
                let item = self.eval(&node.value)?;
                self.roots.push(item);
                for target in &node.targets {
                    self.assign(target, item)?;
                }
                Ok(())
            }
            ast::Stmt::AugAssign(node) => self.exec_aug_assign(node),
            ast::Stmt::AnnAssign(node) => {
                if let Some(item) = &node.value {
                    let evaluated = self.eval(item)?;
                    self.roots.push(evaluated);
                    self.assign(&node.target, evaluated)?;
                }
                Ok(())
            }
            ast::Stmt::Return(node) => {
                let item = match &node.value {
                    Some(expression) => self.eval(expression)?,
                    None => value::NONE,
                };
                Err(Flow::Return(item))
            }
            ast::Stmt::Pass(_) => Ok(()),
            ast::Stmt::Break(_) => Err(Flow::Break),
            ast::Stmt::Continue(_) => Err(Flow::Continue),
            ast::Stmt::If(node) => {
                let test = self.eval(&node.test)?;
                if value::truthy(test) {
                    self.exec_stmts(&node.body)
                } else {
                    self.exec_stmts(&node.orelse)
                }
            }
            ast::Stmt::While(node) => self.exec_while(node),
            ast::Stmt::For(node) => self.exec_for(node),
            ast::Stmt::FunctionDef(node) => {
                let def = DynDef::Function {
                    args: (*node.args).clone(),
                    body: node.body.clone(),
                };
                let function = self.make_function(
                    def,
                    node.name.to_string(),
                    &node.args,
                    &node.decorator_list,
                )?;
                self.bind(&node.name, function);
                Ok(())
            }
            ast::Stmt::ClassDef(node) => self.exec_class(node),
            ast::Stmt::Import(node) => {
                for alias in &node.names {
                    let name = alias.name.to_string();
                    let module = self.import_module(&name)?;
                    match &alias.asname {
                        None => {
                            let root = name.split('.').next().unwrap_or(name.as_str());
                            self.bind(root, module);
                        }
                        Some(binding) => {
                            let mut current = module;
                            for part in name.split('.').skip(1) {
                                self.roots.push(current);
                                current = methods::getattr(current, part.as_ptr(), part.len());
                                if exc::pending() {
                                    return Err(Flow::Raise);
                                }
                            }
                            self.bind(binding, current);
                        }
                    }
                }
                Ok(())
            }
            ast::Stmt::ImportFrom(node) => self.exec_import_from(node),
            ast::Stmt::Global(node) => {
                self.global_names
                    .extend(node.names.iter().map(|name| name.to_string()));
                Ok(())
            }
            ast::Stmt::Nonlocal(node) => {
                self.nonlocal_names
                    .extend(node.names.iter().map(|name| name.to_string()));
                Ok(())
            }
            ast::Stmt::Delete(node) => {
                for target in &node.targets {
                    self.delete_target(target)?;
                }
                Ok(())
            }
            ast::Stmt::Assert(node) => {
                let test = self.eval(&node.test)?;
                if value::truthy(test) {
                    return Ok(());
                }
                let message = match &node.msg {
                    Some(expression) => {
                        let item = self.eval(expression)?;
                        value::as_str(item)
                            .map(str::to_string)
                            .unwrap_or_else(|| value::repr(item))
                    }
                    None => String::new(),
                };
                Err(raise_tag(ExcKind::AssertionError, &message))
            }
            ast::Stmt::Raise(node) => self.exec_raise(node),
            ast::Stmt::Try(node) => {
                self.exec_try(&node.body, &node.handlers, &node.orelse, &node.finalbody)
            }
            ast::Stmt::With(node) => self.exec_with(&node.items, &node.body),
            ast::Stmt::AsyncFunctionDef(_) => Err(raise_tag(
                ExcKind::NotImplementedError,
                "async def inside runtime-compiled source",
            )),
            ast::Stmt::AsyncFor(_) => Err(raise_tag(
                ExcKind::NotImplementedError,
                "async for inside runtime-compiled source",
            )),
            ast::Stmt::AsyncWith(_) => Err(raise_tag(
                ExcKind::NotImplementedError,
                "async with inside runtime-compiled source",
            )),
            ast::Stmt::Match(_) => Err(raise_tag(
                ExcKind::NotImplementedError,
                "match statement inside runtime-compiled source",
            )),
            ast::Stmt::TryStar(_) => Err(raise_tag(
                ExcKind::NotImplementedError,
                "except* inside runtime-compiled source",
            )),
            ast::Stmt::TypeAlias(_) => Err(raise_tag(
                ExcKind::NotImplementedError,
                "type statement inside runtime-compiled source",
            )),
        }
    }

    fn exec_while(&mut self, node: &ast::StmtWhile) -> ExecResult {
        let mut broke = false;
        loop {
            let test = self.eval(&node.test)?;
            if !value::truthy(test) {
                break;
            }
            match self.exec_stmts(&node.body) {
                Ok(()) => {}
                Err(Flow::Break) => {
                    broke = true;
                    break;
                }
                Err(Flow::Continue) => continue,
                Err(other) => return Err(other),
            }
        }
        if broke || node.orelse.is_empty() {
            Ok(())
        } else {
            self.exec_stmts(&node.orelse)
        }
    }

    fn exec_for(&mut self, node: &ast::StmtFor) -> ExecResult {
        let iterable = self.eval(&node.iter)?;
        self.roots.push(iterable);
        let iterator = iter::iter_new(iterable);
        if exc::pending() {
            return Err(Flow::Raise);
        }
        self.roots.push(iterator);
        let mut broke = false;
        while let Some(item) = self.next_item(iterator)? {
            self.assign(&node.target, item)?;
            match self.exec_stmts(&node.body) {
                Ok(()) => {}
                Err(Flow::Break) => {
                    broke = true;
                    break;
                }
                Err(Flow::Continue) => continue,
                Err(other) => return Err(other),
            }
        }
        if broke || node.orelse.is_empty() {
            Ok(())
        } else {
            self.exec_stmts(&node.orelse)
        }
    }

    fn exec_aug_assign(&mut self, node: &ast::StmtAugAssign) -> ExecResult {
        let code = binop_code(node.op);
        match &*node.target {
            ast::Expr::Name(name) => {
                let Some(current) = self.lookup(&name.id) else {
                    return Err(raise_tag(
                        ExcKind::NameError,
                        &format!("name '{}' is not defined", name.id),
                    ));
                };
                self.roots.push(current);
                let right = self.eval(&node.value)?;
                self.roots.push(right);
                let result = ops::binop(code, current, right);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                self.roots.push(result);
                self.bind(&name.id, result);
                Ok(())
            }
            ast::Expr::Attribute(attribute) => {
                let object = self.eval(&attribute.value)?;
                self.roots.push(object);
                let current =
                    methods::getattr(object, attribute.attr.as_ptr(), attribute.attr.len());
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                self.roots.push(current);
                let right = self.eval(&node.value)?;
                self.roots.push(right);
                let result = ops::binop(code, current, right);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                self.roots.push(result);
                let _ = methods::setattr(
                    object,
                    attribute.attr.as_ptr(),
                    attribute.attr.len(),
                    result,
                );
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(())
            }
            ast::Expr::Subscript(subscript) => {
                let object = self.eval(&subscript.value)?;
                self.roots.push(object);
                let current = self.subscript(object, &subscript.slice)?;
                self.roots.push(current);
                let right = self.eval(&node.value)?;
                self.roots.push(right);
                let result = ops::binop(code, current, right);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                self.roots.push(result);
                let index = self.subscript_index(object, &subscript.slice)?;
                let _ = container::subscript_set(object, index, result);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(())
            }
            _ => Err(raise_tag(
                ExcKind::NotImplementedError,
                "augmented assignment to this target",
            )),
        }
    }

    fn exec_raise(&mut self, node: &ast::StmtRaise) -> ExecResult {
        let mut raised = match &node.exc {
            Some(expression) => self.eval(expression)?,
            None => match self.handled.last() {
                Some(item) => *item,
                None => {
                    return Err(raise_tag(
                        ExcKind::RuntimeError,
                        "No active exception to reraise",
                    ))
                }
            },
        };
        self.roots.push(raised);
        if value::as_class(raised).is_some() {
            raised = runtime_builtins::call(raised, 0, std::ptr::null());
            if exc::pending() {
                return Err(Flow::Raise);
            }
            self.roots.push(raised);
        }
        let cause = match &node.cause {
            Some(expression) => {
                let item = self.eval(expression)?;
                self.roots.push(item);
                Some(item)
            }
            None => None,
        };
        match cause {
            Some(cause) => abi::hydro_raise_value_from(raised, cause),
            None => abi::hydro_raise_value(raised),
        };
        Err(Flow::Raise)
    }

    fn exec_try(
        &mut self,
        body: &[ast::Stmt],
        handlers: &[ast::ExceptHandler],
        orelse: &[ast::Stmt],
        finalbody: &[ast::Stmt],
    ) -> ExecResult {
        let mut result = self.exec_stmts(body);
        let mut dispatch = matches!(result, Err(Flow::Raise));
        if result.is_ok() && !orelse.is_empty() {
            result = self.exec_stmts(orelse);
            dispatch = false;
        }
        if dispatch && !handlers.is_empty() {
            let pending = abi::hydro_exc_value();
            self.roots.push(pending);
            abi::hydro_exc_clear();
            let mut matched = false;
            let mut matcher_failed = false;
            for handler in handlers {
                let ast::ExceptHandler::ExceptHandler(handler) = handler;
                let catches = match &handler.type_ {
                    None => true,
                    Some(expression) => match self.eval(expression) {
                        Ok(matcher) => {
                            self.roots.push(matcher);
                            abi::hydro_exc_matches_value(pending, matcher) != 0
                        }
                        Err(Flow::Raise) => {
                            matcher_failed = true;
                            false
                        }
                        Err(other) => {
                            result = Err(other);
                            matcher_failed = true;
                            false
                        }
                    },
                };
                if matcher_failed {
                    break;
                }
                if catches {
                    if let Some(binding) = &handler.name {
                        self.bind(binding, pending);
                    }
                    self.handled.push(pending);
                    let arm = self.exec_stmts(&handler.body);
                    self.handled.pop();
                    result = arm;
                    matched = true;
                    break;
                }
            }
            if matcher_failed {
                result = Err(Flow::Raise);
            } else if !matched {
                abi::hydro_raise_value(pending);
                result = Err(Flow::Raise);
            }
        }
        if finalbody.is_empty() {
            return result;
        }
        let in_flight = matches!(result, Err(Flow::Raise));
        let parked = if in_flight {
            let item = abi::hydro_exc_value();
            self.roots.push(item);
            abi::hydro_exc_clear();
            item
        } else {
            value::NONE
        };
        if let Err(Flow::Return(payload)) = result {
            self.roots.push(payload);
        }
        match self.exec_stmts(finalbody) {
            Ok(()) => {
                if in_flight {
                    abi::hydro_raise_value(parked);
                    Err(Flow::Raise)
                } else {
                    result
                }
            }
            Err(other) => Err(other),
        }
    }

    fn exec_with(&mut self, items: &[ast::WithItem], body: &[ast::Stmt]) -> ExecResult {
        let mut managers: Vec<i64> = Vec::new();
        let mut result: ExecResult = Ok(());
        for item in items {
            let manager = match self.eval(&item.context_expr) {
                Ok(item) => item,
                Err(flow) => {
                    result = Err(flow);
                    break;
                }
            };
            self.roots.push(manager);
            managers.push(manager);
            let entered = methods::call(
                manager,
                b"__enter__".as_ptr(),
                "__enter__".len(),
                0,
                std::ptr::null(),
            );
            if exc::pending() {
                result = Err(Flow::Raise);
                break;
            }
            self.roots.push(entered);
            if let Some(target) = &item.optional_vars {
                if let Err(flow) = self.assign(target, entered) {
                    result = Err(flow);
                    break;
                }
            }
        }
        if result.is_ok() {
            result = self.exec_stmts(body);
        }
        let mut exception = None;
        if matches!(result, Err(Flow::Raise)) {
            let item = abi::hydro_exc_value();
            self.roots.push(item);
            abi::hydro_exc_clear();
            exception = Some(item);
        }
        let mut suppressed = false;
        let mut exit_raised = false;
        for manager in managers.iter().rev() {
            let (type_value, value_value) = match exception {
                Some(item) => (value::type_class(item), item),
                None => (value::NONE, value::NONE),
            };
            let mut args = [type_value, value_value, value::NONE];
            let outcome = methods::call(
                *manager,
                b"__exit__".as_ptr(),
                "__exit__".len(),
                3,
                args.as_mut_ptr(),
            );
            if exc::pending() {
                exit_raised = true;
                break;
            }
            if exception.is_some() && value::truthy(outcome) {
                suppressed = true;
            }
        }
        if exit_raised {
            return Err(Flow::Raise);
        }
        if exception.is_some() {
            if suppressed {
                return Ok(());
            }
            let item = exception.unwrap_or(value::NONE);
            abi::hydro_raise_value(item);
            return Err(Flow::Raise);
        }
        result
    }

    fn delete_target(&mut self, target: &ast::Expr) -> ExecResult {
        match target {
            ast::Expr::Name(name) => {
                let key = value::str_alloc(name.id.to_string());
                self.roots.push(key);
                let _ = methods::call(
                    self.locals,
                    b"__delitem__".as_ptr(),
                    "__delitem__".len(),
                    1,
                    &key,
                );
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(())
            }
            ast::Expr::Attribute(attribute) => {
                let object = self.eval(&attribute.value)?;
                self.roots.push(object);
                let _ = methods::delete_attr(object, attribute.attr.as_ptr(), attribute.attr.len());
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(())
            }
            ast::Expr::Subscript(subscript) => {
                let object = self.eval(&subscript.value)?;
                self.roots.push(object);
                let index = self.subscript_index(object, &subscript.slice)?;
                let _ = container::delete_subscript(object, index);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(())
            }
            _ => Err(raise_tag(
                ExcKind::NotImplementedError,
                "del of this target",
            )),
        }
    }

    fn import_module(&mut self, name: &str) -> EvalResult {
        let name_value = value::str_alloc(name.to_string());
        self.roots.push(name_value);
        self.roots.push(self.globals);
        self.roots.push(self.locals);
        self.roots.push(value::NONE);
        let level = ops::int_result(0);
        let args = [name_value, self.globals, self.locals, value::NONE, level];
        let module = modules::builtins::import_from_core(&args);
        if exc::pending() {
            return Err(Flow::Raise);
        }
        Ok(module)
    }

    fn exec_import_from(&mut self, node: &ast::StmtImportFrom) -> ExecResult {
        let Some(module_name) = &node.module else {
            return Err(raise_tag(
                ExcKind::SyntaxError,
                "relative import with no known parent package",
            ));
        };
        let mut from_items = Vec::with_capacity(node.names.len());
        for alias in &node.names {
            let item = value::str_alloc(alias.name.to_string());
            self.roots.push(item);
            from_items.push(item);
        }
        let fromlist = value::tuple_new(from_items);
        self.roots.push(fromlist);
        let name_value = value::str_alloc(module_name.to_string());
        self.roots.push(name_value);
        // `ImportFrom.level` is the numeric count of leading dots; zero is an
        // absolute import and must not be mistaken for a relative one.
        let level = node.level.as_ref().map(|level| level.to_u32()).unwrap_or(0) as i64;
        let level_value = ops::int_result(level);
        let args = [name_value, self.globals, self.locals, fromlist, level_value];
        let module = modules::builtins::import_from_core(&args);
        if exc::pending() {
            return Err(Flow::Raise);
        }
        self.roots.push(module);
        for alias in &node.names {
            if alias.name.as_str() == "*" {
                // Importlib represents user modules as namespace pairs; native
                // and registered modules expose a dict-backed `module_dict`.
                let entries: Vec<(String, i64)> = if let Some(pairs) = value::as_namespace(module) {
                    pairs.to_vec()
                } else {
                    let namespace = crate::modules::module_dict(module_name);
                    self.roots.push(namespace);
                    value::as_dict(namespace)
                        .map(|pairs| {
                            pairs
                                .iter()
                                .filter_map(|(name, value)| {
                                    value::as_str(*name).map(|name| (name.to_string(), *value))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let all = entries
                    .iter()
                    .find(|(name, _)| name == "__all__")
                    .map(|(_, value)| *value);
                let names: Vec<String> = if let Some(items) = all.and_then(value::as_list) {
                    items
                        .iter()
                        .filter_map(|item| value::as_str(*item).map(str::to_string))
                        .collect()
                } else {
                    entries
                        .iter()
                        .map(|(name, _)| name.clone())
                        .filter(|name| !name.starts_with('_'))
                        .collect()
                };
                let mut bindings = Vec::with_capacity(names.len());
                for name in names {
                    let member = entries
                        .iter()
                        .find(|(key, _)| *key == name)
                        .map(|(_, value)| *value)
                        .unwrap_or_else(|| methods::getattr(module, name.as_ptr(), name.len()));
                    if exc::pending() {
                        return Err(Flow::Raise);
                    }
                    bindings.push((name, member));
                }
                for (name, member) in bindings {
                    self.bind(&name, member);
                }
                continue;
            }
            let member = methods::getattr(module, alias.name.as_ptr(), alias.name.len());
            if exc::pending() {
                return Err(Flow::Raise);
            }
            let binding = alias
                .asname
                .as_deref()
                .unwrap_or_else(|| alias.name.as_str());
            self.bind(binding, member);
        }
        Ok(())
    }

    fn exec_class(&mut self, node: &ast::StmtClassDef) -> ExecResult {
        let name = node.name.to_string();
        let name_value = value::str_alloc(name.clone());
        self.roots.push(name_value);
        let qualname_value = value::str_alloc(name.clone());
        self.roots.push(qualname_value);
        let module_value = value::str_alloc(module_label(self.globals));
        self.roots.push(module_value);
        let key_name = value::str_alloc("__name__".to_string());
        self.roots.push(key_name);
        let key_qualname = value::str_alloc("__qualname__".to_string());
        self.roots.push(key_qualname);
        let key_module = value::str_alloc("__module__".to_string());
        self.roots.push(key_module);
        let namespace = value::dict_new(vec![
            (key_name, name_value),
            (key_qualname, qualname_value),
            (key_module, module_value),
        ]);
        self.roots.push(namespace);

        let envs = self.environment();
        {
            let mut body = Interp::new(
                self.globals,
                namespace,
                None,
                envs,
                self.filename.clone(),
                false,
            );
            body.exec_stmts(&node.body)?;
        }

        let mut base_values = Vec::with_capacity(node.bases.len());
        for base in &node.bases {
            let item = self.eval(base)?;
            self.roots.push(item);
            base_values.push(item);
        }
        let bases = value::tuple_new(base_values);
        self.roots.push(bases);

        // `metaclass=` is `__build_class__`'s parameter, not a class-header
        // keyword: it selects the metaclass and is never forwarded to the
        // metaclass hooks or `__init_subclass__`. A `**mapping` operand binds
        // the same parameter, exactly like CPython's call convention.
        let mut keywords: Vec<(String, i64)> = Vec::with_capacity(node.keywords.len());
        let mut metaclass = None;
        for keyword in &node.keywords {
            let item = self.eval(&keyword.value)?;
            self.roots.push(item);
            match &keyword.arg {
                Some(argument) => {
                    if argument == "metaclass" {
                        if metaclass.replace(item).is_some() {
                            return Err(type_error(
                                "__build_class__() got multiple values for keyword \
                                 argument 'metaclass'",
                            ));
                        }
                    } else {
                        keywords.push((argument.to_string(), item));
                    }
                }
                None => {
                    let Some(entries) = value::as_dict(item) else {
                        return Err(type_error("class keywords ** operand must be a mapping"));
                    };
                    let entries = entries.to_vec();
                    for (key, item) in entries {
                        let Some(argument) = value::as_str(key) else {
                            return Err(type_error("keywords must be strings"));
                        };
                        if argument == "metaclass" {
                            if metaclass.replace(item).is_some() {
                                return Err(type_error(
                                    "__build_class__() got multiple values for keyword \
                                     argument 'metaclass'",
                                ));
                            }
                        } else {
                            self.roots.push(item);
                            keywords.push((argument.to_string(), item));
                        }
                    }
                }
            }
        }

        // Without an explicit metaclass or extra keywords the class comes
        // straight from the bases' metaclass; every other shape goes through
        // the `__build_class__` construction step so the selected metaclass
        // and `__init_subclass__` receive the header keywords.
        let mut class = if metaclass.is_none() && keywords.is_empty() {
            runtime_builtins::type_new(value::NONE, name_value, bases, namespace)
        } else {
            runtime_builtins::type_build_class_keywords(
                metaclass.unwrap_or(value::NONE),
                name_value,
                bases,
                namespace,
                &keywords,
            )
        };
        if exc::pending() {
            return Err(Flow::Raise);
        }
        self.roots.push(class);
        for decorator in node.decorator_list.iter().rev() {
            let function = self.eval(decorator)?;
            self.roots.push(function);
            class = self.call_value(function, vec![class], Vec::new())?;
        }
        self.bind(&name, class);
        Ok(())
    }

    // -- functions --------------------------------------------------------

    fn make_function(
        &mut self,
        def: DynDef,
        name: String,
        args: &ast::Arguments,
        decorators: &[ast::Expr],
    ) -> EvalResult {
        let envs = self.environment();
        let hidden = 2 + envs.len();

        let mut plans: Vec<(String, ParamKind, Option<ast::Expr>)> = Vec::new();
        for argument in &args.posonlyargs {
            plans.push((
                argument.def.arg.to_string(),
                ParamKind::PosOnly,
                argument.default.as_ref().map(|default| (**default).clone()),
            ));
        }
        for argument in &args.args {
            plans.push((
                argument.def.arg.to_string(),
                ParamKind::Positional,
                argument.default.as_ref().map(|default| (**default).clone()),
            ));
        }
        if let Some(vararg) = &args.vararg {
            plans.push((vararg.arg.to_string(), ParamKind::VarArg, None));
        }
        for argument in &args.kwonlyargs {
            plans.push((
                argument.def.arg.to_string(),
                ParamKind::KwOnly,
                argument.default.as_ref().map(|default| (**default).clone()),
            ));
        }
        if let Some(kwarg) = &args.kwarg {
            plans.push((kwarg.arg.to_string(), ParamKind::KwArg, None));
        }

        let slot_names: Vec<String> = plans.iter().map(|(name, _, _)| name.clone()).collect();
        let id = register_def(def, slot_names, hidden);

        let mut signature: Vec<value::ParamMeta> = Vec::with_capacity(hidden + plans.len());
        signature.push(value::ParamMeta {
            name: HIDDEN_ID.to_string(),
            kind: ParamKind::KwOnly,
            default: Some(int_value(id as i64)),
        });
        signature.push(value::ParamMeta {
            name: HIDDEN_GLOBALS.to_string(),
            kind: ParamKind::KwOnly,
            default: Some(self.globals),
        });
        for (index, env) in envs.iter().enumerate() {
            signature.push(value::ParamMeta {
                name: format!("{HIDDEN_ENV}{index}"),
                kind: ParamKind::KwOnly,
                default: Some(*env),
            });
        }
        let mut visible_names: Vec<String> = Vec::new();
        for (param_name, kind, default) in &plans {
            let default_value = match default {
                Some(expression) => {
                    let item = self.eval(expression)?;
                    self.roots.push(item);
                    Some(item)
                }
                None => None,
            };
            if matches!(kind, &ParamKind::PosOnly | &ParamKind::Positional) {
                visible_names.push(param_name.clone());
            }
            signature.push(value::ParamMeta {
                name: param_name.clone(),
                kind: *kind,
                default: default_value,
            });
        }

        let arity = args.posonlyargs.len() + args.args.len();
        let function = value::function_new_with_flags(
            0,
            arity,
            name,
            value::FUNCTION_FLAG_VARARGS | value::FUNCTION_FLAG_DYNAMIC_DEFAULTS,
        );
        self.roots.push(function);
        value::function_set_full_entry(function, dyn_full_entry as *const () as usize);
        value::function_set_signature(function, signature);
        value::function_set_params(function, visible_names);
        let mut function = function;
        for decorator in decorators.iter().rev() {
            let callable = self.eval(decorator)?;
            self.roots.push(callable);
            function = self.call_value(callable, vec![function], Vec::new())?;
        }
        Ok(function)
    }

    fn call_value(
        &mut self,
        function: i64,
        args: Vec<i64>,
        keywords: Vec<(String, i64)>,
    ) -> EvalResult {
        self.roots.push(function);
        for item in &args {
            self.roots.push(*item);
        }
        let mut pairs: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
        for (name, item) in &keywords {
            let key = value::str_alloc(name.clone());
            self.roots.push(key);
            self.roots.push(*item);
            pairs.push(key);
            pairs.push(*item);
        }
        let result = if pairs.is_empty() {
            runtime_builtins::call(function, args.len(), args.as_ptr())
        } else {
            runtime_builtins::call_kw(
                function,
                args.len(),
                args.as_ptr(),
                keywords.len(),
                pairs.as_ptr(),
            )
        };
        if exc::pending() {
            return Err(Flow::Raise);
        }
        Ok(result)
    }

    // -- expressions ------------------------------------------------------

    fn eval(&mut self, expression: &ast::Expr) -> EvalResult {
        match expression {
            ast::Expr::Constant(constant) => Ok(self.constant(&constant.value)),
            ast::Expr::Name(name) => match self.lookup(&name.id) {
                Some(item) => Ok(item),
                None => Err(raise_tag(
                    ExcKind::NameError,
                    &format!("name '{}' is not defined", name.id),
                )),
            },
            ast::Expr::Attribute(attribute) => {
                let object = self.eval(&attribute.value)?;
                self.roots.push(object);
                let item = methods::getattr(object, attribute.attr.as_ptr(), attribute.attr.len());
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(item)
            }
            ast::Expr::Subscript(subscript) => {
                let object = self.eval(&subscript.value)?;
                self.roots.push(object);
                self.subscript(object, &subscript.slice)
            }
            ast::Expr::BinOp(binary) => {
                let left = self.eval(&binary.left)?;
                self.roots.push(left);
                let right = self.eval(&binary.right)?;
                self.roots.push(right);
                let result = ops::binop(binop_code(binary.op), left, right);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(result)
            }
            ast::Expr::UnaryOp(unary) => {
                let operand = self.eval(&unary.operand)?;
                self.roots.push(operand);
                let result = ops::unop(unop_code(unary.op), operand);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(result)
            }
            ast::Expr::BoolOp(boolean) => {
                let deciding_and = matches!(boolean.op, ast::BoolOp::And);
                let mut last = value::NONE;
                for expression in &boolean.values {
                    let item = self.eval(expression)?;
                    self.roots.push(item);
                    let truth = value::truthy(item);
                    last = item;
                    if deciding_and != truth {
                        break;
                    }
                }
                Ok(last)
            }
            ast::Expr::Compare(compare) => self.eval_compare(compare),
            ast::Expr::Call(call) => self.eval_call(call),
            ast::Expr::List(list) => {
                let items = self.eval_elements(&list.elts)?;
                let result = value::list_new(items);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(result)
            }
            ast::Expr::Tuple(tuple) => {
                let items = self.eval_elements(&tuple.elts)?;
                Ok(value::tuple_new(items))
            }
            ast::Expr::Set(set) => {
                let items = self.eval_elements(&set.elts)?;
                let result = value::set_new(items);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(result)
            }
            ast::Expr::Dict(dict) => self.eval_dict(dict),
            ast::Expr::IfExp(conditional) => {
                let test = self.eval(&conditional.test)?;
                if value::truthy(test) {
                    self.eval(&conditional.body)
                } else {
                    self.eval(&conditional.orelse)
                }
            }
            ast::Expr::NamedExpr(named) => {
                let item = self.eval(&named.value)?;
                self.roots.push(item);
                self.assign(&named.target, item)?;
                Ok(item)
            }
            ast::Expr::Lambda(lambda) => self.make_function(
                DynDef::Lambda {
                    args: (*lambda.args).clone(),
                    body: lambda.body.clone(),
                },
                "<lambda>".to_string(),
                &lambda.args,
                &[],
            ),
            ast::Expr::JoinedStr(joined) => self.eval_joined_str(&joined.values),
            ast::Expr::FormattedValue(_) => Err(raise_tag(
                ExcKind::NotImplementedError,
                "a bare formatted-value expression",
            )),
            ast::Expr::Starred(starred) => self.eval(&starred.value),
            ast::Expr::ListComp(comprehension) => self.eval_comprehension(
                &comprehension.generators,
                CompKind::List((*comprehension.elt).clone()),
            ),
            ast::Expr::SetComp(comprehension) => self.eval_comprehension(
                &comprehension.generators,
                CompKind::Set((*comprehension.elt).clone()),
            ),
            ast::Expr::DictComp(comprehension) => self.eval_comprehension(
                &comprehension.generators,
                CompKind::Dict((*comprehension.key).clone(), (*comprehension.value).clone()),
            ),
            ast::Expr::GeneratorExp(_) => Err(raise_tag(
                ExcKind::NotImplementedError,
                "a generator expression",
            )),
            ast::Expr::Await(_) => Err(raise_tag(ExcKind::NotImplementedError, "await")),
            ast::Expr::Yield(_) => Err(raise_tag(ExcKind::NotImplementedError, "yield")),
            ast::Expr::YieldFrom(_) => Err(raise_tag(ExcKind::NotImplementedError, "yield from")),
            ast::Expr::Slice(_) => Err(raise_syntax("a slice is only valid inside a subscript")),
        }
    }

    fn constant(&mut self, constant: &ast::Constant) -> i64 {
        match constant {
            ast::Constant::None => value::NONE,
            ast::Constant::Bool(boolean) => value::bool_value(*boolean),
            ast::Constant::Str(text) => value::str_alloc(text.clone()),
            ast::Constant::Bytes(bytes) => value::bytes_new(bytes.clone()),
            ast::Constant::Int(integer) => integer_literal(&integer.to_string()),
            ast::Constant::Float(float) => value::float_new(*float),
            ast::Constant::Complex { real, imag } => runtime_builtins::build_complex(*real, *imag),
            ast::Constant::Tuple(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    let item = self.constant(item);
                    self.roots.push(item);
                    values.push(item);
                }
                value::tuple_new(values)
            }
            ast::Constant::Ellipsis => modules::module_get_by_name("builtins", "Ellipsis"),
        }
    }

    fn eval_compare(&mut self, compare: &ast::ExprCompare) -> EvalResult {
        let mut left = self.eval(&compare.left)?;
        self.roots.push(left);
        for (operator, expression) in compare.ops.iter().zip(&compare.comparators) {
            let right = self.eval(expression)?;
            self.roots.push(right);
            let result = ops::binop(compare_code(*operator), left, right);
            if exc::pending() {
                return Err(Flow::Raise);
            }
            self.roots.push(result);
            if !value::truthy(result) {
                return Ok(result);
            }
            left = right;
        }
        Ok(value::TRUE)
    }

    fn eval_call(&mut self, call: &ast::ExprCall) -> EvalResult {
        let function = self.eval(&call.func)?;
        self.roots.push(function);
        let mut args: Vec<i64> = Vec::with_capacity(call.args.len());
        for argument in &call.args {
            match argument {
                ast::Expr::Starred(starred) => {
                    let item = self.eval(&starred.value)?;
                    self.roots.push(item);
                    args.extend(self.iterate(item)?);
                }
                other => {
                    let item = self.eval(other)?;
                    self.roots.push(item);
                    args.push(item);
                }
            }
        }
        let mut keywords: Vec<(String, i64)> = Vec::with_capacity(call.keywords.len());
        for keyword in &call.keywords {
            let item = self.eval(&keyword.value)?;
            self.roots.push(item);
            match &keyword.arg {
                Some(argument) => keywords.push((argument.to_string(), item)),
                None => {
                    let Some(entries) = value::as_dict(item) else {
                        return Err(type_error("argument after ** must be a mapping"));
                    };
                    let entries = entries.to_vec();
                    for (key, item) in entries {
                        let Some(argument) = value::as_str(key) else {
                            return Err(type_error("keywords must be strings"));
                        };
                        if keywords.iter().any(|(existing, _)| existing == argument) {
                            return Err(type_error(&format!(
                                "got multiple values for keyword argument '{argument}'"
                            )));
                        }
                        self.roots.push(item);
                        keywords.push((argument.to_string(), item));
                    }
                }
            }
        }
        self.call_value(function, args, keywords)
    }

    fn eval_elements(&mut self, elements: &[ast::Expr]) -> Result<Vec<i64>, Flow> {
        let mut items = Vec::with_capacity(elements.len());
        for element in elements {
            match element {
                ast::Expr::Starred(starred) => {
                    let item = self.eval(&starred.value)?;
                    self.roots.push(item);
                    items.extend(self.iterate(item)?);
                }
                other => {
                    let item = self.eval(other)?;
                    self.roots.push(item);
                    items.push(item);
                }
            }
        }
        Ok(items)
    }

    fn eval_dict(&mut self, dict: &ast::ExprDict) -> EvalResult {
        let mut pairs: Vec<(i64, i64)> = Vec::with_capacity(dict.keys.len());
        for (key, value) in dict.keys.iter().zip(&dict.values) {
            match key {
                Some(expression) => {
                    let key = self.eval(expression)?;
                    self.roots.push(key);
                    let item = self.eval(value)?;
                    self.roots.push(item);
                    pairs.push((key, item));
                }
                None => {
                    let mapping = self.eval(value)?;
                    self.roots.push(mapping);
                    let Some(entries) = value::as_dict(mapping) else {
                        return Err(type_error("a mapping is required after **"));
                    };
                    let entries = entries.to_vec();
                    for (key, item) in entries {
                        self.roots.push(key);
                        self.roots.push(item);
                        pairs.push((key, item));
                    }
                }
            }
        }
        let dict = value::dict_new(pairs);
        if exc::pending() {
            return Err(Flow::Raise);
        }
        Ok(dict)
    }

    fn eval_joined_str(&mut self, parts: &[ast::Expr]) -> EvalResult {
        let mut out = String::new();
        for part in parts {
            match part {
                ast::Expr::Constant(constant) => match &constant.value {
                    ast::Constant::Str(text) => out.push_str(text),
                    other => out.push_str(&value::repr(self.constant(other))),
                },
                ast::Expr::FormattedValue(formatted) => {
                    let item = self.eval(&formatted.value)?;
                    self.roots.push(item);
                    let converted = match formatted.conversion {
                        ast::ConversionFlag::Str => runtime_builtins::to_str(item),
                        ast::ConversionFlag::Repr => runtime_builtins::to_repr(item),
                        ast::ConversionFlag::Ascii => runtime_builtins::to_ascii(item),
                        _ => item,
                    };
                    self.roots.push(converted);
                    let text = match &formatted.format_spec {
                        Some(specification) => {
                            let spec = self.eval(specification)?;
                            self.roots.push(spec);
                            let spec = runtime_builtins::to_str(spec);
                            self.roots.push(spec);
                            runtime_builtins::format_builtin(converted, spec, 0)
                        }
                        None => runtime_builtins::to_str(converted),
                    };
                    if exc::pending() {
                        return Err(Flow::Raise);
                    }
                    self.roots.push(text);
                    match value::as_str(text) {
                        Some(text) => out.push_str(text),
                        None => return Err(type_error("f-string component did not render to str")),
                    }
                }
                other => {
                    let item = self.eval(other)?;
                    self.roots.push(item);
                    let text = runtime_builtins::to_str(item);
                    if exc::pending() {
                        return Err(Flow::Raise);
                    }
                    self.roots.push(text);
                    if let Some(text) = value::as_str(text) {
                        out.push_str(text);
                    }
                }
            }
        }
        Ok(value::str_alloc(out))
    }

    fn eval_comprehension(
        &mut self,
        generators: &[ast::Comprehension],
        kind: CompKind,
    ) -> EvalResult {
        if generators.is_empty() {
            return Err(raise_syntax("comprehension without a generator"));
        }
        if generators.iter().any(|generator| generator.is_async) {
            return Err(raise_tag(
                ExcKind::NotImplementedError,
                "an async comprehension",
            ));
        }
        let output = match &kind {
            CompKind::List(_) => value::list_new(Vec::new()),
            CompKind::Set(_) => value::set_new(Vec::new()),
            CompKind::Dict(_, _) => value::dict_new(Vec::new()),
        };
        self.roots.push(output);
        let envs = self.environment();
        let locals = value::dict_new(Vec::new());
        // A comprehension runs in its own scope: bindings do not leak into the
        // enclosing namespace.
        let mut inner = Interp::new(
            self.globals,
            locals,
            None,
            envs,
            self.filename.clone(),
            false,
        );
        inner.roots.push(output);
        inner.run_comprehension(generators, 0, &kind, output)?;
        if exc::pending() {
            return Err(Flow::Raise);
        }
        Ok(output)
    }

    fn run_comprehension(
        &mut self,
        generators: &[ast::Comprehension],
        index: usize,
        kind: &CompKind,
        output: i64,
    ) -> ExecResult {
        if index == generators.len() {
            match kind {
                CompKind::List(element) => {
                    let item = self.eval(element)?;
                    self.roots.push(item);
                    let _ = container::list_append(output, item);
                }
                CompKind::Set(element) => {
                    let item = self.eval(element)?;
                    self.roots.push(item);
                    let _ = container::set_add(output, item);
                }
                CompKind::Dict(key, value) => {
                    let key = self.eval(key)?;
                    self.roots.push(key);
                    let item = self.eval(value)?;
                    self.roots.push(item);
                    let _ = container::dict_set(output, key, item);
                }
            }
            if exc::pending() {
                return Err(Flow::Raise);
            }
            return Ok(());
        }
        let generator = &generators[index];
        let iterable = self.eval(&generator.iter)?;
        self.roots.push(iterable);
        let iterator = iter::iter_new(iterable);
        if exc::pending() {
            return Err(Flow::Raise);
        }
        self.roots.push(iterator);
        while let Some(item) = self.next_item(iterator)? {
            self.assign(&generator.target, item)?;
            let mut keep = true;
            for condition in &generator.ifs {
                let test = self.eval(condition)?;
                self.roots.push(test);
                if !value::truthy(test) {
                    keep = false;
                    break;
                }
            }
            if keep {
                self.run_comprehension(generators, index + 1, kind, output)?;
            }
        }
        Ok(())
    }

    fn next_item(&mut self, iterator: i64) -> Result<Option<i64>, Flow> {
        let item = iter::next_value(iterator, value::NONE, false);
        if exc::pending() {
            if exc::pending_tag() == ExcKind::StopIteration.tag() as i64 {
                exc::clear();
                return Ok(None);
            }
            return Err(Flow::Raise);
        }
        Ok(Some(item))
    }

    fn iterate(&mut self, value: i64) -> Result<Vec<i64>, Flow> {
        self.roots.push(value);
        let iterator = iter::iter_new(value);
        if exc::pending() {
            return Err(Flow::Raise);
        }
        self.roots.push(iterator);
        let mut items = Vec::new();
        while let Some(item) = self.next_item(iterator)? {
            self.roots.push(item);
            items.push(item);
        }
        Ok(items)
    }

    fn subscript(&mut self, object: i64, slice: &ast::Expr) -> EvalResult {
        match slice {
            ast::Expr::Slice(slice) => {
                let lower = self.eval_opt(&slice.lower)?;
                self.roots.push(lower);
                let upper = self.eval_opt(&slice.upper)?;
                self.roots.push(upper);
                let step = self.eval_opt(&slice.step)?;
                self.roots.push(step);
                let result = container::slice(object, lower, upper, step);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(result)
            }
            other => {
                let index = self.eval(other)?;
                self.roots.push(index);
                let result = container::subscript(object, index);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(result)
            }
        }
    }

    fn subscript_index(&mut self, object: i64, slice: &ast::Expr) -> EvalResult {
        match slice {
            ast::Expr::Slice(slice) => {
                let lower = self.eval_opt(&slice.lower)?;
                self.roots.push(lower);
                let upper = self.eval_opt(&slice.upper)?;
                self.roots.push(upper);
                let step = self.eval_opt(&slice.step)?;
                self.roots.push(step);
                let result = runtime_builtins::slice_new(&[lower, upper, step]);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                let _ = object;
                Ok(result)
            }
            other => self.eval(other),
        }
    }

    fn eval_opt(&mut self, expression: &Option<Box<ast::Expr>>) -> EvalResult {
        match expression {
            Some(expression) => self.eval(expression),
            None => Ok(value::NONE),
        }
    }

    // -- assignment -------------------------------------------------------

    fn assign(&mut self, target: &ast::Expr, item: i64) -> ExecResult {
        match target {
            ast::Expr::Name(name) => {
                self.bind(&name.id, item);
                Ok(())
            }
            ast::Expr::Tuple(tuple) => self.assign_sequence(&tuple.elts, item),
            ast::Expr::List(list) => self.assign_sequence(&list.elts, item),
            ast::Expr::Starred(starred) => self.assign(&starred.value, item),
            ast::Expr::Attribute(attribute) => {
                let object = self.eval(&attribute.value)?;
                self.roots.push(object);
                self.roots.push(item);
                let _ =
                    methods::setattr(object, attribute.attr.as_ptr(), attribute.attr.len(), item);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(())
            }
            ast::Expr::Subscript(subscript) => {
                let object = self.eval(&subscript.value)?;
                self.roots.push(object);
                let index = self.subscript_index(object, &subscript.slice)?;
                self.roots.push(index);
                self.roots.push(item);
                let _ = container::subscript_set(object, index, item);
                if exc::pending() {
                    return Err(Flow::Raise);
                }
                Ok(())
            }
            _ => Err(raise_syntax("cannot assign to this expression")),
        }
    }

    fn assign_sequence(&mut self, targets: &[ast::Expr], item: i64) -> ExecResult {
        let star = targets
            .iter()
            .position(|target| matches!(target, ast::Expr::Starred(_)));
        let values = self.iterate(item)?;
        match star {
            None => {
                if values.len() != targets.len() {
                    return Err(raise_tag(
                        ExcKind::ValueError,
                        if values.len() < targets.len() {
                            "not enough values to unpack"
                        } else {
                            "too many values to unpack"
                        },
                    ));
                }
                for (target, value) in targets.iter().zip(&values) {
                    self.assign(target, *value)?;
                }
                Ok(())
            }
            Some(position) => {
                let after = targets.len() - position - 1;
                if values.len() < position + after {
                    return Err(raise_tag(
                        ExcKind::ValueError,
                        "not enough values to unpack",
                    ));
                }
                for (target, value) in targets[..position].iter().zip(&values[..position]) {
                    self.assign(target, *value)?;
                }
                let middle = &values[position..values.len() - after];
                let mut items = Vec::with_capacity(middle.len());
                for value in middle {
                    self.roots.push(*value);
                    items.push(*value);
                }
                let list = value::list_new(items);
                self.roots.push(list);
                if let ast::Expr::Starred(starred) = &targets[position] {
                    self.assign(&starred.value, list)?;
                }
                for (target, value) in targets[position + 1..]
                    .iter()
                    .zip(&values[values.len() - after..])
                {
                    self.assign(target, *value)?;
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Operator mapping
// ---------------------------------------------------------------------------

fn binop_code(operator: ast::Operator) -> u32 {
    let operation = match operator {
        ast::Operator::Add => BinOp::Add,
        ast::Operator::Sub => BinOp::Sub,
        ast::Operator::Mult => BinOp::Mul,
        ast::Operator::MatMult => BinOp::MatMul,
        ast::Operator::Div => BinOp::Div,
        ast::Operator::Mod => BinOp::Mod,
        ast::Operator::Pow => BinOp::Pow,
        ast::Operator::LShift => BinOp::Shl,
        ast::Operator::RShift => BinOp::Shr,
        ast::Operator::BitOr => BinOp::BitOr,
        ast::Operator::BitXor => BinOp::BitXor,
        ast::Operator::BitAnd => BinOp::BitAnd,
        ast::Operator::FloorDiv => BinOp::FloorDiv,
    };
    operation.code()
}

fn compare_code(operator: ast::CmpOp) -> u32 {
    let operation = match operator {
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
    };
    operation.code()
}

fn unop_code(operator: ast::UnaryOp) -> u32 {
    let operation = match operator {
        ast::UnaryOp::Invert => UnOp::Invert,
        ast::UnaryOp::Not => UnOp::Not,
        ast::UnaryOp::UAdd => UnOp::Pos,
        ast::UnaryOp::USub => UnOp::Neg,
    };
    operation.code()
}

// ---------------------------------------------------------------------------
// Unit execution
// ---------------------------------------------------------------------------

fn parse_mode(mode: UnitMode) -> Mode {
    match mode {
        UnitMode::Exec => Mode::Module,
        UnitMode::Eval => Mode::Expression,
        UnitMode::Single => Mode::Interactive,
    }
}

fn run_unit(unit: &Unit, globals_ns: i64, locals_ns: i64, mirror: Option<String>) -> EvalResult {
    let mut interp = Interp::new(
        globals_ns,
        locals_ns,
        mirror,
        Vec::new(),
        unit.filename.clone(),
        unit.mode == UnitMode::Single,
    );
    match &unit.module {
        ast::Mod::Module(module) => {
            interp.exec_stmts(&module.body)?;
            Ok(value::NONE)
        }
        ast::Mod::Interactive(module) => {
            interp.exec_stmts(&module.body)?;
            Ok(interp.last_value)
        }
        ast::Mod::Expression(expression) => interp.eval(&expression.body),
        _ => Ok(value::NONE),
    }
}

fn run_source(
    text: &str,
    mode: UnitMode,
    filename: &str,
    globals_ns: i64,
    locals_ns: i64,
    mirror: Option<String>,
) -> EvalResult {
    let module = match parse(text, parse_mode(mode), filename) {
        Ok(module) => module,
        Err(error) => {
            let line = line_of_offset(text, error.offset);
            return Err(raise_syntax(&format!(
                "invalid syntax ({filename}, line {line})"
            )));
        }
    };
    let unit = Unit {
        mode,
        filename: filename.to_string(),
        first_line: 1,
        flags: 0,
        module,
    };
    run_unit(&unit, globals_ns, locals_ns, mirror)
}

/// Resolves `exec`/`eval` namespace arguments to `(globals, locals, mirror)`.
///
/// `mirror` is the module key whose global store must observe new bindings;
/// it is set only when the caller did not pass an explicit globals mapping,
/// matching CPython's "use the caller's globals" default.
fn resolve_namespaces(
    globals_arg: i64,
    locals_arg: i64,
    builtin: &str,
) -> Result<(i64, i64, Option<String>), i64> {
    for (label, namespace) in [("globals", globals_arg), ("locals", locals_arg)] {
        if namespace != value::NONE && value::as_dict(namespace).is_none() {
            return Err(exc::raise_with_message(
                ExcKind::TypeError.tag(),
                &format!(
                    "{builtin}() {label} must be a dict, not '{}'",
                    value::type_name(namespace)
                ),
            ));
        }
    }
    let (globals_ns, mirror) = if globals_arg == value::NONE {
        match crate::exc::frame_module_at(0) {
            Some(module) => (globals::view(&module), Some(module)),
            None => (value::dict_new(Vec::new()), None),
        }
    } else {
        (globals_arg, None)
    };
    let locals_ns = if locals_arg == value::NONE {
        globals_ns
    } else {
        locals_arg
    };
    Ok((globals_ns, locals_ns, mirror))
}

// ---------------------------------------------------------------------------
// Builtins entry points
// ---------------------------------------------------------------------------

/// `compile(source, filename, mode, flags=0, dont_inherit=False, optimize=-1)`.
///
/// Returns a real code object whose metadata reflects the source. The AST is
/// retained in the process-wide unit table so `exec`/`eval` can execute it.
pub(crate) fn builtin_compile(args: &[i64]) -> i64 {
    if args.len() < 3 || args.len() > 6 {
        return exc::raise_with_message(
            ExcKind::TypeError.tag(),
            &format!(
                "compile() takes 3 to 6 positional arguments ({} given)",
                args.len()
            ),
        );
    }
    let text = match source_text(args[0]) {
        Ok(text) => text,
        Err(error) => return report_source_error(error),
    };
    let Some(filename) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            ExcKind::TypeError.tag(),
            &format!(
                "compile() arg 2 must be a string, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    let Some(mode_text) = value::as_str(args[2]) else {
        return exc::raise_with_message(
            ExcKind::TypeError.tag(),
            &format!(
                "compile() arg 3 must be a string, not '{}'",
                value::type_name(args[2])
            ),
        );
    };
    let mode = match mode_text {
        "exec" => UnitMode::Exec,
        "eval" => UnitMode::Eval,
        "single" => UnitMode::Single,
        other => {
            return exc::raise_with_message(
                ExcKind::ValueError.tag(),
                &format!("compile() mode must be 'exec', 'eval' or 'single', not '{other}'"),
            )
        }
    };
    let flags = if args.len() > 3 && args[3] != value::NONE {
        match value::as_integer(args[3]) {
            Some(flags) => flags,
            None => {
                return exc::raise_with_message(
                    ExcKind::TypeError.tag(),
                    &format!(
                        "compile() arg 4 must be an integer, not '{}'",
                        value::type_name(args[3])
                    ),
                )
            }
        }
    } else {
        0
    };
    // `dont_inherit` and `optimize` are validated but do not change the
    // executed semantics: the evaluator keeps asserts and annotations as
    // written, exactly like `optimize=-1`.
    let _dont_inherit = args
        .get(4)
        .map(|item| value::truthy(*item))
        .unwrap_or(false);
    if let Some(optimize) = args.get(5) {
        if *optimize != value::NONE && value::as_integer(*optimize).is_none() {
            return exc::raise_with_message(
                ExcKind::TypeError.tag(),
                &format!(
                    "compile() arg 6 must be an integer, not '{}'",
                    value::type_name(*optimize)
                ),
            );
        }
    }
    let module = match parse(&text, parse_mode(mode), filename) {
        Ok(module) => module,
        Err(error) => {
            let line = line_of_offset(&text, error.offset);
            return exc::raise_with_message(
                ExcKind::SyntaxError.tag(),
                &format!("invalid syntax ({filename}, line {line})"),
            );
        }
    };
    store_unit(Unit {
        mode,
        filename: filename.to_string(),
        first_line: 1,
        flags,
        module,
    })
}

/// `exec(source, globals=None, locals=None, /, *, closure=None)`.
pub(crate) fn builtin_exec(args: &[i64]) -> i64 {
    if args.is_empty() || args.len() > 4 {
        return exc::raise_with_message(
            ExcKind::TypeError.tag(),
            &format!(
                "exec() takes 1 to 4 positional arguments ({} given)",
                args.len()
            ),
        );
    }
    if args.len() > 3 && args[3] != value::NONE {
        return exc::raise_with_message(
            ExcKind::TypeError.tag(),
            "closure can only be used when source is a code object",
        );
    }
    let globals_arg = args.get(1).copied().unwrap_or(value::NONE);
    let locals_arg = args.get(2).copied().unwrap_or(value::NONE);
    let (globals_ns, locals_ns, mirror) = match resolve_namespaces(globals_arg, locals_arg, "exec")
    {
        Ok(namespaces) => namespaces,
        Err(raised) => return raised,
    };
    let result = if value::as_code(args[0]).is_some() {
        match unit_for_code(args[0]) {
            Some(unit) => run_unit(&unit, globals_ns, locals_ns, mirror),
            None => {
                return exc::raise_with_message(
                    ExcKind::RuntimeError.tag(),
                    "exec(): this code object is not a runtime-compiled unit",
                )
            }
        }
    } else {
        match source_text(args[0]) {
            Ok(text) => run_source(
                &text,
                UnitMode::Exec,
                "<string>",
                globals_ns,
                locals_ns,
                mirror,
            ),
            Err(error) => return report_source_error(error),
        }
    };
    match result {
        Ok(_) | Err(Flow::Return(_)) => value::NONE,
        Err(_) => value::NONE,
    }
}

/// `eval(source, globals=None, locals=None)`.
pub(crate) fn builtin_eval(args: &[i64]) -> i64 {
    if args.is_empty() || args.len() > 3 {
        return exc::raise_with_message(
            ExcKind::TypeError.tag(),
            &format!(
                "eval() takes 1 to 3 positional arguments ({} given)",
                args.len()
            ),
        );
    }
    let globals_arg = args.get(1).copied().unwrap_or(value::NONE);
    let locals_arg = args.get(2).copied().unwrap_or(value::NONE);
    let (globals_ns, locals_ns, mirror) = match resolve_namespaces(globals_arg, locals_arg, "eval")
    {
        Ok(namespaces) => namespaces,
        Err(raised) => return raised,
    };
    let result = if value::as_code(args[0]).is_some() {
        match unit_for_code(args[0]) {
            Some(unit) => run_unit(&unit, globals_ns, locals_ns, mirror),
            None => {
                return exc::raise_with_message(
                    ExcKind::RuntimeError.tag(),
                    "eval(): this code object is not a runtime-compiled unit",
                )
            }
        }
    } else {
        match source_text(args[0]) {
            Ok(text) => run_source(
                &text,
                UnitMode::Eval,
                "<string>",
                globals_ns,
                locals_ns,
                mirror,
            ),
            Err(error) => return report_source_error(error),
        }
    };
    match result {
        Ok(item) | Err(Flow::Return(item)) => item,
        Err(_) => value::NONE,
    }
}
