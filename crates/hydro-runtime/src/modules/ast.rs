#![allow(clippy::while_let_loop, clippy::explicit_auto_deref)]
#![allow(non_snake_case)]
//! `ast`: the runtime AST surface and `literal_eval`.
//!
//! `ast.literal_eval` is the original Hydro subset used by `rich.markup`. The
//! module also implements the runtime surface `annotationlib` needs:
//!
//! * `parse(source, filename='<unknown>', mode='exec', *, type_comments=False,
//!   feature_version=None)` and `unparse(node)`;
//! * the expression node constructors (`ast.Name`, `ast.Constant`,
//!   `ast.Attribute`, `ast.Subscript`, `ast.Call`, `ast.Tuple`, `ast.List`,
//!   `ast.Set`, `ast.Dict`, `ast.Slice`, `ast.Starred`, `ast.BinOp`,
//!   `ast.UnaryOp`, `ast.BoolOp`, `ast.Compare`, `ast.keyword`,
//!   `ast.Expression`, `ast.Module`, `ast.Expr`);
//! * the operator/context node classes (`ast.Add`, `ast.Eq`, `ast.Load`, ...)
//!   and the `ast.AST`/`ast.NodeVisitor`/`ast.NodeTransformer` classes.
//!
//! The node classes are native class values, so `isinstance(node, ast.AST)`
//! and `class _Fixer(ast.NodeTransformer)` behave like CPython. Construction
//! goes through the module call path (`ast.Name(id=...)`), which accepts the
//! CPython field names positionally or by keyword.
//!
//! Documented limits:
//!
//! * The runtime has no Python compiler/parser dependency, so `parse`
//!   implements the expression grammar used by annotation strings (names,
//!   attributes, subscripts, calls, literals, containers, slices, unary and
//!   binary/boolean/comparison operators). `mode="eval"` and expression
//!   statements in `mode="exec"`/`"single"` are supported; other statements
//!   raise a tagged `NotImplementedError` instead of returning a fabricated
//!   empty tree.
//! * `type_comments=True` is rejected (the grammar has no comment map);
//!   `feature_version` is validated and otherwise ignored because the runtime
//!   grammar has no version gates.
//! * `unparse` reconstructs the canonical expression text; a parsed
//!   `Module`/`Expression` remembers its source and returns it verbatim.
//!   Unsupported node kinds raise a tagged `NotImplementedError`, never a fake
//!   empty string.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use hydro_ir::ParamKind;
use num_bigint::BigInt;

use crate::container;
use crate::exc::{self, NOT_IMPLEMENTED_ERROR, SYNTAX_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::methods;
use crate::ops;
use crate::value::{self, FALSE, NONE, TRUE};

/// Maximum container nesting accepted by [`parse_literal`]/[`LiteralParser`].
const MAX_LITERAL_DEPTH: usize = 64;

/// One constructible AST node: its Python class name, base class name and
/// CPython field order.
struct NodeSpec {
    name: &'static str,
    base: &'static str,
    fields: &'static [&'static str],
    /// Native `__init__` trampoline used when the class value is called
    /// dynamically (`ast.List(elts)` inside `_Stringifier`).
    init: extern "C" fn(*const i64) -> i64,
}

const NODE_SPECS: &[NodeSpec] = &[
    NodeSpec {
        name: "Module",
        base: "mod",
        fields: &["body", "type_ignores"],
        init: init_Module,
    },
    NodeSpec {
        name: "Expression",
        base: "mod",
        fields: &["body"],
        init: init_Expression,
    },
    NodeSpec {
        name: "Expr",
        base: "stmt",
        fields: &["value"],
        init: init_Expr,
    },
    NodeSpec {
        name: "Name",
        base: "expr",
        fields: &["id", "ctx"],
        init: init_Name,
    },
    NodeSpec {
        name: "Constant",
        base: "expr",
        fields: &["value", "kind"],
        init: init_Constant,
    },
    NodeSpec {
        name: "Attribute",
        base: "expr",
        fields: &["value", "attr", "ctx"],
        init: init_Attribute,
    },
    NodeSpec {
        name: "Subscript",
        base: "expr",
        fields: &["value", "slice", "ctx"],
        init: init_Subscript,
    },
    NodeSpec {
        name: "Call",
        base: "expr",
        fields: &["func", "args", "keywords"],
        init: init_Call,
    },
    NodeSpec {
        name: "keyword",
        base: "AST",
        fields: &["arg", "value"],
        init: init_keyword,
    },
    NodeSpec {
        name: "Tuple",
        base: "expr",
        fields: &["elts", "ctx"],
        init: init_Tuple,
    },
    NodeSpec {
        name: "List",
        base: "expr",
        fields: &["elts", "ctx"],
        init: init_List,
    },
    NodeSpec {
        name: "Set",
        base: "expr",
        fields: &["elts"],
        init: init_Set,
    },
    NodeSpec {
        name: "Dict",
        base: "expr",
        fields: &["keys", "values"],
        init: init_Dict,
    },
    NodeSpec {
        name: "Slice",
        base: "expr",
        fields: &["lower", "upper", "step"],
        init: init_Slice,
    },
    NodeSpec {
        name: "Starred",
        base: "expr",
        fields: &["value", "ctx"],
        init: init_Starred,
    },
    NodeSpec {
        name: "BinOp",
        base: "expr",
        fields: &["left", "op", "right"],
        init: init_BinOp,
    },
    NodeSpec {
        name: "UnaryOp",
        base: "expr",
        fields: &["op", "operand"],
        init: init_UnaryOp,
    },
    NodeSpec {
        name: "BoolOp",
        base: "expr",
        fields: &["op", "values"],
        init: init_BoolOp,
    },
    NodeSpec {
        name: "Compare",
        base: "expr",
        fields: &["left", "ops", "comparators"],
        init: init_Compare,
    },
    NodeSpec {
        name: "IfExp",
        base: "expr",
        fields: &["test", "body", "orelse"],
        init: init_IfExp,
    },
];

/// Defines one `__init__` trampoline that stores the declared fields (and any
/// extra keyword attributes) on the instance the class-call path allocated.
macro_rules! node_init {
    ($function:ident, [$($field:literal),*]) => {
        extern "C" fn $function(argv: *const i64) -> i64 {
            // SAFETY: the class-call binder passes one slot per declared
            // parameter (self, the fields in order, then `**kwargs`).
            let instance = unsafe { *argv };
            let mut index = 1usize;
            $(
                let item = unsafe { *argv.add(index) };
                value::instance_setattr(instance, $field.to_string(), item);
                index += 1;
            )*
            let keywords = unsafe { *argv.add(index) };
            if let Some(entries) = value::as_dict(keywords) {
                for (key, item) in entries {
                    if let Some(name) = value::as_str(*key) {
                        value::instance_setattr(instance, name.to_string(), *item);
                    }
                }
            }
            NONE
        }
    };
}

node_init!(init_Module, ["body", "type_ignores"]);
node_init!(init_Expression, ["body"]);
node_init!(init_Expr, ["value"]);
node_init!(init_Name, ["id", "ctx"]);
node_init!(init_Constant, ["value", "kind"]);
node_init!(init_Attribute, ["value", "attr", "ctx"]);
node_init!(init_Subscript, ["value", "slice", "ctx"]);
node_init!(init_Call, ["func", "args", "keywords"]);
node_init!(init_keyword, ["arg", "value"]);
node_init!(init_Tuple, ["elts", "ctx"]);
node_init!(init_List, ["elts", "ctx"]);
node_init!(init_Set, ["elts"]);
node_init!(init_Dict, ["keys", "values"]);
node_init!(init_Slice, ["lower", "upper", "step"]);
node_init!(init_Starred, ["value", "ctx"]);
node_init!(init_BinOp, ["left", "op", "right"]);
node_init!(init_UnaryOp, ["op", "operand"]);
node_init!(init_BoolOp, ["op", "values"]);
node_init!(init_Compare, ["left", "ops", "comparators"]);
node_init!(init_IfExp, ["test", "body", "orelse"]);

/// Holder base classes exposed on the module (`ast.expr`, `ast.operator`, ...).
const BASE_CLASSES: &[&str] = &[
    "AST",
    "mod",
    "stmt",
    "expr",
    "expr_context",
    "boolop",
    "operator",
    "unaryop",
    "cmpop",
    "comprehension",
    "excepthandler",
    "arguments",
    "alias",
];

/// Operator and context node classes, with their holder base.
const OPERATOR_CLASSES: &[(&str, &str)] = &[
    ("Add", "operator"),
    ("Sub", "operator"),
    ("Mult", "operator"),
    ("MatMult", "operator"),
    ("Div", "operator"),
    ("Mod", "operator"),
    ("Pow", "operator"),
    ("LShift", "operator"),
    ("RShift", "operator"),
    ("BitOr", "operator"),
    ("BitXor", "operator"),
    ("BitAnd", "operator"),
    ("FloorDiv", "operator"),
    ("UAdd", "unaryop"),
    ("USub", "unaryop"),
    ("Invert", "unaryop"),
    ("Not", "unaryop"),
    ("And", "boolop"),
    ("Or", "boolop"),
    ("Eq", "cmpop"),
    ("NotEq", "cmpop"),
    ("Lt", "cmpop"),
    ("LtE", "cmpop"),
    ("Gt", "cmpop"),
    ("GtE", "cmpop"),
    ("Is", "cmpop"),
    ("IsNot", "cmpop"),
    ("In", "cmpop"),
    ("NotIn", "cmpop"),
    ("Load", "expr_context"),
    ("Store", "expr_context"),
    ("Del", "expr_context"),
];

struct Registry {
    generation: u64,
    ast_class: i64,
    bases: HashMap<&'static str, i64>,
    nodes: HashMap<&'static str, i64>,
    operators: HashMap<&'static str, i64>,
    visitor: i64,
    transformer: i64,
}

static REGISTRY: LazyLock<Mutex<Option<Registry>>> = LazyLock::new(|| Mutex::new(None));

fn registry_lock() -> std::sync::MutexGuard<'static, Option<Registry>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Builds (or rebuilds after a runtime reset) the stable class values.
fn ensure_registry() {
    let generation = heap::generation();
    {
        let guard = registry_lock();
        if guard
            .as_ref()
            .is_some_and(|registry| registry.generation == generation)
        {
            return;
        }
    }
    let registry = build_registry(generation);
    *registry_lock() = Some(registry);
}

fn build_registry(generation: u64) -> Registry {
    let ast_class = value::class_new("AST".to_string(), NONE, Vec::new());
    heap::add_thread_root(ast_class);
    let mut bases = HashMap::new();
    bases.insert("AST", ast_class);
    for name in BASE_CLASSES.iter().copied().filter(|name| *name != "AST") {
        let class = value::class_new(name.to_string(), ast_class, Vec::new());
        heap::add_thread_root(class);
        bases.insert(name, class);
    }
    let mut nodes = HashMap::new();
    for spec in NODE_SPECS {
        let base = *bases.get(spec.base).unwrap_or(&ast_class);
        let init = value::native_function_new("ast", "_ast_node_init", usize::MAX);
        let mut signature = Vec::with_capacity(spec.fields.len() + 2);
        signature.push(value::ParamMeta {
            name: "self".to_string(),
            kind: ParamKind::Positional,
            default: None,
        });
        for field in spec.fields {
            signature.push(value::ParamMeta {
                name: (*field).to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            });
        }
        signature.push(value::ParamMeta {
            name: "kwargs".to_string(),
            kind: ParamKind::KwArg,
            default: Some(NONE),
        });
        value::function_set_signature(init, signature);
        value::function_set_full_entry(init, spec.init as usize);
        let class = value::class_new(
            spec.name.to_string(),
            base,
            vec![("__init__".to_string(), init)],
        );
        heap::add_thread_root(class);
        nodes.insert(spec.name, class);
    }
    let mut operators = HashMap::new();
    for (name, base) in OPERATOR_CLASSES {
        let base = *bases.get(*base).unwrap_or(&ast_class);
        let class = value::class_new((*name).to_string(), base, Vec::new());
        heap::add_thread_root(class);
        operators.insert(*name, class);
    }
    let visit = value::native_function_new("ast", "_node_visit", 2);
    let generic = value::native_function_new("ast", "_node_generic_visit", 2);
    let visitor = value::class_new(
        "NodeVisitor".to_string(),
        ast_class,
        vec![
            ("visit".to_string(), visit),
            ("generic_visit".to_string(), generic),
        ],
    );
    heap::add_thread_root(visitor);
    let visit = value::native_function_new("ast", "_node_visit", 2);
    let generic = value::native_function_new("ast", "_node_generic_visit", 2);
    let transformer = value::class_new(
        "NodeTransformer".to_string(),
        visitor,
        vec![
            ("visit".to_string(), visit),
            ("generic_visit".to_string(), generic),
        ],
    );
    heap::add_thread_root(transformer);
    Registry {
        generation,
        ast_class,
        bases,
        nodes,
        operators,
        visitor,
        transformer,
    }
}

/// Copies a registry value out and keeps it alive on the calling thread.
fn rooted(class: i64, ast: i64) -> i64 {
    heap::add_thread_root(ast);
    heap::add_thread_root(class);
    class
}

fn ast_class_handle() -> Option<i64> {
    ensure_registry();
    let guard = registry_lock();
    let registry = guard.as_ref()?;
    Some(rooted(registry.ast_class, registry.ast_class))
}

fn base_class(name: &str) -> Option<i64> {
    ensure_registry();
    let guard = registry_lock();
    let registry = guard.as_ref()?;
    let class = *registry.bases.get(name)?;
    Some(rooted(class, registry.ast_class))
}

fn node_class(name: &str) -> Option<i64> {
    ensure_registry();
    let guard = registry_lock();
    let registry = guard.as_ref()?;
    let class = *registry.nodes.get(name)?;
    Some(rooted(class, registry.ast_class))
}

fn operator_class(name: &str) -> Option<i64> {
    ensure_registry();
    let guard = registry_lock();
    let registry = guard.as_ref()?;
    let class = *registry.operators.get(name)?;
    Some(rooted(class, registry.ast_class))
}

fn visitor_class(name: &str) -> Option<i64> {
    ensure_registry();
    let guard = registry_lock();
    let registry = guard.as_ref()?;
    let class = match name {
        "NodeVisitor" => registry.visitor,
        "NodeTransformer" => registry.transformer,
        _ => return None,
    };
    Some(rooted(class, registry.ast_class))
}

/// The class value for `ast.<name>`, if it is one of the stable AST classes.
///
/// `modules::module_get_by_name` consults this before materialising a function
/// spec, so `isinstance(node, ast.Name)` and `class X(ast.NodeTransformer)`
/// see real classes while `ast.Name(...)` still lowers as a module call.
pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    if module != "ast" {
        return None;
    }
    if name == "AST" {
        return ast_class_handle();
    }
    visitor_class(name)
        .or_else(|| base_class(name))
        .or_else(|| operator_class(name))
        .or_else(|| node_class(name))
}

/// Dispatches a native `ast` function. Arity is enforced by the central spec.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    if NODE_SPECS.iter().any(|spec| spec.name == function) {
        return build_node(function, args, &[]);
    }
    if OPERATOR_CLASSES.iter().any(|(name, _)| *name == function) {
        if !args.is_empty() {
            return type_error(&format!("{function}() takes no arguments"));
        }
        return operator_instance(function).unwrap_or(NONE);
    }
    match function {
        "literal_eval" => literal_eval(args.first().copied().unwrap_or(NONE)),
        "parse" => parse_source(args, &[]),
        "unparse" => unparse(args),
        "fix_missing_locations" => fix_missing_locations(args),
        "_node_visit" => match (args.first(), args.get(1)) {
            (Some(&visitor), Some(&node)) => visit_node(visitor, node),
            _ => type_error("visit() missing argument"),
        },
        "_node_generic_visit" => match (args.first(), args.get(1)) {
            (Some(&visitor), Some(&node)) => generic_visit(visitor, node),
            _ => type_error("generic_visit() missing argument"),
        },
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'ast' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware dispatch: node constructors and `parse`.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if NODE_SPECS.iter().any(|spec| spec.name == function) {
        return build_node(function, args, keywords);
    }
    if OPERATOR_CLASSES.iter().any(|(name, _)| *name == function) {
        if !args.is_empty() || !keywords.is_empty() {
            return type_error(&format!("{function}() takes no arguments"));
        }
        return operator_instance(function).unwrap_or(NONE);
    }
    match function {
        "parse" => parse_source(args, keywords),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'ast' has no attribute '{function}'"),
        ),
    }
}

/// Materialises an operator/context instance (`ast.Add()`, `ast.Load()`).
fn operator_instance(name: &str) -> Option<i64> {
    let class = operator_class(name)?;
    Some(value::instance_new(class))
}

/// Constructs one node from positional fields and CPython-style keywords.
fn build_node(name: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(spec) = NODE_SPECS.iter().find(|spec| spec.name == name) else {
        return exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'ast' has no attribute '{name}'"),
        );
    };
    if args.len() > spec.fields.len() {
        return type_error(&format!(
            "{name}() takes at most {} arguments ({} given)",
            spec.fields.len(),
            args.len()
        ));
    }
    // Duplicate positional/keyword spelling of the same field.
    for (index, field) in spec.fields.iter().enumerate() {
        if index < args.len()
            && keywords
                .iter()
                .any(|(keyword, _)| keyword.as_str() == *field)
        {
            return type_error(&format!(
                "{name}() got multiple values for argument '{field}'"
            ));
        }
    }
    let mut extra = keywords.to_vec();
    let mut fields: Vec<(&'static str, i64)> = Vec::with_capacity(spec.fields.len());
    for (index, field) in spec.fields.iter().enumerate() {
        let value = if let Some(&value) = args.get(index) {
            value
        } else if let Some(position) = extra
            .iter()
            .position(|(keyword, _)| keyword.as_str() == *field)
        {
            extra.remove(position).1
        } else {
            NONE
        };
        fields.push((*field, value));
    }
    // Every value lives in Rust memory while the class lookup and the instance
    // attribute table allocate; root them explicitly.
    let mut inputs: Vec<i64> = args.to_vec();
    inputs.extend(keywords.iter().map(|(_, value)| *value));
    let roots = heap::root_values(&inputs);
    let Some(class) = node_class(name) else {
        heap::release_roots(roots);
        return type_error(&format!("unknown AST node '{name}'"));
    };
    let instance = value::instance_new(class);
    for (field, item) in fields {
        value::instance_setattr(instance, field.to_string(), item);
    }
    for (keyword, item) in extra {
        value::instance_setattr(instance, keyword, item);
    }
    heap::release_roots(roots);
    instance
}

// ---------------------------------------------------------------------------
// ast.parse
// ---------------------------------------------------------------------------

/// `ast.parse(source, filename='<unknown>', mode='exec', *, type_comments=False,
/// feature_version=None)`.
fn parse_source(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut source = args.first().copied();
    let mut filename = args.get(1).copied().unwrap_or(NONE);
    let mut mode = args.get(2).copied().unwrap_or(NONE);
    let mut type_comments = args.get(3).copied().unwrap_or(NONE);
    let mut feature_version = args.get(4).copied().unwrap_or(NONE);
    for (keyword, value) in keywords {
        let value = *value;
        match keyword.as_str() {
            "source" => source = Some(value),
            "filename" => filename = value,
            "mode" => mode = value,
            "type_comments" => type_comments = value,
            "feature_version" => feature_version = value,
            other => {
                return type_error(&format!(
                    "parse() got an unexpected keyword argument '{other}'"
                ))
            }
        }
    }
    let Some(source) = source.and_then(value::as_str) else {
        return type_error("parse() argument 'source' must be a string");
    };
    let filename = match filename {
        NONE => "<unknown>".to_string(),
        value => match value::as_str(value) {
            Some(text) => text.to_string(),
            None => return type_error("parse() argument 'filename' must be a string"),
        },
    };
    let mode = match mode {
        NONE => "exec".to_string(),
        value => match value::as_str(value) {
            Some(text) => text.to_string(),
            None => return type_error("parse() argument 'mode' must be a string"),
        },
    };
    if type_comments != NONE && type_comments != value::FALSE {
        return exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "ast.parse(type_comments=True) is not supported in Hydro",
        );
    }
    if feature_version != NONE && !valid_feature_version(feature_version) {
        return type_error("parse() argument 'feature_version' must be an int or a version tuple");
    }
    let source = source.to_string();
    match mode.as_str() {
        "eval" => parse_expression_source(&source, &filename),
        "exec" | "single" => parse_module_source(&source, &filename),
        other => exc::raise_with_message(VALUE_ERROR, &format!("unknown parse mode {other:?}")),
    }
}

fn valid_feature_version(value: i64) -> bool {
    if value::as_integer(value).is_some() {
        return true;
    }
    let items = if let Some(items) = value::as_tuple(value) {
        items.to_vec()
    } else if let Some(items) = value::as_list(value) {
        items.to_vec()
    } else {
        return false;
    };
    (items.len() == 2 || items.len() == 3)
        && items.iter().all(|item| value::as_integer(*item).is_some())
}

fn syntax_error(filename: &str, message: &str) -> i64 {
    exc::raise_with_message(SYNTAX_ERROR, &format!("{filename}: {message}"))
}

fn parse_expression_source(source: &str, filename: &str) -> i64 {
    let mut parser = ExprParser::new(source);
    let expression = match parser.parse_eval() {
        Ok(value) => value,
        Err(message) => return syntax_error(filename, &message),
    };
    let node = build_node("Expression", &[expression], &[]);
    let source = value::str_alloc(source.trim().to_string());
    let roots = heap::root_values(&[node, source]);
    value::instance_setattr(node, "_source".to_string(), source);
    heap::release_roots(roots);
    node
}

fn parse_module_source(source: &str, filename: &str) -> i64 {
    let mut statements = Vec::new();
    for statement in split_statements(source) {
        let trimmed = statement.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(keyword) = statement_keyword(trimmed) {
            return exc::raise_with_message(
                NOT_IMPLEMENTED_ERROR,
                &format!(
                    "ast.parse() does not support the '{keyword}' statement in Hydro's runtime AST surface"
                ),
            );
        }
        let mut parser = ExprParser::new(trimmed);
        let expression = match parser.parse_eval() {
            Ok(value) => value,
            Err(message) => return syntax_error(filename, &message),
        };
        statements.push(build_node("Expr", &[expression], &[]));
    }
    let body = value::list_new(statements);
    let type_ignores = value::list_new(Vec::new());
    let node = build_node("Module", &[body, type_ignores], &[]);
    let source_value = value::str_alloc(source.to_string());
    let roots = heap::root_values(&[node, source_value]);
    value::instance_setattr(node, "_source".to_string(), source_value);
    heap::release_roots(roots);
    node
}

/// Splits module source on top-level newlines and semicolons, keeping bracket
/// nesting and string literals intact.
fn split_statements(source: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in source.chars() {
        if let Some(active) = quote {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                current.push(ch);
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            '\n' | ';' if depth == 0 => {
                statements.push(current.clone());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    statements.push(current);
    statements
}

/// The leading keyword of a source statement, when it is not an expression.
fn statement_keyword(source: &str) -> Option<&'static str> {
    let words = [
        "def", "class", "import", "from", "if", "elif", "else", "for", "while", "with", "try",
        "except", "finally", "return", "yield", "raise", "pass", "break", "continue", "del",
        "global", "nonlocal", "assert", "async", "await", "match", "case", "lambda",
    ];
    let first: String = source
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    words.iter().copied().find(|word| first.as_str() == *word)
}

// ---------------------------------------------------------------------------
// Expression parser
// ---------------------------------------------------------------------------

/// Bounds recursive descent so malformed input cannot exhaust the stack.
const MAX_EXPR_DEPTH: usize = 256;

struct ExprParser {
    chars: Vec<char>,
    position: usize,
    depth: usize,
}

impl ExprParser {
    fn new(source: &str) -> ExprParser {
        ExprParser {
            chars: source.chars().collect(),
            position: 0,
            depth: 0,
        }
    }

    fn parse_eval(&mut self) -> Result<i64, String> {
        self.skip_ws();
        let value = self.parse_test()?;
        self.skip_ws();
        if self.position != self.chars.len() {
            return Err(self.unexpected("trailing characters"));
        }
        Ok(value)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.position + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.position += 1;
        Some(ch)
    }

    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(ch) if ch.is_whitespace() => self.position += 1,
                Some('#') => {
                    while let Some(ch) = self.peek() {
                        self.position += 1;
                        if ch == '\n' {
                            break;
                        }
                    }
                }
                Some('\\') if self.peek_at(1) == Some('\n') => self.position += 2,
                _ => break,
            }
        }
    }

    fn unexpected(&self, detail: &str) -> String {
        match self.peek() {
            Some(ch) => format!("unexpected character {ch:?} ({detail})"),
            None => format!("unexpected end of input ({detail})"),
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), String> {
        self.skip_ws();
        if self.advance() == Some(expected) {
            Ok(())
        } else {
            Err(self.unexpected(&format!("expected '{expected}'")))
        }
    }

    fn eat(&mut self, expected: char) -> bool {
        self.skip_ws();
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            return Err("expression is nested too deeply".to_string());
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn parse_test(&mut self) -> Result<i64, String> {
        self.enter()?;
        let result = self.parse_or();
        self.leave();
        result
    }

    fn parse_or(&mut self) -> Result<i64, String> {
        let first = self.parse_and()?;
        let mut values = vec![first];
        while self.eat_word("or") {
            values.push(self.parse_and()?);
        }
        if values.len() == 1 {
            return Ok(first);
        }
        let op = operator_instance("Or").ok_or_else(|| "missing ast.Or class".to_string())?;
        let values = value::list_new(values);
        Ok(build_node("BoolOp", &[op, values], &[]))
    }

    fn parse_and(&mut self) -> Result<i64, String> {
        let first = self.parse_not()?;
        let mut values = vec![first];
        while self.eat_word("and") {
            values.push(self.parse_not()?);
        }
        if values.len() == 1 {
            return Ok(first);
        }
        let op = operator_instance("And").ok_or_else(|| "missing ast.And class".to_string())?;
        let values = value::list_new(values);
        Ok(build_node("BoolOp", &[op, values], &[]))
    }

    fn parse_not(&mut self) -> Result<i64, String> {
        if self.eat_word("not") {
            let operand = self.parse_not()?;
            let op = operator_instance("Not").ok_or_else(|| "missing ast.Not class".to_string())?;
            return Ok(build_node("UnaryOp", &[op, operand], &[]));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<i64, String> {
        let left = self.parse_bit_or()?;
        let mut ops = Vec::new();
        let mut comparators = Vec::new();
        loop {
            let Some(op) = self.comparison_operator() else {
                break;
            };
            ops.push(operator_instance(op).ok_or_else(|| format!("missing ast.{op} class"))?);
            comparators.push(self.parse_bit_or()?);
        }
        if ops.is_empty() {
            return Ok(left);
        }
        let ops = value::list_new(ops);
        let comparators = value::list_new(comparators);
        Ok(build_node("Compare", &[left, ops, comparators], &[]))
    }

    fn comparison_operator(&mut self) -> Option<&'static str> {
        self.skip_ws();
        if self.eat_str("<=") {
            return Some("LtE");
        }
        if self.eat_str(">=") {
            return Some("GtE");
        }
        if self.eat_str("==") {
            return Some("Eq");
        }
        if self.eat_str("!=") {
            return Some("NotEq");
        }
        if self.eat_str("<") {
            return Some("Lt");
        }
        if self.eat_str(">") {
            return Some("Gt");
        }
        if self.eat_word("is") {
            if self.eat_word("not") {
                return Some("IsNot");
            }
            return Some("Is");
        }
        if self.eat_word("not") {
            if self.eat_word("in") {
                return Some("NotIn");
            }
            // Not a comparison; the `not` belongs to the next expression.
            return None;
        }
        if self.eat_word("in") {
            return Some("In");
        }
        None
    }

    fn parse_bit_or(&mut self) -> Result<i64, String> {
        self.parse_binary(|parser| parser.parse_bit_xor(), "|", "BitOr")
    }

    fn parse_bit_xor(&mut self) -> Result<i64, String> {
        self.parse_binary(|parser| parser.parse_bit_and(), "^", "BitXor")
    }

    fn parse_bit_and(&mut self) -> Result<i64, String> {
        self.parse_binary(|parser| parser.parse_shift(), "&", "BitAnd")
    }

    fn parse_shift(&mut self) -> Result<i64, String> {
        let mut left = self.parse_arith()?;
        loop {
            let op = if self.eat_str("<<") {
                "LShift"
            } else if self.eat_str(">>") {
                "RShift"
            } else {
                break;
            };
            let right = self.parse_arith()?;
            left = self.binary_node(left, op, right)?;
        }
        Ok(left)
    }

    fn parse_arith(&mut self) -> Result<i64, String> {
        let mut left = self.parse_term()?;
        loop {
            let op = if self.eat_str("+") {
                "Add"
            } else if self.eat_str("-") {
                "Sub"
            } else {
                break;
            };
            let right = self.parse_term()?;
            left = self.binary_node(left, op, right)?;
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> Result<i64, String> {
        let mut left = self.parse_factor()?;
        loop {
            let op = if self.eat_str("//") {
                "FloorDiv"
            } else if self.eat_str("**") {
                // `**` binds tighter than the multiplicative operators; parse
                // it here only after `parse_factor` already handled it, so a
                // stray occurrence is a syntax error.
                return Err("unexpected '**'".to_string());
            } else if self.eat_str("*") {
                "Mult"
            } else if self.eat_str("@") {
                "MatMult"
            } else if self.eat_str("/") {
                "Div"
            } else if self.eat_str("%") {
                "Mod"
            } else {
                break;
            };
            let right = self.parse_factor()?;
            left = self.binary_node(left, op, right)?;
        }
        Ok(left)
    }

    fn parse_factor(&mut self) -> Result<i64, String> {
        self.skip_ws();
        let op = match self.peek() {
            Some('+') => Some("UAdd"),
            Some('-') => Some("USub"),
            Some('~') => Some("Invert"),
            _ => None,
        };
        if let Some(op) = op {
            self.position += 1;
            let operand = self.parse_factor()?;
            let op = operator_instance(op).ok_or_else(|| format!("missing ast.{op} class"))?;
            return Ok(build_node("UnaryOp", &[op, operand], &[]));
        }
        self.parse_power()
    }

    fn parse_power(&mut self) -> Result<i64, String> {
        let base = self.parse_primary()?;
        if self.eat_str("**") {
            let exponent = self.parse_factor()?;
            return self.binary_node(base, "Pow", exponent);
        }
        Ok(base)
    }

    fn parse_binary<F>(
        &mut self,
        mut next: F,
        operator: &str,
        name: &'static str,
    ) -> Result<i64, String>
    where
        F: FnMut(&mut ExprParser) -> Result<i64, String>,
    {
        let mut left = next(self)?;
        while self.eat_str(operator) {
            let right = next(self)?;
            left = self.binary_node(left, name, right)?;
        }
        Ok(left)
    }

    fn binary_node(&mut self, left: i64, op: &str, right: i64) -> Result<i64, String> {
        let op = operator_instance(op).ok_or_else(|| format!("missing ast.{op} class"))?;
        Ok(build_node("BinOp", &[left, op, right], &[]))
    }

    fn parse_primary(&mut self) -> Result<i64, String> {
        let mut value = self.parse_atom()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('(') => {
                    self.position += 1;
                    value = self.parse_call(value)?;
                }
                Some('[') => {
                    self.position += 1;
                    let slice = self.parse_subscript()?;
                    value = build_node("Subscript", &[value, slice], &[]);
                }
                Some('.') => {
                    self.position += 1;
                    self.skip_ws();
                    let Some(name) = self.parse_identifier() else {
                        return Err("expected an attribute name after '.'".to_string());
                    };
                    let attr = value::str_alloc(name);
                    value = build_node("Attribute", &[value, attr], &[]);
                }
                _ => break,
            }
        }
        Ok(value)
    }

    fn parse_atom(&mut self) -> Result<i64, String> {
        self.skip_ws();
        match self.peek() {
            None => Err("unexpected end of input".to_string()),
            Some('(') => {
                self.position += 1;
                self.skip_ws();
                if self.eat(')') {
                    return Ok(build_node("Tuple", &[value::list_new(Vec::new())], &[]));
                }
                let first = self.parse_test()?;
                self.skip_ws();
                if self.eat(',') {
                    let mut items = vec![first];
                    loop {
                        self.skip_ws();
                        if self.eat(')') {
                            break;
                        }
                        items.push(self.parse_test()?);
                        self.skip_ws();
                        if !self.eat(',') {
                            self.expect(')')?;
                            break;
                        }
                    }
                    Ok(build_node("Tuple", &[value::list_new(items)], &[]))
                } else {
                    self.expect(')')?;
                    Ok(first)
                }
            }
            Some('[') => {
                self.position += 1;
                let items = self.parse_sequence(']')?;
                Ok(build_node("List", &[value::list_new(items)], &[]))
            }
            Some('{') => {
                self.position += 1;
                self.skip_ws();
                if self.eat('}') {
                    return Ok(build_node(
                        "Dict",
                        &[value::list_new(Vec::new()), value::list_new(Vec::new())],
                        &[],
                    ));
                }
                let mut keys = Vec::new();
                let mut values = Vec::new();
                let mut is_dict = false;
                loop {
                    self.skip_ws();
                    if self.eat_str("**") {
                        keys.push(NONE);
                        values.push(self.parse_test()?);
                        is_dict = true;
                    } else {
                        let first = self.parse_test()?;
                        self.skip_ws();
                        if self.eat(':') {
                            is_dict = true;
                            keys.push(first);
                            values.push(self.parse_test()?);
                        } else {
                            keys.push(first);
                        }
                    }
                    self.skip_ws();
                    if self.eat('}') {
                        break;
                    }
                    self.expect(',')?;
                    self.skip_ws();
                    if self.eat('}') {
                        break;
                    }
                }
                if is_dict {
                    Ok(build_node(
                        "Dict",
                        &[value::list_new(keys), value::list_new(values)],
                        &[],
                    ))
                } else {
                    Ok(build_node("Set", &[value::list_new(keys)], &[]))
                }
            }
            Some('*') => {
                self.position += 1;
                let value = self.parse_test()?;
                Ok(build_node("Starred", &[value], &[]))
            }
            Some(quote) if quote == '\'' || quote == '"' => {
                let mut text = self.parse_string()?;
                loop {
                    self.skip_ws();
                    match self.peek() {
                        Some(next) if next == '\'' || next == '"' => {
                            text.push_str(&self.parse_string()?);
                        }
                        _ => break,
                    }
                }
                Ok(build_node("Constant", &[value::str_alloc(text)], &[]))
            }
            Some(ch) if ch.is_ascii_digit() || ch == '.' => {
                let number = self.parse_number()?;
                Ok(build_node("Constant", &[number], &[]))
            }
            Some(ch) if ch == '_' || ch.is_alphanumeric() => {
                let Some(name) = self.parse_identifier() else {
                    return Err("invalid identifier".to_string());
                };
                match name.as_str() {
                    "True" => Ok(build_node("Constant", &[value::TRUE], &[])),
                    "False" => Ok(build_node("Constant", &[value::FALSE], &[])),
                    "None" => Ok(build_node("Constant", &[NONE], &[])),
                    "and" | "or" | "not" | "in" | "is" | "if" | "else" | "for" | "lambda" => {
                        Err(format!("unexpected keyword {name:?}"))
                    }
                    _ => Ok(build_node("Name", &[value::str_alloc(name)], &[])),
                }
            }
            Some(other) => Err(format!("unexpected character {other:?}")),
        }
    }

    fn parse_sequence(&mut self, closer: char) -> Result<Vec<i64>, String> {
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if self.eat(closer) {
                break;
            }
            items.push(self.parse_test()?);
            self.skip_ws();
            if self.eat(closer) {
                break;
            }
            self.expect(',')?;
            self.skip_ws();
            if self.eat(closer) {
                break;
            }
        }
        Ok(items)
    }

    fn parse_call(&mut self, function: i64) -> Result<i64, String> {
        let mut args = Vec::new();
        let mut keywords = Vec::new();
        loop {
            self.skip_ws();
            if self.eat(')') {
                break;
            }
            if self.eat_str("**") {
                let value = self.parse_test()?;
                keywords.push(build_node("keyword", &[NONE, value], &[]));
            } else if self.eat_str("*") {
                let value = self.parse_test()?;
                args.push(build_node("Starred", &[value], &[]));
            } else if let Some(name) = self.peek_keyword_argument() {
                let _ = self.parse_identifier();
                self.skip_ws();
                self.expect('=')?;
                let value = self.parse_test()?;
                let name_value = value::str_alloc(name);
                keywords.push(build_node("keyword", &[name_value, value], &[]));
            } else {
                args.push(self.parse_test()?);
            }
            self.skip_ws();
            if self.eat(')') {
                break;
            }
            self.expect(',')?;
            self.skip_ws();
            if self.eat(')') {
                break;
            }
        }
        Ok(build_node(
            "Call",
            &[function, value::list_new(args), value::list_new(keywords)],
            &[],
        ))
    }

    /// Returns the keyword name when the next tokens are `NAME =` (and not
    /// `NAME ==`), leaving the position untouched.
    fn peek_keyword_argument(&mut self) -> Option<String> {
        let saved = self.position;
        self.skip_ws();
        let name = self.parse_identifier();
        let result = if name.is_some() {
            self.skip_ws();
            if self.peek() == Some('=') && self.peek_at(1) != Some('=') {
                name
            } else {
                None
            }
        } else {
            None
        };
        self.position = saved;
        result
    }

    fn parse_subscript(&mut self) -> Result<i64, String> {
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            let item = self.parse_subscript_item()?;
            items.push(item);
            self.skip_ws();
            if self.eat(']') {
                break;
            }
            self.expect(',')?;
            self.skip_ws();
            if self.eat(']') {
                break;
            }
        }
        if items.len() == 1 {
            Ok(items[0])
        } else {
            Ok(build_node("Tuple", &[value::list_new(items)], &[]))
        }
    }

    fn parse_subscript_item(&mut self) -> Result<i64, String> {
        self.skip_ws();
        let lower = if self.eat(':') {
            NONE
        } else {
            let lower = self.parse_test()?;
            self.skip_ws();
            if self.peek() != Some(':') {
                return Ok(lower);
            }
            self.position += 1;
            lower
        };
        // A slice. `lower` is NONE when the text started with ':'.
        self.skip_ws();
        let upper =
            if self.peek() == Some(':') || self.peek() == Some(',') || self.peek() == Some(']') {
                NONE
            } else {
                self.parse_test()?
            };
        self.skip_ws();
        let step = if self.eat(':') {
            self.skip_ws();
            if self.peek() == Some(',') || self.peek() == Some(']') {
                NONE
            } else {
                self.parse_test()?
            }
        } else {
            NONE
        };
        Ok(build_node("Slice", &[lower, upper, step], &[]))
    }

    fn parse_identifier(&mut self) -> Option<String> {
        let start = self.position;
        while let Some(ch) = self.peek() {
            if ch == '_' || ch.is_alphanumeric() {
                self.position += 1;
            } else {
                break;
            }
        }
        if self.position == start {
            return None;
        }
        Some(self.chars[start..self.position].iter().collect())
    }

    fn eat_word(&mut self, word: &str) -> bool {
        self.skip_ws();
        let start = self.position;
        let mut index = start;
        while let Some(ch) = self.chars.get(index) {
            if ch.is_ascii_alphanumeric() || *ch == '_' {
                index += 1;
            } else {
                break;
            }
        }
        if self.chars[start..index].iter().copied().eq(word.chars()) {
            self.position = index;
            true
        } else {
            false
        }
    }

    fn eat_str(&mut self, text: &str) -> bool {
        self.skip_ws();
        let mut index = self.position;
        for expected in text.chars() {
            if self.chars.get(index).copied() != Some(expected) {
                return false;
            }
            index += 1;
        }
        self.position = index;
        true
    }

    fn parse_number(&mut self) -> Result<i64, String> {
        let start = self.position;
        let mut seen_dot = false;
        let mut seen_exponent = false;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() || ch == '_' {
                self.position += 1;
            } else if ch == '.' && !seen_dot && !seen_exponent {
                seen_dot = true;
                self.position += 1;
            } else if (ch == 'e' || ch == 'E') && !seen_exponent {
                seen_exponent = true;
                self.position += 1;
                if matches!(self.peek(), Some('+' | '-')) {
                    self.position += 1;
                }
            } else {
                break;
            }
        }
        let text: String = self.chars[start..self.position]
            .iter()
            .filter(|ch| **ch != '_')
            .collect();
        if text.is_empty() || text == "." {
            return Err("invalid number literal".to_string());
        }
        if seen_dot || seen_exponent {
            let value = text
                .parse::<f64>()
                .map_err(|_| format!("invalid float literal {text:?}"))?;
            return Ok(value::float_new(value));
        }
        match text.parse::<i64>() {
            Ok(number) => Ok(ops::int_result(number)),
            Err(_) => match text.parse::<BigInt>() {
                Ok(number) => Ok(value::bigint_new(number)),
                Err(_) => Err(format!("invalid integer literal {text:?}")),
            },
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let Some(quote) = self.advance() else {
            return Err("unterminated string literal".to_string());
        };
        let triple = self.peek() == Some(quote) && self.peek_at(1) == Some(quote);
        if triple {
            self.position += 2;
        }
        let mut text = String::new();
        loop {
            let Some(ch) = self.advance() else {
                return Err("unterminated string literal".to_string());
            };
            if ch == quote {
                if !triple {
                    break;
                }
                if self.peek() == Some(quote) && self.peek_at(1) == Some(quote) {
                    self.position += 2;
                    break;
                }
                text.push(ch);
                continue;
            }
            if ch == '\\' {
                let Some(escaped) = self.advance() else {
                    return Err("unterminated escape sequence".to_string());
                };
                text.push(match escaped {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    '0' => '\0',
                    'a' => '\u{7}',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'v' => '\u{b}',
                    '\\' => '\\',
                    '\'' => '\'',
                    '"' => '"',
                    'x' => self.parse_hex_escape(2)?,
                    'u' => self.parse_hex_escape(4)?,
                    'U' => self.parse_hex_escape(8)?,
                    other => other,
                });
            } else {
                text.push(ch);
            }
        }
        Ok(text)
    }

    fn parse_hex_escape(&mut self, digits: usize) -> Result<char, String> {
        let start = self.position;
        for _ in 0..digits {
            match self.peek() {
                Some(ch) if ch.is_ascii_hexdigit() => self.position += 1,
                _ => return Err("invalid hexadecimal escape".to_string()),
            }
        }
        let text: String = self.chars[start..self.position].iter().collect();
        let code = u32::from_str_radix(&text, 16).map_err(|_| "invalid escape".to_string())?;
        char::from_u32(code).ok_or_else(|| "invalid escape code point".to_string())
    }
}

// ---------------------------------------------------------------------------
// ast.unparse
// ---------------------------------------------------------------------------

const PREC_OR: u8 = 1;
const PREC_AND: u8 = 2;
const PREC_NOT: u8 = 3;
const PREC_CMP: u8 = 4;
const PREC_BOR: u8 = 5;
const PREC_BXOR: u8 = 6;
const PREC_BAND: u8 = 7;
const PREC_SHIFT: u8 = 8;
const PREC_SUM: u8 = 9;
const PREC_TERM: u8 = 10;
const PREC_FACTOR: u8 = 11;
const PREC_POWER: u8 = 12;
const PREC_ATOM: u8 = 13;

fn unparse(args: &[i64]) -> i64 {
    let Some(node) = args.first().copied() else {
        return type_error("unparse() missing required argument 'node'");
    };
    if !is_ast_instance(node) {
        return type_error(&format!(
            "unparse() argument must be an AST node, not {}",
            value::type_name(node)
        ));
    }
    match render(node, 0) {
        Ok(text) => value::str_alloc(text),
        Err(message) => exc::raise_with_message(NOT_IMPLEMENTED_ERROR, &message),
    }
}

fn is_ast_instance(value_arg: i64) -> bool {
    let Some(class) = value::instance_class_value(value_arg) else {
        return false;
    };
    ast_class_handle().is_some_and(|ast| value::is_subclass_of(class, ast))
}

fn node_name(node: i64) -> Option<&'static str> {
    let class = value::instance_class_value(node)?;
    value::class_display_name(class)
}

fn field(node: i64, name: &str) -> i64 {
    value::instance_attr(node, name).unwrap_or(NONE)
}

fn render(node: i64, min_precedence: u8) -> Result<String, String> {
    let (text, precedence) = render_inner(node)?;
    if precedence < min_precedence {
        Ok(format!("({text})"))
    } else {
        Ok(text)
    }
}

fn render_inner(node: i64) -> Result<(String, u8), String> {
    let Some(name) = node_name(node) else {
        return Err(format!(
            "ast.unparse() cannot render {}",
            value::type_name(node)
        ));
    };
    let result = match name {
        "Expression" => (render(field(node, "body"), 0)?, 0),
        "Module" => {
            if let Some(source) = value::as_str(field(node, "_source")) {
                return Ok((source.to_string(), 0));
            }
            let mut parts = Vec::new();
            for statement in value::as_list(field(node, "body")).unwrap_or(&[]) {
                match node_name(*statement) {
                    Some("Expr") => parts.push(render(field(*statement, "value"), 0)?),
                    _ => {
                        return Err(format!(
                            "ast.unparse() does not support module node {}",
                            node_name(*statement).unwrap_or("?")
                        ))
                    }
                }
            }
            (parts.join("\n"), 0)
        }
        "Expr" => (render(field(node, "value"), 0)?, 0),
        "Name" => match value::as_str(field(node, "id")) {
            Some(id) => (id.to_string(), PREC_ATOM),
            None => return Err("ast.Name is missing its id".to_string()),
        },
        "Constant" => (value::repr(field(node, "value")), PREC_ATOM),
        "Attribute" => {
            let value_text = render(field(node, "value"), PREC_ATOM)?;
            let Some(attr) = value::as_str(field(node, "attr")) else {
                return Err("ast.Attribute is missing its attr".to_string());
            };
            (format!("{value_text}.{attr}"), PREC_ATOM)
        }
        "Subscript" => {
            let value_text = render(field(node, "value"), PREC_ATOM)?;
            let slice = field(node, "slice");
            let slice_text = match node_name(slice) {
                Some("Slice") => render_slice(slice)?,
                _ => render(slice, 0)?,
            };
            (format!("{value_text}[{slice_text}]"), PREC_ATOM)
        }
        "Call" => {
            let function = render(field(node, "func"), PREC_ATOM)?;
            let mut parts = Vec::new();
            for argument in value::as_list(field(node, "args")).unwrap_or(&[]) {
                parts.push(render(*argument, 0)?);
            }
            for keyword in value::as_list(field(node, "keywords")).unwrap_or(&[]) {
                parts.push(render_keyword(*keyword)?);
            }
            (format!("{function}({})", parts.join(", ")), PREC_ATOM)
        }
        "keyword" => return Err("ast.keyword cannot be rendered outside a call".to_string()),
        "Tuple" | "List" | "Set" => render_sequence(name, node)?,
        "Dict" => render_dict(node)?,
        "Slice" => return Err("ast.Slice cannot be rendered outside a subscript".to_string()),
        "Starred" => (
            format!("*{}", render(field(node, "value"), PREC_FACTOR)?),
            PREC_FACTOR,
        ),
        "BinOp" => render_binop(node)?,
        "UnaryOp" => render_unary(node)?,
        "BoolOp" => render_boolop(node)?,
        "Compare" => render_compare(node)?,
        "IfExp" => {
            let test = render(field(node, "test"), PREC_OR)?;
            let body = render(field(node, "body"), 0)?;
            let orelse = render(field(node, "orelse"), 0)?;
            (format!("{body} if {test} else {orelse}"), PREC_OR)
        }
        other => {
            return Err(format!(
                "ast.unparse() does not support the '{other}' node in Hydro"
            ))
        }
    };
    Ok(result)
}

fn render_slice(node: i64) -> Result<String, String> {
    let mut text = String::new();
    let lower = field(node, "lower");
    if lower != NONE {
        text.push_str(&render(lower, 0)?);
    }
    text.push(':');
    let upper = field(node, "upper");
    if upper != NONE {
        text.push_str(&render(upper, 0)?);
    }
    let step = field(node, "step");
    if step != NONE {
        text.push(':');
        text.push_str(&render(step, 0)?);
    }
    Ok(text)
}

fn render_keyword(node: i64) -> Result<String, String> {
    let value_text = render(field(node, "value"), 0)?;
    let argument = field(node, "arg");
    if argument == NONE {
        Ok(format!("**{value_text}"))
    } else {
        let Some(name) = value::as_str(argument) else {
            return Err("ast.keyword has an invalid arg".to_string());
        };
        Ok(format!("{name}={value_text}"))
    }
}

fn render_sequence(name: &str, node: i64) -> Result<(String, u8), String> {
    let items = value::as_list(field(node, "elts")).unwrap_or(&[]);
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        parts.push(render(*item, 0)?);
    }
    let body = parts.join(", ");
    let (open, close) = match name {
        "List" => ('[', ']'),
        "Set" => ('{', '}'),
        _ => ('(', ')'),
    };
    let trailing = if name == "Tuple" && parts.len() == 1 {
        ","
    } else {
        ""
    };
    Ok((format!("{open}{body}{trailing}{close}"), PREC_ATOM))
}

fn render_dict(node: i64) -> Result<(String, u8), String> {
    let keys = value::as_list(field(node, "keys")).unwrap_or(&[]);
    let values = value::as_list(field(node, "values")).unwrap_or(&[]);
    if keys.len() != values.len() {
        return Err("ast.Dict has mismatched keys/values".to_string());
    }
    let mut parts = Vec::with_capacity(keys.len());
    for (key, value) in keys.iter().zip(values) {
        if *key == NONE {
            parts.push(format!("**{}", render(*value, 0)?));
        } else {
            parts.push(format!("{}: {}", render(*key, 0)?, render(*value, 0)?));
        }
    }
    Ok((format!("{{{}}}", parts.join(", ")), PREC_ATOM))
}

fn operator_symbol(node: i64) -> Result<(&'static str, u8), String> {
    let Some(name) = node_name(node) else {
        return Err("operator is not an AST node".to_string());
    };
    Ok(match name {
        "Add" => ("+", PREC_SUM),
        "Sub" => ("-", PREC_SUM),
        "Mult" => ("*", PREC_TERM),
        "MatMult" => ("@", PREC_TERM),
        "Div" => ("/", PREC_TERM),
        "FloorDiv" => ("//", PREC_TERM),
        "Mod" => ("%", PREC_TERM),
        "Pow" => ("**", PREC_POWER),
        "LShift" => ("<<", PREC_SHIFT),
        "RShift" => (">>", PREC_SHIFT),
        "BitOr" => ("|", PREC_BOR),
        "BitXor" => ("^", PREC_BXOR),
        "BitAnd" => ("&", PREC_BAND),
        other => return Err(format!("unknown binary operator '{other}'")),
    })
}

fn render_binop(node: i64) -> Result<(String, u8), String> {
    let op = field(node, "op");
    let (symbol, precedence) = operator_symbol(op)?;
    let (left_min, right_min) = if symbol == "**" {
        (PREC_ATOM, PREC_POWER)
    } else {
        (precedence, precedence + 1)
    };
    let left = render(field(node, "left"), left_min)?;
    let right = render(field(node, "right"), right_min)?;
    Ok((format!("{left} {symbol} {right}"), precedence))
}

fn render_unary(node: i64) -> Result<(String, u8), String> {
    let op = field(node, "op");
    let Some(name) = node_name(op) else {
        return Err("unary operator is not an AST node".to_string());
    };
    let operand = render(field(node, "operand"), PREC_FACTOR)?;
    match name {
        "UAdd" => Ok((format!("+{operand}"), PREC_FACTOR)),
        "USub" => Ok((format!("-{operand}"), PREC_FACTOR)),
        "Invert" => Ok((format!("~{operand}"), PREC_FACTOR)),
        "Not" => {
            let operand = render(field(node, "operand"), PREC_NOT)?;
            Ok((format!("not {operand}"), PREC_NOT))
        }
        other => Err(format!("unknown unary operator '{other}'")),
    }
}

fn render_boolop(node: i64) -> Result<(String, u8), String> {
    let op = field(node, "op");
    let Some(name) = node_name(op) else {
        return Err("boolean operator is not an AST node".to_string());
    };
    let (word, precedence) = match name {
        "And" => ("and", PREC_AND),
        "Or" => ("or", PREC_OR),
        other => return Err(format!("unknown boolean operator '{other}'")),
    };
    let mut parts = Vec::new();
    for value in value::as_list(field(node, "values")).unwrap_or(&[]) {
        parts.push(render(*value, precedence)?);
    }
    if parts.len() < 2 {
        return Err("ast.BoolOp needs at least two values".to_string());
    }
    Ok((parts.join(&format!(" {word} ")), precedence))
}

fn render_compare(node: i64) -> Result<(String, u8), String> {
    let mut text = render(field(node, "left"), PREC_BOR)?;
    let ops = value::as_list(field(node, "ops")).unwrap_or(&[]);
    let comparators = value::as_list(field(node, "comparators")).unwrap_or(&[]);
    if ops.len() != comparators.len() {
        return Err("ast.Compare has mismatched ops/comparators".to_string());
    }
    for (op, comparator) in ops.iter().zip(comparators) {
        let Some(name) = node_name(*op) else {
            return Err("comparison operator is not an AST node".to_string());
        };
        let symbol = match name {
            "Eq" => "==",
            "NotEq" => "!=",
            "Lt" => "<",
            "LtE" => "<=",
            "Gt" => ">",
            "GtE" => ">=",
            "Is" => "is",
            "IsNot" => "is not",
            "In" => "in",
            "NotIn" => "not in",
            other => return Err(format!("unknown comparison operator '{other}'")),
        };
        text.push_str(&format!(" {symbol} {}", render(*comparator, PREC_BOR)?));
    }
    Ok((text, PREC_CMP))
}

/// `ast.fix_missing_locations(node)`: fills absent source positions.
fn fix_missing_locations(args: &[i64]) -> i64 {
    let Some(node) = args.first().copied() else {
        return type_error("fix_missing_locations() missing required argument 'node'");
    };
    fill_locations(node);
    node
}

fn fill_locations(node: i64) {
    if !is_ast_instance(node) {
        return;
    }
    let Some(name) = node_name(node) else {
        return;
    };
    for (field_name, default) in [
        ("lineno", 1),
        ("col_offset", 0),
        ("end_lineno", 1),
        ("end_col_offset", 0),
    ] {
        let current = value::instance_attr(node, field_name);
        if matches!(current, None | Some(NONE)) {
            value::instance_setattr(node, field_name.to_string(), ops::int_result(default));
        }
    }
    let Some(spec) = NODE_SPECS.iter().find(|spec| spec.name == name) else {
        return;
    };
    for field_name in spec.fields {
        let child = field(node, *field_name);
        if child == NONE {
            continue;
        }
        if is_ast_instance(child) {
            fill_locations(child);
        } else if let Some(items) = value::as_list(child) {
            for item in items {
                fill_locations(*item);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NodeVisitor / NodeTransformer
// ---------------------------------------------------------------------------

fn visit_node(visitor: i64, node: i64) -> i64 {
    if !is_ast_instance(node) {
        return node;
    }
    let Some(name) = node_name(node) else {
        return node;
    };
    let method = format!("visit_{name}");
    if let Some(class) = value::instance_class_value(visitor) {
        if value::class_method(class, &method).is_some() {
            return methods::call(visitor, method.as_ptr(), method.len(), 1, &node);
        }
    }
    generic_visit(visitor, node)
}

fn generic_visit(visitor: i64, node: i64) -> i64 {
    let Some(name) = node_name(node) else {
        return node;
    };
    let Some(spec) = NODE_SPECS.iter().find(|spec| spec.name == name) else {
        return node;
    };
    for field_name in spec.fields {
        let child = field(node, *field_name);
        if child == NONE {
            continue;
        }
        if is_ast_instance(child) {
            let replacement = visit_node(visitor, child);
            if replacement != child && replacement != NONE {
                value::instance_setattr(node, (*field_name).to_string(), replacement);
            }
        } else if let Some(items) = value::as_list(child) {
            let length = items.len();
            for index in 0..length {
                let item = items[index];
                if !is_ast_instance(item) {
                    continue;
                }
                let replacement = visit_node(visitor, item);
                if replacement != item && replacement != NONE {
                    if let Some(mut items) = value::list_items_mut(child) {
                        items[index] = replacement;
                    }
                    heap::write_barrier_edge(child, replacement);
                }
            }
        }
    }
    node
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// `ast.literal_eval(text)`: evaluates a Python literal.
fn literal_eval(value_arg: i64) -> i64 {
    let Some(source) = value::as_str(value_arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "literal_eval() argument must be str, not {}",
                value::type_name(value_arg)
            ),
        );
    };
    // Copy before parsing: the parser allocates (strings, containers), and a
    // borrow into the source object would then span those allocations.
    let source = source.to_string();
    let mut parser = LiteralParser::new(&source);
    match parser.parse_document() {
        Ok(value) => value,
        Err(message) => {
            // `dict_set` reports an unhashable key itself; keep that pending
            // exception instead of replacing it with a generic parse error.
            if exc::pending() {
                return NONE;
            }
            exc::raise_with_message(VALUE_ERROR, &format!("malformed node or string: {message}"))
        }
    }
}

/// Recursive-descent parser over the characters of one literal expression.
struct LiteralParser {
    chars: Vec<char>,
    position: usize,
}

impl LiteralParser {
    fn new(source: &str) -> LiteralParser {
        LiteralParser {
            chars: source.chars().collect(),
            position: 0,
        }
    }

    /// Parses one value and rejects trailing non-whitespace input.
    fn parse_document(&mut self) -> Result<i64, String> {
        let parsed = self.parse_value(0)?;
        self.skip_whitespace();
        if self.position != self.chars.len() {
            return Err("unexpected trailing characters".to_string());
        }
        Ok(parsed)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.position += 1;
        Some(ch)
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.position += 1;
        }
    }

    /// Consumes `expected` after optional whitespace, reporting a parse error
    /// when it is absent.
    fn expect(&mut self, expected: char) -> Result<(), String> {
        self.skip_whitespace();
        if self.advance() == Some(expected) {
            Ok(())
        } else {
            Err(format!("expected '{expected}'"))
        }
    }

    /// Consumes `expected` when present (after optional whitespace).
    fn eat(&mut self, expected: char) -> bool {
        self.skip_whitespace();
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<i64, String> {
        if depth > MAX_LITERAL_DEPTH {
            return Err("literal is nested too deeply".to_string());
        }
        self.skip_whitespace();
        match self.peek() {
            None => Err("unexpected end of input".to_string()),
            Some('[') => self.parse_list(depth),
            Some('(') => self.parse_tuple(depth),
            Some('{') => self.parse_dict(depth),
            Some('"' | '\'') => self.parse_string().map(value::str_alloc),
            Some('+' | '-') => self.parse_signed_value(depth),
            Some(ch) if ch.is_ascii_digit() || ch == '.' => self.parse_number(),
            Some(ch) if ch.is_alphabetic() || ch == '_' => self.parse_name(),
            Some(other) => Err(format!("unexpected character '{other}'")),
        }
    }

    /// Unary `+`/`-`: only numeric literals may carry a sign in
    /// `literal_eval`.
    fn parse_signed_value(&mut self, depth: usize) -> Result<i64, String> {
        let negative = self.advance() == Some('-');
        self.skip_whitespace();
        let parsed = self.parse_value(depth + 1)?;
        if let Some(integer) = value::as_integer(parsed) {
            let signed = if negative {
                integer.checked_neg().ok_or("integer literal too large")?
            } else {
                integer
            };
            return Ok(ops::int_result(signed));
        }
        if let Some(float) = value::as_float(parsed) {
            return Ok(value::float_new(if negative { -float } else { float }));
        }
        Err("a sign must precede a number".to_string())
    }

    fn parse_name(&mut self) -> Result<i64, String> {
        let start = self.position;
        while self
            .peek()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
        {
            self.position += 1;
        }
        let name: String = self.chars[start..self.position].iter().collect();
        match name.as_str() {
            "True" => Ok(TRUE),
            "False" => Ok(FALSE),
            "None" => Ok(NONE),
            _ => Err(format!("unknown name '{name}'")),
        }
    }

    fn parse_number(&mut self) -> Result<i64, String> {
        let start = self.position;
        let mut previous = '\0';
        while let Some(ch) = self.peek() {
            let numeric = ch.is_ascii_digit() || ch == '.';
            let exponent_marker = matches!(ch, 'e' | 'E');
            let exponent_sign = matches!(ch, '+' | '-') && matches!(previous, 'e' | 'E');
            if !(numeric || exponent_marker || exponent_sign) {
                break;
            }
            previous = ch;
            self.position += 1;
        }
        if start == self.position {
            return Err("expected a number".to_string());
        }
        let text: String = self.chars[start..self.position].iter().collect();
        if text.contains('.') || text.contains('e') || text.contains('E') {
            let parsed: f64 = text
                .parse()
                .map_err(|_| format!("invalid float '{text}'"))?;
            return Ok(value::float_new(parsed));
        }
        let parsed: i64 = text
            .parse()
            .map_err(|_| format!("invalid integer '{text}'"))?;
        Ok(ops::int_result(parsed))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let quote = self.advance().ok_or("unterminated string")?;
        let mut out = String::new();
        loop {
            let ch = self.advance().ok_or("unterminated string")?;
            if ch == quote {
                return Ok(out);
            }
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            let escape = self.advance().ok_or("unterminated escape")?;
            match escape {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '0' => out.push('\0'),
                '\\' => out.push('\\'),
                '\'' => out.push('\''),
                '"' => out.push('"'),
                'x' => out.push(self.parse_hex_escape(2)?),
                'u' => out.push(self.parse_hex_escape(4)?),
                // Python preserves unknown escapes verbatim.
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        }
    }

    /// Reads `digits` hexadecimal digits and returns the scalar they encode.
    fn parse_hex_escape(&mut self, digits: usize) -> Result<char, String> {
        let mut code = 0u32;
        for _ in 0..digits {
            let ch = self.advance().ok_or("truncated escape")?;
            let digit = ch.to_digit(16).ok_or("invalid hexadecimal escape")?;
            code = code * 16 + digit;
        }
        char::from_u32(code).ok_or_else(|| "invalid Unicode escape".to_string())
    }

    fn parse_list(&mut self, depth: usize) -> Result<i64, String> {
        self.expect('[')?;
        let result = value::list_new(Vec::new());
        if self.eat(']') {
            return Ok(result);
        }
        loop {
            let item = self.parse_value(depth + 1)?;
            push_item(result, item);
            if self.eat(',') {
                if self.eat(']') {
                    return Ok(result);
                }
                continue;
            }
            self.expect(']')?;
            return Ok(result);
        }
    }

    fn parse_tuple(&mut self, depth: usize) -> Result<i64, String> {
        self.expect('(')?;
        if self.eat(')') {
            return Ok(value::tuple_new(Vec::new()));
        }
        // Elements are accumulated in a rooted list; `(value)` without a
        // trailing comma is plain grouping.
        let result = value::list_new(Vec::new());
        let mut had_comma = false;
        loop {
            let item = self.parse_value(depth + 1)?;
            push_item(result, item);
            if self.eat(',') {
                had_comma = true;
                if self.eat(')') {
                    break;
                }
                continue;
            }
            self.expect(')')?;
            break;
        }
        let items = value::as_list(result)
            .map(|items| items.to_vec())
            .unwrap_or_default();
        if items.len() == 1 && !had_comma {
            return Ok(items[0]);
        }
        Ok(value::tuple_new(items))
    }

    fn parse_dict(&mut self, depth: usize) -> Result<i64, String> {
        self.expect('{')?;
        let result = value::dict_new(Vec::new());
        if self.eat('}') {
            return Ok(result);
        }
        loop {
            let key = self.parse_value(depth + 1)?;
            self.expect(':')?;
            let item = self.parse_value(depth + 1)?;
            container::dict_set(result, key, item);
            if exc::pending() {
                return Err("unhashable dict key".to_string());
            }
            if self.eat(',') {
                if self.eat('}') {
                    return Ok(result);
                }
                continue;
            }
            self.expect('}')?;
            return Ok(result);
        }
    }
}

/// Appends `item` to `list`, recording the GC edge.
fn push_item(list: i64, item: i64) {
    if let Some(mut items) = value::list_items_mut(list) {
        items.push(item);
        crate::heap::write_barrier_edge(list, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_dict, as_list, as_tuple, encode_int};

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    fn evaluate(source: &str) -> i64 {
        call("literal_eval", &[text(source)])
    }

    #[test]
    fn scalars_are_parsed() {
        exc::init();
        assert_eq!(value::as_integer(evaluate("42")), Some(42));
        assert_eq!(value::as_integer(evaluate("-7")), Some(-7));
        assert_eq!(value::as_integer(evaluate("+3")), Some(3));
        assert_eq!(value::as_float(evaluate("2.5")), Some(2.5));
        assert_eq!(value::as_float(evaluate("-1e2")), Some(-100.0));
        assert_eq!(value::as_str(evaluate("'red'")), Some("red"));
        assert_eq!(value::as_str(evaluate("\"a\\nb\"")), Some("a\nb"));
        assert_eq!(evaluate("True"), TRUE);
        assert_eq!(evaluate("False"), FALSE);
        assert_eq!(evaluate("None"), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn containers_are_parsed() {
        exc::init();
        let list = evaluate("[1, 2, 3]");
        let items = as_list(list).expect("list");
        assert_eq!(items.len(), 3);
        assert_eq!(value::as_integer(items[2]), Some(3));

        let tuple = evaluate("('a', 1)");
        let items = as_tuple(tuple).expect("tuple");
        assert_eq!(value::as_str(items[0]), Some("a"));
        assert_eq!(value::as_integer(items[1]), Some(1));

        assert_eq!(as_tuple(evaluate("()")).map(|items| items.len()), Some(0));
        assert_eq!(value::as_integer(evaluate("(5)")), Some(5));
        assert_eq!(as_tuple(evaluate("(5,)")).map(|items| items.len()), Some(1));

        let dict = evaluate("{'k': [1, 2]}");
        let pairs = as_dict(dict).expect("dict");
        assert_eq!(pairs.len(), 1);
        assert_eq!(value::as_str(pairs[0].0), Some("k"));
        assert_eq!(as_list(pairs[0].1).map(|items| items.len()), Some(2));
        assert!(!exc::pending());
    }

    #[test]
    fn malformed_input_raises_value_error() {
        exc::init();
        assert_eq!(evaluate("1 + 2"), NONE);
        assert!(exc::pending());
        exc::clear();
        assert_eq!(evaluate("1 2"), NONE);
        assert!(exc::pending());
        exc::clear();
        assert_eq!(evaluate("{'k' 1}"), NONE);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn non_string_argument_raises_type_error() {
        exc::init();
        let number = encode_int(1).expect("small int");
        call("literal_eval", &[number]);
        assert!(exc::pending());
        exc::clear();
    }
}
