use super::*;
use rustpython_parser::Mode;

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
fn module_values_are_rejected() {
    let source = "import math\nm = math\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(diags[0].message.contains("module values"), "{diags:?}");
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
fn imports_inside_functions_are_rejected() {
    let source = "def f():\n    import math\n    return 1\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert_eq!(
        diags[0].message,
        "imports are only supported at module level in Phase 2.10"
    );
}

#[test]
fn imports_inside_blocks_are_rejected() {
    let source = "if True:\n    from math import sqrt\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0].message,
        "imports are only supported at module level in Phase 2.10"
    );
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
    let source = "assert True\n";
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
fn user_functions_still_reject_keywords() {
    let source = "def f(a):\n    return a\nx = f(a=1)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags.iter().any(|d| {
            d.code == codes::CALL
                && d.message
                    == "keyword arguments are not supported for user-defined functions and builtins"
        }),
        "{diags:?}"
    );
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
fn slice_assignment_is_rejected() {
    let source = "xs = [1, 2, 3]\nxs[0:1] = [9]\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].message, "slice assignment is not supported yet");
}

#[test]
fn augmented_slice_assignment_is_rejected() {
    let source = "xs = [1, 2, 3]\nxs[0:1] += [9]\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].message, "slice assignment is not supported yet");
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

    let HExprKind::IndirectCall { callee, args } = &assigned(&hir, "x").kind else {
        panic!("expected an indirect call");
    };
    assert!(matches!(&callee.kind, HExprKind::Local(_)));
    assert_eq!(args.len(), 1);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
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
        .map(|id| hir.functions[0].locals[*id as usize].name.as_str())
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
fn nested_unpack_target_is_rejected() {
    let source = "(a, (b, c)) = (1, (2, 3))\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].message, "nested unpacking is not supported yet");
}

#[test]
fn starred_unpack_target_is_rejected() {
    for source in ["a, *b = (1, 2, 3)\n", "for a, *b in [(1, 2)]:\n    pass\n"] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].message, "starred unpacking is not supported yet");
    }
}

#[test]
fn nested_for_unpack_target_is_rejected() {
    let source = "for (a, (b, c)) in [(1, (2, 3))]:\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].message, "nested unpacking is not supported yet");
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
        (*callee, args.len())
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
    for id in targets {
        assert!(hir.functions[0].locals[*id as usize]
            .name
            .starts_with("$comp"));
    }
    assert!(
        matches!(&element.kind, HExprKind::Local(id) if id == &targets[0]),
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
    for source in [
        "xs = [1 for a.b in [1]]\n",
        "xs = [1 for a, *b in [[1, 2]]]\n",
    ] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert_eq!(diags.len(), 1, "{source}: {diags:?}");
        assert_eq!(
            diags[0].message, "unsupported comprehension target",
            "{source}"
        );
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
fn builtin_named_functions_are_rejected() {
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
        let (_, _, diags) = lower_source(&source, &parse(&source));
        assert_eq!(diags.len(), 1, "{name}: {diags:?}");
        assert_eq!(diags[0].code, codes::DECL, "{name}");
        assert!(diags[0].message.contains("shadows"), "{name}");
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
fn bytes_builtin_name_cannot_be_shadowed_by_a_function() {
    let source = "def bytes(x):\n    return x\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("shadows a Phase 1 builtin"),
        "{:?}",
        diags[0].message
    );
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
    let HExprKind::IndirectCall { callee, args } = &c.kind else {
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
fn class_level_assignments_are_rejected() {
    let source = "class C:\n    x = 1\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("class body may only contain"),
        "{:?}",
        diags[0].message
    );
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
fn attribute_unpack_targets_are_rejected() {
    let source = "a.x, b.y = 1, 2\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        !diags.is_empty(),
        "attribute unpack targets must be rejected"
    );
    assert!(
        diags[0].message.contains("unpacking target must be a name"),
        "{:?}",
        diags[0].message
    );
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
fn yield_inside_an_expression_is_rejected() {
    for source in [
        "def g():\n    x = yield 1\n",
        "def g():\n    print((yield 1))\n",
    ] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert!(
            diags.iter().any(|d| {
                d.message == "yield expressions are not supported; use `yield value` as a statement"
                    && d.code == codes::UNSUPPORTED
            }),
            "{source}: {diags:?}"
        );
    }
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
fn yield_from_is_rejected() {
    let source = "def g():\n    yield from [1, 2]\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].message, "yield from is not supported yet");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
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
