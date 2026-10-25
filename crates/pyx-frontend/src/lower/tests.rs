use super::*;
use rustpython_parser::Mode;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn parse(source: &str) -> ast::ModModule {
    match rustpython_parser::parse(source, Mode::Module, "test.py") {
        Ok(ast::Mod::Module(module)) => module,
        Ok(_) => panic!("expected a module"),
        Err(error) => panic!("parse failed: {}", error.error),
    }
}

fn lower_source(source: &str, module: &ast::ModModule) -> (LHir, Infer, Vec<Diagnostic>) {
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    let hir = lowerer.lower_module(module);
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    (hir, infer, diags)
}

fn assigned<'a>(hir: &'a LHir, name: &str) -> &'a HExpr {
    let locals = &hir.functions[0].locals;
    for stmt in &hir.functions[0].body {
        if let HStmtKind::Assign { target, value } = &stmt.kind {
            if locals[*target as usize].name == name {
                return value;
            }
        }
    }
    panic!("no assignment to '{name}'");
}

/// True when `stmts` or any nested block emits a `GlobalSet`.
fn has_global_set(stmts: &[HStmt]) -> bool {
    stmts.iter().any(|stmt| match &stmt.kind {
        HStmtKind::GlobalSet { .. } => true,
        HStmtKind::If {
            then_body,
            else_body,
            ..
        } => has_global_set(then_body) || has_global_set(else_body),
        HStmtKind::While {
            body, else_body, ..
        }
        | HStmtKind::ForRange {
            body, else_body, ..
        }
        | HStmtKind::ForEach {
            body, else_body, ..
        } => has_global_set(body) || has_global_set(else_body),
        HStmtKind::Try {
            body,
            handlers,
            else_body,
            finally_body,
        } => {
            has_global_set(body)
                || handlers.iter().any(|handler| has_global_set(&handler.body))
                || has_global_set(else_body)
                || has_global_set(finally_body)
        }
        _ => false,
    })
}

#[test]
fn import_forms_resolve_to_module_nodes() {
    let source = "import math, sys as system\n\
                      import time as t\n\
                      from functools import reduce\n\
                      from operator import add\n\
                      from math import sqrt, pi as PI\n\
                      a = math.sqrt(9)\n\
                      b = sqrt(PI)\n\
                      c = t.time()\n\
                      d = reduce(add, [1, 2])\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    match &assigned(&hir, "a").kind {
        HExprKind::ModuleCall {
            module,
            function,
            args,
            ..
        } => {
            assert_eq!(module, "math");
            assert_eq!(function, "sqrt");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected a module call, got {other:?}"),
    }
    match &assigned(&hir, "b").kind {
        HExprKind::ModuleCall {
            module, function, ..
        } => {
            assert_eq!((module.as_str(), function.as_str()), ("math", "sqrt"));
        }
        other => panic!("expected a module call, got {other:?}"),
    }
    match &assigned(&hir, "c").kind {
        HExprKind::ModuleCall {
            module, function, ..
        } => {
            assert_eq!((module.as_str(), function.as_str()), ("time", "time"));
        }
        other => panic!("expected a module call, got {other:?}"),
    }
    match &assigned(&hir, "d").kind {
        HExprKind::ModuleCall {
            module, function, ..
        } => {
            assert_eq!(
                (module.as_str(), function.as_str()),
                ("functools", "reduce")
            );
        }
        other => panic!("expected a module call, got {other:?}"),
    }
}

#[test]
fn imported_members_and_aliases_lower_to_module_attributes() {
    let source = "from math import pi as PI, sqrt\n\
                      import time as t\n\
                      a = PI\n\
                      b = sqrt\n\
                      c = t.perf_counter\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for (name, module, member) in [
        ("a", "math", "pi"),
        ("b", "math", "sqrt"),
        ("c", "time", "perf_counter"),
    ] {
        match &assigned(&hir, name).kind {
            HExprKind::ModuleAttr {
                module: got_module,
                name: got_name,
            } => {
                assert_eq!((got_module.as_str(), got_name.as_str()), (module, member));
            }
            other => panic!("expected a module attribute, got {other:?}"),
        }
    }
}

#[test]
fn nested_os_path_calls_lower_to_module_calls() {
    let source = "import os\n\
                      a = os.path.join(\"a\", \"b\")\n\
                      b = os.path.dirname(\"a/b\")\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for (name, function) in [("a", "join"), ("b", "dirname")] {
        match &assigned(&hir, name).kind {
            HExprKind::ModuleCall {
                module,
                function: got_function,
                ..
            } => {
                assert_eq!(module, "os.path");
                assert_eq!(got_function, function);
            }
            other => panic!("expected a module call, got {other:?}"),
        }
    }
}

#[test]
fn from_os_path_import_lowers_to_module_calls() {
    let source = "from os.path import join\nc = join(\"a\", \"b\")\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "c").kind {
        HExprKind::ModuleCall {
            module, function, ..
        } => {
            assert_eq!((module.as_str(), function.as_str()), ("os.path", "join"));
        }
        other => panic!("expected a module call, got {other:?}"),
    }
}

#[test]
fn os_path_value_is_rejected() {
    let source = "import os\np = os.path\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(diags[0].message.contains("os"), "{diags:?}");
}

#[test]
fn imports_are_visible_inside_functions() {
    let source =
        "from math import sqrt\nimport math\ndef f(x):\n    return math.floor(x) + sqrt(x)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    let HStmtKind::Return(Some(value)) = &function.body[0].kind else {
        panic!("expected a return statement");
    };
    let HExprKind::Bin { lhs, rhs, .. } = &value.kind else {
        panic!("expected a binary expression, got {:?}", value.kind);
    };
    assert!(matches!(
        &lhs.kind,
        HExprKind::ModuleCall { module, function, .. }
            if module == "math" && function == "floor"
    ));
    assert!(matches!(
        &rhs.kind,
        HExprKind::ModuleCall { module, function, .. }
            if module == "math" && function == "sqrt"
    ));
}

#[test]
fn locals_and_parameters_shadow_imports() {
    let source = "from math import sqrt\nsqrt = 1\ndef f(sqrt):\n    return sqrt(4)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    let HStmtKind::Return(Some(value)) = &function.body[0].kind else {
        panic!("expected a return statement");
    };
    assert!(
        matches!(&value.kind, HExprKind::IndirectCall { .. }),
        "shadowed name must not resolve as a module call: {:?}",
        value.kind
    );
}

#[test]
fn unknown_module_is_rejected_with_help() {
    let source = "import numpy\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert_eq!(diags[0].message, "module 'numpy' is not supported yet");
    assert!(diags[0]
        .help
        .as_deref()
        .is_some_and(|help| help.contains("math")));
}

#[test]
fn unknown_from_member_is_rejected() {
    let source = "from math import nope\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::ATTRIBUTE);
    assert_eq!(diags[0].message, "module 'math' has no attribute 'nope'");
}

#[test]
fn unknown_module_attribute_value_is_rejected() {
    let source = "import math\nx = math.nope\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::ATTRIBUTE);
    assert_eq!(diags[0].message, "module 'math' has no attribute 'nope'");
}

#[test]
fn stdlib_module_values_are_accepted() {
    // A standard-library module is a first-class value; user modules still
    // report the "not supported yet" diagnostic (covered by the CLI tests).
    let source = "import math\nm = math\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn calling_a_module_constant_is_rejected() {
    let source = "import sys\nx = sys.maxsize()\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::CALL);
    assert_eq!(diags[0].message, "'int' object is not callable");
}

#[test]
fn imports_inside_functions_bind_names() {
    let source = "def f():\n    import math\n    return math.sqrt(4)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = function_body(&hir, "f");
    assert_eq!(body.len(), 1, "{body:?}");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement, got {:?}", body[0].kind);
    };
    match &value.kind {
        HExprKind::ModuleCall {
            module, function, ..
        } => assert_eq!((module.as_str(), function.as_str()), ("math", "sqrt")),
        other => panic!("expected a direct module call, got {other:?}"),
    }
}

#[test]
fn imports_inside_functions_bind_deferred_names() {
    let source = "def f():\n    from helper import add\n    return add(1, 2)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = function_body(&hir, "f");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement, got {:?}", body[0].kind);
    };
    let HExprKind::IndirectCall { callee, .. } = &value.kind else {
        panic!("expected a deferred call, got {:?}", value.kind);
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::DeferredImport { module, member }
            if module == "helper" && member.as_deref() == Some("add")
    ));
}

#[test]
fn imports_inside_functions_bind_deferred_module_attributes() {
    let source = "def f():\n    import helper\n    return helper.add(1, 2)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = function_body(&hir, "f");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement, got {:?}", body[0].kind);
    };
    let HExprKind::IndirectCall { callee, .. } = &value.kind else {
        panic!("expected a deferred call, got {:?}", value.kind);
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::DeferredImport { module, member }
            if module == "helper" && member.as_deref() == Some("add")
    ));
}

#[test]
fn function_local_deferred_names_do_not_shadow_top_level_imports() {
    let source =
        "from math import sqrt\ndef f():\n    from helper import sqrt\n    return sqrt(4)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = function_body(&hir, "f");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement, got {:?}", body[0].kind);
    };
    assert!(matches!(
        &value.kind,
        HExprKind::ModuleCall { module, function, .. }
            if module == "math" && function == "sqrt"
    ));
}

#[test]
fn imports_inside_blocks_bind_names() {
    let source = "if True:\n    from math import sqrt\n\nx = sqrt(4)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::ModuleCall {
            module, function, ..
        } => assert_eq!((module.as_str(), function.as_str()), ("math", "sqrt")),
        other => panic!("expected a direct module call, got {other:?}"),
    }
}

#[test]
fn star_and_relative_imports_are_rejected() {
    for source in ["from math import *\n", "from . import math\n"] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert_eq!(diags.len(), 1, "{source}: {diags:?}");
        assert_eq!(diags[0].code, codes::UNSUPPORTED);
    }
}

#[test]
fn imports_emit_no_statements() {
    let source = "import math\nfrom math import pi\nx = pi\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(hir.functions[0].body.len(), 1);
    assert!(matches!(
        &hir.functions[0].body[0].kind,
        HStmtKind::Assign { .. }
    ));
}

#[test]
fn undefined_name_suggests_a_local() {
    let source = "value = 1\nprint(valu)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::NAME);
    assert_eq!(diags[0].message, "name 'valu' is not defined");
    assert_eq!(diags[0].help.as_deref(), Some("did you mean `value`?"));
}

#[test]
fn undefined_name_without_a_close_match_has_no_help() {
    let source = "value = 1\nprint(zzzzzz)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::NAME);
    assert_eq!(diags[0].help, None);
}

#[test]
fn unsupported_construct_sets_the_span_and_help() {
    let source = "nonlocal x\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert_eq!((diags[0].line, diags[0].col), (1, 1));
    assert!(diags[0].end_col > 1, "span clamps to the first line");
    assert_eq!(
        diags[0].help.as_deref(),
        Some("this construct is not implemented yet; see docs/TODO.md")
    );
}

#[test]
fn declaration_errors_use_the_decl_code() {
    let source = "def f():\n    pass\ndef f():\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::DECL);
    assert_eq!(diags[0].message, "duplicate function definition 'f'");
}

#[test]
fn try_with_else_and_finally_lowers_all_parts() {
    let source = "try:\n    print('body')\nexcept ValueError:\n    print('handler')\nelse:\n    print('else')\nfinally:\n    print('fin')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = &hir.functions[0].body;
    assert_eq!(body.len(), 1);
    let HStmtKind::Try {
        body,
        handlers,
        else_body,
        finally_body,
    } = &body[0].kind
    else {
        panic!("expected a try statement");
    };
    assert_eq!(body.len(), 1);
    assert_eq!(handlers.len(), 1);
    assert_eq!(handlers[0].exc, Some(ExcKind::ValueError));
    assert_eq!(handlers[0].binding, None);
    assert_eq!(handlers[0].body.len(), 1);
    assert_eq!(else_body.len(), 1);
    assert_eq!(finally_body.len(), 1);
}

#[test]
fn bare_except_lowers_without_exception_kind() {
    let source = "try:\n    raise KeyError('x')\nexcept:\n    print('bare')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Try { handlers, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try statement");
    };
    assert_eq!(handlers.len(), 1);
    assert_eq!(handlers[0].exc, None);
}

#[test]
fn bare_except_must_be_last() {
    let source = "try:\n    pass\nexcept:\n    pass\nexcept ValueError:\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|d| d.message == "bare except must be the last handler"),
        "{diags:?}"
    );
}

#[test]
fn exception_and_base_exception_handlers_resolve() {
    let source = "try:\n    pass\nexcept Exception:\n    pass\nexcept BaseException:\n    pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Try { handlers, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try statement");
    };
    let kinds: Vec<Option<ExcKind>> = handlers.iter().map(|h| h.exc).collect();
    assert_eq!(
        kinds,
        vec![Some(ExcKind::Exception), Some(ExcKind::BaseException)]
    );
}

#[test]
fn tuple_handler_expands_in_source_order_with_a_shared_body() {
    let source = "try:\n    pass\nexcept (TypeError, ValueError) as e:\n    print(e)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Try { handlers, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try statement");
    };
    assert_eq!(handlers.len(), 2, "{handlers:?}");
    assert_eq!(handlers[0].exc, Some(ExcKind::TypeError));
    assert_eq!(handlers[1].exc, Some(ExcKind::ValueError));
    let binding = handlers[0].binding.expect("handler binding");
    assert_eq!(handlers[1].binding, Some(binding));
    assert_eq!(handlers[0].body.len(), 1);
    assert_eq!(
        format!("{:?}", handlers[0].body),
        format!("{:?}", handlers[1].body),
        "tuple handlers must share the same lowered body"
    );
}

#[test]
fn single_element_exception_tuple_expands() {
    let source = "try:\n    pass\nexcept (ValueError,) as e:\n    print(e)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Try { handlers, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try statement");
    };
    assert_eq!(handlers.len(), 1);
    assert_eq!(handlers[0].exc, Some(ExcKind::ValueError));
}

#[test]
fn tuple_handler_approximates_user_classes_and_rejects_non_names() {
    // Non-builtin names and dotted names approximate RuntimeError; builtin
    // tags stay exact.
    let source = "try:\n    pass\nexcept (ValueError, NopeError, ex.Error):\n    pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Try { handlers, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try statement");
    };
    let kinds: Vec<_> = handlers.iter().map(|handler| handler.exc).collect();
    assert_eq!(
        kinds,
        vec![
            Some(ExcKind::ValueError),
            Some(ExcKind::RuntimeError),
            Some(ExcKind::RuntimeError)
        ]
    );

    let source = "try:\n    pass\nexcept (ValueError, 1):\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|d| d.message == "only simple exception names are supported in exception tuples"),
        "{diags:?}"
    );
}

#[test]
fn empty_exception_tuple_is_rejected() {
    let source = "try:\n    pass\nexcept ():\n    pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|d| d.message == "empty exception tuple in except handler"),
        "{diags:?}"
    );
    let HStmtKind::Try { handlers, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try statement");
    };
    assert!(handlers.is_empty(), "{handlers:?}");
}

#[test]
fn raise_from_lowers_the_cause_after_the_message() {
    let source =
        "try:\n    pass\nexcept ValueError as e:\n    raise RuntimeError('outer') from e\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Try { handlers, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try statement");
    };
    let binding = handlers[0].binding.expect("handler binding");
    let HStmtKind::Raise { exc, msg, cause } = &handlers[0].body[0].kind else {
        panic!("expected a raise statement, got {:?}", handlers[0].body[0]);
    };
    assert_eq!(*exc, Some(ExcKind::RuntimeError));
    assert!(matches!(
        &msg.as_ref().expect("message").kind,
        HExprKind::Const(Const::Str(_))
    ));
    assert!(
        matches!(&cause.as_ref().expect("cause").kind, HExprKind::Local(id) if *id == binding),
        "{cause:?}"
    );
}

#[test]
fn raise_from_none_lowers_the_none_constant_as_cause() {
    let source = "raise ValueError('inner') from None\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Raise { exc, msg, cause } = &hir.functions[0].body[0].kind else {
        panic!("expected a raise statement");
    };
    assert_eq!(*exc, Some(ExcKind::ValueError));
    assert!(msg.is_some());
    assert!(matches!(
        &cause.as_ref().expect("cause").kind,
        HExprKind::Const(Const::None)
    ));
}

#[test]
fn raise_from_without_an_exception_is_rejected() {
    let source = "raise ValueError\n";
    let mut module = parse(source);
    let ast::Stmt::Raise(raise) = &mut module.body[0] else {
        panic!("expected a raise statement");
    };
    raise.exc = None;
    let range = raise.range;
    raise.cause = Some(Box::new(ast::Expr::Constant(ast::ExprConstant {
        value: ast::Constant::None,
        kind: None,
        range,
    })));
    let (hir, _, diags) = lower_source(source, &module);
    assert!(
        diags
            .iter()
            .any(|d| d.message == "raise ... from ... requires an exception"),
        "{diags:?}"
    );
    assert!(hir.functions[0].body.is_empty());
}

#[test]
fn try_else_without_except_is_rejected() {
    let source = "try:\n    pass\nexcept ValueError:\n    pass\nelse:\n    pass\n";
    let mut module = parse(source);
    let ast::Stmt::Try(try_stmt) = &mut module.body[0] else {
        panic!("expected a try statement");
    };
    try_stmt.handlers.clear();
    let (hir, _, diags) = lower_source(source, &module);
    assert!(
        diags
            .iter()
            .any(|d| d.message == "try with else requires at least one except handler"),
        "{diags:?}"
    );
    assert!(hir.functions[0].body.is_empty());
}

#[test]
fn locals_assigned_in_all_try_parts_get_slots() {
    let source =
        "try:\n    a = 1\nexcept ValueError:\n    b = 2\nelse:\n    c = 3\nfinally:\n    d = 4\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let locals: Vec<&str> = hir.functions[0]
        .locals
        .iter()
        .map(|l| l.name.as_str())
        .collect();
    for name in ["a", "b", "c", "d"] {
        assert!(locals.contains(&name), "missing local '{name}': {locals:?}");
    }
}

/// Name of the hidden local whose `__exit__(None, None, None)` runs in a
/// desugared `with` `finally` block.
fn exit_local_name<'a>(locals: &'a [HLocal], finally_body: &[HStmt]) -> &'a str {
    assert_eq!(finally_body.len(), 1, "finally must only exit the item");
    let HStmtKind::If {
        then_body,
        else_body,
        ..
    } = &finally_body[0].kind
    else {
        panic!("expected the guarded __exit__ block");
    };
    assert!(else_body.is_empty());
    assert_eq!(then_body.len(), 1);
    let HStmtKind::Expr(exit) = &then_body[0].kind else {
        panic!("expected an __exit__() expression");
    };
    let HExprKind::MethodCall {
        object,
        method,
        args,
        keywords,
        ..
    } = &exit.kind
    else {
        panic!("expected a method call, got {:?}", exit.kind);
    };
    assert_eq!(method, "__exit__");
    assert!(keywords.is_empty());
    assert_eq!(args.len(), 3, "three exception arguments are passed");
    for arg in args {
        assert!(
            matches!(&arg.kind, HExprKind::Const(Const::None)),
            "expected None, got {:?}",
            arg.kind
        );
    }
    let HExprKind::Local(local) = &object.kind else {
        panic!("expected __exit__() on a hidden local");
    };
    &locals[*local as usize].name
}

#[test]
fn with_open_lowers_to_try_finally_with_exit() {
    let source = "with open('x') as f:\n    print(f.read())\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let locals = &hir.functions[0].locals;
    assert_eq!(hir.functions[0].body.len(), 1);

    let HStmtKind::Try {
        body,
        handlers,
        else_body,
        finally_body,
    } = &hir.functions[0].body[0].kind
    else {
        panic!("expected a try/finally statement");
    };
    assert!(handlers.is_empty());
    assert!(else_body.is_empty());
    assert_eq!(exit_local_name(locals, finally_body), "$with0");

    // Prelude: evaluate `open(...)` once into `$with0`, then bind `f` to the
    // result of `$with0.__enter__()`.
    assert_eq!(body.len(), 3);
    let HStmtKind::Assign { target, value } = &body[0].kind else {
        panic!("expected the hidden local assignment");
    };
    assert_eq!(locals[*target as usize].name, "$with0");
    assert_eq!(locals[*target as usize].ty, Ty::Concrete(Type::Any));
    let HExprKind::ModuleCall {
        module,
        function,
        args,
        ..
    } = &value.kind
    else {
        panic!("expected io.open, got {:?}", value.kind);
    };
    assert_eq!((module.as_str(), function.as_str()), ("io", "open"));
    assert_eq!(args.len(), 1, "positional call keeps only the given args");
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::Str(s)) if s == "x"));

    let HStmtKind::Assign { target, value } = &body[1].kind else {
        panic!("expected the `as f` assignment");
    };
    assert_eq!(locals[*target as usize].name, "f");
    let HExprKind::MethodCall {
        object,
        method,
        args,
        keywords,
        ..
    } = &value.kind
    else {
        panic!("expected __enter__(), got {:?}", value.kind);
    };
    assert_eq!(method, "__enter__");
    assert!(args.is_empty());
    assert!(keywords.is_empty());
    let HExprKind::Local(source_local) = &object.kind else {
        panic!("expected __enter__() on the hidden local");
    };
    assert_eq!(locals[*source_local as usize].name, "$with0");

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn with_without_target_still_calls_enter() {
    let source = "with open('x'):\n    print('body')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let locals = &hir.functions[0].locals;
    let HStmtKind::Try { body, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try/finally statement");
    };
    assert_eq!(body.len(), 3);
    let HStmtKind::Expr(enter) = &body[1].kind else {
        panic!("expected an __enter__() expression statement");
    };
    let HExprKind::MethodCall {
        object,
        method,
        args,
        ..
    } = &enter.kind
    else {
        panic!("expected __enter__(), got {:?}", enter.kind);
    };
    assert_eq!(method, "__enter__");
    assert!(args.is_empty());
    let HExprKind::Local(source_local) = &object.kind else {
        panic!("expected __enter__() on the hidden local");
    };
    assert_eq!(locals[*source_local as usize].name, "$with0");
}

#[test]
fn multiple_with_items_exit_in_reverse_order() {
    let source = "with open('a') as a, open('b') as b:\n    pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let locals = &hir.functions[0].locals;
    assert_eq!(hir.functions[0].body.len(), 1);

    let HStmtKind::Try {
        body, finally_body, ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!("expected the outer try");
    };
    assert_eq!(exit_local_name(locals, finally_body), "$with1");
    assert_eq!(body.len(), 3, "outer assignments plus the inner try");
    let HStmtKind::Try { finally_body, .. } = &body[2].kind else {
        panic!("expected the inner try");
    };
    assert_eq!(exit_local_name(locals, finally_body), "$with0");
}

#[test]
fn with_exit_runs_from_finally_after_a_raising_body() {
    let source = "def f():\n    with open('x'):\n        raise ValueError('boom')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let HStmtKind::Try {
        body, finally_body, ..
    } = &f.body[0].kind
    else {
        panic!("expected a try/finally statement");
    };
    // The raising body stays inside the try, so the exit call still runs and
    // the original exception propagates.
    assert!(matches!(
        &body.last().expect("with body").kind,
        HStmtKind::Raise { .. }
    ));
    assert_eq!(exit_local_name(&f.locals, finally_body), "$with0");
}

#[test]
fn with_open_for_loop_checks_cleanly() {
    let source = "with open('data.txt') as f:\n    for line in f:\n        print(line)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Try { body, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try/finally statement");
    };
    assert!(matches!(&body[2].kind, HStmtKind::ForEach { .. }));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn with_tuple_as_target_is_diagnosed() {
    let source = "with open('x') as (a, b):\n    pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|d| d.message == "with ... as only supports a simple name target"),
        "{diags:?}"
    );
    // The body is still lowered and exited so later errors stay useful.
    assert_eq!(hir.functions[0].body.len(), 1);
    assert!(matches!(
        &hir.functions[0].body[0].kind,
        HStmtKind::Try { .. }
    ));
}

#[test]
fn with_target_gets_a_local_slot_inside_functions() {
    let source = "def g():\n    with open('x') as f:\n        f.write('y')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name == "g")
        .expect("function g");
    let names: Vec<&str> = function.locals.iter().map(|l| l.name.as_str()).collect();
    assert!(names.contains(&"f"), "missing local f: {names:?}");
    assert!(matches!(&function.body[0].kind, HStmtKind::Try { .. }));
}

#[test]
fn open_keywords_are_reordered_into_module_args() {
    let source = "f = open('x', encoding='utf-8', mode='r')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::ModuleCall {
        module,
        function,
        args,
        ..
    } = &assigned(&hir, "f").kind
    else {
        panic!("expected io.open");
    };
    assert_eq!((module.as_str(), function.as_str()), ("io", "open"));
    assert_eq!(args.len(), 5);
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::Str(s)) if s == "x"));
    assert!(matches!(&args[1].kind, HExprKind::Const(Const::Str(s)) if s == "r"));
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::Str(s)) if s == "utf-8"));
    assert!(matches!(&args[3].kind, HExprKind::Const(Const::None)));
    assert!(matches!(&args[4].kind, HExprKind::Const(Const::None)));
}

#[test]
fn user_open_shadows_the_io_alias() {
    let source = "def open(x):\n    return x\nf = open(1)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "f").kind,
        HExprKind::Call {
            callee: Callee::Function(_),
            ..
        }
    ));
}

#[test]
fn local_open_binding_shadows_the_io_alias() {
    let source = "def g(open):\n    return open(1)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name == "g")
        .expect("function g");
    assert!(matches!(
        &function.body[0].kind,
        HStmtKind::Return(Some(HExpr {
            kind: HExprKind::IndirectCall { .. },
            ..
        }))
    ));
}

#[test]
fn user_function_keywords_bind_at_compile_time() {
    let source = "def f(a):\n    return a\nx = f(a=1)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::Call {
            callee: Callee::Function(_),
            args,
        } => {
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, HExprKind::Const(Const::Int(1))));
        }
        other => panic!("expected a direct call, got {other:?}"),
    }
}

#[test]
fn returns_in_finally_affect_return_type() {
    let source = "def f():\n    try:\n        return 1\n    finally:\n        return 2\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let function = program
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    assert_eq!(function.ret_ty, Type::Int);
}

#[test]
fn fstring_without_interpolation_is_an_empty_string() {
    let source = "x = f''\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    assert!(matches!(&value.kind, HExprKind::Const(Const::Str(s)) if s.is_empty()));
    assert_eq!(value.ty, Ty::Concrete(Type::Str));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn fstring_lowers_to_left_associative_str_concatenation() {
    let source = "n = 7\nx = f'a{n}b'\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[1].kind else {
        panic!("expected an assignment");
    };
    let HExprKind::Bin { op, lhs, rhs } = &value.kind else {
        panic!("expected concatenation, got {:?}", value.kind);
    };
    assert_eq!(*op, BinOp::Add);
    assert!(matches!(&rhs.kind, HExprKind::Const(Const::Str(s)) if s == "b"));
    let HExprKind::Bin {
        lhs: inner_lhs,
        rhs: inner_rhs,
        ..
    } = &lhs.kind
    else {
        panic!("expected nested concatenation, got {:?}", lhs.kind);
    };
    assert!(matches!(&inner_lhs.kind, HExprKind::Const(Const::Str(s)) if s == "a"));
    assert!(matches!(
        &inner_rhs.kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Str),
            ..
        }
    ));
    assert_eq!(value.ty, Ty::Concrete(Type::Str));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn fstring_format_spec_is_lowered_to_the_format_builtin() {
    let source = "n = 7\nx = f'{n:>5}'\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::Call { callee, args } => {
            assert_eq!(*callee, Callee::Builtin(Builtin::Format));
            assert_eq!(args.len(), 3);
            assert!(matches!(&args[0].kind, HExprKind::Local(_)));
            assert!(matches!(&args[1].kind, HExprKind::Const(Const::Str(s)) if s == ">5"));
            assert!(matches!(&args[2].kind, HExprKind::Const(Const::Int(0))));
            assert_eq!(args[1].ty, Ty::Concrete(Type::Str));
        }
        other => panic!("expected a format call, got {other:?}"),
    }
    assert_eq!(assigned(&hir, "x").ty, Ty::Concrete(Type::Str));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn fstring_empty_format_spec_is_accepted() {
    let source = "n = 7\nprint(f'{n:}')\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn fstring_repr_and_ascii_conversions_are_lowered() {
    for (source, expected) in [
        ("s = 'x'\nprint(f'{s!r}')\n", 2),
        ("s = 'x'\nprint(f'{s!a}')\n", 3),
    ] {
        let (hir, mut infer, diags) = lower_source(source, &parse(source));
        assert!(diags.is_empty(), "{diags:?}");
        let call = find_format_call(&hir).unwrap_or_else(|| panic!("no format call for {source}"));
        assert!(matches!(&call.2.kind, HExprKind::Const(Const::Int(flag)) if *flag == expected));
        assert!(matches!(&call.1.kind, HExprKind::Const(Const::Str(s)) if s.is_empty()));
        assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    }
}

#[test]
fn fstring_str_conversion_is_accepted() {
    let source = "s = 'x'\nprint(f'{s!s}')\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn fstring_conversion_with_spec_keeps_both() {
    let source = "s = 'x'\nprint(f'{s!r:>6}')\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let call = find_format_call(&hir).expect("format call");
    assert!(matches!(&call.1.kind, HExprKind::Const(Const::Str(s)) if s == ">6"));
    assert!(matches!(&call.2.kind, HExprKind::Const(Const::Int(2))));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn fstring_dynamic_format_spec_is_rejected() {
    let source = "n = 7\nw = 5\nprint(f'{n:{w}}')\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0].message,
        "dynamic f-string format specs are not supported"
    );
}

/// Finds the first `Builtin::Format` call and returns `(value, spec, flag)`.
fn find_format_call(hir: &LHir) -> Option<(&HExpr, &HExpr, &HExpr)> {
    fn walk(expr: &HExpr) -> Option<(&HExpr, &HExpr, &HExpr)> {
        match &expr.kind {
            HExprKind::Call { callee, args } => {
                if *callee == Callee::Builtin(Builtin::Format) && args.len() == 3 {
                    Some((&args[0], &args[1], &args[2]))
                } else {
                    args.iter().find_map(walk)
                }
            }
            HExprKind::Bin { lhs, rhs, .. } => walk(lhs).or_else(|| walk(rhs)),
            HExprKind::MethodCall { object, args, .. } => {
                walk(object).or_else(|| args.iter().find_map(walk))
            }
            _ => None,
        }
    }
    hir.functions
        .iter()
        .flat_map(|function| function.body.iter())
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Expr(expr) => walk(expr),
            HStmtKind::Assign { value, .. } => walk(value),
            _ => None,
        })
}

#[test]
fn percent_formatting_lowers_to_a_modulo_binary_operation() {
    let source = "n = 3\nx = '%s:%d' % ('a', n)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::Bin { op, .. } = &assigned(&hir, "x").kind else {
        panic!(
            "expected a binary operation, got {:?}",
            assigned(&hir, "x").kind
        );
    };
    assert_eq!(*op, BinOp::Mod);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn str_format_lowers_to_a_method_call() {
    let source = "x = '{} {:.1f}'.format('a', 2.25)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::MethodCall { method, args, .. } = &assigned(&hir, "x").kind else {
        panic!("expected a method call, got {:?}", assigned(&hir, "x").kind);
    };
    assert_eq!(method, "format");
    assert_eq!(args.len(), 2);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    assert_eq!(
        infer.resolve(assigned(&hir, "x").ty),
        Ty::Concrete(Type::Str)
    );
}

#[test]
fn fstring_multiple_fields_keep_static_specs() {
    let source = "a = 1.5\nb = 42\nx = f'a{a:.1f}b{b:04d}c'\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let mut specs = Vec::new();
    fn collect(expr: &HExpr, specs: &mut Vec<String>) {
        match &expr.kind {
            HExprKind::Bin { lhs, rhs, .. } => {
                collect(lhs, specs);
                collect(rhs, specs);
            }
            HExprKind::Call { callee, args } if *callee == Callee::Builtin(Builtin::Format) => {
                if let HExprKind::Const(Const::Str(spec)) = &args[1].kind {
                    specs.push(spec.clone());
                }
            }
            _ => {}
        }
    }
    collect(assigned(&hir, "x"), &mut specs);
    assert_eq!(specs, vec![".1f".to_string(), "04d".to_string()]);
}

#[test]
fn subscript_slice_lowers_with_optional_bounds() {
    let source = "s = 'abcdef'\na = s[1:4:2]\nb = s[::-1]\nc = s[:]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[1].kind else {
        panic!("expected an assignment");
    };
    let HExprKind::Slice {
        start, stop, step, ..
    } = &value.kind
    else {
        panic!("expected a slice, got {:?}", value.kind);
    };
    assert!(start.is_some() && stop.is_some() && step.is_some());

    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[2].kind else {
        panic!("expected an assignment");
    };
    let HExprKind::Slice {
        start, stop, step, ..
    } = &value.kind
    else {
        panic!("expected a slice");
    };
    assert!(start.is_none() && stop.is_none() && step.is_some());

    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[3].kind else {
        panic!("expected an assignment");
    };
    let HExprKind::Slice {
        start, stop, step, ..
    } = &value.kind
    else {
        panic!("expected a slice");
    };
    assert!(start.is_none() && stop.is_none() && step.is_none());
}

#[test]
fn slice_assignment_lowers_to_set_slice() {
    let source = "xs = [0, 1, 2]\nxs[1:] = [9]\nobj = [[0], [1]]\nobj[0][:-1] = [7]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let body = &hir.functions[0].body;
    let HStmtKind::SetSlice {
        object,
        start,
        stop,
        step,
        value,
    } = &body[1].kind
    else {
        panic!("expected a slice assignment, got {:?}", body[1].kind);
    };
    assert!(matches!(&object.kind, HExprKind::Local(_)));
    assert!(matches!(
        &start.as_ref().expect("expected start bound").kind,
        HExprKind::Const(Const::Int(1))
    ));
    assert!(stop.is_none() && step.is_none());
    assert!(matches!(&value.kind, HExprKind::List(_)));

    // A sliced subscript target lowers its receiver as a normal expression,
    // so nested attribute and subscript chains work.
    let HStmtKind::SetSlice {
        object,
        start,
        stop,
        step,
        ..
    } = &body[3].kind
    else {
        panic!("expected a nested slice assignment, got {:?}", body[3].kind);
    };
    assert!(matches!(&object.kind, HExprKind::Subscript { .. }));
    assert!(start.is_none() && step.is_none());
    assert!(matches!(
        &stop.as_ref().expect("expected stop bound").kind,
        HExprKind::Const(Const::Int(-1))
    ));
}

#[test]
fn slice_assignment_with_an_attribute_receiver_lowers() {
    let source = "class Box:\n    pass\nbox = Box()\nbox.items = [0, 1]\nbox.items[0:1] = [9]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let object = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::SetSlice { object, .. } => Some(object),
            _ => None,
        })
        .expect("expected a slice assignment");
    assert!(
        matches!(&object.kind, HExprKind::GetAttr { name, .. } if name.as_str() == "items"),
        "expected an attribute receiver, got {:?}",
        object.kind
    );
}

#[test]
fn augmented_slice_assignment_is_rejected() {
    let source = "xs = [1, 2, 3]\nxs[0:1] += [9]\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0].message,
        "augmented slice assignment is not supported yet"
    );
}

/// `d[k] op= v` becomes `if True: $aug0 = d; $aug1 = k; $aug0[$aug1] = $aug0[$aug1] op v`
/// so that neither the object nor the index is evaluated twice.
fn subscript_augassign_block(hir: &LHir) -> &[HStmt] {
    let body = &hir.functions[0].body;
    let if_stmt = body
        .iter()
        .find(|stmt| matches!(&stmt.kind, HStmtKind::If { .. }))
        .expect("expected a desugared if block");
    let HStmtKind::If {
        cond,
        then_body,
        else_body,
    } = &if_stmt.kind
    else {
        unreachable!()
    };
    assert!(matches!(&cond.kind, HExprKind::Const(Const::Bool(true))));
    assert!(else_body.is_empty());
    then_body
}

#[test]
fn dict_subscript_augassign_uses_single_evaluation_temps() {
    let source = "d = {}\nd['x'] = 1\nd['x'] += 2\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let then_body = subscript_augassign_block(&hir);
    assert_eq!(then_body.len(), 3, "{then_body:?}");

    let locals = &hir.functions[0].locals;
    let HStmtKind::Assign {
        target: object_local,
        value: object,
    } = &then_body[0].kind
    else {
        panic!(
            "expected the object temp assignment, got {:?}",
            then_body[0].kind
        );
    };
    let HStmtKind::Assign {
        target: index_local,
        value: index,
    } = &then_body[1].kind
    else {
        panic!(
            "expected the index temp assignment, got {:?}",
            then_body[1].kind
        );
    };
    assert_eq!(locals[*object_local as usize].name, "$aug0");
    assert_eq!(locals[*index_local as usize].name, "$aug1");
    assert!(
        matches!(&object.kind, HExprKind::Local(id) if locals[*id as usize].name == "d"),
        "{:?}",
        object.kind
    );
    assert!(matches!(&index.kind, HExprKind::Const(Const::Str(s)) if s == "x"));

    let HStmtKind::SetSubscript {
        object: set_object,
        index: set_index,
        value,
    } = &then_body[2].kind
    else {
        panic!("expected a subscript store, got {:?}", then_body[2].kind);
    };
    assert_eq!(local_of(set_object), *object_local);
    assert_eq!(local_of(set_index), *index_local);

    let HExprKind::Bin { op, lhs, rhs } = &value.kind else {
        panic!("expected a bin op, got {:?}", value.kind);
    };
    assert_eq!(*op, BinOp::Add);
    assert!(
        matches!(&rhs.kind, HExprKind::Const(Const::Int(2))),
        "{:?}",
        rhs.kind
    );
    let HExprKind::Subscript {
        object: read_object,
        index: read_index,
    } = &lhs.kind
    else {
        panic!("expected a subscript read, got {:?}", lhs.kind);
    };
    assert_eq!(local_of(read_object), *object_local);
    assert_eq!(local_of(read_index), *index_local);

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn list_subscript_augassign_lowers_and_typechecks() {
    let source = "xs = [1]\nxs[0] *= 3\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let then_body = subscript_augassign_block(&hir);
    assert_eq!(then_body.len(), 3, "{then_body:?}");
    let HStmtKind::SetSubscript { value, .. } = &then_body[2].kind else {
        panic!("expected a subscript store, got {:?}", then_body[2].kind);
    };
    let HExprKind::Bin { op, .. } = &value.kind else {
        panic!("expected a bin op, got {:?}", value.kind);
    };
    assert_eq!(*op, BinOp::Mul);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn subscript_augassign_evaluates_object_and_index_once() {
    let source = "def f():\n    return {}\ndef g():\n    return 'x'\nf()[g()] += 1\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let then_body = subscript_augassign_block(&hir);
    assert_eq!(then_body.len(), 3, "{then_body:?}");
    let HStmtKind::Assign { value: object, .. } = &then_body[0].kind else {
        panic!(
            "expected the object temp assignment, got {:?}",
            then_body[0].kind
        );
    };
    let HStmtKind::Assign { value: index, .. } = &then_body[1].kind else {
        panic!(
            "expected the index temp assignment, got {:?}",
            then_body[1].kind
        );
    };
    assert!(
        matches!(object.kind, HExprKind::Call { .. }),
        "{:?}",
        object.kind
    );
    assert!(
        matches!(index.kind, HExprKind::Call { .. }),
        "{:?}",
        index.kind
    );
    // The store and the read only mention the temps, never the calls again.
    let HStmtKind::SetSubscript { value, .. } = &then_body[2].kind else {
        panic!("expected a subscript store, got {:?}", then_body[2].kind);
    };
    let HExprKind::Bin { lhs, .. } = &value.kind else {
        panic!("expected a bin op, got {:?}", value.kind);
    };
    let HExprKind::Subscript { object, index } = &lhs.kind else {
        panic!("expected a subscript read, got {:?}", lhs.kind);
    };
    assert!(matches!(object.kind, HExprKind::Local(_)));
    assert!(matches!(index.kind, HExprKind::Local(_)));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn nested_subscript_augassign_lowers_the_object_once() {
    let source = "a = [[1]]\na[0][1] += 1\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let then_body = subscript_augassign_block(&hir);
    let HStmtKind::Assign { value: object, .. } = &then_body[0].kind else {
        panic!(
            "expected the object temp assignment, got {:?}",
            then_body[0].kind
        );
    };
    let HExprKind::Subscript { index, .. } = &object.kind else {
        panic!(
            "expected the outer read to become the object, got {:?}",
            object.kind
        );
    };
    assert!(
        matches!(&index.kind, HExprKind::Const(Const::Int(0))),
        "{:?}",
        index.kind
    );
    let HStmtKind::SetSubscript {
        object: set_object, ..
    } = &then_body[2].kind
    else {
        panic!("expected a subscript store, got {:?}", then_body[2].kind);
    };
    assert!(matches!(set_object.kind, HExprKind::Local(_)));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

fn local_of(expr: &HExpr) -> LocalId {
    match &expr.kind {
        HExprKind::Local(id) => *id,
        other => panic!("expected a local, got {other:?}"),
    }
}

#[test]
fn slice_types_follow_the_container() {
    use pyx_ir::ExprKind;

    let source = "s = 'abc'\nxs = [1, 2]\na = s[1:]\nb = xs[1:]\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let module = &program
        .functions
        .iter()
        .find(|f| f.name == MODULE_NAME)
        .expect("module function")
        .body;
    let type_of = |statement: &pyx_ir::Stmt| match &statement.kind {
        pyx_ir::StmtKind::Assign { value, .. } => match &value.kind {
            ExprKind::Slice { .. } => value.ty,
            other => panic!("expected a slice, got {other:?}"),
        },
        other => panic!("expected an assignment, got {other:?}"),
    };
    assert_eq!(type_of(&module[2]), Type::Str);
    assert_eq!(type_of(&module[3]), Type::List);
}

#[test]
fn dict_slice_reports_unhashable() {
    let source = "d = {'a': 1}\nprint(d[1:2])\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let errors = crate::sema::check(&hir, &mut infer, "test.py").expect_err("expected errors");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "unhashable type: 'slice'");
}

#[test]
fn slice_bound_must_be_integer_or_none() {
    let source = "s = 'abc'\nprint(s[1.5:])\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let errors = crate::sema::check(&hir, &mut infer, "test.py").expect_err("expected errors");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "slice indices must be integers or None");
}

#[test]
fn slice_of_non_subscriptable_reports_the_type() {
    let source = "x = 5\nprint(x[0:1])\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let errors = crate::sema::check(&hir, &mut infer, "test.py").expect_err("expected errors");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "'int' object is not subscriptable");
}

#[test]
fn chained_slicing_lowers_and_typechecks() {
    let source = "s = 'abcdef'\nprint(s[1:][0])\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn method_call_lowers_to_method_call() {
    let source = "x = 'ab'.replace('a', 'b', 1)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    let HExprKind::MethodCall {
        object,
        method,
        args,
        keywords,
        ..
    } = &value.kind
    else {
        panic!("expected a method call");
    };
    assert_eq!(method, "replace");
    assert_eq!(args.len(), 3);
    assert!(keywords.is_empty());
    assert!(matches!(&object.kind, HExprKind::Const(Const::Str(s)) if s == "ab"));
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::Str(s)) if s == "a"));
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::Int(1))));
}

#[test]
fn builtin_names_lower_to_builtin_refs() {
    let source = "f = int\nx = f('42')\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let reference = assigned(&hir, "f");
    let HExprKind::BuiltinRef(builtin) = &reference.kind else {
        panic!("expected a builtin reference, got {:?}", reference.kind);
    };
    assert_eq!(*builtin, Builtin::Int);
    assert_eq!(reference.ty, Ty::Concrete(Type::Function));

    let HExprKind::IndirectCall { callee, args, .. } = &assigned(&hir, "x").kind else {
        panic!("expected an indirect call");
    };
    assert!(matches!(&callee.kind, HExprKind::Local(_)));
    assert_eq!(args.len(), 1);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn del_lowers_name_attr_subscript_and_slice_targets() {
    let source =
        "def f(obj, xs, table):\n    y = 1\n    del y, obj.name, table['k'], xs[0], xs[1:]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = hir
        .functions
        .iter()
        .find(|function| function.name == "f")
        .expect("function f");
    let HStmtKind::Delete { targets } = &f.body[1].kind else {
        panic!("expected a delete statement, got {:?}", f.body[1].kind);
    };
    assert_eq!(targets.len(), 5);

    let HDeleteTarget::Name(local) = &targets[0] else {
        panic!("expected a name target, got {:?}", targets[0]);
    };
    assert_eq!(f.locals[*local as usize].name, "y");

    let HDeleteTarget::Attr { object, name } = &targets[1] else {
        panic!("expected an attribute target, got {:?}", targets[1]);
    };
    assert!(matches!(&object.kind, HExprKind::Local(_)));
    assert_eq!(name, "name");

    let HDeleteTarget::Subscript { object, index } = &targets[2] else {
        panic!("expected a subscript target, got {:?}", targets[2]);
    };
    assert!(matches!(&object.kind, HExprKind::Local(_)));
    assert!(matches!(
        &index.kind,
        HExprKind::Const(Const::Str(text)) if text == "k"
    ));

    let HDeleteTarget::Subscript { index, .. } = &targets[3] else {
        panic!("expected a subscript target, got {:?}", targets[3]);
    };
    assert!(matches!(&index.kind, HExprKind::Const(Const::Int(0))));

    let HDeleteTarget::Slice {
        object,
        start,
        stop,
        step,
    } = &targets[4]
    else {
        panic!("expected a slice target, got {:?}", targets[4]);
    };
    assert!(matches!(&object.kind, HExprKind::Local(_)));
    assert!(matches!(
        start.as_deref().map(|bound| &bound.kind),
        Some(HExprKind::Const(Const::Int(1)))
    ));
    assert!(stop.is_none());
    assert!(step.is_none());
}

#[test]
fn module_level_del_clears_the_mirrored_global() {
    let source = "x = 1\ndef read():\n    return x\ndel x\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let targets = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Delete { targets } => Some(targets),
            _ => None,
        })
        .expect("module-level delete");
    assert_eq!(targets.len(), 2, "a module local mirrors its global");
    assert!(matches!(&targets[0], HDeleteTarget::Name(_)));
    let HDeleteTarget::Global { module, name } = &targets[1] else {
        panic!("expected a global target, got {:?}", targets[1]);
    };
    assert_eq!(module, "__main__");
    assert_eq!(name, "x");
}

#[test]
fn del_of_a_declared_global_lowers_to_a_global_target() {
    let source = "x = 1\ndef f():\n    global x\n    del x\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = hir
        .functions
        .iter()
        .find(|function| function.name == "f")
        .expect("function f");
    let HStmtKind::Delete { targets } = &f.body[0].kind else {
        panic!("expected a delete statement, got {:?}", f.body[0].kind);
    };
    assert_eq!(targets.len(), 1);
    let HDeleteTarget::Global { module, name } = &targets[0] else {
        panic!("expected a global target, got {:?}", targets[0]);
    };
    assert_eq!(module, "__main__");
    assert_eq!(name, "x");
}

#[test]
fn deleting_an_unknown_name_is_a_compile_error() {
    let source = "def f():\n    del missing\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags.iter().any(|diag| diag
            .message
            .contains("cannot delete unknown name 'missing'")),
        "{diags:?}"
    );
}

#[test]
fn ellipsis_lowers_to_the_none_typed_constant() {
    let source = "x = ...\ndef f():\n    ...\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let value = assigned(&hir, "x");
    assert!(matches!(&value.kind, HExprKind::Const(Const::Ellipsis)));
    assert_eq!(value.ty, Ty::Concrete(Type::None));
}

#[test]
fn input_and_get_ipython_resolve_to_builtins() {
    let source = "def f():\n    a = input('> ')\n    return get_ipython()\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = hir
        .functions
        .iter()
        .find(|function| function.name == "f")
        .expect("function f");
    let HStmtKind::Assign { value, .. } = &f.body[0].kind else {
        panic!("expected an assignment, got {:?}", f.body[0].kind);
    };
    let HExprKind::Call { callee, args } = &value.kind else {
        panic!("expected an input() call, got {:?}", value.kind);
    };
    assert_eq!(*callee, Callee::Builtin(Builtin::Input));
    assert_eq!(args.len(), 1);

    let HStmtKind::Return(Some(value)) = &f.body[1].kind else {
        panic!("expected a return, got {:?}", f.body[1].kind);
    };
    let HExprKind::Call { callee, args } = &value.kind else {
        panic!("expected a get_ipython() call, got {:?}", value.kind);
    };
    assert_eq!(*callee, Callee::Builtin(Builtin::GetIpython));
    assert!(args.is_empty());
}

#[test]
fn builtin_value_drives_map_lowering() {
    let source = "x = list(map(int, ['1', '2']))\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::Call { callee, args } = &assigned(&hir, "x").kind else {
        panic!("expected list()");
    };
    assert_eq!(*callee, Callee::Builtin(Builtin::List));
    let HExprKind::Call {
        callee,
        args: map_args,
    } = &args[0].kind
    else {
        panic!("expected map()");
    };
    assert_eq!(*callee, Callee::Builtin(Builtin::Map));
    assert!(matches!(
        &map_args[0].kind,
        HExprKind::BuiltinRef(Builtin::Int)
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn method_call_keywords_are_lowered() {
    let source = "x = 'a-a'.replace('a', 'b', count=1)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::MethodCall {
        method,
        args,
        keywords,
        ..
    } = &assigned(&hir, "x").kind
    else {
        panic!("expected a method call");
    };
    assert_eq!(method, "replace");
    assert_eq!(args.len(), 2);
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "count");
    assert!(matches!(
        &keywords[0].1.kind,
        HExprKind::Const(Const::Int(1))
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());

    // Unknown keywords are lowered and validated by the runtime, not the
    // static method tables.
    let source = "x = 'a'.replace('a', 'b', nope=1)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn path_method_keywords_are_lowered() {
    let source = "from pathlib import Path\n\
                  p = Path('x')\n\
                  p.mkdir(parents=True, exist_ok=True)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HStmtKind::Expr(call) = &hir.functions[0].body[1].kind else {
        panic!("expected the mkdir expression statement");
    };
    let HExprKind::MethodCall {
        method,
        args,
        keywords,
        ..
    } = &call.kind
    else {
        panic!("expected a method call");
    };
    assert_eq!(method, "mkdir");
    assert!(args.is_empty());
    assert_eq!(keywords.len(), 2);
    assert_eq!(keywords[0].0, "parents");
    assert_eq!(keywords[1].0, "exist_ok");
    assert!(matches!(
        &keywords[0].1.kind,
        HExprKind::Const(Const::Bool(true))
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn chained_method_calls_nest() {
    let source = "print(' x '.strip().upper())\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Expr(call) = &hir.functions[0].body[0].kind else {
        panic!("expected an expression statement");
    };
    let HExprKind::Call { args, .. } = &call.kind else {
        panic!("expected print()");
    };
    let HExprKind::MethodCall { object, method, .. } = &args[0].kind else {
        panic!("expected a method call");
    };
    assert_eq!(method, "upper");
    assert!(matches!(
        &object.kind,
        HExprKind::MethodCall { method, .. } if method == "strip"
    ));
}

#[test]
fn bare_attribute_value_lowers_to_get_attr() {
    // Method attributes without a call are rejected at runtime, not at
    // lowering time: only module attributes stay resolved in the frontend.
    let source = "def f(p):\n    return p.name\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    let HStmtKind::Return(Some(value)) = &function.body[0].kind else {
        panic!("expected a return statement");
    };
    let HExprKind::GetAttr { object, name } = &value.kind else {
        panic!("expected an attribute read, got {:?}", value.kind);
    };
    assert_eq!(name, "name");
    assert!(matches!(object.kind, HExprKind::Local(_)));
}

#[test]
fn any_annotation_is_accepted() {
    let source = "def f(x: Any):\n    return x\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    assert_eq!(function.params[0].ty, Ty::Concrete(Type::Any));
}

#[test]
fn tuple_literal_lowers_with_tuple_type() {
    let source = "t = (1, 'a')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    assert_eq!(value.ty, Ty::Concrete(Type::Tuple));
    let HExprKind::Tuple(elems) = &value.kind else {
        panic!("expected a tuple literal, got {:?}", value.kind);
    };
    assert_eq!(elems.len(), 2);
    assert_eq!(elems[0].ty, Ty::Concrete(Type::Int));
    assert_eq!(elems[1].ty, Ty::Concrete(Type::Str));
}

#[test]
fn empty_and_singleton_tuples_lower() {
    let source = "a = ()\nb = (1,)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for expected in [0usize, 1] {
        let HStmtKind::Assign { value, .. } = &hir.functions[0].body[expected].kind else {
            panic!("expected an assignment");
        };
        let HExprKind::Tuple(elems) = &value.kind else {
            panic!("expected a tuple literal, got {:?}", value.kind);
        };
        assert_eq!(elems.len(), expected);
    }
}

#[test]
fn name_sequence_assignment_lowers_to_unpack() {
    let source = "t = (1, 2)\na, b = t\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Unpack { targets, value } = &hir.functions[0].body[1].kind else {
        panic!(
            "expected an unpack statement, got {:?}",
            hir.functions[0].body[1].kind
        );
    };
    assert_eq!(targets.len(), 2);
    let names: Vec<&str> = targets
        .iter()
        .map(|target| match target {
            HUnpackTarget::Name(id) => hir.functions[0].locals[*id as usize].name.as_str(),
            other => panic!("expected a name target, got {other:?}"),
        })
        .collect();
    assert_eq!(names, ["a", "b"]);
    assert!(matches!(&value.kind, HExprKind::Local(_)));
}

#[test]
fn list_sequence_assignment_lowers_to_unpack() {
    let source = "t = (1, 2)\n[a, b] = t\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &hir.functions[0].body[1].kind,
        HStmtKind::Unpack { .. }
    ));
}

#[test]
fn chained_assignment_stores_the_value_once_left_to_right() {
    let source = "def f():\n    a = b = c = 1\n    return a + b + c\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let HStmtKind::If { then_body, .. } = &f.body[0].kind else {
        panic!(
            "expected the chained assignment wrapper, got {:?}",
            f.body[0].kind
        );
    };
    assert_eq!(then_body.len(), 4, "{then_body:?}");
    let HStmtKind::Assign {
        target: temp,
        value,
    } = &then_body[0].kind
    else {
        panic!(
            "expected the hidden value store, got {:?}",
            then_body[0].kind
        );
    };
    assert!(matches!(value.kind, HExprKind::Const(Const::Int(1))));
    assert_eq!(f.locals[*temp as usize].name, "$multi0");
    let mut names = Vec::new();
    for stmt in &then_body[1..] {
        let HStmtKind::Assign { target, value } = &stmt.kind else {
            panic!("expected a target store, got {:?}", stmt.kind);
        };
        assert!(
            matches!(&value.kind, HExprKind::Local(local) if local == temp),
            "every target must read the hidden local, got {:?}",
            value.kind
        );
        names.push(f.locals[*target as usize].name.as_str());
    }
    assert_eq!(names, ["a", "b", "c"]);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn chained_assignment_accepts_attribute_and_subscript_targets() {
    let source = "def f(a, b, d, xs):\n    a.x = b.y = d[0] = xs[1] = 1\n    return a\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let HStmtKind::If { then_body, .. } = &f.body[0].kind else {
        panic!("expected the chained assignment wrapper");
    };
    assert_eq!(then_body.len(), 5, "{then_body:?}");
    let HStmtKind::Assign { target: temp, .. } = &then_body[0].kind else {
        panic!("expected the hidden value store");
    };
    let stores = &then_body[1..];
    assert!(matches!(stores[0].kind, HStmtKind::SetAttr { .. }));
    assert!(matches!(stores[1].kind, HStmtKind::SetAttr { .. }));
    assert!(matches!(stores[2].kind, HStmtKind::SetSubscript { .. }));
    assert!(matches!(stores[3].kind, HStmtKind::SetSubscript { .. }));
    for store in stores {
        let value = match &store.kind {
            HStmtKind::SetAttr { value, .. } | HStmtKind::SetSubscript { value, .. } => value,
            other => panic!("expected a store, got {other:?}"),
        };
        assert!(
            matches!(&value.kind, HExprKind::Local(local) if local == temp),
            "every target must read the hidden local, got {:?}",
            value.kind
        );
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn module_level_chained_assignment_mirrors_every_name() {
    let source = "a = b = 1\ndef f():\n    return a + b\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        hir.functions[0].body.len(),
        1,
        "{:?}",
        hir.functions[0].body
    );
    let HStmtKind::If { then_body, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected the chained assignment wrapper");
    };
    assert_eq!(then_body.len(), 5, "{then_body:?}");
    let global_names: Vec<&str> = then_body
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            HStmtKind::GlobalSet { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(global_names, ["a", "b"]);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn from_math_import_gcd_lowers_to_a_module_call() {
    let source = "from math import gcd\ng = gcd(12, 8)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "g").kind {
        HExprKind::ModuleCall {
            module,
            function,
            args,
            ..
        } => {
            assert_eq!((module.as_str(), function.as_str()), ("math", "gcd"));
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected a module call, got {other:?}"),
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn finditer_flags_keyword_lowers_to_a_three_argument_module_call() {
    let source = "import re\nmatches = re.finditer('a', 'a', flags=re.IGNORECASE)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "matches").kind {
        HExprKind::ModuleCall {
            module,
            function,
            args,
            keywords,
        } => {
            assert_eq!((module.as_str(), function.as_str()), ("re", "finditer"));
            assert_eq!(args.len(), 3);
            assert!(keywords.is_empty(), "{keywords:?}");
            match &args[2].kind {
                HExprKind::ModuleAttr { module, name } => {
                    assert_eq!((module.as_str(), name.as_str()), ("re", "IGNORECASE"));
                }
                other => panic!("expected the flags constant, got {other:?}"),
            }
        }
        other => panic!("expected a module call, got {other:?}"),
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn for_unpack_target_desugars_to_hidden_local() {
    let source = "xs = [(1, 2)]\nfor a, b in xs:\n    print(a, b)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ForEach { var, body, .. } = &hir.functions[0].body[1].kind else {
        panic!("expected a foreach loop");
    };
    let hidden = &hir.functions[0].locals[*var as usize];
    assert!(hidden.name.starts_with("$unpack"), "{}", hidden.name);
    assert_eq!(hidden.ty, Ty::Concrete(Type::Any));
    let HStmtKind::Unpack { targets, value } = &body[0].kind else {
        panic!("expected a prepended unpack, got {:?}", body[0].kind);
    };
    assert_eq!(targets.len(), 2);
    assert!(
        matches!(&value.kind, HExprKind::Local(id) if id == var),
        "{:?}",
        value.kind
    );
    assert_eq!(body.len(), 2, "unpack plus the original loop body");
}

#[test]
fn nested_unpack_target_binds_nested_elements() {
    let source = "t = (1, (2, 3))\n(a, (b, c)) = t\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Unpack { targets, .. } = &hir.functions[0].body[1].kind else {
        panic!("expected an unpack statement");
    };
    let HUnpackTarget::Name(a) = &targets[0] else {
        panic!("expected a name target, got {:?}", targets[0]);
    };
    let HUnpackTarget::Nested(inner) = &targets[1] else {
        panic!("expected a nested target, got {:?}", targets[1]);
    };
    let names: Vec<&str> = inner
        .iter()
        .map(|target| match target {
            HUnpackTarget::Name(id) => hir.functions[0].locals[*id as usize].name.as_str(),
            other => panic!("expected a name target, got {other:?}"),
        })
        .collect();
    assert_eq!(names, ["b", "c"]);
    assert_eq!(hir.functions[0].locals[*a as usize].name, "a");
}

#[test]
fn starred_unpack_targets_are_accepted() {
    for source in ["a, *b = (1, 2, 3)\n", "for a, *b in [(1, 2)]:\n    pass\n"] {
        let (hir, _, diags) = lower_source(source, &parse(source));
        assert!(diags.is_empty(), "{source}: {diags:?}");
        let targets = hir.functions[0]
            .body
            .iter()
            .find_map(|stmt| match &stmt.kind {
                HStmtKind::Unpack { targets, .. } => Some(targets),
                HStmtKind::ForEach { body, .. } => {
                    body.iter().find_map(|inner| match &inner.kind {
                        HStmtKind::Unpack { targets, .. } => Some(targets),
                        _ => None,
                    })
                }
                _ => None,
            })
            .expect("expected an unpack target");
        assert!(
            targets
                .iter()
                .any(|target| matches!(target, HUnpackTarget::Star(_))),
            "{source}: {targets:?}"
        );
    }
}

#[test]
fn nested_for_unpack_target_is_accepted() {
    let source = "for (a, (b, c)) in [(1, (2, 3))]:\n    pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ForEach { body, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a foreach loop");
    };
    let HStmtKind::Unpack { targets, .. } = &body[0].kind else {
        panic!("expected a prepended unpack, got {:?}", body[0].kind);
    };
    assert!(matches!(&targets[0], HUnpackTarget::Name(_)));
    match &targets[1] {
        HUnpackTarget::Nested(inner) => assert_eq!(inner.len(), 2),
        other => panic!("expected a nested target, got {other:?}"),
    }
}

#[test]
fn range_unpack_target_is_rejected() {
    let source = "for a, b in range(3):\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0].message,
        "range() yields a single value and cannot be unpacked"
    );
}

#[test]
fn tuple_annotation_is_accepted() {
    let source = "def f(t: tuple) -> tuple:\n    return t\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    assert_eq!(function.params[0].ty, Ty::Concrete(Type::Tuple));
    assert_eq!(function.ret_ty, Ty::Concrete(Type::Tuple));
}

#[test]
fn set_literal_lowers_with_set_type() {
    let source = "s = {1, 2}\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    assert_eq!(value.ty, Ty::Concrete(Type::Set));
    let HExprKind::Set(elems) = &value.kind else {
        panic!("expected a set literal, got {:?}", value.kind);
    };
    assert_eq!(elems.len(), 2);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn membership_lowers_to_binary_operators() {
    let source = "a = 1 in [1]\nb = 2 not in [1]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for (index, expected) in [(0usize, BinOp::In), (1, BinOp::NotIn)] {
        let HStmtKind::Assign { value, .. } = &hir.functions[0].body[index].kind else {
            panic!("expected an assignment");
        };
        let HExprKind::Bin { op, .. } = &value.kind else {
            panic!("expected a comparison, got {:?}", value.kind);
        };
        assert_eq!(*op, expected);
    }
}

#[test]
fn set_name_and_annotation_are_supported() {
    let source = "def f(s: set) -> set:\n    return s\nx = set([1])\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    assert_eq!(function.params[0].ty, Ty::Concrete(Type::Set));
    assert_eq!(function.ret_ty, Ty::Concrete(Type::Set));
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    assert!(matches!(
        &value.kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Set),
            ..
        }
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

fn assigned_call(source: &str) -> (LHir, Infer, Vec<Diagnostic>, Callee, usize) {
    let (hir, infer, diags) = lower_source(source, &parse(source));
    let (callee, arity) = {
        let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
            panic!("expected an assignment");
        };
        let HExprKind::Call { callee, args } = &value.kind else {
            panic!("expected a call, got {:?}", value.kind);
        };
        (callee.clone(), args.len())
    };
    (hir, infer, diags, callee, arity)
}

#[test]
fn new_builtins_lower_to_builtin_calls() {
    for (source, expected, arity) in [
        ("r = range(3)\n", Builtin::Range, 1),
        ("r = range(1, 4)\n", Builtin::Range, 2),
        ("r = range(1, 4, 2)\n", Builtin::Range, 3),
        ("x = list([1])\n", Builtin::List, 1),
        ("x = list()\n", Builtin::List, 0),
        ("x = dict([(1, 2)])\n", Builtin::Dict, 1),
        ("x = sum([1, 2])\n", Builtin::Sum, 1),
        ("x = sum([1, 2], 3)\n", Builtin::Sum, 2),
        ("x = enumerate([1])\n", Builtin::Enumerate, 1),
        ("x = enumerate([1], 2)\n", Builtin::Enumerate, 2),
        ("x = zip([1], [2])\n", Builtin::Zip, 2),
        ("x = zip()\n", Builtin::Zip, 0),
    ] {
        let (hir, mut infer, diags, callee, got_arity) = assigned_call(source);
        assert!(diags.is_empty(), "{source}: {diags:?}");
        assert_eq!(
            callee,
            Callee::Builtin(expected),
            "{source}: unexpected callee"
        );
        assert_eq!(got_arity, arity, "{source}");
        assert!(
            crate::sema::check(&hir, &mut infer, "test.py").is_ok(),
            "{source}: type checking failed"
        );
    }
}

#[test]
fn range_in_for_stays_for_range_but_value_uses_the_builtin() {
    let for_source = "for i in range(3):\n    pass\n";
    let (hir, _, diags) = lower_source(for_source, &parse(for_source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        matches!(&hir.functions[0].body[0].kind, HStmtKind::ForRange { .. }),
        "{:?}",
        hir.functions[0].body[0].kind
    );

    let value_source = "r = range(3)\n";
    let (hir, _, diags) = lower_source(value_source, &parse(value_source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    assert!(matches!(
        &value.kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Range),
            ..
        }
    ));
}

#[test]
fn bare_range_lowers_to_a_builtin_reference() {
    let source = "x = range\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "x").kind,
        HExprKind::BuiltinRef(Builtin::Range)
    ));
}

#[test]
fn range_annotation_is_accepted() {
    let source = "def f(r: range) -> range:\n    return r\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    assert_eq!(function.params[0].ty, Ty::Concrete(Type::Range));
    assert_eq!(function.ret_ty, Ty::Concrete(Type::Range));
}

#[test]
fn list_comprehension_shadows_and_restores_outer_names() {
    let source = "i = 100\nxs = [i * 2 for i in range(3) if i > 0]\ny = i\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = &hir.functions[0].body;
    let HStmtKind::Assign {
        target: outer_i, ..
    } = &body[0].kind
    else {
        panic!("expected the outer assignment");
    };
    let HStmtKind::Assign { value, .. } = &body[1].kind else {
        panic!("expected the comprehension assignment");
    };
    let HExprKind::ListComp { element, clauses } = &value.kind else {
        panic!("expected a list comprehension, got {:?}", value.kind);
    };
    assert_eq!(clauses.len(), 1);
    assert_eq!(clauses[0].conditions.len(), 1);
    let HCompTarget::Name(comp_i) = &clauses[0].target else {
        panic!("expected a name target");
    };
    assert_ne!(comp_i, outer_i, "the target must use a synthetic local");
    let target_local = &hir.functions[0].locals[*comp_i as usize];
    assert!(
        target_local.name.starts_with("$comp"),
        "{}",
        target_local.name
    );
    assert_eq!(target_local.ty, Ty::Concrete(Type::Any));
    let HExprKind::Bin { lhs, .. } = &element.kind else {
        panic!("expected the element expression");
    };
    assert!(matches!(&lhs.kind, HExprKind::Local(id) if id == comp_i));
    let HStmtKind::Assign { value: after, .. } = &body[2].kind else {
        panic!("expected the trailing assignment");
    };
    assert!(
        matches!(&after.kind, HExprKind::Local(id) if id == outer_i),
        "outer `i` must be restored, got {:?}",
        after.kind
    );
}

#[test]
fn comprehension_tuple_targets_unpack_synthetic_locals() {
    let source = "data = [(1, 'a')]\nprint([k for k, v in data if k > 0])\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Expr(call) = &hir.functions[0].body[1].kind else {
        panic!("expected a print call");
    };
    let HExprKind::Call { args, .. } = &call.kind else {
        panic!("expected a call");
    };
    let HExprKind::ListComp { element, clauses } = &args[0].kind else {
        panic!("expected a list comprehension");
    };
    assert_eq!(clauses.len(), 1);
    assert_eq!(clauses[0].conditions.len(), 1);
    let HCompTarget::Unpack(targets) = &clauses[0].target else {
        panic!("expected an unpack target");
    };
    assert_eq!(targets.len(), 2);
    let ids: Vec<LocalId> = targets
        .iter()
        .map(|target| match target {
            HUnpackTarget::Name(id) => *id,
            other => panic!("expected a name target, got {other:?}"),
        })
        .collect();
    for id in &ids {
        assert!(hir.functions[0].locals[*id as usize]
            .name
            .starts_with("$comp"));
    }
    assert!(
        matches!(&element.kind, HExprKind::Local(id) if id == &ids[0]),
        "{:?}",
        element.kind
    );
}

#[test]
fn multi_clause_comprehension_lowers_each_clause_in_order() {
    let source = "xs = [(i, j) for i in range(2) for j in range(2) if i != j]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    let HExprKind::ListComp { element, clauses } = &value.kind else {
        panic!("expected a list comprehension");
    };
    assert_eq!(clauses.len(), 2);
    assert_eq!(clauses[1].conditions.len(), 1);
    let HCompTarget::Name(first) = &clauses[0].target else {
        panic!("expected a name target");
    };
    assert!(
        matches!(&clauses[1].iterable.kind, HExprKind::Call { .. }),
        "{:?}",
        clauses[1].iterable.kind
    );
    let HExprKind::Tuple(elems) = &element.kind else {
        panic!("expected a tuple element");
    };
    assert!(matches!(&elems[0].kind, HExprKind::Local(id) if id == first));
}

#[test]
fn comprehension_captures_outer_locals() {
    let source = "n = 3\nxs = [n + i for i in range(n)]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign {
        target: outer_n, ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!("expected the outer assignment");
    };
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[1].kind else {
        panic!("expected the comprehension assignment");
    };
    let HExprKind::ListComp { element, clauses } = &value.kind else {
        panic!("expected a list comprehension");
    };
    let HExprKind::Bin { lhs, .. } = &element.kind else {
        panic!("expected a binary element");
    };
    assert!(matches!(&lhs.kind, HExprKind::Local(id) if id == outer_n));
    let HExprKind::Call { args, .. } = &clauses[0].iterable.kind else {
        panic!("expected a range() iterable");
    };
    assert!(matches!(&args[0].kind, HExprKind::Local(id) if id == outer_n));
}

#[test]
fn nested_comprehensions_keep_both_scopes() {
    let source = "xs = [[j for j in range(i)] for i in range(3)]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    let HExprKind::ListComp { element, clauses } = &value.kind else {
        panic!("expected the outer list comprehension");
    };
    let HCompTarget::Name(outer_target) = &clauses[0].target else {
        panic!("expected a name target");
    };
    let HExprKind::ListComp {
        clauses: inner_clauses,
        ..
    } = &element.kind
    else {
        panic!("expected the nested list comprehension");
    };
    let HCompTarget::Name(inner_target) = &inner_clauses[0].target else {
        panic!("expected a name target");
    };
    assert_ne!(outer_target, inner_target);
    let HExprKind::Call { args, .. } = &inner_clauses[0].iterable.kind else {
        panic!("expected a range() iterable");
    };
    assert!(
        matches!(&args[0].kind, HExprKind::Local(id) if id == outer_target),
        "nested iterable must capture the outer target: {:?}",
        args[0].kind
    );
}

#[test]
fn async_comprehension_is_rejected() {
    let source = "xs = [x async for x in range(3)]\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0].message,
        "async comprehensions are not supported yet"
    );
}

#[test]
fn unsupported_comprehension_target_is_rejected() {
    for (source, expected) in [
        (
            "xs = [1 for a[0:1] in [[1, 2]]]\n",
            "unsupported comprehension target",
        ),
        (
            "xs = [1 for a, b[0:1] in [[1, [2]]]]\n",
            "slice assignment is not supported in a comprehension target",
        ),
    ] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert_eq!(diags.len(), 1, "{source}: {diags:?}");
        assert_eq!(diags[0].message, expected, "{source}");
    }
}

#[test]
fn comprehension_attribute_target_lowers() {
    let source = "items = [0]\nxs = [1 for items.x, items.y in [(1, 2)]]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[1].kind else {
        panic!("expected an assignment");
    };
    let HExprKind::ListComp { clauses, .. } = &value.kind else {
        panic!("expected a list comprehension, got {:?}", value.kind);
    };
    let HCompTarget::Unpack(targets) = &clauses[0].target else {
        panic!("expected an unpack target");
    };
    assert_eq!(targets.len(), 2);
    for (target, expected) in targets.iter().zip(["x", "y"]) {
        match target {
            HUnpackTarget::Attr { name, .. } => assert_eq!(name, expected),
            other => panic!("expected an attribute target, got {other:?}"),
        }
    }
}

#[test]
fn comprehension_result_types_are_static_containers() {
    use pyx_ir::ExprKind;

    let source = "a = [x for x in range(3)]\nb = {x for x in range(3)}\nc = {x: x for x in range(3)}\nd = (x for x in range(3))\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let module = &program
        .functions
        .iter()
        .find(|f| f.name == MODULE_NAME)
        .expect("module function")
        .body;
    let types: Vec<Type> = module
        .iter()
        .map(|stmt| match &stmt.kind {
            pyx_ir::StmtKind::Assign { value, .. } => value.ty,
            other => panic!("expected an assignment, got {other:?}"),
        })
        .collect();
    assert_eq!(
        types,
        vec![Type::List, Type::Set, Type::Dict, Type::List],
        "generator expressions are materialised as lists"
    );
    assert!(
        matches!(&module[0].kind, pyx_ir::StmtKind::Assign { value, .. }
            if matches!(&value.kind, ExprKind::ListComp { .. }))
    );
    assert!(
        matches!(&module[1].kind, pyx_ir::StmtKind::Assign { value, .. }
            if matches!(&value.kind, ExprKind::SetComp { .. }))
    );
    assert!(
        matches!(&module[2].kind, pyx_ir::StmtKind::Assign { value, .. }
            if matches!(&value.kind, ExprKind::DictComp { .. }))
    );
    assert!(
        matches!(&module[3].kind, pyx_ir::StmtKind::Assign { value, .. }
            if matches!(&value.kind, ExprKind::GeneratorExp { .. }))
    );
}

#[test]
fn comprehension_over_a_non_iterable_is_rejected() {
    let diags = crate::check_source("a = [x for x in 5]\n", "test.py")
        .expect_err("non-iterable comprehension");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].message, "'int' object is not iterable");
}

#[test]
fn unhashable_comprehension_elements_are_rejected() {
    for source in [
        "a = {[x] for x in range(3)}\n",
        "a = {[x]: 1 for x in range(3)}\n",
    ] {
        let diags = crate::check_source(source, "test.py").expect_err("unhashable element");
        assert_eq!(diags.len(), 1, "{source}: {diags:?}");
        assert_eq!(diags[0].message, "unhashable type: 'list'", "{source}");
    }
}

#[test]
fn builtin_named_functions_shadow_the_builtin() {
    for name in [
        "range",
        "list",
        "dict",
        "sum",
        "enumerate",
        "zip",
        "map",
        "filter",
    ] {
        let source = format!("def {name}():\n    pass\n");
        let (hir, _, diags) = lower_source(&source, &parse(&source));
        assert!(diags.is_empty(), "{name}: {diags:?}");
        assert!(
            hir.functions.iter().any(|function| function.name == name),
            "{name}"
        );
    }
}

#[test]
fn lambda_lowers_to_a_synthetic_function_reference() {
    let source = "f = lambda a, b: a + b\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    let HExprKind::FunctionRef(id) = &value.kind else {
        panic!("expected a function reference, got {:?}", value.kind);
    };
    let lambda = &hir.functions[*id as usize];
    assert_eq!(lambda.name, "<lambda>");
    assert_eq!(lambda.params.len(), 2);
    assert_eq!(lambda.params[0].name, "a");
    assert_eq!(lambda.params[1].name, "b");
    assert_eq!(lambda.locals.len(), 2, "lambda has only its parameters");
    assert_eq!(lambda.ret_ty, Ty::Concrete(Type::Any));
    assert_eq!(lambda.body.len(), 1);
    let HStmtKind::Return(Some(body)) = &lambda.body[0].kind else {
        panic!("expected the lambda return statement");
    };
    assert!(matches!(&body.kind, HExprKind::Bin { op: BinOp::Add, .. }));
}

#[test]
fn lambda_rejects_defaults_and_variadic_parameters() {
    for (source, expected) in [
        ("f = lambda x=1: x\n", "default"),
        ("f = lambda *args: args\n", "varargs"),
        ("f = lambda *, x: x\n", "keyword-only"),
        ("f = lambda **kwargs: kwargs\n", "kwargs"),
    ] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert_eq!(diags.len(), 1, "{source}: {diags:?}");
        assert!(
            diags[0].message.contains(expected),
            "{source}: {}",
            diags[0].message
        );
    }
}

#[test]
fn varargs_and_keyword_only_parameters_materialize_slots() {
    let source = "def f(a, *args, key=1, **kwargs):\n    return a\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let kinds: Vec<ParamKind> = f.params.iter().map(|param| param.kind).collect();
    assert_eq!(
        kinds,
        vec![
            ParamKind::Positional,
            ParamKind::VarArg,
            ParamKind::KwOnly,
            ParamKind::KwArg,
        ]
    );
    assert_eq!(f.params[1].ty, Ty::Concrete(Type::Any));
    assert_eq!(f.params[3].ty, Ty::Concrete(Type::Dict));
    assert_eq!(f.params[2].default, Some(Const::Int(1)));
    assert!(f.locals.iter().any(|local| local.name == "args"));
    assert!(f.locals.iter().any(|local| local.name == "kwargs"));
}

#[test]
fn direct_call_binds_varargs_keyword_only_and_kwargs() {
    let source = "\
def f(a, *args, key=1, **kwargs):
    return a
x = f(1, 2, 3, key=4, extra=5)
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::Call {
        callee: Callee::Function(_),
        args,
    } = &assigned(&hir, "x").kind
    else {
        panic!("expected a direct call, got {:?}", assigned(&hir, "x").kind);
    };
    // Slot order: positional, *args, keyword-only, **kwargs.
    assert_eq!(args.len(), 4);
    assert!(matches!(args[0].kind, HExprKind::Const(Const::Int(1))));
    assert!(matches!(&args[1].kind, HExprKind::List(items) if items.len() == 2));
    assert!(matches!(args[2].kind, HExprKind::Const(Const::Int(4))));
    match &args[3].kind {
        HExprKind::Dict(pairs) => {
            assert_eq!(pairs.len(), 1);
            assert!(
                matches!(&pairs[0].0.kind, HExprKind::Const(Const::Str(name)) if name == "extra")
            );
            assert!(matches!(pairs[0].1.kind, HExprKind::Const(Const::Int(5))));
        }
        other => panic!("expected a kwargs dict, got {other:?}"),
    }
}

#[test]
fn keyword_only_defaults_fill_direct_calls() {
    let source = "def f(*, key=3):\n    return key\nx = f()\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::Call { args, .. } = &assigned(&hir, "x").kind else {
        panic!("expected a direct call");
    };
    assert_eq!(args.len(), 1);
    assert!(matches!(args[0].kind, HExprKind::Const(Const::Int(3))));
}

#[test]
fn starred_calls_use_the_general_starred_abi() {
    let source = "def f(a):\n    return a\nx = f(*[1])\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::IndirectCall { args, star, .. } = &assigned(&hir, "x").kind else {
        panic!(
            "expected a starred indirect call, got {:?}",
            assigned(&hir, "x").kind
        );
    };
    assert!(args.is_empty());
    assert!(star.is_some());
}

#[test]
fn double_star_calls_defer_to_runtime() {
    let source = "def f(**kwargs):\n    return kwargs\nx = f(**{\"a\": 1})\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::DeferredCall { args, .. } = &assigned(&hir, "x").kind else {
        panic!(
            "expected a deferred call, got {:?}",
            assigned(&hir, "x").kind
        );
    };
    assert_eq!(args.len(), 1);
}

#[test]
fn keyword_call_on_a_function_value_uses_the_runtime_binder() {
    let source = "def g(f):\n    return f(a=1)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    assert!(matches!(
        &g.body[0].kind,
        HStmtKind::Return(Some(HExpr {
            kind: HExprKind::CallKw { .. },
            ..
        }))
    ));
}

#[test]
fn method_double_star_call_defers() {
    let source = "\
class C:
    def m(self, a):
        return a
c = C()
x = c.m(**{\"a\": 1})
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::DeferredCall { callee, args } = &assigned(&hir, "x").kind else {
        panic!(
            "expected a deferred call, got {:?}",
            assigned(&hir, "x").kind
        );
    };
    assert!(callee.is_some(), "the object is still evaluated");
    assert_eq!(args.len(), 1);
}

#[test]
fn async_varargs_calls_go_through_the_function_value() {
    let source = "async def f(*args):\n    return 1\nx = f(1, 2)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "x").kind,
        HExprKind::IndirectCall { .. }
    ));
}

#[test]
fn global_declarations_read_and_write_the_module_table() {
    let source = "\
x = 0
def bump():
    global x
    x = x + 1
    return x
bump()
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let bump = function(&hir, "bump");
    assert!(
        !bump.locals.iter().any(|local| local.name == "x"),
        "a global name must not get a local slot"
    );
    let HStmtKind::GlobalSet { name, value, .. } = &bump.body[0].kind else {
        panic!("expected a global store, got {:?}", bump.body[0].kind);
    };
    assert_eq!(name, "x");
    assert!(matches!(
        &value.kind,
        HExprKind::Bin {
            lhs,
            op: BinOp::Add,
            ..
        } if matches!(&lhs.kind, HExprKind::GlobalGet { name, .. } if name == "x")
    ));
    assert!(matches!(
        &bump.body[1].kind,
        HStmtKind::Return(Some(HExpr {
            kind: HExprKind::GlobalGet { name, .. },
            ..
        })) if name == "x"
    ));
    // The module body mirrors its own `x = 0` so the global table is defined.
    assert!(hir.functions[0]
        .body
        .iter()
        .any(|stmt| matches!(&stmt.kind, HStmtKind::GlobalSet { name, .. } if name == "x")));
}

#[test]
fn annotated_assignments_lower_like_plain_assignments() {
    let source = "x: int = 3\ny: int\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        hir.functions[0].body.len(),
        1,
        "bare annotations emit nothing"
    );
    assert!(matches!(
        &hir.functions[0].body[0].kind,
        HStmtKind::Assign { .. }
    ));
}

#[test]
fn annotated_assignment_to_a_global_stores_through_the_table() {
    let source = "x = 0\ndef f():\n    global x\n    x: int = 5\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert!(matches!(
        &f.body[0].kind,
        HStmtKind::GlobalSet { name, .. } if name == "x"
    ));
}

#[test]
fn file_not_found_error_is_a_known_handler() {
    let source = "\
def f():
    try:
        open(\"missing\")
    except FileNotFoundError:
        return 1
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let HStmtKind::Try { handlers, .. } = &f.body[0].kind else {
        panic!("expected a try statement");
    };
    assert_eq!(handlers[0].exc, Some(ExcKind::FileNotFoundError));
}

#[test]
fn emitted_varargs_parameters_keep_defaults_and_kinds() {
    let source = "def f(*args, key=1, **kwargs):\n    return args\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let f = &program.functions[1];
    assert_eq!(f.params.len(), 3);
    assert_eq!(f.params[0].kind, ParamKind::VarArg);
    assert_eq!(f.params[1].kind, ParamKind::KwOnly);
    assert_eq!(f.params[1].default, Some(Const::Int(1)));
    assert_eq!(f.params[2].kind, ParamKind::KwArg);
    assert!(f.has_dynamic_signature());
    assert_eq!(f.call_arity(), 0);
}

#[test]
fn lambda_captures_are_rejected_as_closures() {
    let source = "x = 1\nf = lambda: x\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]
            .message
            .contains("closures are not supported yet; lambda can only use its parameters"),
        "{diags:?}"
    );

    // A parameter must still be callable inside the lambda.
    let source = "f = lambda fn: fn(1)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn function_references_and_indirect_calls_lower() {
    let source = "def add(a, b):\n    return a + b\nf = add\nfs = [add]\nf(1, 2)\nfs[0](1, 2)\nadd(1, 2)(3)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let add = hir
        .functions
        .iter()
        .position(|f| f.name == "add")
        .expect("add function") as FunctionId;

    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    assert!(matches!(
        &value.kind,
        HExprKind::FunctionRef(id) if *id == add
    ));

    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[1].kind else {
        panic!("expected a list assignment");
    };
    let HExprKind::List(elems) = &value.kind else {
        panic!("expected a list literal");
    };
    assert!(matches!(
        &elems[0].kind,
        HExprKind::FunctionRef(id) if *id == add
    ));

    // Local, subscript and call-result callees all become indirect calls.
    let HStmtKind::Expr(expr) = &hir.functions[0].body[2].kind else {
        panic!("expected a call statement");
    };
    let HExprKind::IndirectCall { callee, .. } = &expr.kind else {
        panic!("expected an indirect call, got {:?}", expr.kind);
    };
    assert!(matches!(&callee.kind, HExprKind::Local(_)));

    let HStmtKind::Expr(expr) = &hir.functions[0].body[3].kind else {
        panic!("expected a call statement");
    };
    let HExprKind::IndirectCall { callee, .. } = &expr.kind else {
        panic!("expected an indirect call");
    };
    assert!(matches!(&callee.kind, HExprKind::Subscript { .. }));

    let HStmtKind::Expr(expr) = &hir.functions[0].body[4].kind else {
        panic!("expected a call statement");
    };
    let HExprKind::IndirectCall { callee, .. } = &expr.kind else {
        panic!("expected an indirect call");
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::Call {
            callee: Callee::Function(id),
            ..
        } if *id == add
    ));
}

#[test]
fn native_constructor_keywords_are_reordered() {
    let source = "import threading\n\ndef f():\n    return None\n\nt = threading.Thread(target=f, args=(), name='w')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let value = assigned(&hir, "t");
    let HExprKind::ModuleCall {
        module,
        function,
        args,
        ..
    } = &value.kind
    else {
        panic!("expected a module call");
    };
    assert_eq!(
        (module.as_str(), function.as_str()),
        ("threading", "Thread")
    );
    assert_eq!(args.len(), 3);
    assert!(matches!(args[1].kind, HExprKind::Tuple { .. }));
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::Str(s)) if s == "w"));
}

#[test]
fn imported_native_constructor_keywords_are_reordered() {
    let source =
        "from threading import Thread\n\ndef f():\n    return None\n\nt = Thread(target=f)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let value = assigned(&hir, "t");
    let HExprKind::ModuleCall { args, .. } = &value.kind else {
        panic!("expected a module call");
    };
    assert_eq!(args.len(), 3);
}

#[test]
fn native_constructor_unknown_keyword_is_reported() {
    let source = "import queue\n\nq = queue.Queue(size=1)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1);
    assert_eq!(
        diags[0].message,
        "Queue() got an unexpected keyword argument 'size'"
    );
}

#[test]
fn method_keywords_lower_and_are_validated_at_runtime() {
    let source = "import queue\n\nq = queue.Queue()\nq.get(timeout=1)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Expr(call) = &hir.functions[0].body[1].kind else {
        panic!("expected the q.get() expression");
    };
    let HExprKind::MethodCall {
        method,
        args,
        keywords,
        ..
    } = &call.kind
    else {
        panic!("expected a method call");
    };
    assert_eq!(method, "get");
    assert!(args.is_empty());
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "timeout");
    assert!(matches!(
        &keywords[0].1.kind,
        HExprKind::Const(Const::Int(1))
    ));
}

#[test]
fn map_and_filter_lower_to_builtins() {
    let source = "a = map(lambda i: i, [1])\nb = filter(None, [1])\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    assert!(matches!(
        &value.kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Map),
            ..
        }
    ));
    let HStmtKind::Assign { value, .. } = &hir.functions[0].body[1].kind else {
        panic!("expected an assignment");
    };
    assert!(matches!(
        &value.kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Filter),
            ..
        }
    ));
}

#[test]
fn bytes_literals_lower_to_const_bytes() {
    // Parser-level escapes: newline, NUL and a backslash.
    let source = "x = b\"a\\n\\x00\\\\\"\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let value = assigned(&hir, "x");
    assert_eq!(value.ty, Ty::Concrete(Type::Bytes));
    match &value.kind {
        HExprKind::Const(Const::Bytes(bytes)) => {
            assert_eq!(bytes, &vec![b'a', b'\n', 0, b'\\']);
        }
        other => panic!("expected bytes literal, got {other:?}"),
    }
}

#[test]
fn bytes_builtin_resolves_with_its_arguments() {
    let source = "a = bytes()\n\
                      b = bytes(3)\n\
                      c = bytes([65, 66])\n\
                      d = bytes(\"x\", \"utf-8\")\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for (name, argc) in [("a", 0), ("b", 1), ("c", 1), ("d", 2)] {
        match &assigned(&hir, name).kind {
            HExprKind::Call {
                callee: Callee::Builtin(Builtin::Bytes),
                args,
            } => assert_eq!(args.len(), argc, "{name}"),
            other => panic!("expected bytes() call for {name}, got {other:?}"),
        }
    }
}

#[test]
fn user_functions_may_shadow_builtins() {
    let source =
        "def bytes(x):\n    return x\ndef print(x):\n    return x\na = bytes(1)\nb = print(2)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for name in ["a", "b"] {
        match &assigned(&hir, name).kind {
            HExprKind::Call {
                callee: Callee::Function(_),
                ..
            } => {}
            other => panic!("expected a call to the user function, got {other:?}"),
        }
    }
}

#[test]
fn class_definition_lowers_with_methods_and_self_attributes() {
    let source = "class C:\n    def __init__(self, x):\n        self.x = x\n    def get(self):\n        return self.x\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let locals = &hir.functions[0].locals;
    let HStmtKind::ClassDef {
        target,
        name,
        base,
        methods,
        ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!(
            "expected a class definition, got {:?}",
            hir.functions[0].body[0].kind
        );
    };
    assert_eq!(name, "C");
    assert_eq!(locals[*target as usize].name, "C");
    assert!(base.is_none(), "{base:?}");
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0].0, "__init__");
    assert_eq!(methods[1].0, "get");

    // Methods are plain functions named `Class.method`, never globals.
    let init = &hir.functions[methods[0].1 as usize];
    assert_eq!(init.name, "C.__init__");
    assert_eq!(
        init.params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        vec!["self", "x"]
    );
    let HStmtKind::SetAttr {
        object,
        name,
        value,
    } = &init.body[0].kind
    else {
        panic!(
            "expected a self attribute store, got {:?}",
            init.body[0].kind
        );
    };
    assert_eq!(name, "x");
    assert_eq!(local_of(object), init.params[0].local);
    assert_eq!(local_of(value), init.params[1].local);

    let get = &hir.functions[methods[1].1 as usize];
    assert_eq!(get.name, "C.get");
    let HStmtKind::Return(Some(value)) = &get.body[0].kind else {
        panic!("expected a return, got {:?}", get.body[0].kind);
    };
    let HExprKind::GetAttr { object, name } = &value.kind else {
        panic!("expected a self attribute read, got {:?}", value.kind);
    };
    assert_eq!(name, "x");
    assert_eq!(local_of(object), get.params[0].local);

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn class_instantiation_is_an_indirect_call_through_the_class_local() {
    let source = "class C:\n    pass\nc = C()\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HStmtKind::ClassDef { target, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a class definition");
    };
    let c = assigned(&hir, "c");
    let HExprKind::IndirectCall { callee, args, .. } = &c.kind else {
        panic!("expected an indirect call, got {c:?}");
    };
    assert!(args.is_empty());
    assert_eq!(local_of(callee), *target);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn class_inheritance_keeps_the_base_expression() {
    let source = "class C:\n    pass\nclass D(C):\n    pass\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let locals = &hir.functions[0].locals;
    let HStmtKind::ClassDef {
        target: c_target, ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!("expected class C");
    };
    let HStmtKind::ClassDef {
        target,
        name,
        base: Some(base),
        ..
    } = &hir.functions[0].body[1].kind
    else {
        panic!(
            "expected class D with a base, got {:?}",
            hir.functions[0].body[1].kind
        );
    };
    assert_eq!(name, "D");
    assert_eq!(local_of(base), *c_target);
    assert_eq!(locals[*target as usize].name, "D");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn builtin_exception_names_resolve_as_class_values() {
    let source = "\
x = ValueError
class E(Exception):
    pass
class F(E):
    pass
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let x = assigned(&hir, "x");
    assert!(
        matches!(&x.kind, HExprKind::ExceptionClass(ExcKind::ValueError)),
        "expected an exception class value, got {x:?}"
    );

    let HStmtKind::ClassDef {
        target: e_target,
        name: e_name,
        base: Some(e_base),
        ..
    } = &hir.functions[0].body[1].kind
    else {
        panic!("expected class E, got {:?}", hir.functions[0].body[1].kind);
    };
    assert_eq!(e_name, "E");
    assert!(
        matches!(&e_base.kind, HExprKind::ExceptionClass(ExcKind::Exception)),
        "expected the Exception class as base, got {e_base:?}"
    );

    let HStmtKind::ClassDef {
        name: f_name,
        base: Some(f_base),
        ..
    } = &hir.functions[0].body[2].kind
    else {
        panic!("expected class F, got {:?}", hir.functions[0].body[2].kind);
    };
    assert_eq!(f_name, "F");
    assert_eq!(local_of(f_base), *e_target);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn exception_class_names_resolve_inside_functions_and_calls() {
    let source = "\
def make():
    return ValueError
def build():
    return Exception()
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let make = function(&hir, "make");
    let HStmtKind::Return(Some(value)) = &make.body[0].kind else {
        panic!("expected a return, got {:?}", make.body[0].kind);
    };
    assert!(
        matches!(&value.kind, HExprKind::ExceptionClass(ExcKind::ValueError)),
        "expected an exception class value, got {value:?}"
    );

    let build = function(&hir, "build");
    let HStmtKind::Return(Some(value)) = &build.body[0].kind else {
        panic!("expected a return, got {:?}", build.body[0].kind);
    };
    let HExprKind::IndirectCall { callee, args, .. } = &value.kind else {
        panic!("expected an indirect call, got {value:?}");
    };
    assert!(args.is_empty());
    assert!(
        matches!(&callee.kind, HExprKind::ExceptionClass(ExcKind::Exception)),
        "expected the Exception class as callee, got {callee:?}"
    );
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn local_bindings_shadow_builtin_exception_names() {
    let source = "ValueError = 3\nx = ValueError\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(&assigned(&hir, "x").kind, HExprKind::Local(_)));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn class_methods_may_share_names_and_stay_out_of_the_global_namespace() {
    let source = "class A:\n    def f(self):\n        return 1\nclass B:\n    def f(self):\n        return 2\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let mut method_names: Vec<&str> = hir
        .functions
        .iter()
        .filter(|f| f.name.contains('.'))
        .map(|f| f.name.as_str())
        .collect();
    method_names.sort_unstable();
    assert_eq!(method_names, vec!["A.f", "B.f"]);
}

#[test]
fn class_attributes_are_frozen_in_the_class_statement() {
    let source = "class C:\n    __slots__ = ['a', 'b']\n    mark = 7\n    ratio: int = 1\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ClassDef {
        methods,
        attributes,
        ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!(
            "expected a class definition, got {:?}",
            hir.functions[0].body[0].kind
        );
    };
    assert!(methods.is_empty(), "{methods:?}");
    let names: Vec<&str> = attributes.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["__slots__", "mark", "ratio"]);
    assert!(
        matches!(&attributes[0].1.kind, HExprKind::List(items) if items.len() == 2),
        "{:?}",
        attributes[0].1.kind
    );
    assert!(matches!(
        &attributes[1].1.kind,
        HExprKind::Const(Const::Int(7))
    ));
    assert!(matches!(
        &attributes[2].1.kind,
        HExprKind::Const(Const::Int(1))
    ));

    crate::sema::check(&hir, &mut infer, "test.py").expect("valid program");
    let program = crate::sema::emit(&hir, &mut infer);
    let pyx_ir::StmtKind::ClassDef {
        attributes: emitted,
        ..
    } = &program.functions[0].body[0].kind
    else {
        panic!("expected a class statement");
    };
    assert_eq!(emitted.len(), 3);
    assert_eq!(emitted[1].name, "mark");
}

#[test]
fn method_decorators_select_the_binding_kind() {
    let source = "\
class C:
    @property
    def value(self):
        return 1
    @classmethod
    def build(cls):
        return cls()
    @staticmethod
    def helper(x):
        return x
    @lru_cache(maxsize=1024)
    def cached(self):
        return 2
    @value.setter
    def value(self, new):
        self._value = new
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ClassDef { methods, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a class definition");
    };
    let kinds: Vec<(&str, MethodKind)> = methods
        .iter()
        .map(|(name, _, kind)| (name.as_str(), *kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("value", MethodKind::Property),
            ("build", MethodKind::Classmethod),
            ("helper", MethodKind::Staticmethod),
            ("cached", MethodKind::Normal),
        ]
    );
}

#[test]
fn unknown_method_decorators_are_still_diagnosed() {
    let source = "class C:\n    @custom\n    def f(self):\n        return 1\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("decorators are not supported"),
        "{:?}",
        diags[0].message
    );
}

#[test]
fn namedtuple_fields_synthesize_an_init() {
    let source = "\
from typing import NamedTuple
class Point(NamedTuple):
    x: int
    y: int
    def total(self):
        return self.x
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ClassDef {
        name,
        base: Some(_),
        methods,
        ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!(
            "expected class Point with a base, got {:?}",
            hir.functions[0].body[0].kind
        );
    };
    assert_eq!(name, "Point");
    assert_eq!(methods.len(), 2, "{methods:?}");
    assert_eq!(methods[0].0, "total");
    assert_eq!(methods[0].2, MethodKind::Normal);

    let init = function(&hir, "Point.__init__");
    assert_eq!(
        init.params
            .iter()
            .map(|param| param.name.as_str())
            .collect::<Vec<_>>(),
        vec!["self", "x", "y"]
    );
    assert!(matches!(&init.body[0].kind, HStmtKind::SetAttr { name, .. } if name == "x"));
    assert!(matches!(&init.body[1].kind, HStmtKind::SetAttr { name, .. } if name == "y"));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn future_annotations_and_dynamic_annotations_are_accepted() {
    let source = "\
from __future__ import annotations
def f(x: Widget, y: list[int], z: 'RenderableType') -> Result:
    return x
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert!(f
        .params
        .iter()
        .all(|param| matches!(param.ty, Ty::Concrete(Type::Any))));
}

#[test]
fn overload_stubs_are_dropped_and_noop_decorators_are_ignored() {
    let source = "\
from functools import lru_cache
from typing import overload
@overload
def f(x: int) -> int: ...
@overload
def f(x: str) -> str: ...
@lru_cache(maxsize=16)
def f(x):
    return x
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let named: Vec<&str> = hir
        .functions
        .iter()
        .map(|function| function.name.as_str())
        .filter(|name| *name == "f")
        .collect();
    assert_eq!(named, vec!["f"]);
}

#[test]
fn assert_lowers_to_a_test_and_an_optional_message() {
    let source = "x = 1\nassert x > 0, 'x must be positive'\nassert x\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let asserts: Vec<&HStmtKind> = hir.functions[0]
        .body
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            HStmtKind::Assert { .. } => Some(&stmt.kind),
            _ => None,
        })
        .collect();
    assert_eq!(asserts.len(), 2, "{asserts:?}");
    let HStmtKind::Assert { test, msg } = asserts[0] else {
        unreachable!()
    };
    assert!(
        matches!(&test.kind, HExprKind::Bin { .. }),
        "the test must be lowered as an expression: {:?}",
        test.kind
    );
    assert!(
        matches!(
            msg.as_ref().map(|msg| &msg.kind),
            Some(HExprKind::Const(Const::Str(text))) if text == "x must be positive"
        ),
        "the message must be kept on the statement: {msg:?}"
    );
    let HStmtKind::Assert { test: _, msg } = asserts[1] else {
        unreachable!()
    };
    assert!(msg.is_none(), "a bare assert has no message");
}

#[test]
fn rich_repr_class_decorator_is_ignored() {
    let source = "\
@rich_repr
class C:
    pass
@rich.repr.rich_repr
class D:
    pass
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let names: Vec<&str> = hir.functions[0]
        .body
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            HStmtKind::ClassDef { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["C", "D"]);
}

#[test]
fn class_decorators_are_rejected() {
    let source = "@dec\nclass C:\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("class decorators"),
        "{:?}",
        diags[0].message
    );
}

#[test]
fn attribute_and_subscript_unpack_targets_lower() {
    let source = "class C:\n    pass\nobj = C()\nitems = [0]\nt = (1, 2)\nobj.x, items[0] = t\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Unpack { targets, value } = &hir.functions[0].body[4].kind else {
        panic!(
            "expected an unpack statement, got {:?}",
            hir.functions[0].body[4].kind
        );
    };
    assert_eq!(targets.len(), 2);
    match &targets[0] {
        HUnpackTarget::Attr { object, name } => {
            assert_eq!(name, "x");
            assert!(matches!(&object.kind, HExprKind::Local(_)));
        }
        other => panic!("expected an attribute target, got {other:?}"),
    }
    match &targets[1] {
        HUnpackTarget::Subscript { object, index } => {
            assert!(matches!(&object.kind, HExprKind::Local(_)));
            assert!(matches!(&index.kind, HExprKind::Const(_)));
        }
        other => panic!("expected a subscript target, got {other:?}"),
    }
    assert!(matches!(&value.kind, HExprKind::Local(_)));
}

#[test]
fn duplicate_underscore_unpack_targets_share_a_local() {
    let source = "t = (1, 2)\n_, _ = t\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Unpack { targets, .. } = &hir.functions[0].body[1].kind else {
        panic!("expected an unpack statement");
    };
    let HUnpackTarget::Name(first) = &targets[0] else {
        panic!("expected a name target");
    };
    let HUnpackTarget::Name(second) = &targets[1] else {
        panic!("expected a name target");
    };
    assert_eq!(first, second);
    assert_eq!(hir.functions[0].locals[*first as usize].name, "_");
}

#[test]
fn class_methods_are_not_globally_callable() {
    let source = "class A:\n    def f(self):\n        return 1\nx = f\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].message, "name 'f' is not defined");
}

#[test]
fn class_docstrings_and_pass_are_allowed() {
    let source = "class C:\n    \"\"\"doc\"\"\"\n    pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ClassDef { methods, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a class definition");
    };
    assert!(methods.is_empty());
}

/// Returns the function with `name`, or panics.
fn function<'a>(hir: &'a LHir, name: &str) -> &'a HFunction {
    hir.functions
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no function '{name}'"))
}

#[test]
fn generator_function_marks_the_function_and_emits_yield_statements() {
    let source = "def g():\n    yield 1\n    yield 2\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let g = function(&hir, "g");
    assert!(g.is_generator);
    assert!(!g.is_async);
    assert!(!g.is_module);
    assert!(
        g.locals.is_empty(),
        "no hidden generator local: {:?}",
        g.locals
    );

    assert_eq!(g.body.len(), 2, "{:?}", g.body);
    for (stmt, expected) in [(&g.body[0], 1), (&g.body[1], 2)] {
        let HStmtKind::Yield { value: Some(value) } = &stmt.kind else {
            panic!("expected a yield statement, got {:?}", stmt.kind);
        };
        assert!(matches!(
            &value.kind,
            HExprKind::Const(Const::Int(n)) if *n == expected
        ));
    }
}

#[test]
fn generator_yields_inside_control_flow_are_statements() {
    let source = "def g(n):\n    if n > 0:\n        yield n\n    for i in range(n):\n        yield i\n    while n > 5:\n        yield 0\n        n = 5\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let g = function(&hir, "g");
    assert!(g.is_generator);

    fn count_yields(stmts: &[HStmt], count: &mut usize) {
        for stmt in stmts {
            match &stmt.kind {
                HStmtKind::Yield { .. } => *count += 1,
                HStmtKind::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    count_yields(then_body, count);
                    count_yields(else_body, count);
                }
                HStmtKind::ForEach { body, .. } | HStmtKind::ForRange { body, .. } => {
                    count_yields(body, count)
                }
                HStmtKind::While { body, .. } => count_yields(body, count),
                HStmtKind::Try {
                    body,
                    handlers,
                    else_body,
                    finally_body,
                } => {
                    count_yields(body, count);
                    for handler in handlers {
                        count_yields(&handler.body, count);
                    }
                    count_yields(else_body, count);
                    count_yields(finally_body, count);
                }
                _ => {}
            }
        }
    }

    let mut yields = 0;
    count_yields(&g.body, &mut yields);
    assert_eq!(yields, 3);
}

#[test]
fn generator_return_value_is_allowed() {
    for (source, has_value) in [
        ("def g():\n    yield 1\n    return 2\n", true),
        ("def g():\n    yield 1\n    return\n", false),
    ] {
        let (hir, _, diags) = lower_source(source, &parse(source));
        assert!(diags.is_empty(), "{source}: {diags:?}");
        let g = function(&hir, "g");
        assert!(g.is_generator, "{source}");
        match (&g.body.last().expect("last statement").kind, has_value) {
            (HStmtKind::Return(Some(value)), true) => {
                assert!(
                    matches!(&value.kind, HExprKind::Const(Const::Int(2))),
                    "{source}"
                );
            }
            (HStmtKind::Return(None), false) => {}
            (other, _) => panic!("unexpected return lowering for {source}: {other:?}"),
        }
    }
}

#[test]
fn yield_expressions_lower_and_mark_the_generator() {
    for source in [
        "def g():\n    x = yield 1\n    yield x\n",
        "def g():\n    x = (yield 1) + 2\n    yield x\n",
        "def g():\n    print((yield 1))\n",
        "def g():\n    return (yield 1)\n",
    ] {
        let (hir, _, diags) = lower_source(source, &parse(source));
        assert!(diags.is_empty(), "{source}: {diags:?}");
        assert!(function(&hir, "g").is_generator, "{source}");
    }
}

#[test]
fn assignment_yield_lowers_to_a_yield_expression() {
    let source = "def g():\n    x = (yield 1) + 2\n    yield x\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");

    let HStmtKind::Assign { value, .. } = &g.body[0].kind else {
        panic!("expected an assignment, got {:?}", g.body[0].kind);
    };
    let HExprKind::Bin { lhs, .. } = &value.kind else {
        panic!("expected a bin op, got {:?}", value.kind);
    };
    let HExprKind::Yield { value: Some(inner) } = &lhs.kind else {
        panic!("expected a yield expression, got {:?}", lhs.kind);
    };
    assert!(matches!(&inner.kind, HExprKind::Const(Const::Int(1))));

    // The following `yield x` is still the statement form.
    let HStmtKind::Yield { value: Some(value) } = &g.body[1].kind else {
        panic!("expected a yield statement, got {:?}", g.body[1].kind);
    };
    assert!(matches!(&value.kind, HExprKind::Local(_)));
}

#[test]
fn return_yield_expression_lowers_to_a_returned_yield() {
    let source = "def g():\n    return (yield 1)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    assert!(g.is_generator);
    let HStmtKind::Return(Some(value)) = &g.body[0].kind else {
        panic!("expected a returned expression, got {:?}", g.body[0].kind);
    };
    assert!(matches!(
        &value.kind,
        HExprKind::Yield { value: Some(inner) }
            if matches!(&inner.kind, HExprKind::Const(Const::Int(1)))
    ));
}

#[test]
fn bare_yield_lowers_to_a_yield_statement() {
    let source = "def g():\n    yield\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    assert!(g.is_generator);
    assert_eq!(g.body.len(), 1, "{:?}", g.body);
    assert!(matches!(&g.body[0].kind, HStmtKind::Yield { value: None }));
}

#[test]
fn yield_from_lowers_as_a_statement() {
    let source = "def g():\n    yield from [1, 2]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    assert!(g.is_generator);
    assert_eq!(g.body.len(), 1, "{:?}", g.body);
    let HStmtKind::Expr(value) = &g.body[0].kind else {
        panic!("expected an expression statement, got {:?}", g.body[0].kind);
    };
    let HExprKind::YieldFrom { value: iterable } = &value.kind else {
        panic!("expected a yield-from expression, got {:?}", value.kind);
    };
    assert!(matches!(&iterable.kind, HExprKind::List(_)));
}

#[test]
fn yield_from_lowers_as_an_expression() {
    let source = "def g():\n    x = yield from range(3)\n    yield x\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    assert!(g.is_generator);
    let HStmtKind::Assign { value, .. } = &g.body[0].kind else {
        panic!("expected an assignment, got {:?}", g.body[0].kind);
    };
    assert!(matches!(&value.kind, HExprKind::YieldFrom { .. }));
}

#[test]
fn yield_from_outside_a_generator_is_rejected() {
    let source = "yield from [1, 2]\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|d| d.message == "yield outside a function"),
        "{diags:?}"
    );
}

#[test]
fn yield_outside_a_function_is_rejected() {
    let source = "yield 1\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].message, "yield outside a function");
    assert_eq!(diags[0].code, codes::CONTROL);
}

#[test]
fn async_def_marks_the_function_and_lowers_await() {
    let source =
        "import asyncio\nasync def f():\n    await 1\n    return 7\nx = asyncio.run(f())\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert!(f.is_async);
    assert!(!f.is_generator);
    assert_eq!(f.body.len(), 2, "{:?}", f.body);
    let HStmtKind::Expr(awaited) = &f.body[0].kind else {
        panic!("expected the awaited expression");
    };
    let HExprKind::Await { value } = &awaited.kind else {
        panic!("expected an await node, got {:?}", awaited.kind);
    };
    assert!(matches!(&value.kind, HExprKind::Const(Const::Int(1))));
    assert!(matches!(&f.body[1].kind, HStmtKind::Return(Some(_))));
}

#[test]
fn await_outside_an_async_function_is_rejected() {
    let source = "def f():\n    await 1\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0].message,
        "await is only allowed inside async functions"
    );
    assert_eq!(diags[0].code, codes::CONTROL);
}

#[test]
fn async_generator_is_rejected() {
    let source = "async def g():\n    yield 1\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].message, "async generators are not supported");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    let g = function(&hir, "g");
    assert!(g.is_async);
    assert!(g.is_generator);
}

#[test]
fn async_with_and_async_for_reuse_the_synchronous_lowering() {
    let source = "async def f():\n    async with open('x.txt') as fh:\n        for line in fh:\n            print(line)\n    async for x in [1, 2]:\n        print(x)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert!(matches!(&f.body[0].kind, HStmtKind::Try { .. }));
    assert!(matches!(&f.body[1].kind, HStmtKind::ForEach { .. }));
}

#[test]
fn async_methods_are_registered_in_classes() {
    let source = "class A:\n    async def f(self):\n        return 1\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let method = function(&hir, "A.f");
    assert!(method.is_async);
    assert!(!method.is_generator);
    assert!(matches!(&method.body[0].kind, HStmtKind::Return(Some(_))));
}

#[test]
fn nested_function_captures_outer_locals_by_value() {
    let source = "def outer(x):\n    def inner(y):\n        return x + y\n    return inner\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let outer = function(&hir, "outer");
    let x_local = outer
        .locals
        .iter()
        .position(|local| local.name == "x")
        .expect("outer parameter x") as LocalId;
    let inner_local = outer
        .locals
        .iter()
        .position(|local| local.name == "inner")
        .expect("nested function local") as LocalId;

    assert_eq!(outer.body.len(), 2, "{:?}", outer.body);
    let HStmtKind::Closure {
        target,
        function: fid,
        captures,
    } = &outer.body[0].kind
    else {
        panic!(
            "expected a closure definition, got {:?}",
            outer.body[0].kind
        );
    };
    assert_eq!(*target, inner_local);
    assert_eq!(captures, &vec![x_local]);

    let inner = &hir.functions[*fid as usize];
    assert_eq!(inner.name, "outer.inner");
    assert_eq!(inner.capture_count, 1);
    assert_eq!(
        inner
            .params
            .iter()
            .map(|param| param.name.as_str())
            .collect::<Vec<_>>(),
        vec!["x", "y"]
    );
    assert_eq!(inner.locals[0].name, "x");
    assert_eq!(inner.locals[1].name, "y");

    let HStmtKind::Return(Some(value)) = &outer.body[1].kind else {
        panic!("expected the returned nested function");
    };
    assert!(
        matches!(&value.kind, HExprKind::Local(local) if *local == inner_local),
        "returning a nested function must read its local: {:?}",
        value.kind
    );
}

#[test]
fn nested_function_in_a_loop_captures_the_current_iteration_value() {
    let source = "def outer(n):\n    fns = []\n    for i in range(n):\n        def get():\n            return i\n        fns.append(get)\n    return fns\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let outer = function(&hir, "outer");
    let i_local = outer
        .locals
        .iter()
        .position(|local| local.name == "i")
        .expect("loop variable i") as LocalId;
    let get_local = outer
        .locals
        .iter()
        .position(|local| local.name == "get")
        .expect("nested function local get") as LocalId;

    let HStmtKind::ForRange { body, .. } = &outer.body[1].kind else {
        panic!("expected the range loop, got {:?}", outer.body[1].kind);
    };
    let HStmtKind::Closure {
        target,
        function: fid,
        captures,
    } = &body[0].kind
    else {
        panic!("expected a closure inside the loop, got {:?}", body[0].kind);
    };
    assert_eq!(*target, get_local);
    assert_eq!(captures, &vec![i_local]);
    assert_eq!(hir.functions[*fid as usize].capture_count, 1);
}

#[test]
fn nested_functions_propagate_captures_through_intermediate_scopes() {
    let source = "def outer(x):\n    def middle():\n        def inner():\n            return x\n        return inner\n    return middle\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let middle = function(&hir, "outer.middle");
    assert_eq!(middle.capture_count, 1);
    assert_eq!(middle.params[0].name, "x");

    let inner = function(&hir, "outer.middle.inner");
    assert_eq!(inner.capture_count, 1);
    assert_eq!(inner.params[0].name, "x");

    // `middle` captures the value from `outer` even though it never loads
    // `x` itself.
    let outer = function(&hir, "outer");
    let HStmtKind::Closure { captures, .. } = &outer.body[0].kind else {
        panic!("expected the middle closure");
    };
    let x_local = outer
        .locals
        .iter()
        .position(|local| local.name == "x")
        .expect("outer parameter x") as LocalId;
    assert_eq!(captures, &vec![x_local]);
}

#[test]
fn recursive_nested_functions_are_rejected() {
    let source = "def outer(x):\n    def inner(y):\n        return inner(y)\n    return inner\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]
            .message
            .contains("recursive nested functions are not supported"),
        "{diags:?}"
    );
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
}

#[test]
fn nested_function_free_name_errors_are_reported() {
    let source = "def outer(x):\n    def inner():\n        return z\n    return inner\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("name 'z' is not defined"),
        "{diags:?}"
    );
    assert_eq!(diags[0].code, codes::NAME);
}

#[test]
fn nested_function_does_not_capture_names_assigned_in_its_body() {
    let source =
        "def outer(x):\n    def inner():\n        x = 2\n        return x\n    return inner\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let inner = function(&hir, "outer.inner");
    assert_eq!(inner.capture_count, 0);
    assert_eq!(inner.params.len(), 0);
    assert_eq!(
        inner
            .locals
            .iter()
            .filter(|local| local.name == "x")
            .count(),
        1,
        "the assignment must create a nested local, not a capture"
    );
}

#[test]
fn integer_arithmetic_folds_to_constants() {
    let source = "a = 2 + 3\n\
                  b = 7 - 10\n\
                  c = 4 * 6\n\
                  d = 7 // 2\n\
                  e = -7 // 2\n\
                  f = -7 % 3\n\
                  g = 2 ** 10\n\
                  h = 7 / 2\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    for (name, expected) in [
        ("a", 5),
        ("b", -3),
        ("c", 24),
        ("d", 3),
        ("e", -4),
        ("f", 2),
        ("g", 1024),
    ] {
        let expr = assigned(&hir, name);
        assert!(
            matches!(&expr.kind, HExprKind::Const(Const::Int(v)) if *v == expected),
            "{name}: {:?}",
            expr.kind
        );
        assert_eq!(expr.ty, Ty::Concrete(Type::Int), "{name}");
    }

    let h = assigned(&hir, "h");
    assert!(matches!(
        &h.kind,
        HExprKind::Const(Const::Float(v)) if *v == 3.5
    ));
    assert_eq!(h.ty, Ty::Concrete(Type::Float));
}

#[test]
fn float_arithmetic_folds_to_constants() {
    let source = "a = 1.5 + 2.5\n\
                  b = 5.0 - 1.25\n\
                  c = 2.0 * 3.5\n\
                  d = 7.0 / 2.0\n\
                  e = 7.5 // 2.0\n\
                  f = 7.5 % 2.0\n\
                  g = 2.0 ** 3.0\n\
                  h = 1 + 2.5\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    for (name, expected) in [
        ("a", 4.0),
        ("b", 3.75),
        ("c", 7.0),
        ("d", 3.5),
        ("e", 3.0),
        ("f", 1.5),
        ("g", 8.0),
        ("h", 3.5),
    ] {
        let expr = assigned(&hir, name);
        assert!(
            matches!(&expr.kind, HExprKind::Const(Const::Float(v)) if *v == expected),
            "{name}: {:?}",
            expr.kind
        );
        assert_eq!(expr.ty, Ty::Concrete(Type::Float), "{name}");
    }
}

#[test]
fn string_concatenation_folds_to_a_constant() {
    let source = "a = \"ab\" + \"cd\"\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let expr = assigned(&hir, "a");
    assert!(matches!(
        &expr.kind,
        HExprKind::Const(Const::Str(s)) if s == "abcd"
    ));
    assert_eq!(expr.ty, Ty::Concrete(Type::Str));
}

#[test]
fn constant_comparisons_fold_to_bools() {
    let source = "a = 1 < 2\n\
                  b = 2.5 >= 2\n\
                  c = 1 == 1.0\n\
                  d = \"abc\" == \"abc\"\n\
                  e = \"a\" != \"b\"\n\
                  f = 3 > 4\n\
                  g = \"ell\" in \"hello\"\n\
                  h = \"zz\" not in \"hello\"\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    for (name, expected) in [
        ("a", true),
        ("b", true),
        ("c", true),
        ("d", true),
        ("e", true),
        ("f", false),
        ("g", true),
        ("h", true),
    ] {
        let expr = assigned(&hir, name);
        assert!(
            matches!(&expr.kind, HExprKind::Const(Const::Bool(v)) if *v == expected),
            "{name}: {:?}",
            expr.kind
        );
        assert_eq!(expr.ty, Ty::Concrete(Type::Bool), "{name}");
    }
}

#[test]
fn constant_unary_operations_fold() {
    let source = "a = -5\nb = -2.5\nc = not True\nd = -(2 + 3)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    assert!(matches!(
        &assigned(&hir, "a").kind,
        HExprKind::Const(Const::Int(-5))
    ));
    assert!(matches!(
        &assigned(&hir, "b").kind,
        HExprKind::Const(Const::Float(v)) if *v == -2.5
    ));
    assert!(matches!(
        &assigned(&hir, "c").kind,
        HExprKind::Const(Const::Bool(false))
    ));
    assert!(matches!(
        &assigned(&hir, "d").kind,
        HExprKind::Const(Const::Int(-5))
    ));
}

#[test]
fn raising_and_overflowing_operations_stay_unfused() {
    let source = "a = 1 / 0\n\
                  b = 1 // 0\n\
                  c = 1 % 0\n\
                  d = 9223372036854775807 + 1\n\
                  e = 1152921504606846975 + 1\n\
                  f = 2 ** 9223372036854775807\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    for name in ["a", "b", "c", "d", "e", "f"] {
        let expr = assigned(&hir, name);
        assert!(
            matches!(&expr.kind, HExprKind::Bin { .. }),
            "{name} folded: {:?}",
            expr.kind
        );
    }
}

#[test]
fn non_constant_operands_stay_unfused() {
    let source = "a = 1\nb = a + 1\nc = a * 2.5\nd = a < 2\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for name in ["b", "c", "d"] {
        let expr = assigned(&hir, name);
        assert!(
            matches!(&expr.kind, HExprKind::Bin { .. }),
            "{name} folded: {:?}",
            expr.kind
        );
    }
}

#[test]
fn literal_conditions_drop_dead_statements() {
    let source = "if False:\n    x = 1\nelse:\n    y = 2\n\
                  if True:\n    z = 3\nelse:\n    w = 4\n\
                  while False:\n    q = 5\n\
                  while True:\n    break\n\
                  v = 6\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let body = &hir.functions[0].body;
    assert_eq!(body.len(), 4, "dead while must be dropped");

    let HStmtKind::If {
        cond,
        then_body,
        else_body,
    } = &body[0].kind
    else {
        panic!("expected the false if");
    };
    assert!(matches!(&cond.kind, HExprKind::Const(Const::Bool(false))));
    assert!(then_body.is_empty(), "dead branch was lowered");
    assert_eq!(else_body.len(), 1);
    assert!(matches!(&else_body[0].kind, HStmtKind::Assign { .. }));

    let HStmtKind::If {
        cond,
        then_body,
        else_body,
    } = &body[1].kind
    else {
        panic!("expected the true if");
    };
    assert!(matches!(&cond.kind, HExprKind::Const(Const::Bool(true))));
    assert_eq!(then_body.len(), 1);
    assert!(else_body.is_empty(), "dead branch was lowered");

    // `while True` stays a loop so `break` is still emitted.
    assert!(
        matches!(&body[2].kind, HStmtKind::While { .. }),
        "{:?}",
        body[2].kind
    );
    assert!(matches!(&body[3].kind, HStmtKind::Assign { .. }));
}

#[test]
fn while_else_lowers_the_else_body_outside_the_loop() {
    let source = "i = 0\n\
                  while i < 3:\n    i += 1\n\
                  else:\n    print('done')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let loop_stmt = hir.functions[0]
        .body
        .iter()
        .find(|stmt| matches!(&stmt.kind, HStmtKind::While { .. }))
        .expect("expected a while loop");
    let HStmtKind::While {
        body, else_body, ..
    } = &loop_stmt.kind
    else {
        unreachable!();
    };
    assert_eq!(body.len(), 1, "the loop body must stay in `body`");
    assert_eq!(else_body.len(), 1, "the else clause must be lowered");
}

#[test]
fn for_range_and_for_each_else_lower_the_else_body() {
    let range_source = "for i in range(3):\n    print(i)\n\
                        else:\n    print('done')\n";
    let (hir, _, diags) = lower_source(range_source, &parse(range_source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ForRange {
        body, else_body, ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!("expected a range loop");
    };
    assert_eq!(body.len(), 1);
    assert_eq!(else_body.len(), 1);

    let each_source = "xs = [1]\nfor x in xs:\n    print(x)\n\
                       else:\n    print('done')\n";
    let (hir, _, diags) = lower_source(each_source, &parse(each_source));
    assert!(diags.is_empty(), "{diags:?}");
    let loop_stmt = hir.functions[0]
        .body
        .iter()
        .find(|stmt| matches!(&stmt.kind, HStmtKind::ForEach { .. }))
        .expect("expected a for-each loop");
    let HStmtKind::ForEach {
        body, else_body, ..
    } = &loop_stmt.kind
    else {
        unreachable!();
    };
    assert_eq!(body.len(), 1);
    assert_eq!(else_body.len(), 1);
}

#[test]
fn break_in_a_loop_else_binds_to_the_enclosing_loop() {
    // The inner `else` is outside the inner loop, so `break` targets the
    // outer loop; the same clause with no enclosing loop is an error.
    let source = "for i in range(3):\n\
                  \x20   for j in range(3):\n        pass\n\
                  \x20   else:\n        break\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ForRange {
        body, else_body, ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!("expected the outer range loop");
    };
    assert!(else_body.is_empty(), "the outer loop has no else clause");
    let HStmtKind::ForRange {
        body: inner_body,
        else_body: inner_else,
        ..
    } = &body[0].kind
    else {
        panic!("expected the inner range loop in the outer body");
    };
    assert_eq!(inner_body.len(), 1);
    assert!(
        matches!(&inner_else[0].kind, HStmtKind::Break),
        "break in a loop else must bind to the enclosing loop"
    );

    let source = "while True:\n    pass\nelse:\n    break\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|diag| diag.message.contains("break outside a loop")),
        "{diags:?}"
    );
}

#[test]
fn while_false_with_else_keeps_only_the_else_body() {
    let source = "while False:\n    print('body')\nelse:\n    print('done')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::While {
        body, else_body, ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!("a `while False` with `else` must keep the else clause");
    };
    assert!(body.is_empty(), "the dead loop body was lowered");
    assert_eq!(else_body.len(), 1);
}

// ----------------------------------------------------------------------
// Compiled extensions
// ----------------------------------------------------------------------
static EXT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Temporary directory holding a source file and compiled-extension stubs.
struct ExtensionDir {
    path: PathBuf,
}

impl ExtensionDir {
    fn new(test_name: &str) -> ExtensionDir {
        let unique = EXT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-frontend-ext-{}-{test_name}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        ExtensionDir { path }
    }

    fn add_extension(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create extension dir");
        }
        fs::write(&path, b"").expect("write extension stub");
        path
    }
}

impl Drop for ExtensionDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn lower_source_in(dir: &Path, source: &str) -> (LHir, Infer, Vec<Diagnostic>) {
    let filename = dir.join("test.py");
    let filename = filename.to_string_lossy();
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, &filename, &mut infer);
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    (hir, infer, diags)
}

fn function_body<'a>(hir: &'a LHir, name: &str) -> &'a [HStmt] {
    &hir.functions
        .iter()
        .find(|function| function.name == name)
        .unwrap_or_else(|| panic!("no function '{name}'"))
        .body
}

fn extension_callee(expr: &HExpr) -> (&str, &str) {
    match &expr.kind {
        HExprKind::Call {
            callee: Callee::Extension { path, member },
            ..
        } => (path.as_str(), member.as_str()),
        other => panic!("expected an extension call, got {other:?}"),
    }
}

#[test]
fn extension_import_lowers_calls_and_attributes() {
    let dir = ExtensionDir::new("import");
    let so = dir.add_extension("mod_ext.abi3.so");
    let expected = fs::canonicalize(&so).expect("canonical extension path");
    let expected_path = expected.to_string_lossy().into_owned();

    let source = r#"import mod_ext as ext
a = ext.add(2, 3)
def f(x):
    return ext.add(x, 1)
b = ext.tag
c = ext.add
"#;
    let (hir, _, diags) = lower_source_in(&dir.path, source);
    assert!(diags.is_empty(), "{diags:?}");

    let (path, member) = extension_callee(assigned(&hir, "a"));
    assert_eq!((path, member), (expected_path.as_str(), "add"));
    assert_eq!(assigned(&hir, "a").ty, Ty::Concrete(Type::Any));

    let body = function_body(&hir, "f");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    assert_eq!(extension_callee(value), (expected_path.as_str(), "add"));

    match &assigned(&hir, "b").kind {
        HExprKind::ExtensionAttr { path, member } => {
            assert_eq!(
                (path.as_str(), member.as_str()),
                (expected_path.as_str(), "tag")
            );
        }
        other => panic!("expected an extension attribute, got {other:?}"),
    }
    match &assigned(&hir, "c").kind {
        HExprKind::ExtensionAttr { path, member } => {
            assert_eq!(
                (path.as_str(), member.as_str()),
                (expected_path.as_str(), "add")
            );
        }
        other => panic!("expected an extension attribute, got {other:?}"),
    }
    assert_eq!(assigned(&hir, "b").ty, Ty::Concrete(Type::Any));
}

#[test]
fn from_extension_import_lowers_members() {
    let dir = ExtensionDir::new("from-import");
    let so = dir.add_extension("mod_ext.abi3.so");
    let expected = fs::canonicalize(&so).expect("canonical extension path");
    let expected_path = expected.to_string_lossy().into_owned();

    let source = r#"from mod_ext import add, tag as label
a = add(1, 2)
def f():
    return add(3, 4)
b = label
c = add
"#;
    let (hir, _, diags) = lower_source_in(&dir.path, source);
    assert!(diags.is_empty(), "{diags:?}");

    assert_eq!(
        extension_callee(assigned(&hir, "a")),
        (expected_path.as_str(), "add")
    );
    let body = function_body(&hir, "f");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    assert_eq!(extension_callee(value), (expected_path.as_str(), "add"));

    for (binding, member) in [("b", "tag"), ("c", "add")] {
        match &assigned(&hir, binding).kind {
            HExprKind::ExtensionAttr {
                path,
                member: got_member,
            } => {
                assert_eq!(
                    (path.as_str(), got_member.as_str()),
                    (expected_path.as_str(), member)
                );
            }
            other => panic!("expected an extension attribute, got {other:?}"),
        }
    }
}

#[test]
fn locals_shadow_extension_aliases() {
    let dir = ExtensionDir::new("shadow");
    dir.add_extension("mod_ext.abi3.so");

    // A module-level local shadows the alias in the module body; a parameter
    // shadows it inside a function.
    let source = r#"import mod_ext as ext
ext = 1
a = ext.add(2, 3)
def f(ext):
    return ext.add(4)
"#;
    let (hir, _, diags) = lower_source_in(&dir.path, source);
    assert!(diags.is_empty(), "{diags:?}");

    assert!(
        matches!(&assigned(&hir, "a").kind, HExprKind::MethodCall { .. }),
        "{:?}",
        assigned(&hir, "a").kind
    );
    let body = function_body(&hir, "f");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    assert!(
        matches!(&value.kind, HExprKind::MethodCall { .. }),
        "{:?}",
        value.kind
    );
}

#[test]
fn extension_type_suffixes_are_discovered() {
    // `<name>.so` is preferred over other suffixes; a matching wheel-style
    // name (`<name>.cpython-314-<platform>.so`) is used when no plain `.so`
    // exists.
    let dir = ExtensionDir::new("suffixes");
    let suffixed = dir.add_extension("mod_ext.cpython-314-x86_64-linux-gnu.so");
    dir.add_extension("mod_ext.cpython-312-x86_64-linux-gnu.so");

    let source = "import mod_ext\nx = mod_ext.add(1, 2)\n";
    let (hir, _, diags) = lower_source_in(&dir.path, source);
    assert!(diags.is_empty(), "{diags:?}");

    let expected = fs::canonicalize(&suffixed).expect("canonical extension path");
    let expected_path = expected.to_string_lossy().into_owned();
    match &assigned(&hir, "x").kind {
        HExprKind::Call {
            callee: Callee::Extension { path, member },
            ..
        } => {
            assert_eq!(
                (path.as_str(), member.as_str()),
                (expected_path.as_str(), "add")
            );
        }
        other => panic!("expected an extension call, got {other:?}"),
    }
}

#[test]
fn missing_extension_module_still_diagnoses() {
    let dir = ExtensionDir::new("missing");
    let source = "import missing_ext\n";
    let (_, _, diags) = lower_source_in(&dir.path, source);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert_eq!(
        diags[0].message,
        "module 'missing_ext' is not supported yet"
    );
}

#[test]
fn extension_alias_used_as_value_is_rejected() {
    let dir = ExtensionDir::new("alias-value");
    dir.add_extension("mod_ext.abi3.so");
    let source = "import mod_ext as ext\nx = ext\n";
    let (_, _, diags) = lower_source_in(&dir.path, source);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert!(diags[0].message.contains("'ext' is a module"), "{diags:?}");
}

#[test]
fn extension_keyword_arguments_defer_to_runtime() {
    let dir = ExtensionDir::new("keywords");
    dir.add_extension("mod_ext.abi3.so");
    let source = r#"import mod_ext as ext
def f():
    return ext.add(x=1)
a = ext.add(y=2)
"#;
    let (hir, _, diags) = lower_source_in(&dir.path, source);
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    assert!(matches!(
        &function.body[0].kind,
        HStmtKind::Return(Some(HExpr {
            kind: HExprKind::DeferredCall { .. },
            ..
        }))
    ));
    assert!(matches!(
        &assigned(&hir, "a").kind,
        HExprKind::DeferredCall { .. }
    ));
}

#[test]
fn extension_star_import_is_rejected() {
    let dir = ExtensionDir::new("star");
    dir.add_extension("mod_ext.abi3.so");
    let source = "from mod_ext import *\n";
    let (_, _, diags) = lower_source_in(&dir.path, source);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert_eq!(
        diags[0].message,
        "'from mod_ext import *' is not supported for extension modules"
    );
}

#[test]
fn dotted_extension_names_remain_diagnostics() {
    let dir = ExtensionDir::new("dotted");
    dir.add_extension("mod_ext.abi3.so");
    let source = "import mod_ext.sub\n";
    let (_, _, diags) = lower_source_in(&dir.path, source);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert_eq!(
        diags[0].message,
        "module 'mod_ext.sub' is not supported yet"
    );
}

#[test]
fn dotted_extension_alias_and_members_are_lowered() {
    let dir = ExtensionDir::new("dotted-import");
    let so = dir.add_extension("pkg/sub_ext.abi3.so");
    let expected = fs::canonicalize(&so).expect("canonical extension path");
    let expected_path = expected.to_string_lossy().into_owned();

    let source = r#"import pkg.sub_ext as e
from pkg.sub_ext import add as plus, tag
a = e.add(1, 2)
b = plus(3, 4)
c = tag
d = e.add
"#;
    let (hir, _, diags) = lower_source_in(&dir.path, source);
    assert!(diags.is_empty(), "{diags:?}");

    assert_eq!(
        extension_callee(assigned(&hir, "a")),
        (expected_path.as_str(), "add")
    );
    assert_eq!(
        extension_callee(assigned(&hir, "b")),
        (expected_path.as_str(), "add")
    );
    for (binding, member) in [("c", "tag"), ("d", "add")] {
        match &assigned(&hir, binding).kind {
            HExprKind::ExtensionAttr {
                path,
                member: got_member,
            } => {
                assert_eq!(
                    (path.as_str(), got_member.as_str()),
                    (expected_path.as_str(), member)
                );
            }
            other => panic!("expected an extension attribute, got {other:?}"),
        }
    }
}

#[test]
fn dotted_extension_import_without_alias_is_rejected() {
    let dir = ExtensionDir::new("dotted-no-alias");
    dir.add_extension("pkg/sub_ext.abi3.so");
    let source = "import pkg.sub_ext\n";
    let (_, _, diags) = lower_source_in(&dir.path, source);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert_eq!(
        diags[0].message,
        "dotted extension imports require an alias"
    );
}

#[test]
fn missing_dotted_extension_module_still_diagnoses() {
    let dir = ExtensionDir::new("dotted-missing");
    dir.add_extension("pkg/sub_ext.abi3.so");
    let source = "import pkg.missing as m\n";
    let (_, _, diags) = lower_source_in(&dir.path, source);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert_eq!(
        diags[0].message,
        "module 'pkg.missing' is not supported yet"
    );
}

#[test]
fn incompatible_wheel_tags_are_rejected() {
    let dir = ExtensionDir::new("abi-policy");
    dir.add_extension("bad.cpython-312-x86_64-linux-gnu.so");

    let source = "import bad as b\n";
    let (_, _, diags) = lower_source_in(&dir.path, source);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert!(
        diags[0].message.contains("cpython-312-x86_64-linux-gnu"),
        "{diags:?}"
    );
    assert!(
        diags[0]
            .message
            .contains("bad.cpython-312-x86_64-linux-gnu.so"),
        "{diags:?}"
    );

    let dir = ExtensionDir::new("abi-policy-from");
    dir.add_extension("bad.pp310-pypy310_pp73-x86_64-linux-gnu.so");
    let source = "from bad import add\n";
    let (_, _, diags) = lower_source_in(&dir.path, source);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert!(diags[0].message.contains("pp310"), "{diags:?}");
}

#[test]
fn cpython_314_and_plain_suffixes_are_preferred_in_order() {
    let dir = ExtensionDir::new("abi-order");
    let suffixed = dir.add_extension("mod_ext.cpython-314-aarch64-linux-gnu.so");
    let abi3 = dir.add_extension("mod_ext.abi3.so");
    let plain = dir.add_extension("mod_ext.so");

    let (hir, _, diags) = lower_source_in(&dir.path, "import mod_ext\nx = mod_ext.add(1, 2)\n");
    assert!(diags.is_empty(), "{diags:?}");
    let expected_path = fs::canonicalize(&plain)
        .expect("canonical extension path")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        extension_callee(assigned(&hir, "x")).0,
        expected_path.as_str()
    );

    fs::remove_file(&plain).expect("remove plain stub");
    let (hir, _, diags) = lower_source_in(&dir.path, "import mod_ext\nx = mod_ext.add(1, 2)\n");
    assert!(diags.is_empty(), "{diags:?}");
    let expected_path = fs::canonicalize(&abi3)
        .expect("canonical extension path")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        extension_callee(assigned(&hir, "x")).0,
        expected_path.as_str()
    );

    fs::remove_file(&abi3).expect("remove abi3 stub");
    let (hir, _, diags) = lower_source_in(&dir.path, "import mod_ext\nx = mod_ext.add(1, 2)\n");
    assert!(diags.is_empty(), "{diags:?}");
    let expected_path = fs::canonicalize(&suffixed)
        .expect("canonical extension path")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        extension_callee(assigned(&hir, "x")).0,
        expected_path.as_str()
    );

    // An incompatible tag next to a compatible one never wins.
    dir.add_extension("mod_ext.cpython-312-x86_64-linux-gnu.so");
    let (hir, _, diags) = lower_source_in(&dir.path, "import mod_ext\nx = mod_ext.add(1, 2)\n");
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        extension_callee(assigned(&hir, "x")).0,
        expected_path.as_str()
    );
}

fn join_path_list(dirs: &[&Path]) -> std::ffi::OsString {
    std::env::join_paths(dirs.iter().map(|dir| dir.to_path_buf())).expect("join path list")
}

fn resolved(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[test]
fn extension_search_path_order_and_source_precedence() {
    let root = ExtensionDir::new("search-order");
    let src = root.path.join("project");
    let ext = root.path.join("ext-path");
    let python = root.path.join("python-path");
    let site_override = root.path.join("site-packages");
    let venv = root.path.join("venv");
    let venv_site = venv.join("lib").join("python3.14").join("site-packages");
    for dir in [&src, &ext, &python, &site_override, &venv_site] {
        fs::create_dir_all(dir).expect("create search dir");
        fs::write(dir.join("mod_ext.abi3.so"), b"").expect("write extension stub");
    }

    let extension_path = join_path_list(&[ext.as_path()]);
    let pythonpath = join_path_list(&[python.as_path()]);
    let site_list = join_path_list(&[site_override.as_path()]);
    let search = extension_search_paths_from(
        Some(extension_path.as_os_str()),
        Some(pythonpath.as_os_str()),
        Some(site_list.as_os_str()),
        Some(venv.as_path()),
        None,
    );
    assert_eq!(
        search,
        vec![
            ext.clone(),
            python.clone(),
            site_override.clone(),
            venv_site.clone()
        ],
        "PYX_SITE_PACKAGES must override auto-detected venv site-packages"
    );

    // The source directory always wins; afterwards each search entry is tried
    // in the documented order as the previous winner disappears.
    let source = src.join("main.py");
    for dir in [&src, &ext, &python, &site_override, &venv_site] {
        let expected = resolved(&dir.join("mod_ext.abi3.so"));
        assert_eq!(
            find_extension_in_search_dirs(&source, &search, "mod_ext"),
            Some(expected.clone()),
            "unexpected winner while resolving from {}",
            dir.display()
        );
        fs::remove_file(dir.join("mod_ext.abi3.so")).expect("remove stub");
    }
    assert_eq!(
        find_extension_in_search_dirs(&source, &search, "mod_ext"),
        None
    );
}

#[test]
fn extension_search_path_dedups_and_skips_missing_entries() {
    let root = ExtensionDir::new("search-dedup");
    let shared = root.path.join("shared");
    fs::create_dir_all(&shared).expect("create shared dir");
    let missing = root.path.join("missing");
    let not_a_dir = root.path.join("not-a-dir");
    fs::write(&not_a_dir, b"").expect("write regular file");

    let extension_path =
        join_path_list(&[shared.as_path(), missing.as_path(), not_a_dir.as_path()]);
    let pythonpath = join_path_list(&[shared.as_path()]);
    let search = extension_search_paths_from(
        Some(extension_path.as_os_str()),
        Some(pythonpath.as_os_str()),
        None,
        Some(missing.as_path()),
        None,
    );
    assert_eq!(search, vec![shared]);
}

#[test]
fn extension_search_path_finds_wheel_style_suffix() {
    let root = ExtensionDir::new("search-wheel");
    let site = root.path.join("site-packages");
    fs::create_dir_all(&site).expect("create site-packages");
    let so = site.join("mod_ext.cpython-314-x86_64-linux-gnu.so");
    fs::write(&so, b"").expect("write extension stub");

    let list = join_path_list(&[site.as_path()]);
    let search = extension_search_paths_from(Some(list.as_os_str()), None, None, None, None);
    let source = root.path.join("project").join("main.py");
    assert_eq!(
        find_extension_in_search_dirs(&source, &search, "mod_ext"),
        Some(resolved(&so))
    );
}

#[test]
fn extension_search_path_reads_environment_prefixes() {
    let root = ExtensionDir::new("search-prefixes");
    let venv = root.path.join("venv");
    let windows_site = venv.join("Lib").join("site-packages");
    let posix_site = venv.join("lib").join("python3.13").join("site-packages");
    let conda = root.path.join("conda");
    let conda_site = conda.join("lib").join("python3.14").join("site-packages");
    for dir in [&windows_site, &posix_site, &conda_site] {
        fs::create_dir_all(dir).expect("create site-packages");
    }

    let search = extension_search_paths_from(
        None,
        None,
        None,
        Some(venv.as_path()),
        Some(conda.as_path()),
    );
    assert_eq!(
        search,
        vec![windows_site, posix_site, conda_site],
        "Windows and POSIX site-packages layouts must both be discovered"
    );
}

#[test]
fn module_value_arguments_lower_to_module_refs() {
    let source = "import errno\nx = getattr(errno, \"EDOM\")\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let x = assigned(&hir, "x");
    let HExprKind::Call { callee, args } = &x.kind else {
        panic!("expected a direct builtin call, got {x:?}");
    };
    assert_eq!(*callee, Callee::Builtin(Builtin::Getattr));
    assert_eq!(args.len(), 2);
    match &args[0].kind {
        HExprKind::ModuleRef { module } => assert_eq!(module, "errno"),
        other => panic!("expected a module reference, got {other:?}"),
    }
    match &args[1].kind {
        HExprKind::Const(pyx_ir::Const::Str(name)) => assert_eq!(name, "EDOM"),
        other => panic!("expected a string constant, got {other:?}"),
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn module_alias_still_rejects_user_module_values() {
    let source = "import helper\nx = helper\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    lowerer.set_user_modules(HashMap::from([(
        "helper".to_string(),
        UserModule {
            name: "helper".to_string(),
            exports: HashMap::new(),
        },
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    let _ = hir;
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]
            .message
            .contains("module values are not supported yet"),
        "{:?}",
        diags[0].message
    );
}

#[test]
fn unittest_test_case_is_accepted_as_a_class_base() {
    let source =
        "import unittest\nclass T(unittest.TestCase):\n    def test_errno(self):\n        pass\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ClassDef {
        base: Some(base), ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!(
            "expected a class with a base, got {:?}",
            hir.functions[0].body[0].kind
        );
    };
    match &base.kind {
        HExprKind::ModuleAttr { module, name } => {
            assert_eq!((module.as_str(), name.as_str()), ("unittest", "TestCase"));
        }
        other => panic!("expected a module attribute base, got {other:?}"),
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn multiple_class_bases_use_the_first_base() {
    let source = "class A:\n    pass\nclass B:\n    pass\nclass T(A, B):\n    pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let class = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::ClassDef { name, base, .. } if name == "T" => Some(base),
            _ => None,
        })
        .expect("class T");
    assert!(class.is_some(), "the first base must be kept");
}

#[test]
fn module_assignment_emits_a_global_set_after_the_local_assign() {
    let source = "x = 1\ndef f():\n    return x\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = &hir.functions[0].body;
    assert_eq!(body.len(), 2, "{body:?}");
    let HStmtKind::Assign { target, .. } = &body[0].kind else {
        panic!("expected the local assignment, got {:?}", body[0].kind);
    };
    let HStmtKind::GlobalSet {
        module,
        name,
        value,
    } = &body[1].kind
    else {
        panic!("expected a global store, got {:?}", body[1].kind);
    };
    assert_eq!(module, "__main__");
    assert_eq!(name, "x");
    match &value.kind {
        HExprKind::Local(local) => assert_eq!(local, target),
        other => panic!("expected a read of the just-assigned local, got {other:?}"),
    }
}

#[test]
fn nested_module_level_assignments_are_mirrored_to_globals() {
    let source = "if True:\n    x = 1\ndef f():\n    return x\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::If { then_body, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected an if statement");
    };
    assert_eq!(then_body.len(), 2, "{then_body:?}");
    assert!(matches!(
        then_body[1].kind,
        HStmtKind::GlobalSet { ref name, .. } if name == "x"
    ));
}

#[test]
fn functions_read_module_level_names_as_globals() {
    let source = "value = 41\ndef f():\n    return value\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = &function(&hir, "f").body;
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    match &value.kind {
        HExprKind::GlobalGet { module, name } => {
            assert_eq!((module.as_str(), name.as_str()), ("__main__", "value"));
        }
        other => panic!("expected a global read, got {other:?}"),
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn module_loop_variables_are_not_mirrored_when_no_function_reads_them() {
    // Entry modules only mirror the names their own functions read. A
    // benchmark-style top-level loop must therefore stay entirely local:
    // no per-iteration store into the runtime global table.
    let sources = [
        "\
n = 0
total = 0
while n < 10:
    i = 0
    while i < 10:
        total = total + i
        i = i + 1
    n = n + 1
",
        "\
acc = 0
for k in range(10):
    acc = acc + k
",
    ];
    for source in sources {
        let (hir, _, diags) = lower_source(source, &parse(source));
        assert!(diags.is_empty(), "{diags:?}");
        assert!(
            !has_global_set(&hir.functions[0].body),
            "loop locals must not be mirrored: {:?}",
            hir.functions[0].body
        );
    }
}

#[test]
fn module_assignment_read_by_a_method_mirrors_before_the_class_definition() {
    // The method body is lowered before the module body, so the name it reads
    // must be known before the assignment is lowered; the assignment then
    // stores the value its method reads.
    let source = "value = 41\nclass C:\n    def get(self):\n        return value\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = &hir.functions[0].body;
    let HStmtKind::Assign { target, .. } = &body[0].kind else {
        panic!("expected the local assignment, got {:?}", body[0].kind);
    };
    let HStmtKind::GlobalSet {
        module,
        name,
        value,
    } = &body[1].kind
    else {
        panic!("expected a global store, got {:?}", body[1].kind);
    };
    assert_eq!(module, "__main__");
    assert_eq!(name, "value");
    assert!(matches!(&value.kind, HExprKind::Local(local) if *local == *target));

    let get = function(&hir, "C.get");
    let HStmtKind::Return(Some(value)) = &get.body[0].kind else {
        panic!("expected a return, got {:?}", get.body[0].kind);
    };
    assert!(
        matches!(&value.kind, HExprKind::GlobalGet { module, name } if module == "__main__" && name == "value"),
        "the method must read the mirrored global, got {:?}",
        value.kind
    );
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn module_class_read_by_a_function_mirrors_with_selective_entry_mirroring() {
    // `lower_source` mirrors selectively, like the entry module. A class read
    // by a function of the same unit must still be mirrored, or the function
    // would read an undefined global.
    let source = "class C:\n    pass\n\ndef make():\n    return C()\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = &hir.functions[0].body;
    assert_eq!(
        body.len(),
        2,
        "the class must be mirrored exactly once: {body:?}"
    );
    assert!(
        matches!(body[0].kind, HStmtKind::ClassDef { .. }),
        "expected a class definition, got {:?}",
        body[0].kind
    );
    let HStmtKind::GlobalSet {
        module,
        name,
        value,
    } = &body[1].kind
    else {
        panic!("expected the class to be mirrored, got {:?}", body[1].kind);
    };
    assert_eq!(module, "__main__");
    assert_eq!(name, "C");
    assert!(matches!(&value.kind, HExprKind::Local(_)), "{value:?}");

    let make = function(&hir, "make");
    let HStmtKind::Return(Some(value)) = &make.body[0].kind else {
        panic!("expected a return, got {:?}", make.body[0].kind);
    };
    let HExprKind::IndirectCall { callee, .. } = &value.kind else {
        panic!("expected an indirect call, got {:?}", value.kind);
    };
    assert!(
        matches!(&callee.kind, HExprKind::GlobalGet { module, name } if module == "__main__" && name == "C"),
        "the constructor must read the mirrored class, got {:?}",
        callee.kind
    );
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn local_bindings_shadow_module_level_names() {
    let source = "value = 41\ndef f(value):\n    return value\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = &function(&hir, "f").body;
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    assert!(
        matches!(value.kind, HExprKind::Local(_)),
        "a parameter must shadow the module global, got {:?}",
        value.kind
    );
}

#[test]
fn conditional_expressions_lower_to_a_lazy_ifexp() {
    let source = "x = 1 if True else 2\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::IfExp { test, then, orelse } = &assigned(&hir, "x").kind else {
        panic!(
            "expected a conditional expression, got {:?}",
            assigned(&hir, "x")
        );
    };
    assert!(matches!(&test.kind, HExprKind::Const(Const::Bool(true))));
    assert!(matches!(&then.kind, HExprKind::Const(Const::Int(1))));
    assert!(matches!(&orelse.kind, HExprKind::Const(Const::Int(2))));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn identity_comparisons_lower_to_is_operators() {
    let source = "def f(x):\n    if x is None:\n        return True\n    return x is not None\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = &function(&hir, "f").body;
    let HStmtKind::If { cond, .. } = &body[0].kind else {
        panic!("expected an if statement, got {:?}", body[0].kind);
    };
    match &cond.kind {
        HExprKind::Bin { op, .. } => assert_eq!(*op, BinOp::Is),
        other => panic!("expected an identity comparison, got {other:?}"),
    }
    let HStmtKind::Return(Some(value)) = &body[1].kind else {
        panic!("expected a return statement, got {:?}", body[1].kind);
    };
    match &value.kind {
        HExprKind::Bin { op, .. } => assert_eq!(*op, BinOp::IsNot),
        other => panic!("expected an identity comparison, got {other:?}"),
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn dataclass_fields_synthesize_an_init() {
    let source = "\
from dataclasses import dataclass
@dataclass
class Bench:
    key: str
    quick: list
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ClassDef { name, methods, .. } = &hir.functions[0].body[0].kind else {
        panic!(
            "expected a class definition, got {:?}",
            hir.functions[0].body[0].kind
        );
    };
    assert_eq!(name, "Bench");
    let init_id = methods
        .iter()
        .find(|(method, _, _)| method.as_str() == "__init__")
        .map(|(_, id, _)| *id)
        .expect("a dataclass must synthesize __init__");

    let init = &hir.functions[init_id as usize];
    assert_eq!(init.name, "Bench.__init__");
    assert_eq!(init.capture_count, 0);
    assert_eq!(
        init.params
            .iter()
            .map(|param| param.name.as_str())
            .collect::<Vec<_>>(),
        vec!["self", "key", "quick"]
    );
    // self is local 0, then one local per field, in declaration order.
    assert_eq!(init.body.len(), 3, "{:?}", init.body);
    for (index, field) in ["key", "quick"].iter().enumerate() {
        let HStmtKind::SetAttr {
            object,
            name,
            value,
        } = &init.body[index].kind
        else {
            panic!(
                "expected a field assignment, got {:?}",
                init.body[index].kind
            );
        };
        assert_eq!(name.as_str(), *field);
        assert!(matches!(&object.kind, HExprKind::Local(0)));
        assert!(
            matches!(&value.kind, HExprKind::Local(local) if *local as usize == index + 1),
            "field {field} must read local {}",
            index + 1
        );
    }
    assert!(matches!(&init.body[2].kind, HStmtKind::Return(None)));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn dataclass_literal_defaults_and_field_factories() {
    let source = "\
from dataclasses import dataclass, field
@dataclass
class Bench:
    key: str
    items: list = field(default_factory=list)
    fallback: int = field(default=5)
    count: int = 0
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let init = function(&hir, "Bench.__init__");
    assert_eq!(
        init.params
            .iter()
            .map(|param| param.name.as_str())
            .collect::<Vec<_>>(),
        vec!["self", "key", "items", "fallback", "count"]
    );
    // Required fields have no default; `field(...)` parameters default to
    // `None` and let the body resolve the real value; literal defaults are
    // recorded on the parameter.
    assert_eq!(init.params[1].default, None);
    assert_eq!(init.params[2].default, Some(Const::None));
    assert_eq!(init.params[3].default, Some(Const::None));
    assert_eq!(init.params[4].default, Some(Const::Int(0)));

    assert_eq!(init.body.len(), 5, "{:?}", init.body);
    let HStmtKind::SetAttr { name, value, .. } = &init.body[0].kind else {
        panic!("expected the key assignment, got {:?}", init.body[0].kind);
    };
    assert_eq!(name, "key");
    assert!(matches!(&value.kind, HExprKind::Local(1)));

    // `field(default_factory=list)` checks for `None` and calls the factory.
    let HStmtKind::SetAttr { name, value, .. } = &init.body[1].kind else {
        panic!("expected the items assignment, got {:?}", init.body[1].kind);
    };
    assert_eq!(name, "items");
    let HExprKind::IfExp { test, then, orelse } = &value.kind else {
        panic!("expected a default fallback, got {:?}", value.kind);
    };
    assert!(matches!(&test.kind, HExprKind::Bin { op: BinOp::Is, .. }));
    assert!(
        matches!(&then.kind, HExprKind::IndirectCall { callee, args, .. }
            if args.is_empty() && matches!(&callee.kind, HExprKind::BuiltinRef(Builtin::List))),
        "the factory must be called with no arguments: {:?}",
        then.kind
    );
    assert!(matches!(&orelse.kind, HExprKind::Local(2)));

    // `field(default=5)` assigns the supplied default when the arg is `None`.
    let HStmtKind::SetAttr { value, .. } = &init.body[2].kind else {
        panic!(
            "expected the fallback assignment, got {:?}",
            init.body[2].kind
        );
    };
    let HExprKind::IfExp { then, orelse, .. } = &value.kind else {
        panic!("expected a default fallback, got {:?}", value.kind);
    };
    assert!(matches!(&then.kind, HExprKind::Const(Const::Int(5))));
    assert!(matches!(&orelse.kind, HExprKind::Local(3)));

    // A literal default is simply stored.
    let HStmtKind::SetAttr { name, value, .. } = &init.body[3].kind else {
        panic!("expected the count assignment, got {:?}", init.body[3].kind);
    };
    assert_eq!(name, "count");
    assert!(matches!(&value.kind, HExprKind::Local(4)));
    assert!(matches!(&init.body[4].kind, HStmtKind::Return(None)));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn field_default_and_factory_together_are_rejected() {
    let source = "\
from dataclasses import dataclass, field
@dataclass
class Bad:
    value: int = field(default=1, default_factory=int)
";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0].message,
        "field() cannot specify both default and default_factory"
    );
}

#[test]
fn same_unit_class_constructor_keywords_are_reordered() {
    let source = "\
class Point:
    def __init__(self, x, y=2):
        self.x = x
        self.y = y
p = Point(y=3, x=1)
q = Point(4)
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::IndirectCall { callee, args, .. } = &assigned(&hir, "p").kind else {
        panic!(
            "expected an indirect class call, got {:?}",
            assigned(&hir, "p").kind
        );
    };
    assert!(matches!(&callee.kind, HExprKind::Local(_)));
    assert_eq!(args.len(), 2);
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::Int(1))));
    assert!(matches!(&args[1].kind, HExprKind::Const(Const::Int(3))));

    // Omitted parameters receive the literal default.
    let HExprKind::IndirectCall { args, .. } = &assigned(&hir, "q").kind else {
        panic!(
            "expected an indirect class call, got {:?}",
            assigned(&hir, "q").kind
        );
    };
    assert_eq!(args.len(), 2);
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::Int(4))));
    assert!(matches!(&args[1].kind, HExprKind::Const(Const::Int(2))));
}

#[test]
fn console_thread_locals_style_dataclass_constructs_with_keywords() {
    let source = "\
from dataclasses import dataclass, field
import threading

class ThemeStack:
    def __init__(self, themes=None):
        self.themes = themes

@dataclass
class ConsoleThreadLocals(threading.local):
    theme_stack: ThemeStack
    buffer: list = field(default_factory=list)
    buffer_index: int = 0

class Console:
    def __init__(self, theme):
        self._thread_locals = ConsoleThreadLocals(
            theme_stack=ThemeStack(themes=[theme])
        )
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HStmtKind::ClassDef { base, .. } = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::ClassDef { name, .. } if name == "ConsoleThreadLocals" => Some(&stmt.kind),
            _ => None,
        })
        .expect("ConsoleThreadLocals class")
    else {
        panic!("expected the ConsoleThreadLocals class");
    };
    assert!(
        matches!(
            base.as_ref().map(|base| &base.kind),
            Some(HExprKind::ModuleAttr { module, name })
                if module == "threading" && name == "local"
        ),
        "threading.local must stay the base class"
    );

    let init = function(&hir, "ConsoleThreadLocals.__init__");
    assert_eq!(
        init.params
            .iter()
            .map(|param| param.name.as_str())
            .collect::<Vec<_>>(),
        vec!["self", "theme_stack", "buffer", "buffer_index"]
    );
    assert_eq!(init.params[1].default, None);
    assert_eq!(init.params[2].default, Some(Const::None));
    assert_eq!(init.params[3].default, Some(Const::Int(0)));

    let console_init = function(&hir, "Console.__init__");
    let HStmtKind::SetAttr { name, value, .. } = &console_init.body[0].kind else {
        panic!(
            "expected the thread-locals assignment, got {:?}",
            console_init.body[0].kind
        );
    };
    assert_eq!(name, "_thread_locals");
    let HExprKind::IndirectCall { callee, args, .. } = &value.kind else {
        panic!("expected a class call, got {:?}", value.kind);
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::GlobalGet { name, .. } if name == "ConsoleThreadLocals"
    ));
    assert_eq!(args.len(), 3, "{args:?}");
    // The keyword value is the nested `ThemeStack(themes=[theme])` call.
    let HExprKind::IndirectCall {
        callee,
        args: theme_args,
        ..
    } = &args[0].kind
    else {
        panic!("expected the ThemeStack call, got {:?}", args[0].kind);
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::GlobalGet { name, .. } if name == "ThemeStack"
    ));
    assert_eq!(theme_args.len(), 1);
    assert!(matches!(&theme_args[0].kind, HExprKind::List(items) if items.len() == 1));
    // Omitted factory and literal fields become `None` placeholders; the
    // generated body fills the real values.
    assert!(matches!(&args[1].kind, HExprKind::Const(Const::None)));
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::Int(0))));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn new_builtins_are_recognised_and_called_directly() {
    let source = "\
a = max
b = min
c = round
d = format
e = isinstance
f = max(1, 2)
g = round(1.5)
h = format(1, \"d\")
i = isinstance(1, int)
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for (name, expected) in [
        ("a", Builtin::Max),
        ("b", Builtin::Min),
        ("c", Builtin::Round),
        ("d", Builtin::Format),
        ("e", Builtin::Isinstance),
    ] {
        match &assigned(&hir, name).kind {
            HExprKind::BuiltinRef(builtin) => assert_eq!(*builtin, expected, "{name}"),
            other => panic!("expected a builtin reference for {name}, got {other:?}"),
        }
    }
    for (name, expected, argc) in [
        ("f", Builtin::Max, 3),
        ("g", Builtin::Round, 1),
        ("h", Builtin::Format, 2),
        ("i", Builtin::Isinstance, 2),
    ] {
        match &assigned(&hir, name).kind {
            HExprKind::Call {
                callee: Callee::Builtin(builtin),
                args,
            } => {
                assert_eq!(*builtin, expected, "{name}");
                assert_eq!(args.len(), argc, "{name}");
            }
            other => panic!("expected a direct builtin call for {name}, got {other:?}"),
        }
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn literal_defaults_fill_omitted_direct_call_arguments() {
    let source = "\
def bar(f, width=18):
    return width
a = bar(1)
b = bar(1, 2)
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let bar = hir
        .functions
        .iter()
        .find(|function| function.name == "bar")
        .expect("function bar");
    assert_eq!(bar.params[1].default, Some(Const::Int(18)));

    for (name, expected) in [("a", Const::Int(18)), ("b", Const::Int(2))] {
        let HExprKind::Call {
            callee: Callee::Function(_),
            args,
        } = &assigned(&hir, name).kind
        else {
            panic!("expected a direct call for {name}");
        };
        assert_eq!(args.len(), 2, "{name}");
        assert!(
            matches!(&args[1].kind, HExprKind::Const(value) if *value == expected),
            "{name}: {:?}",
            args[1]
        );
    }
}

#[test]
fn non_literal_defaults_are_accepted_with_none_metadata() {
    // A non-literal default (here a list literal) is accepted so library
    // signatures compile; parameter metadata records `None` (documented
    // limitation) and no diagnostic is produced.
    let source = "def f(x=[1]):\n    return x\ny = f()\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|function| function.name == "f")
        .expect("function f");
    assert_eq!(function.params[0].default, Some(Const::None));
}

#[test]
fn class_attribute_defaults_are_accepted() {
    let source = "\
def render(text=\"\", *, color_system=ColorSystem.TRUECOLOR):
    return text
x = render(\"a\")
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|function| function.name == "render")
        .expect("function render");
    assert_eq!(function.params[0].default, Some(Const::Str(String::new())));
    assert_eq!(function.params[1].default, Some(Const::None));
}

#[test]
fn keywords_bind_to_user_function_parameters() {
    let source = "\
def md_table(heads, rows, left=1):
    return left
x = md_table(['h'], [], left=2)
y = md_table(rows=[], heads=[])
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for (name, expected_len) in [("x", 3usize), ("y", 3)] {
        let HExprKind::Call {
            callee: Callee::Function(_),
            args,
        } = &assigned(&hir, name).kind
        else {
            panic!("expected a direct call for {name}");
        };
        assert_eq!(args.len(), expected_len, "{name}");
    }
    let HExprKind::Call { args, .. } = &assigned(&hir, "x").kind else {
        unreachable!()
    };
    assert!(matches!(&args[0].kind, HExprKind::List(_)), "{:?}", args[0]);
    assert!(matches!(&args[1].kind, HExprKind::List(_)), "{:?}", args[1]);
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::Int(2))));

    // Positional arguments bind before keywords, so `y` keeps `left`'s default.
    let HExprKind::Call { args, .. } = &assigned(&hir, "y").kind else {
        unreachable!()
    };
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::Int(1))));
}

#[test]
fn keyword_binding_diagnostics_are_specific() {
    let cases = [
        (
            "def f(a):\n    return a\nx = f(nope=1)\n",
            "f() got an unexpected keyword argument 'nope'",
        ),
        (
            "def f(a, b):\n    return a\nx = f(1, a=2)\n",
            "f() got multiple values for argument 'a'",
        ),
        (
            "def f(a, b):\n    return a\nx = f(1)\n",
            "f() missing required argument 'b'",
        ),
        (
            "def f(a):\n    return a\nx = f(1, 2)\n",
            "f() takes 1 positional argument(s) but 2 were given",
        ),
    ];
    for (source, expected) in cases {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert_eq!(diags.len(), 1, "{source}: {diags:?}");
        assert_eq!(diags[0].code, codes::CALL, "{source}");
        assert!(
            diags[0].message.contains(expected),
            "{}: {diags:?}",
            diags[0].message
        );
    }
}

#[test]
fn min_and_max_pass_the_key_as_the_trailing_slot() {
    let source = "\
pairs = [(1, 2)]
best = min(pairs, key=lambda p: p[1])
worst = max(pairs)
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Min),
        args,
    } = &assigned(&hir, "best").kind
    else {
        panic!("expected a min() call");
    };
    assert_eq!(args.len(), 2);
    assert!(
        matches!(&args[1].kind, HExprKind::FunctionRef(_)),
        "{:?}",
        args[1]
    );

    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Max),
        args,
    } = &assigned(&hir, "worst").kind
    else {
        panic!("expected a max() call");
    };
    assert_eq!(args.len(), 2);
    assert!(matches!(&args[1].kind, HExprKind::Const(Const::None)));
}

#[test]
fn sorted_materialises_the_key_and_reverse_slots() {
    let source = "\
words = ['b', 'a']
plain = sorted(words)
by_len = sorted(words, key=len)
descending = sorted(words, reverse=True)
positional = sorted(words, None, True)
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Sorted),
        args,
    } = &assigned(&hir, "plain").kind
    else {
        panic!("expected a sorted() call");
    };
    assert_eq!(args.len(), 3);
    assert!(matches!(&args[1].kind, HExprKind::Const(Const::None)));
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::None)));

    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Sorted),
        args,
    } = &assigned(&hir, "by_len").kind
    else {
        panic!("expected a sorted() call");
    };
    assert_eq!(args.len(), 3);
    assert!(matches!(&args[1].kind, HExprKind::BuiltinRef(Builtin::Len)));
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::None)));

    // Both the keyword and the positional form leave `True` in the reverse
    // slot.
    for name in ["descending", "positional"] {
        let HExprKind::Call {
            callee: Callee::Builtin(Builtin::Sorted),
            args,
        } = &assigned(&hir, name).kind
        else {
            panic!("expected a sorted() call for {name}");
        };
        assert_eq!(args.len(), 3, "{name}");
        assert!(
            matches!(&args[2].kind, HExprKind::Const(Const::Bool(true))),
            "{name}: {:?}",
            args[2]
        );
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn sorted_rejects_conflicting_or_unsupported_keywords() {
    for (source, expected) in [
        (
            "x = sorted([1], 2, key=len)\n",
            "sorted() got multiple values for argument 'key'",
        ),
        (
            "x = sorted([1], None, True, reverse=True)\n",
            "sorted() got multiple values for argument 'reverse'",
        ),
        (
            "x = sorted([1], nope=1)\n",
            "sorted() does not accept the keyword argument 'nope'",
        ),
        ("x = sorted()\n", "sorted() expects at least 1 argument"),
        (
            "x = sorted([1], None, True, 4)\n",
            "sorted() takes at most 3 arguments (4 given)",
        ),
    ] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert_eq!(diags.len(), 1, "{source}: {diags:?}");
        assert!(
            diags[0].message.contains(expected),
            "{}: {diags:?}",
            diags[0].message
        );
    }
}

#[test]
fn list_displays_expand_starred_operands_as_concatenation() {
    let source = "xs = [1, 2]\ny = ['a', *xs, 'b']\nz = [*xs]\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for name in ["y", "z"] {
        let HExprKind::Bin { op: BinOp::Add, .. } = &assigned(&hir, name).kind else {
            panic!("expected list concatenation for {name}");
        };
        assert_eq!(assigned(&hir, name).ty, Ty::Concrete(Type::List));
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn dict_displays_merge_unpacked_mappings_in_order() {
    let source = "\
h = {'a': 1}
m = {**h, 'benchmark': 2}
n = {**m, **h}
o = {**h}
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for name in ["m", "n", "o"] {
        let HExprKind::Call {
            callee: Callee::Builtin(Builtin::DictMerge),
            args,
        } = &assigned(&hir, name).kind
        else {
            panic!("expected a dict merge for {name}");
        };
        assert_eq!(args.len(), 2, "{name}");
    }
    // A display that starts with `**` merges onto an empty dict, so the
    // result is a copy rather than an alias.
    let HExprKind::Call { args, .. } = &assigned(&hir, "o").kind else {
        unreachable!()
    };
    assert!(
        matches!(&args[0].kind, HExprKind::Dict(pairs) if pairs.is_empty()),
        "{:?}",
        args[0]
    );
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn method_calls_support_one_starred_operand() {
    let source = "\
def f(t, cells, style):
    return t.add_row(*cells, style=style)
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|function| function.name == "f")
        .expect("function f");
    let HStmtKind::Return(Some(value)) = &function.body[0].kind else {
        panic!("expected a return statement");
    };
    let HExprKind::MethodCall {
        object,
        method,
        args,
        star,
        keywords,
    } = &value.kind
    else {
        panic!("expected a method call");
    };
    assert!(matches!(&object.kind, HExprKind::Local(_)));
    assert_eq!(method, "add_row");
    assert!(args.is_empty());
    assert!(matches!(
        star.as_deref().map(|star| &star.kind),
        Some(HExprKind::Local(_))
    ));
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "style");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());

    for (source, expected) in [
        (
            "def f(t, a, b):\n    return t.add_row(*a, *b)\n",
            "only one starred argument is supported",
        ),
        (
            "def f(t, a):\n    return t.add_row(1, *a)\n",
            "a starred argument cannot be combined with positional arguments",
        ),
    ] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert_eq!(diags.len(), 1, "{source}: {diags:?}");
        assert!(diags[0].message.contains(expected), "{diags:?}");
    }
}

#[test]
fn lambdas_may_call_builtins_and_named_functions() {
    let source = "\
def helper(x):
    return x
esc = lambda s: helper(s) + str(s).replace('|', '\\\\|')
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "esc").kind,
        HExprKind::FunctionRef(_)
    ));

    // Enclosing locals are still a closure and stay rejected.
    let source = "def outer():\n    x = 1\n    return lambda: x + 1\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|diag| diag.message.contains("closures are not supported yet")),
        "{diags:?}"
    );
}

#[test]
fn all_builtin_is_available() {
    let source = "flags = [1, 2]\nok = all(flags)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::All),
        args,
    } = &assigned(&hir, "ok").kind
    else {
        panic!("expected an all() call");
    };
    assert_eq!(args.len(), 1);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    assert!(program.functions[0].body.iter().any(|stmt| matches!(
        &stmt.kind,
        pyx_ir::StmtKind::Assign { value, .. }
            if matches!(
                &value.kind,
                pyx_ir::ExprKind::Call {
                    callee: Callee::Builtin(Builtin::All),
                    ..
                }
            )
    )));
}

#[test]
fn repr_lowers_to_the_builtin_call() {
    let source = "x = repr('a')\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Repr),
        args,
    } = &assigned(&hir, "x").kind
    else {
        panic!("expected a repr() call, got {:?}", assigned(&hir, "x").kind);
    };
    assert_eq!(args.len(), 1);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let x = program.functions[0]
        .locals
        .iter()
        .find(|local| local.name == "x")
        .expect("x local");
    assert_eq!(x.ty, Type::Str);
}

#[test]
fn starred_function_calls_lower_to_indirect_calls() {
    let source = "def f(a, b):\n    return a + b\n\
                  xs = (1, 2)\n\
                  x = f(*xs)\n\
                  y = f(1, *xs)\n\
                  z = print(*xs)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::IndirectCall { callee, args, star } = &assigned(&hir, "x").kind else {
        panic!(
            "expected a starred indirect call, got {:?}",
            assigned(&hir, "x").kind
        );
    };
    assert!(
        matches!(
            &callee.kind,
            HExprKind::Local(_) | HExprKind::FunctionRef(_)
        ),
        "unexpected callee: {callee:?}"
    );
    assert!(args.is_empty());
    assert!(star.is_some());

    let HExprKind::IndirectCall { args, star, .. } = &assigned(&hir, "y").kind else {
        panic!(
            "expected a starred indirect call, got {:?}",
            assigned(&hir, "y").kind
        );
    };
    assert_eq!(args.len(), 1);
    assert!(star.is_some());

    let HExprKind::IndirectCall { callee, args, star } = &assigned(&hir, "z").kind else {
        panic!(
            "expected a starred indirect call, got {:?}",
            assigned(&hir, "z").kind
        );
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::BuiltinRef(Builtin::Print)
    ));
    assert!(args.is_empty());
    assert!(star.is_some());

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    assert!(program
        .functions
        .iter()
        .any(|function| function.body.iter().any(|stmt| matches!(
            &stmt.kind,
            pyx_ir::StmtKind::Assign { value, .. }
                if matches!(
                    &value.kind,
                    pyx_ir::ExprKind::IndirectCall { star: Some(_), .. }
                )
        ))));
}

#[test]
fn starred_module_function_calls_use_the_general_abi() {
    let source = "import os\nparts = ('a', 'b')\nx = os.path.join(*parts)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::IndirectCall { callee, args, star } = &assigned(&hir, "x").kind else {
        panic!(
            "expected a starred indirect call, got {:?}",
            assigned(&hir, "x").kind
        );
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::ModuleAttr { module, name }
            if module == "os.path" && name == "join"
    ));
    assert!(args.is_empty());
    assert!(star.is_some());
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn starred_calls_diagnose_multiple_or_trailing_arguments() {
    let source = "def f(*args):\n    return args\n\
                  xs = (1, 2)\n\
                  a = f(*xs, *xs)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|diag| diag.message.contains("only one starred argument")),
        "{diags:?}"
    );

    let source = "def f(*args):\n    return args\n\
                  xs = (1, 2)\n\
                  a = f(*xs, 3)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|diag| diag.message.contains("cannot follow a starred argument")),
        "{diags:?}"
    );
}

#[test]
fn bitwise_operators_lower_and_typecheck() {
    let source = "a = 6 & 3\nb = 6 | 3\nc = 6 ^ 3\nd = 1 << 4\ne = 8 >> 1\nf = True & False\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for (name, expected) in [
        ("a", BinOp::BitAnd),
        ("b", BinOp::BitOr),
        ("c", BinOp::BitXor),
        ("d", BinOp::Shl),
        ("e", BinOp::Shr),
        ("f", BinOp::BitAnd),
    ] {
        let HExprKind::Bin { op, .. } = &assigned(&hir, name).kind else {
            panic!("expected a binary expression for {name}");
        };
        assert_eq!(*op, expected);
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    for name in ["a", "b", "c", "d", "e"] {
        let local = program.functions[0]
            .locals
            .iter()
            .find(|local| local.name == name)
            .unwrap_or_else(|| panic!("missing local {name}"));
        assert_eq!(local.ty, Type::Int, "wrong type for {name}");
    }
}

#[test]
fn tuple_and_hasattr_builtins_lower_and_typecheck() {
    let source =
        "t = tuple()\nu = tuple('ab')\nr = hasattr(t, 'count')\nv = (\n    1,\n)\nw = tuple(v)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::Call { callee, args } = &assigned(&hir, "t").kind else {
        panic!("expected a tuple() call");
    };
    assert_eq!(*callee, Callee::Builtin(Builtin::Tuple));
    assert!(args.is_empty());

    let HExprKind::Call { callee, args } = &assigned(&hir, "u").kind else {
        panic!("expected a tuple(iterable) call");
    };
    assert_eq!(*callee, Callee::Builtin(Builtin::Tuple));
    assert_eq!(args.len(), 1);

    let HExprKind::Call { callee, args } = &assigned(&hir, "r").kind else {
        panic!("expected a hasattr() call");
    };
    assert_eq!(*callee, Callee::Builtin(Builtin::Hasattr));
    assert_eq!(args.len(), 2);

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let r = program.functions[0]
        .locals
        .iter()
        .find(|local| local.name == "r")
        .expect("missing local r");
    assert_eq!(r.ty, Type::Bool);
}

#[test]
fn starred_unpack_target_binds_the_remaining_elements() {
    let source = "key, value, *default = (1, 2, 3, 4)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let locals = &hir.functions[0].locals;
    let default = locals
        .iter()
        .position(|local| local.name == "default")
        .expect("starred target has a local slot");
    let unpack = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Unpack { targets, .. } => Some(targets),
            _ => None,
        })
        .expect("unpack statement");
    assert_eq!(unpack.len(), 3);
    assert!(matches!(unpack[0], HUnpackTarget::Name(_)));
    assert!(matches!(unpack[1], HUnpackTarget::Name(_)));
    let HUnpackTarget::Star(inner) = &unpack[2] else {
        panic!("expected a starred target, got {:?}", unpack[2]);
    };
    assert!(matches!(**inner, HUnpackTarget::Name(id) if id == default as LocalId));

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let emitted = program.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            pyx_ir::StmtKind::Unpack { targets, .. } => Some(targets),
            _ => None,
        })
        .expect("emitted unpack");
    assert!(matches!(
        emitted[2],
        pyx_ir::UnpackTarget::Star(ref inner)
            if matches!(**inner, pyx_ir::UnpackTarget::Name(_))
    ));
}

#[test]
fn nested_starred_unpack_targets_are_rejected() {
    let source = "(a, *b), c = ((1, 2), 3)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|diag| diag.message.contains("starred unpacking cannot appear")),
        "{diags:?}"
    );

    let source = "*a, *b = (1, 2)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|diag| diag.message.contains("multiple starred unpacking targets")),
        "{diags:?}"
    );
}

#[test]
fn functools_partial_keeps_dynamic_keyword_names() {
    let source = "from functools import partial
def f(cls, angular=None):
    return cls
p = partial(f, angular=True)
q = partial(func=f, angular=False)
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::ModuleCall {
        module,
        function,
        args,
        keywords,
    } = &assigned(&hir, "p").kind
    else {
        panic!("expected a module call");
    };
    assert_eq!(
        (module.as_str(), function.as_str()),
        ("functools", "partial")
    );
    assert_eq!(args.len(), 1);
    assert!(matches!(args[0].kind, HExprKind::FunctionRef(_)));
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "angular");

    // `func=` names the fixed leading parameter, so it takes the positional
    // slot instead of becoming a forwarded keyword.
    let HExprKind::ModuleCall { args, keywords, .. } = &assigned(&hir, "q").kind else {
        panic!("expected a module call");
    };
    assert_eq!(args.len(), 1);
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "angular");

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn user_exception_raise_lowers_to_runtime_error_with_the_class_name() {
    let source = "class ReprError(Exception):\n\
                  \x20   pass\n\
                  \n\
                  def fail():\n\
                  \x20   raise ReprError('boom') from None\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let raise = hir
        .functions
        .iter()
        .flat_map(|function| function.body.iter())
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Raise { exc, msg, cause } => Some((*exc, msg, cause)),
            _ => None,
        })
        .expect("raise statement");
    assert_eq!(raise.0, Some(ExcKind::RuntimeError));
    assert!(raise.2.is_some(), "the `from None` cause must be preserved");
    let HExprKind::Bin { op, lhs, rhs } = &raise.1.as_ref().expect("message").kind else {
        panic!("expected the prefixed message concatenation");
    };
    assert_eq!(*op, BinOp::Add);
    assert!(matches!(
        &lhs.kind,
        HExprKind::Const(Const::Str(prefix)) if prefix == "ReprError: "
    ));
    assert!(matches!(
        &rhs.kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Str),
            ..
        }
    ));

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn invert_lowers_to_the_un_invert_operator() {
    let source = "x = 3\na = ~x\nb = ~5\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        matches!(
            &assigned(&hir, "a").kind,
            HExprKind::Un {
                op: pyx_ir::UnOp::Invert,
                ..
            }
        ),
        "{:?}",
        assigned(&hir, "a").kind
    );
    // A literal operand folds at compile time (`~5 == -6`).
    assert!(matches!(
        &assigned(&hir, "b").kind,
        HExprKind::Const(Const::Int(-6))
    ));
}

#[test]
fn not_implemented_lowers_to_the_reserved_constant() {
    let source = "x = NotImplemented\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "x").kind,
        HExprKind::Const(Const::NotImplemented)
    ));
}

#[test]
fn dotted_raise_lowers_to_the_runtime_error_fallback() {
    let source = "def fail():\n    raise errors.StyleSyntaxError('bad') from None\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let raise = hir
        .functions
        .iter()
        .flat_map(|function| function.body.iter())
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Raise { exc, msg, cause } => Some((*exc, msg, cause)),
            _ => None,
        })
        .expect("raise statement");
    assert_eq!(raise.0, Some(ExcKind::RuntimeError));
    assert!(raise.2.is_some(), "the `from None` cause must be preserved");
    let HExprKind::Bin { op, lhs, .. } = &raise.1.as_ref().expect("message").kind else {
        panic!("expected the prefixed message concatenation");
    };
    assert_eq!(*op, BinOp::Add);
    assert!(matches!(
        &lhs.kind,
        HExprKind::Const(Const::Str(prefix)) if prefix == "errors.StyleSyntaxError: "
    ));
}

#[test]
fn bare_dotted_raise_uses_the_runtime_error_fallback() {
    let source = "raise errors.StyleSyntaxError\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Raise { exc, msg, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a raise statement");
    };
    assert_eq!(*exc, Some(ExcKind::RuntimeError));
    assert!(matches!(
        &msg.as_ref().expect("message").kind,
        HExprKind::Const(Const::Str(name)) if name == "errors.StyleSyntaxError"
    ));
}

#[test]
fn user_and_dotted_handlers_approximate_runtime_error() {
    let source = "\
try:
    pass
except errors.StyleSyntaxError:
    pass
except ColorParseError:
    pass
except ValueError:
    pass
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Try { handlers, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try statement");
    };
    let kinds: Vec<_> = handlers.iter().map(|handler| handler.exc).collect();
    assert_eq!(
        kinds,
        vec![
            Some(ExcKind::RuntimeError),
            Some(ExcKind::RuntimeError),
            Some(ExcKind::ValueError)
        ]
    );
}

#[test]
fn sys_streams_and_getrandbits_lower_to_the_expected_nodes() {
    let source = "\
import sys
from random import getrandbits
n = getrandbits(24)
stream = sys.stderr
sys.stdout.write('x')
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "n").kind,
        HExprKind::ModuleCall { module, function, .. }
            if module == "random" && function == "getrandbits"
    ));
    assert!(matches!(
        &assigned(&hir, "stream").kind,
        HExprKind::ModuleAttr { module, name } if module == "sys" && name == "stderr"
    ));
    let written = hir.functions[0].body.iter().find_map(|stmt| {
        let HStmtKind::Expr(value) = &stmt.kind else {
            return None;
        };
        let HExprKind::MethodCall {
            object,
            method,
            args,
            ..
        } = &value.kind
        else {
            return None;
        };
        let HExprKind::ModuleAttr { module, name } = &object.kind else {
            return None;
        };
        Some((module.as_str(), name.as_str(), method.as_str(), args.len()))
    });
    assert_eq!(written, Some(("sys", "stdout", "write", 1)));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn ord_and_chr_lower_to_direct_builtin_calls() {
    let source = "a = ord('A')\nb = chr(65)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for (name, expected) in [("a", Builtin::Ord), ("b", Builtin::Chr)] {
        match &assigned(&hir, name).kind {
            HExprKind::Call {
                callee: Callee::Builtin(builtin),
                args,
            } => {
                assert_eq!(*builtin, expected);
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected a direct {name} builtin call, got {other:?}"),
        }
    }
}

#[test]
fn assignment_expressions_lower_to_named_expr_nodes() {
    let source = "if (n := len('abc')):\n    x = n\n\
                  while (line := 1):\n    break\n\
                  values = [y := v for v in [1, 2]]\n\
                  after = y\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = &hir.functions[0];
    let local_id = |name: &str| -> LocalId {
        function
            .locals
            .iter()
            .position(|local| local.name == name)
            .map(|index| index as LocalId)
            .unwrap_or_else(|| panic!("no local named '{name}'"))
    };

    let HStmtKind::If {
        cond, then_body, ..
    } = &function.body[0].kind
    else {
        panic!("expected an if statement");
    };
    let HExprKind::NamedExpr { target, value } = &cond.kind else {
        panic!("expected a walrus condition, got {:?}", cond.kind);
    };
    assert_eq!(*target, local_id("n"));
    assert!(matches!(
        &value.kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Len),
            ..
        }
    ));
    let HStmtKind::Assign { value, .. } = &then_body[0].kind else {
        panic!("expected an assignment in the if body");
    };
    assert!(matches!(&value.kind, HExprKind::Local(id) if *id == *target));

    let HStmtKind::While { cond, .. } = &function.body[1].kind else {
        panic!("expected a while statement");
    };
    let HExprKind::NamedExpr { target: line, .. } = &cond.kind else {
        panic!("expected a walrus while condition, got {:?}", cond.kind);
    };
    assert_eq!(*line, local_id("line"));

    let HExprKind::ListComp { element, .. } = &assigned(&hir, "values").kind else {
        panic!("expected a list comprehension");
    };
    let HExprKind::NamedExpr { target: y, value } = &element.kind else {
        panic!("expected a walrus element, got {:?}", element.kind);
    };
    // The walrus binds the enclosing scope, not the comprehension target.
    assert_eq!(function.locals[*y as usize].name, "y");
    assert!(matches!(
        &value.kind,
        HExprKind::Local(id) if function.locals[*id as usize].name.starts_with("$comp")
    ));
    assert!(matches!(
        &assigned(&hir, "after").kind,
        HExprKind::Local(id) if *id == *y
    ));
}

#[test]
fn assignment_expression_cannot_rebind_a_comprehension_target() {
    let source = "values = [(x := 1) for x in [1, 2]]\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]
            .message
            .contains("cannot rebind comprehension iteration variable 'x'"),
        "{diags:?}"
    );
}

#[test]
fn chained_comparisons_evaluate_each_middle_operand_once() {
    let source = "def f(a, b, c):\n    return a < b <= c\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|function| function.name == "f")
        .expect("function f");
    assert_eq!(function.locals[1].name, "b");
    let body = function_body(&hir, "f");
    let HStmtKind::Return(Some(expr)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    let HExprKind::BoolOp {
        op: pyx_ir::BoolOp::And,
        lhs: first,
        rhs: second,
    } = &expr.kind
    else {
        panic!("expected a short-circuit and, got {:?}", expr.kind);
    };
    let HExprKind::Bin {
        op: BinOp::Lt,
        rhs: middle,
        ..
    } = &first.kind
    else {
        panic!("expected the first comparison, got {:?}", first.kind);
    };
    let HExprKind::NamedExpr { target, value } = &middle.kind else {
        panic!(
            "expected the middle operand to be bound once, got {:?}",
            middle.kind
        );
    };
    assert!(function.locals[*target as usize].name.starts_with("$cmp"));
    assert!(matches!(&value.kind, HExprKind::Local(id) if *id == 1));
    let HExprKind::Bin {
        op: BinOp::Le,
        lhs: second_lhs,
        ..
    } = &second.kind
    else {
        panic!("expected the second comparison, got {:?}", second.kind);
    };
    assert!(matches!(&second_lhs.kind, HExprKind::Local(id) if id == target));
}

#[test]
fn long_comparison_chains_use_one_temp_per_middle_operand() {
    let source = "def f(a, b, c, d):\n    return a < b < c < d\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|function| function.name == "f")
        .expect("function f");
    let temps: Vec<&str> = function
        .locals
        .iter()
        .map(|local| local.name.as_str())
        .filter(|name| name.starts_with("$cmp"))
        .collect();
    assert_eq!(temps, vec!["$cmp0", "$cmp1"]);

    let body = function_body(&hir, "f");
    let HStmtKind::Return(Some(expr)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    let HExprKind::BoolOp {
        lhs: outer_lhs,
        rhs: third,
        ..
    } = &expr.kind
    else {
        panic!("expected a boolean and, got {:?}", expr.kind);
    };
    assert!(matches!(&outer_lhs.kind, HExprKind::BoolOp { .. }));
    assert!(matches!(&third.kind, HExprKind::Bin { op: BinOp::Lt, .. }));
}
