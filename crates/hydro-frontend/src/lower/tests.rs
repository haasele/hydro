use super::*;
use rustpython_parser::template::TemplateString;
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
fn os_path_value_is_a_first_class_module() {
    let source = "import os\np = os.path\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "p").kind,
        HExprKind::ModuleRef { module } if module == "os.path"
    ));
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
fn module_level_try_import_binds_a_preloaded_submodule() {
    let source =
        "try:\n    from . import sub\nexcept ImportError:\n    pass\n\ndef get():\n    return sub.value()\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "pkg/__init__.py", &mut infer);
    lowerer.set_dunder_name("pkg");
    lowerer.set_package("pkg");
    lowerer.set_mirror_all_globals(true);
    lowerer.set_user_modules(HashMap::from([(
        "pkg.sub".to_string(),
        UserModule {
            name: "pkg.sub".to_string(),
            exports: HashMap::from([("value".to_string(), UserExport::Function(9))]),
            all: None,
            dynamic_globals: false,
            extension_stars: Vec::new(),
        },
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");

    // The function links the submodule export directly.
    let body = function_body(&hir, "get");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    assert!(
        matches!(
            &value.kind,
            HExprKind::Call {
                callee: Callee::Function(9),
                ..
            }
        ),
        "expected a linked submodule function, got {:?}",
        value.kind
    );

    // The try body mirrors the module binding so later code and importers can
    // read `pkg.sub` through the package module value.
    let bound = hir.functions[0].body.iter().any(|stmt| {
        matches!(
            &stmt.kind,
            HStmtKind::Try { body, .. }
                if body.iter().any(|inner| matches!(
                    &inner.kind,
                    HStmtKind::GlobalSet { name, .. } if name == "sub"
                ))
        )
    });
    assert!(
        bound,
        "the try body must bind the submodule: {:?}",
        hir.functions[0].body
    );
}

#[test]
fn module_level_conditional_imports_bind_top_level_and_defer_absent_modules() {
    // NumPy imports its `_core` package inside `if __NUMPY_SETUP__: ... else:`
    // and binds its public surface from that branch. Those statements execute
    // while the module initializes, so their bindings are module-level (not
    // function-local deferred stubs); a branch-specific module that does not
    // exist on this host stays a runtime import error instead of a compile
    // error.
    let source = "STATE = False\nif STATE:\n    from . import absent\nelse:\n    from . import core\n    from .core import value\n\ndef f():\n    return value()\n\ndef g():\n    return absent\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "pkg/__init__.py", &mut infer);
    lowerer.set_dunder_name("pkg");
    lowerer.set_package("pkg");
    lowerer.set_mirror_all_globals(true);
    lowerer.set_deferred_imports(std::collections::HashSet::from(["pkg.absent".to_string()]));
    lowerer.set_user_modules(HashMap::from([(
        "pkg.core".to_string(),
        UserModule {
            name: "pkg.core".to_string(),
            exports: HashMap::from([("value".to_string(), UserExport::Function(9))]),
            all: None,
            dynamic_globals: false,
            extension_stars: Vec::new(),
        },
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");

    // `core` is a module attribute of the package, bound by the `else` body.
    // Import statements sit inside the lowered `if`/`else` block, not at the
    // top level of the module body.
    fn binds_global(stmts: &[HStmt], expected: &str) -> bool {
        stmts.iter().any(|stmt| match &stmt.kind {
            HStmtKind::GlobalSet { name, .. } => name == expected,
            HStmtKind::If {
                then_body,
                else_body,
                ..
            } => binds_global(then_body, expected) || binds_global(else_body, expected),
            _ => false,
        })
    }
    assert!(
        binds_global(&hir.functions[0].body, "core"),
        "the else branch must mirror the imported module: {:?}",
        hir.functions[0].body
    );

    // The member import from the same branch links to `core.value` directly.
    // A linked user function with the generic call path lowers to `CallKw`
    // (empty args/keywords here); both shapes must target the linked id.
    let body = function_body(&hir, "f");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    let linked = match &value.kind {
        HExprKind::Call {
            callee: Callee::Function(9),
            ..
        } => true,
        HExprKind::CallKw {
            callee,
            args,
            keywords,
        } => {
            args.is_empty()
                && keywords.is_empty()
                && matches!(&callee.kind, HExprKind::FunctionRef(9))
        }
        _ => false,
    };
    assert!(
        linked,
        "expected the linked core.value call, got {:?}",
        value.kind
    );

    // The absent platform module is a late import error, not a compile error.
    let body = function_body(&hir, "g");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    assert!(
        matches!(
            &value.kind,
            HExprKind::DeferredImport { module, member }
                if module == "pkg" && member.as_deref() == Some("absent")
        ),
        "expected a deferred import, got {:?}",
        value.kind
    );
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
fn relative_import_outside_a_package_is_rejected() {
    let source = "from . import math\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{source}: {diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
}

/// Builds the linked module the loader hands the lowerer for `name`.
fn user_module(name: &str, exports: &[(&str, UserExport)]) -> UserModule {
    UserModule {
        name: name.to_string(),
        exports: exports
            .iter()
            .map(|(member, export)| (member.to_string(), *export))
            .collect(),
        all: None,
        dynamic_globals: false,
        extension_stars: Vec::new(),
    }
}

/// Like [`user_module`], for a module that installs names into its own
/// namespace at runtime (`globals()[key] = value`), so its export table is
/// incomplete.
fn dynamic_user_module(name: &str, exports: &[(&str, UserExport)]) -> UserModule {
    UserModule {
        dynamic_globals: true,
        ..user_module(name, exports)
    }
}

/// Like [`user_module`], for a module that star-imported the compiled
/// extension at `path` (directly or through another star import), so its
/// static export table cannot enumerate the extension namespace.
fn extension_star_user_module(
    name: &str,
    exports: &[(&str, UserExport)],
    path: &str,
) -> UserModule {
    UserModule {
        extension_stars: vec![path.to_string()],
        ..user_module(name, exports)
    }
}

/// Lowers `source` as the package submodule `pkg/sub.py` with `modules`
/// linked and `late` naming the modules that are still initializing.
fn lower_package_submodule<'a>(
    source: &'a str,
    infer: &'a mut Infer,
    modules: Vec<UserModule>,
    late: &[&str],
) -> Lowerer<'a> {
    let mut lowerer = Lowerer::new(source, "pkg/sub.py", infer);
    lowerer.set_dunder_name("pkg.sub");
    lowerer.set_package("pkg");
    lowerer.set_user_modules(
        modules
            .into_iter()
            .map(|module| (module.name.clone(), module))
            .collect(),
    );
    lowerer.set_late_modules(late.iter().map(|name| name.to_string()).collect());
    lowerer
}

#[test]
fn relative_from_import_prefers_a_sibling_module() {
    // `from . import sibling` names the file `pkg/sibling.py`, not an
    // attribute of `pkg/__init__.py`, and the binding keeps that identity.
    let source = "from . import sibling\nm = sibling\n";
    let mut infer = Infer::new();
    let mut lowerer = lower_package_submodule(
        source,
        &mut infer,
        vec![user_module("pkg", &[]), user_module("pkg.sibling", &[])],
        &[],
    );
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "m").kind {
        HExprKind::ModuleRef { module } => assert_eq!(module, "pkg.sibling"),
        other => panic!("expected the sibling module value, got {other:?}"),
    }
}

#[test]
fn relative_from_import_binds_a_sibling_package() {
    // `pkg/sibling/__init__.py` is a package: the binding is the subpackage
    // itself, so its exports link directly.
    let source = "from . import sibling\nx = sibling.value()\n";
    let mut infer = Infer::new();
    let mut lowerer = lower_package_submodule(
        source,
        &mut infer,
        vec![
            user_module("pkg", &[]),
            user_module("pkg.sibling", &[("value", UserExport::Function(7))]),
        ],
        &[],
    );
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::Call {
            callee: Callee::Function(7),
            args,
        } => assert!(args.is_empty(), "{args:?}"),
        other => panic!("expected the linked subpackage function, got {other:?}"),
    }
}

#[test]
fn relative_from_import_mixes_submodules_and_package_attributes() {
    // A submodule name binds the module; every other name falls back to the
    // package `__init__` exports, both functions and values.
    let source = "from . import sibling, CONST, helper\nm = sibling\nc = CONST\ny = helper(1)\n";
    let mut infer = Infer::new();
    let mut lowerer = lower_package_submodule(
        source,
        &mut infer,
        vec![
            user_module(
                "pkg",
                &[
                    ("CONST", UserExport::Global),
                    ("helper", UserExport::Function(3)),
                ],
            ),
            user_module("pkg.sibling", &[]),
        ],
        &[],
    );
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "m").kind {
        HExprKind::ModuleRef { module } => assert_eq!(module, "pkg.sibling"),
        other => panic!("expected the sibling module value, got {other:?}"),
    }
    match &assigned(&hir, "c").kind {
        HExprKind::GlobalGet { module, name } => {
            assert_eq!((module.as_str(), name.as_str()), ("pkg", "CONST"))
        }
        other => panic!("expected the package value, got {other:?}"),
    }

    // A linked package function may lower to either direct-call shape; both
    // must resolve to the package's `helper` id with the one argument intact.
    match &assigned(&hir, "y").kind {
        HExprKind::Call {
            callee: Callee::Function(3),
            args,
        } => assert_eq!(args.len(), 1),
        HExprKind::CallKw {
            callee,
            args,
            keywords,
        } if matches!(&callee.kind, HExprKind::FunctionRef(3)) => {
            assert_eq!(args.len(), 1);
            assert!(keywords.is_empty(), "{keywords:?}");
        }
        other => panic!("expected the linked package function, got {other:?}"),
    }
}

#[test]
fn relative_from_import_keeps_aliases_on_their_submodules() {
    // Multiple aliases in one statement keep their own module identity.
    let source = "from . import sibling as sub, other\nx = sub\ny = other\n";
    let mut infer = Infer::new();
    let mut lowerer = lower_package_submodule(
        source,
        &mut infer,
        vec![
            user_module("pkg", &[]),
            user_module("pkg.sibling", &[]),
            user_module("pkg.other", &[]),
        ],
        &[],
    );
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    for (binding, expected) in [("x", "pkg.sibling"), ("y", "pkg.other")] {
        match &assigned(&hir, binding).kind {
            HExprKind::ModuleRef { module } => assert_eq!(module, expected),
            other => panic!("expected the module value '{expected}', got {other:?}"),
        }
    }
}

#[test]
fn relative_from_import_of_a_missing_name_reports_once() {
    // Neither a submodule nor an export of the linked package: one
    // diagnostic, and the binding is not also treated as a module.
    let source = "from . import missing\n";
    let mut infer = Infer::new();
    let mut lowerer =
        lower_package_submodule(source, &mut infer, vec![user_module("pkg", &[])], &[]);
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(
        hir.functions[0].body.is_empty(),
        "{:?}",
        hir.functions[0].body
    );
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::ATTRIBUTE);
    assert!(
        diags[0].message.contains("'pkg'") && diags[0].message.contains("'missing'"),
        "{diags:?}"
    );
}

#[test]
fn relative_from_import_binds_an_initializing_sibling_submodule() {
    // NumPy's `numpy/_core/shape_base.py` imports `numeric` while `numeric`
    // imports `shape_base` back: `numeric` has no linked unit, but it is a
    // sibling submodule, so the binding must be the module itself — never a
    // `pkg` attribute, which CPython would not consult either.
    let source =
        "from . import fromnumeric as _from_nx, numeric as _nx, overrides\na = _from_nx\nb = _nx\nc = overrides\n";
    let mut infer = Infer::new();
    let mut lowerer = lower_package_submodule(
        source,
        &mut infer,
        vec![
            user_module("pkg", &[]),
            user_module("pkg.fromnumeric", &[]),
            user_module("pkg.overrides", &[]),
        ],
        &["pkg", "pkg.numeric"],
    );
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    for (binding, expected) in [
        ("a", "pkg.fromnumeric"),
        ("b", "pkg.numeric"),
        ("c", "pkg.overrides"),
    ] {
        match &assigned(&hir, binding).kind {
            HExprKind::ModuleRef { module } => assert_eq!(module, expected),
            other => panic!("expected the module value '{expected}', got {other:?}"),
        }
    }
}

#[test]
fn relative_from_import_falls_back_to_a_late_package_attribute() {
    // The package is still initializing, so its export table is unknown: the
    // non-submodule name keeps the package attribute lookup through the
    // runtime global table (the same binding the late branch produces).
    let source = "from . import sibling, CONST\nm = sibling\nc = CONST\n";
    let mut infer = Infer::new();
    let mut lowerer = lower_package_submodule(
        source,
        &mut infer,
        vec![user_module("pkg.sibling", &[])],
        &["pkg"],
    );
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "m").kind {
        HExprKind::ModuleRef { module } => assert_eq!(module, "pkg.sibling"),
        other => panic!("expected the sibling module value, got {other:?}"),
    }
    match &assigned(&hir, "c").kind {
        HExprKind::GlobalGet { module, name } => {
            assert_eq!((module.as_str(), name.as_str()), ("pkg", "CONST"))
        }
        other => panic!("expected the late package attribute, got {other:?}"),
    }
}

#[test]
fn native_star_import_expands_spec_functions_and_constants() {
    let source = "from math import *\nx = sqrt(9)\ny = pi\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::ModuleCall {
            module, function, ..
        } => assert_eq!((module.as_str(), function.as_str()), ("math", "sqrt")),
        other => panic!("expected a direct module call, got {other:?}"),
    }
    match &assigned(&hir, "y").kind {
        HExprKind::ModuleAttr { module, name } => {
            assert_eq!((module.as_str(), name.as_str()), ("math", "pi"))
        }
        other => panic!("expected a module attribute, got {other:?}"),
    }
}

#[test]
fn user_star_import_exports_public_names_only() {
    let source = "from helper import *\nx = add(1, 2)\ny = VALUE\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    lowerer.set_user_modules(HashMap::from([(
        "helper".to_string(),
        UserModule {
            name: "helper".to_string(),
            exports: HashMap::from([
                ("add".to_string(), UserExport::Function(1)),
                ("VALUE".to_string(), UserExport::Global),
                ("_private".to_string(), UserExport::Global),
            ]),
            all: None,
            dynamic_globals: false,
            extension_stars: Vec::new(),
        },
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "x").kind,
        HExprKind::CallKw { callee, .. } | HExprKind::IndirectCall { callee, .. }
            if matches!(&callee.kind, HExprKind::FunctionRef(1))
    ));
    assert!(matches!(
        &assigned(&hir, "y").kind,
        HExprKind::GlobalGet { module, name } if module == "helper" && name == "VALUE"
    ));

    // `_private` was not bound by the star import.
    let source = "from helper import *\ny = _private\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    lowerer.set_user_modules(HashMap::from([(
        "helper".to_string(),
        UserModule {
            name: "helper".to_string(),
            exports: HashMap::from([("_private".to_string(), UserExport::Global)]),
            all: None,
            dynamic_globals: false,
            extension_stars: Vec::new(),
        },
    )]));
    let _ = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(
        diags
            .iter()
            .any(|diag| diag.code == codes::NAME && diag.message.contains("'_private'")),
        "{diags:?}"
    );
}

#[test]
fn user_star_import_honors_a_literal_all() {
    let source = "from helper import *\nx = keep()\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    lowerer.set_user_modules(HashMap::from([(
        "helper".to_string(),
        UserModule {
            name: "helper".to_string(),
            exports: HashMap::from([
                ("keep".to_string(), UserExport::Function(1)),
                ("drop".to_string(), UserExport::Function(2)),
            ]),
            all: Some(vec!["keep".to_string()]),
            dynamic_globals: false,
            extension_stars: Vec::new(),
        },
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "x").kind,
        HExprKind::CallKw { callee, .. } | HExprKind::IndirectCall { callee, .. }
            if matches!(&callee.kind, HExprKind::FunctionRef(1))
    ));

    // A name defined by the module but absent from `__all__` is not bound.
    let source = "from helper import *\nx = drop()\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    lowerer.set_user_modules(HashMap::from([(
        "helper".to_string(),
        UserModule {
            name: "helper".to_string(),
            exports: HashMap::from([
                ("keep".to_string(), UserExport::Function(1)),
                ("drop".to_string(), UserExport::Function(2)),
            ]),
            all: Some(vec!["keep".to_string()]),
            dynamic_globals: false,
            extension_stars: Vec::new(),
        },
    )]));
    let _ = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(
        diags
            .iter()
            .any(|diag| diag.code == codes::NAME && diag.message.contains("'drop'")),
        "{diags:?}"
    );
}

#[test]
fn missing_member_of_an_extension_star_module_binds_the_extension() {
    // `numeric.py` does `from .umath import NAN, PINF`; `umath` itself is
    // `from ._multiarray_umath import *`. Those names are not in umath's
    // static export table, so each member must become a lazy read of the
    // extension namespace instead of a "does not export" error.
    let path = "/tmp/_multiarray_umath.abi3.so";
    let source = "from umath import NAN, PINF\nx = NAN\ny = PINF\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "numeric.py", &mut infer);
    lowerer.set_dunder_name("numeric");
    lowerer.set_user_modules(HashMap::from([(
        "umath".to_string(),
        extension_star_user_module("umath", &[], path),
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    assert!(lowerer.exported_extension_stars().is_empty());
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    for (binding, member) in [("x", "NAN"), ("y", "PINF")] {
        match &assigned(&hir, binding).kind {
            HExprKind::ExtensionAttr {
                path: got_path,
                member: got_member,
            } => assert_eq!((got_path.as_str(), got_member.as_str()), (path, member)),
            other => panic!("expected extension attribute '{member}', got {other:?}"),
        }
    }
}

#[test]
fn star_import_propagates_extension_stars_through_multiple_modules() {
    // `umath` re-exports the `_multiarray_umath` namespace and `numeric`
    // star-imports umath. The extension path must survive that second star
    // import, so unresolved names in `numeric` still bind to the extension.
    // Statically known exports of umath keep their direct binding.
    let path = "/tmp/_multiarray_umath.abi3.so";
    let source = "from umath import *\ns = sin\nx = NAN\ny = invert\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "numeric.py", &mut infer);
    lowerer.set_dunder_name("numeric");
    lowerer.set_user_modules(HashMap::from([(
        "umath".to_string(),
        extension_star_user_module("umath", &[("sin", UserExport::Function(4))], path),
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let stars = lowerer.exported_extension_stars();
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(stars, vec![path.to_string()]);
    assert!(matches!(
        &assigned(&hir, "s").kind,
        HExprKind::FunctionRef(4)
    ));
    for (binding, member) in [("x", "NAN"), ("y", "invert")] {
        match &assigned(&hir, binding).kind {
            HExprKind::ExtensionAttr {
                path: got_path,
                member: got_member,
            } => assert_eq!((got_path.as_str(), got_member.as_str()), (path, member)),
            other => panic!("expected extension attribute '{member}', got {other:?}"),
        }
    }
}

#[test]
fn star_import_propagates_dynamic_globals() {
    // `numerictypes` installs type aliases through `globals()[key] = ...`,
    // so its export table is incomplete. `numeric` star-imports it and must
    // inherit the dynamic flag: an unresolved name becomes a runtime read of
    // numeric's own global table.
    let source = "from numerictypes import *\nx = GENERIC_TYPE\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "numeric.py", &mut infer);
    lowerer.set_dunder_name("numeric");
    lowerer.set_user_modules(HashMap::from([(
        "numerictypes".to_string(),
        dynamic_user_module("numerictypes", &[]),
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let dynamic = lowerer.exports_dynamic_globals();
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(dynamic, "the importer inherits the dynamic namespace");
    assert!(
        matches!(
            &assigned(&hir, "x").kind,
            HExprKind::GlobalGet { module, name }
                if module == "numeric" && name == "GENERIC_TYPE"
        ),
        "expected a dynamic global read, got {:?}",
        assigned(&hir, "x").kind
    );
}

#[test]
fn missing_member_of_a_dynamic_module_reads_its_global_table() {
    // A direct import names the source module, so the fallback reads that
    // module's global table rather than the importer's.
    let source = "from numerictypes import generic\nx = generic\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "numeric.py", &mut infer);
    lowerer.set_dunder_name("numeric");
    lowerer.set_user_modules(HashMap::from([(
        "numerictypes".to_string(),
        dynamic_user_module("numerictypes", &[]),
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        matches!(
            &assigned(&hir, "x").kind,
            HExprKind::GlobalGet { module, name }
                if module == "numerictypes" && name == "generic"
        ),
        "expected the source module's dynamic global read, got {:?}",
        assigned(&hir, "x").kind
    );
}

#[test]
fn star_import_all_filtering_survives_dynamic_propagation() {
    // A literal `__all__` still selects the statically bound names. A name
    // outside it is not copied by the star import; with a dynamic source the
    // importer falls back to its own global table instead of linking it.
    let source = "from numerictypes import *\nx = keep\ny = dropped\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "numeric.py", &mut infer);
    lowerer.set_dunder_name("numeric");
    lowerer.set_user_modules(HashMap::from([(
        "numerictypes".to_string(),
        UserModule {
            all: Some(vec!["keep".to_string()]),
            ..dynamic_user_module(
                "numerictypes",
                &[
                    ("keep", UserExport::Function(2)),
                    ("dropped", UserExport::Function(3)),
                ],
            )
        },
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "x").kind,
        HExprKind::FunctionRef(2)
    ));
    assert!(
        matches!(
            &assigned(&hir, "y").kind,
            HExprKind::GlobalGet { module, name }
                if module == "numeric" && name == "dropped"
        ),
        "a name filtered out of `__all__` must not link, got {:?}",
        assigned(&hir, "y").kind
    );
}

#[test]
fn propagated_dynamic_globals_do_not_shadow_explicit_definitions() {
    // The dynamic fallback only applies to names with no static binding:
    // a module-level definition and a parameter still resolve locally.
    let source = "from numerictypes import *\ngeneric = 3\nx = generic\n\ndef use(generic):\n    return generic\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "numeric.py", &mut infer);
    lowerer.set_dunder_name("numeric");
    lowerer.set_user_modules(HashMap::from([(
        "numerictypes".to_string(),
        dynamic_user_module("numerictypes", &[]),
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        matches!(&assigned(&hir, "x").kind, HExprKind::Local(_)),
        "the module-level definition must win, got {:?}",
        assigned(&hir, "x").kind
    );
    let body = function_body(&hir, "use");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return, got {:?}", body[0].kind);
    };
    assert!(
        matches!(&value.kind, HExprKind::Local(_)),
        "the parameter must win, got {:?}",
        value.kind
    );
}

#[test]
fn star_import_of_an_ordinary_module_keeps_unknown_name_errors() {
    // Without extension stars or dynamic globals neither fallback applies:
    // a name the source never exports is still a compile error.
    let source = "from helper import *\nx = add(1, 2)\ny = missing\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "main.py", &mut infer);
    lowerer.set_user_modules(HashMap::from([(
        "helper".to_string(),
        user_module("helper", &[("add", UserExport::Function(1))]),
    )]));
    let _hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(
        diags
            .iter()
            .any(|diag| diag.code == codes::NAME && diag.message.contains("'missing'")),
        "{diags:?}"
    );
}

#[test]
fn native_star_import_of_a_submodule_binds_its_functions() {
    let source = "from os.path import *\nx = basename('/tmp/a.txt')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::ModuleCall {
            module, function, ..
        } => assert_eq!(
            (module.as_str(), function.as_str()),
            ("os.path", "basename")
        ),
        other => panic!("expected a direct module call, got {other:?}"),
    }
}

#[test]
fn native_star_import_provides_token_constants() {
    // `tokenize.py` does `from token import *` and then uses `OP`,
    // `ENCODING` and `tok_name`; the native `token` module's public
    // constants must resolve as plain compile-time attribute reads.
    let source = "from token import *\nx = OP\ny = ENCODING\nz = tok_name\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for (binding, member) in [("x", "OP"), ("y", "ENCODING"), ("z", "tok_name")] {
        match &assigned(&hir, binding).kind {
            HExprKind::ModuleAttr { module, name } => {
                assert_eq!((module.as_str(), name.as_str()), ("token", member))
            }
            other => panic!("expected a token constant, got {other:?}"),
        }
    }
}

#[test]
fn module_level_conditional_star_imports_are_legal() {
    // A module-level `if`/`try` body still runs at module level, so CPython
    // accepts the wildcard form there.
    let source = "if True:\n    from math import *\nx = sqrt(9)\ny = pi\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "x").kind,
        HExprKind::ModuleCall { module, function, .. }
            if module == "math" && function == "sqrt"
    ));
    assert!(matches!(
        &assigned(&hir, "y").kind,
        HExprKind::ModuleAttr { module, name } if module == "math" && name == "pi"
    ));

    let source = "try:\n    from math import *\nexcept ImportError:\n    pass\n\nx = sqrt(9)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "x").kind,
        HExprKind::ModuleCall { module, function, .. }
            if module == "math" && function == "sqrt"
    ));
}

#[test]
fn star_import_inside_a_function_is_a_syntax_error() {
    let source = "def f():\n    from math import *\n    return 1\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::SYNTAX);
    assert_eq!(diags[0].message, "import * only allowed at module level");

    // The same rule applies to a star import nested in a module-level block
    // inside a function.
    let source = "def f():\n    if True:\n        from math import *\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::SYNTAX);
    assert_eq!(diags[0].message, "import * only allowed at module level");
}

#[test]
fn star_import_inside_a_class_body_is_a_syntax_error() {
    let source = "class C:\n    from math import *\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::SYNTAX);
    assert_eq!(diags[0].message, "import * only allowed at module level");

    let source = "def outer():\n    class C:\n        from math import *\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::SYNTAX);
    assert_eq!(diags[0].message, "import * only allowed at module level");
}

/// True when `stmts` or a nested `if` block binds `expected` through the
/// runtime global table.
fn binds_module_global(stmts: &[HStmt], expected: &str) -> bool {
    stmts.iter().any(|stmt| match &stmt.kind {
        HStmtKind::GlobalSet { name, .. } => name == expected,
        HStmtKind::If {
            then_body,
            else_body,
            ..
        } => binds_module_global(then_body, expected) || binds_module_global(else_body, expected),
        _ => false,
    })
}

#[test]
fn native_star_import_is_mirrored_into_module_globals() {
    // An imported unit (or a session fragment) mirrors each star binding into
    // its runtime global table, so the names are real module attributes for
    // importers and later fragments.
    let source = "from math import *\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "helper.py", &mut infer);
    lowerer.set_mirror_all_globals(true);
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    let body = &hir.functions[0].body;
    assert!(
        binds_module_global(body, "sqrt"),
        "star-imported function must be mirrored: {body:?}"
    );
    assert!(
        binds_module_global(body, "pi"),
        "star-imported constant must be mirrored: {body:?}"
    );
}

#[test]
fn user_star_import_is_mirrored_into_module_globals() {
    let source = "from helper import *\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "consumer.py", &mut infer);
    lowerer.set_mirror_all_globals(true);
    lowerer.set_user_modules(HashMap::from([(
        "helper".to_string(),
        UserModule {
            name: "helper".to_string(),
            exports: HashMap::from([
                ("add".to_string(), UserExport::Function(7)),
                ("VALUE".to_string(), UserExport::Global),
                ("_private".to_string(), UserExport::Global),
            ]),
            all: None,
            dynamic_globals: false,
            extension_stars: Vec::new(),
        },
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    let body = &hir.functions[0].body;
    assert!(binds_module_global(body, "add"), "{body:?}");
    assert!(binds_module_global(body, "VALUE"), "{body:?}");
    assert!(!binds_module_global(body, "_private"), "{body:?}");
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
    // A `for` target outside the supported subset is rejected with the
    // generic unsupported diagnostic at the target's own span.
    let source = "obj = None\nitems = []\nfor obj.attr in items:\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::UNSUPPORTED);
    assert_eq!((diags[0].line, diags[0].col), (3, 5));
    assert!(diags[0].end_col > 5, "span clamps to the first line");
    assert_eq!(
        diags[0].help.as_deref(),
        Some("this construct is not implemented yet; see docs/TODO.md")
    );
}

#[test]
fn complex_literals_lower_to_constants() {
    let source = "a = 0j\nb = 1j\nc = 1.5j\nd = -2j\ne = 1+2j\nf = 1-2.5j\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let expected = [
        ("a", 0.0, 0.0),
        ("b", 0.0, 1.0),
        ("c", 0.0, 1.5),
        ("e", 1.0, 2.0),
        ("f", 1.0, -2.5),
    ];
    for (name, real, imag) in expected {
        let value = assigned(&hir, name);
        assert_eq!(value.ty, Ty::Concrete(Type::Complex), "{name}");
        match &value.kind {
            HExprKind::Const(Const::Complex {
                real: got_real,
                imag: got_imag,
            }) => assert_eq!((*got_real, *got_imag), (real, imag), "{name}"),
            other => panic!("expected a complex constant for {name}, got {other:?}"),
        }
    }
    // Unary minus keeps the sign of the zero real part, like CPython.
    let value = assigned(&hir, "d");
    assert_eq!(value.ty, Ty::Concrete(Type::Complex));
    let HExprKind::Const(Const::Complex { real, imag }) = &value.kind else {
        panic!("expected a complex constant, got {:?}", value.kind);
    };
    assert!(real.is_sign_negative(), "real part keeps its -0 sign");
    assert_eq!(*imag, -2.0);
}

#[test]
fn complex_literal_arithmetic_folds_to_complex_constants() {
    let source = "a = 1j + 1\nb = 1 + 1j\nc = 2j * 3\nd = 1j / 2\ne = 1j - 1\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let expected = [
        ("a", 1.0, 1.0),
        ("b", 1.0, 1.0),
        ("c", 0.0, 6.0),
        ("d", 0.0, 0.5),
        ("e", -1.0, 1.0),
    ];
    for (name, real, imag) in expected {
        let value = assigned(&hir, name);
        assert_eq!(value.ty, Ty::Concrete(Type::Complex), "{name}");
        let HExprKind::Const(Const::Complex {
            real: got_real,
            imag: got_imag,
        }) = &value.kind
        else {
            panic!(
                "expected a folded complex constant for {name}, got {:?}",
                value.kind
            );
        };
        assert_eq!((*got_real, *got_imag), (real, imag), "{name}");
    }
}

#[test]
fn complex_defaults_lower_to_complex_constants() {
    let source = "def f(z=1j, w=-2.5j):\n    pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let params = &function(&hir, "f").params;
    assert_eq!(params.len(), 2);
    assert!(matches!(
        &params[0].default,
        Some(Const::Complex { real, imag }) if *real == 0.0 && *imag == 1.0
    ));
    assert!(matches!(
        &params[1].default,
        Some(Const::Complex { real, imag }) if real.is_sign_negative() && *imag == -2.5
    ));
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
fn raise_of_a_caught_exception_value_keeps_the_object() {
    let source = "try:\n    pass\nexcept Exception as e:\n    raise e from None\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Try { handlers, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try statement");
    };
    let binding = handlers[0].binding.expect("handler binding");
    let HStmtKind::Raise { exc, msg, cause } = &handlers[0].body[0].kind else {
        panic!("expected a raise statement, got {:?}", handlers[0].body[0]);
    };
    assert_eq!(*exc, None, "the value form replaces the tag form");
    assert!(
        matches!(
            &msg.as_ref().expect("raised value").kind,
            HExprKind::Local(id) if *id == binding
        ),
        "{msg:?}"
    );
    assert!(matches!(
        &cause.as_ref().expect("cause").kind,
        HExprKind::Const(Const::None)
    ));

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let handlers = program.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            hydro_ir::StmtKind::Try { handlers, .. } => Some(handlers),
            _ => None,
        })
        .expect("IR try statement");
    let hydro_ir::StmtKind::Raise { exc, msg, cause } = &handlers[0].body[0].kind else {
        panic!(
            "expected an IR raise statement, got {:?}",
            handlers[0].body[0]
        );
    };
    assert_eq!(*exc, None);
    assert!(matches!(
        msg.as_ref().map(|value| &value.kind),
        Some(hydro_ir::ExprKind::Local(_))
    ));
    assert!(matches!(
        cause.as_ref().map(|value| &value.kind),
        Some(hydro_ir::ExprKind::Const(hydro_ir::Const::None))
    ));
}

#[test]
fn raise_with_an_exception_value_and_cause_lowers_both_operands() {
    let source = "def f(e, cause):\n    raise e from cause\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let e = f.params.iter().find(|param| param.name == "e").expect("e");
    let cause_param = f
        .params
        .iter()
        .find(|param| param.name == "cause")
        .expect("cause");
    let HStmtKind::Raise { exc, msg, cause } = &f.body[0].kind else {
        panic!("expected a raise statement, got {:?}", f.body[0]);
    };
    assert_eq!(*exc, None);
    assert!(
        matches!(&msg.as_ref().expect("raised value").kind, HExprKind::Local(id) if *id == e.local),
        "{msg:?}"
    );
    assert!(
        matches!(
            &cause.as_ref().expect("explicit cause").kind,
            HExprKind::Local(id) if *id == cause_param.local
        ),
        "{cause:?}"
    );
}

#[test]
fn raise_with_two_argument_exception_constructor_raises_the_value() {
    let source = "class TokenError(Exception):\n\
                  \x20   pass\n\
                  \n\
                  def fail(msg, pos):\n\
                  \x20   raise TokenError(msg, pos) from None\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let raise = function(&hir, "fail")
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Raise { exc, msg, cause } => Some((*exc, msg, cause)),
            _ => None,
        })
        .expect("raise statement");
    assert_eq!(raise.0, None, "the value form replaces the tag form");
    assert!(raise.2.is_some(), "the `from None` cause must be preserved");
    let HExprKind::IndirectCall { callee, args, .. } =
        &raise.1.as_ref().expect("raised value").kind
    else {
        panic!(
            "expected the constructor call, got {:?}",
            raise.1.as_ref().map(|expr| &expr.kind)
        );
    };
    assert_eq!(args.len(), 2, "both positional arguments must be passed");
    assert!(matches!(
        &callee.kind,
        HExprKind::GlobalGet { name, .. } if name == "TokenError"
    ));

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let raise = program
        .functions
        .iter()
        .find(|f| f.name == "fail")
        .expect("fail")
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            hydro_ir::StmtKind::Raise { exc, msg, .. } => Some((*exc, msg)),
            _ => None,
        })
        .expect("IR raise");
    assert_eq!(raise.0, None);
    let Some(
        hydro_ir::ExprKind::Call { args, .. }
        | hydro_ir::ExprKind::IndirectCall { args, .. }
        | hydro_ir::ExprKind::ClassCall { args, .. },
    ) = raise.1.as_ref().map(|value| &value.kind)
    else {
        panic!(
            "expected the emitted constructor call, got {:?}",
            raise.1.as_ref().map(|value| &value.kind)
        );
    };
    assert_eq!(args.len(), 2);
}

#[test]
fn raise_with_a_multi_argument_builtin_constructor_raises_the_value() {
    let source = "def f(errno, text):\n    raise OSError(errno, text)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Raise { exc, msg, cause } = &function(&hir, "f").body[0].kind else {
        panic!("expected a raise statement");
    };
    assert_eq!(*exc, None, "the value form replaces the tag form");
    assert!(cause.is_none());
    let HExprKind::IndirectCall { callee, args, .. } = &msg.as_ref().expect("raised value").kind
    else {
        panic!(
            "expected the constructor call, got {:?}",
            msg.as_ref().map(|expr| &expr.kind)
        );
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::ExceptionClass(ExcKind::OSError)
    ));
    assert_eq!(args.len(), 2, "both positional arguments must be passed");
}

#[test]
fn raise_without_an_exception_stays_a_bare_re_raise() {
    let source = "try:\n    pass\nexcept Exception:\n    raise\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Try { handlers, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected a try statement");
    };
    let HStmtKind::Raise { exc, msg, cause } = &handlers[0].body[0].kind else {
        panic!("expected a raise statement, got {:?}", handlers[0].body[0]);
    };
    assert_eq!(*exc, None);
    assert!(msg.is_none());
    assert!(cause.is_none());

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let handlers = program.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            hydro_ir::StmtKind::Try { handlers, .. } => Some(handlers),
            _ => None,
        })
        .expect("IR try statement");
    let hydro_ir::StmtKind::Raise { exc, msg, cause } = &handlers[0].body[0].kind else {
        panic!(
            "expected an IR raise statement, got {:?}",
            handlers[0].body[0]
        );
    };
    assert_eq!(*exc, None);
    assert!(msg.is_none());
    assert!(cause.is_none());
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
///
/// The block must be guarded by the `$wexc is None` sentinel, so an exception
/// path never reaches the normal exit call.
fn exit_local_name<'a>(locals: &'a [HLocal], finally_body: &[HStmt]) -> &'a str {
    assert_eq!(finally_body.len(), 1, "finally must only exit the item");
    let HStmtKind::If {
        cond,
        then_body,
        else_body,
    } = &finally_body[0].kind
    else {
        panic!("expected the guarded __exit__ block");
    };
    let HExprKind::Bin { op, lhs, rhs } = &cond.kind else {
        panic!("expected `$wexc is None`, got {:?}", cond.kind);
    };
    assert_eq!(*op, BinOp::Is);
    let HExprKind::Local(guard) = &lhs.kind else {
        panic!("expected the exception sentinel, got {:?}", lhs.kind);
    };
    assert!(
        locals[*guard as usize].name.starts_with("$wexc"),
        "the guard must read the exception sentinel, not the context object"
    );
    assert!(matches!(&rhs.kind, HExprKind::Const(Const::None)));
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
fn with_open_lowers_to_try_with_exit_guard() {
    let source = "with open('x') as f:\n    print(f.read())\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let locals = &hir.functions[0].locals;
    assert_eq!(hir.functions[0].body.len(), 1);

    let (prelude, try_stmt) = with_parts(&hir.functions[0].body[0]);
    let HStmtKind::Try {
        body,
        handlers,
        else_body,
        finally_body,
    } = &try_stmt.kind
    else {
        panic!("expected the __exit__ try");
    };
    assert!(else_body.is_empty());
    assert_eq!(exit_local_name(locals, finally_body), "$with0");

    // Prelude: evaluate `open(...)` once into `$with0`, seed the exception
    // slot with `None`, then bind `f` to the result of
    // `$with0.__enter__()`. All of this runs before the `try`, so a failing
    // `__enter__` never reaches `__exit__`.
    assert_eq!(prelude.len(), 3);
    let HStmtKind::Assign { target, value } = &prelude[0].kind else {
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

    let HStmtKind::Assign { target, value } = &prelude[1].kind else {
        panic!("expected the exception slot seed");
    };
    assert!(locals[*target as usize].name.starts_with("$wexc"));
    assert!(matches!(value.kind, HExprKind::Const(Const::None)));

    let HStmtKind::Assign { target, value } = &prelude[2].kind else {
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

    assert_eq!(handlers.len(), 1, "the body exception reaches __exit__");
    assert_eq!(handlers[0].exc, Some(ExcKind::BaseException));
    assert_eq!(body.len(), 1, "only the user body is guarded");

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn with_without_target_still_calls_enter() {
    let source = "with open('x'):\n    print('body')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let locals = &hir.functions[0].locals;
    let (prelude, _) = with_parts(&hir.functions[0].body[0]);
    assert_eq!(prelude.len(), 3);
    let HStmtKind::Expr(enter) = &prelude[2].kind else {
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

    let (outer_prelude, outer) = with_parts(&hir.functions[0].body[0]);
    assert_eq!(outer_prelude.len(), 3);
    // The first written manager is evaluated and entered first, so its try is
    // the outer one and its exit runs last. Items are lowered last-to-first,
    // so the outer manager owns the later `$withN` slots.
    let outer_manager = assigned_local(locals, &outer_prelude[0]);
    assert_eq!(open_argument(&outer_prelude[0]), "a");
    let HStmtKind::Try {
        body, finally_body, ..
    } = &outer.kind
    else {
        panic!("expected the outer try");
    };
    assert_eq!(exit_local_name(locals, finally_body), outer_manager);

    assert_eq!(body.len(), 4, "second prelude plus its try");
    let inner_manager = assigned_local(locals, &body[0]);
    assert_eq!(open_argument(&body[0]), "b");
    assert_ne!(inner_manager, outer_manager);
    let HStmtKind::Try {
        body: inner_body,
        finally_body,
        ..
    } = &body[3].kind
    else {
        panic!("expected the inner try");
    };
    assert_eq!(exit_local_name(locals, finally_body), inner_manager);
    assert!(matches!(&inner_body[0].kind, HStmtKind::Pass));
}

#[test]
fn with_exit_runs_from_finally_after_a_raising_body() {
    let source = "def f():\n    with open('x'):\n        raise ValueError('boom')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let (prelude, try_stmt) = with_parts(&f.body[0]);
    // Without an `as` target the enter call still runs, as an expression
    // statement; only the assignment to the target is missing.
    assert_eq!(
        prelude.len(),
        3,
        "context expression, exception seed, enter"
    );
    let HStmtKind::Expr(enter) = &prelude[2].kind else {
        panic!(
            "expected the __enter__() statement, got {:?}",
            prelude[2].kind
        );
    };
    assert_eq!(method_name(enter), "__enter__");
    let HStmtKind::Try {
        body,
        handlers,
        finally_body,
        ..
    } = &try_stmt.kind
    else {
        panic!("expected the __exit__ try");
    };
    // The raising body stays inside the try, so the handler calls `__exit__`
    // with the real exception and the `finally` guard stays closed.
    assert!(matches!(
        &body.last().expect("with body").kind,
        HStmtKind::Raise {
            exc: Some(ExcKind::ValueError),
            ..
        }
    ));
    assert_eq!(handlers[0].exc, Some(ExcKind::BaseException));
    assert_eq!(exit_local_name(&f.locals, finally_body), "$with0");
}

#[test]
fn with_open_for_loop_checks_cleanly() {
    let source = "with open('data.txt') as f:\n    for line in f:\n        print(line)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let (_, try_stmt) = with_parts(&hir.functions[0].body[0]);
    let HStmtKind::Try { body, .. } = &try_stmt.kind else {
        panic!("expected the __exit__ try");
    };
    assert!(matches!(&body[0].kind, HStmtKind::ForEach { .. }));
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
    let (_, try_stmt) = with_parts(&hir.functions[0].body[0]);
    assert!(matches!(&try_stmt.kind, HStmtKind::Try { .. }));
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
    assert!(matches!(&function.body[0].kind, HStmtKind::If { .. }));
}

/// Splits a lowered `with` block into its prelude statements and the guarded
/// `try`.
fn with_parts(stmt: &HStmt) -> (&[HStmt], &HStmt) {
    let stmts = block_body(stmt);
    let (try_stmt, prelude) = stmts.split_last().expect("a with block is never empty");
    assert!(
        matches!(&try_stmt.kind, HStmtKind::Try { .. }),
        "expected the __exit__ try"
    );
    (prelude, try_stmt)
}

/// Name of the hidden local assigned by `stmt` (`$withN` or `$wexcN`).
fn assigned_local<'a>(locals: &'a [HLocal], stmt: &HStmt) -> &'a str {
    let HStmtKind::Assign { target, .. } = &stmt.kind else {
        panic!("expected an assignment, got {:?}", stmt.kind);
    };
    &locals[*target as usize].name
}

/// String literal passed to the `open(...)` call assigned by `stmt`.
fn open_argument(stmt: &HStmt) -> &str {
    let HStmtKind::Assign { value, .. } = &stmt.kind else {
        panic!("expected an assignment, got {:?}", stmt.kind);
    };
    let HExprKind::ModuleCall { args, .. } = &value.kind else {
        panic!("expected a module call, got {:?}", value.kind);
    };
    match &args[0].kind {
        HExprKind::Const(Const::Str(s)) => s.as_str(),
        other => panic!("expected a string literal, got {other:?}"),
    }
}

/// Method name of an `object.method(...)` expression.
fn method_name(expr: &HExpr) -> &str {
    match &expr.kind {
        HExprKind::MethodCall { method, .. } => method,
        other => panic!("expected a method call, got {other:?}"),
    }
}

/// Counts `__exit__` calls in `stmts` and every statement nested inside them.
///
/// The lowering places them either as a statement (`__exit__(None, None,
/// None)` in a guard) or inside an `if not __exit__(...)` condition, so those
/// two shapes are what the walk looks for.
fn count_exit_calls(stmts: &[HStmt]) -> usize {
    let mut total = 0;
    for stmt in stmts {
        match &stmt.kind {
            HStmtKind::Expr(expr) if method_name_opt(expr) == Some("__exit__") => {
                total += 1;
            }
            HStmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                if let HExprKind::Un { operand, .. } = &cond.kind {
                    if method_name_opt(operand) == Some("__exit__") {
                        total += 1;
                    }
                }
                total += count_exit_calls(then_body) + count_exit_calls(else_body);
            }
            HStmtKind::While {
                body, else_body, ..
            }
            | HStmtKind::ForEach {
                body, else_body, ..
            }
            | HStmtKind::ForRange {
                body, else_body, ..
            } => total += count_exit_calls(body) + count_exit_calls(else_body),
            HStmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => {
                total += count_exit_calls(body)
                    + count_exit_calls(else_body)
                    + count_exit_calls(finally_body);
                for handler in handlers {
                    total += count_exit_calls(&handler.body);
                }
            }
            _ => {}
        }
    }
    total
}

/// Method name when `expr` is an `object.method(...)` call.
fn method_name_opt(expr: &HExpr) -> Option<&str> {
    match &expr.kind {
        HExprKind::MethodCall { method, .. } => Some(method.as_str()),
        _ => None,
    }
}

/// Checks the `except BaseException as $wexc: if not __exit__(...): raise`
/// arm of a lowered `with` and returns the handler binding name together with
/// the `__exit__` call.
fn with_handler_check<'a>(locals: &'a [HLocal], try_stmt: &'a HStmt) -> (&'a str, &'a HExpr) {
    let HStmtKind::Try {
        handlers,
        finally_body,
        ..
    } = &try_stmt.kind
    else {
        panic!("expected the __exit__ try");
    };
    assert_eq!(handlers.len(), 1, "one BaseException handler");
    let handler = &handlers[0];
    assert_eq!(handler.exc, Some(ExcKind::BaseException));
    let exc = handler.binding.expect("the handler binds the exception");
    assert!(!handler.binding_cell);

    assert_eq!(finally_body.len(), 1);
    let HStmtKind::If { cond, .. } = &finally_body[0].kind else {
        panic!("expected the normal-exit guard");
    };
    let HExprKind::Bin { op, lhs, rhs } = &cond.kind else {
        panic!("expected `$wexc is None`, got {:?}", cond.kind);
    };
    assert_eq!(*op, BinOp::Is);
    assert!(matches!(&lhs.kind, HExprKind::Local(local) if *local == exc));
    assert!(matches!(&rhs.kind, HExprKind::Const(Const::None)));

    assert_eq!(handler.body.len(), 1, "only the suppression check");
    let HStmtKind::If {
        cond, then_body, ..
    } = &handler.body[0].kind
    else {
        panic!("expected `if not __exit__(...): raise`");
    };
    let HExprKind::Un { op, operand } = &cond.kind else {
        panic!("expected `not __exit__(...)`, got {:?}", cond.kind);
    };
    assert_eq!(*op, hydro_ir::UnOp::Not);
    assert!(matches!(
        &then_body[0].kind,
        HStmtKind::Raise {
            exc: None,
            msg: None,
            cause: None,
        }
    ));
    (locals[exc as usize].name.as_str(), operand)
}

#[test]
fn with_exit_truthy_suppresses_the_body_exception() {
    let source = "def f(cm):\n    with cm:\n        raise ValueError('boom')\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let (_, try_stmt) = with_parts(&f.body[0]);
    let (_, exit_call) = with_handler_check(&f.locals, try_stmt);
    // The handler raises only when `__exit__` is falsy, so a truthy result
    // falls through and the body exception is suppressed.
    let HExprKind::MethodCall { method, args, .. } = &exit_call.kind else {
        panic!("expected __exit__, got {:?}", exit_call.kind);
    };
    assert_eq!(method, "__exit__");
    assert_eq!(args.len(), 3);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn with_exit_falsy_reraises_with_a_bare_raise() {
    let source = "def f(cm):\n    with cm:\n        raise ValueError('boom')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let (_, try_stmt) = with_parts(&f.body[0]);
    // `with_handler_check` already asserted the re-raise is bare: it carries
    // no exception expression, so the runtime re-raises the original
    // `ValueError` instead of wrapping it.
    let (exc_local, _) = with_handler_check(&f.locals, try_stmt);
    assert!(exc_local.starts_with("$wexc"));
}

#[test]
fn with_enter_failure_does_not_call_exit() {
    let source = "def f(cm):\n    with cm:\n        print('body')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let (prelude, try_stmt) = with_parts(&f.body[0]);
    // `$with = cm`, `$wexc = None`, `cm.__enter__()` all precede the try, so
    // an exception from either the context expression or `__enter__` leaves
    // this manager without an `__exit__` call.
    assert_eq!(prelude.len(), 3);
    assert_eq!(assigned_local(&f.locals, &prelude[0]), "$with0");
    assert!(assigned_local(&f.locals, &prelude[1]).starts_with("$wexc"));
    let HStmtKind::Expr(enter) = &prelude[2].kind else {
        panic!("expected the __enter__() statement");
    };
    assert_eq!(method_name(enter), "__enter__");
    let HStmtKind::Try { body, .. } = &try_stmt.kind else {
        panic!("expected the __exit__ try");
    };
    assert_eq!(body.len(), 1, "only the user body is guarded");
    assert!(
        !nested_stmt_any(prelude, &|kind| matches!(kind, HStmtKind::Try { .. })),
        "the prelude must not be wrapped in a try of its own"
    );
}

#[test]
fn with_normal_path_calls_exit_once_with_none() {
    let source = "def f(cm):\n    with cm:\n        print('body')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let (_, try_stmt) = with_parts(&f.body[0]);
    let HStmtKind::Try {
        handlers,
        finally_body,
        ..
    } = &try_stmt.kind
    else {
        panic!("expected the __exit__ try");
    };
    // The handler only fires for an exception, so a normal exit is the
    // `finally` call with `(None, None, None)`.
    assert_eq!(exit_local_name(&f.locals, finally_body), "$with0");
    assert_eq!(
        count_exit_calls(std::slice::from_ref(try_stmt)),
        2,
        "one guarded normal exit plus the handler's exception exit"
    );
    assert_eq!(handlers.len(), 1);
}

#[test]
fn with_exception_path_passes_type_and_instance() {
    let source = "def f(cm):\n    with cm:\n        raise ValueError('boom')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let (_, try_stmt) = with_parts(&f.body[0]);
    let (exc_name, exit_call) = with_handler_check(&f.locals, try_stmt);
    let HExprKind::MethodCall { args, .. } = &exit_call.kind else {
        panic!("expected __exit__, got {:?}", exit_call.kind);
    };
    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Type),
        args: type_args,
    } = &args[0].kind
    else {
        panic!("expected `type($wexc)`, got {:?}", args[0].kind);
    };
    assert_eq!(type_args.len(), 1);
    let HExprKind::Local(exc) = &type_args[0].kind else {
        panic!("expected the handler binding");
    };
    assert_eq!(f.locals[*exc as usize].name.as_str(), exc_name);
    assert!(matches!(&args[1].kind, HExprKind::Local(local) if local == exc));
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::None)));
}

#[test]
fn with_exit_is_called_once_per_manager_on_every_path() {
    // `break`, `continue` and `return` leave the guarded try through its
    // `finally`, which holds exactly one `__exit__` call; the handler's call
    // is the only other one and fires solely for an exception.
    for body in ["break", "continue", "return"] {
        let source = format!("def f(cm):\n    while True:\n        with cm:\n            {body}\n");
        let (hir, _, diags) = lower_source(&source, &parse(&source));
        assert!(diags.is_empty(), "{diags:?}");
        let f = function(&hir, "f");
        let HStmtKind::While {
            body: loop_body, ..
        } = &f.body[0].kind
        else {
            panic!("expected the loop");
        };
        let (_, try_stmt) = with_parts(&loop_body[0]);
        assert_eq!(
            count_exit_calls(std::slice::from_ref(try_stmt)),
            2,
            "`{body}` must not add or drop an __exit__ call"
        );
        let HStmtKind::Try { finally_body, .. } = &try_stmt.kind else {
            panic!("expected the __exit__ try");
        };
        assert_eq!(exit_local_name(&f.locals, finally_body), "$with0");
    }
}

#[test]
fn with_exit_exception_replaces_the_body_exception() {
    let source = "def f(cm):\n    with cm:\n        raise ValueError('boom')\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let (_, try_stmt) = with_parts(&f.body[0]);
    // The handler's `__exit__` call is not itself guarded: an exception it
    // raises leaves the handler and replaces the in-flight `ValueError`.
    // The `finally` guard then sees `$wexc` bound and does not call it again.
    let (_, exit_call) = with_handler_check(&f.locals, try_stmt);
    assert_eq!(
        count_exit_calls(std::slice::from_ref(try_stmt)),
        2,
        "the failing call is the only exception-path call"
    );
    assert_eq!(method_name(exit_call), "__exit__");
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
fn fstring_dynamic_format_spec_is_lowered_to_the_format_builtin() {
    let source = "n = 7\nw = 5\nx = f'{n:>{w}}'\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let call = find_format_call(&hir).expect("format call");
    // The spec is evaluated at runtime: a nested replacement field lowers to
    // an HIR expression (here `'>' + str(w)`), never to a constant.
    assert!(
        !matches!(&call.1.kind, HExprKind::Const(_)),
        "{:?}",
        call.1.kind
    );
    assert_eq!(call.1.ty, Ty::Concrete(Type::Str));
    assert!(matches!(&call.2.kind, HExprKind::Const(Const::Int(0))));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
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

/// The `slice(start, stop, step)` call that a slice element of a subscript
/// index lowers to. Returns the argument list.
fn slice_value_args(expr: &HExpr) -> &[HExpr] {
    let HExprKind::IndirectCall {
        callee,
        args,
        star: None,
    } = &expr.kind
    else {
        panic!("expected a slice value call, got {:?}", expr.kind);
    };
    assert!(
        matches!(
            &callee.kind,
            HExprKind::ModuleAttr { module, name }
                if module.as_str() == "builtins" && name.as_str() == "slice"
        ),
        "expected the builtins.slice class value, got {:?}",
        callee.kind
    );
    args
}

#[test]
fn subscript_tuple_slice_elements_build_slice_values() {
    let source = "\
a = [[1, 2], [3, 4]]
x = a[:, 1]
y = a[1, :]
z = a[::2, ...]
w = a[0:1]
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    // A tuple index carrying a slice is a dynamic key: sema leaves the
    // verdict to the receiver instead of demanding an integer index.
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());

    let HExprKind::Subscript { index, .. } = &assigned(&hir, "x").kind else {
        panic!("expected a subscript, got {:?}", assigned(&hir, "x").kind);
    };
    let HExprKind::Tuple(elems) = &index.kind else {
        panic!("expected a tuple index, got {:?}", index.kind);
    };
    assert_eq!(elems.len(), 2);
    let args = slice_value_args(&elems[0]);
    assert_eq!(args.len(), 3);
    for arg in args {
        assert!(
            matches!(&arg.kind, HExprKind::Const(Const::None)),
            "omitted bounds become None, got {:?}",
            arg.kind
        );
    }
    assert!(matches!(&elems[1].kind, HExprKind::Const(Const::Int(1))));

    let HExprKind::Subscript { index, .. } = &assigned(&hir, "y").kind else {
        panic!("expected a subscript, got {:?}", assigned(&hir, "y").kind);
    };
    let HExprKind::Tuple(elems) = &index.kind else {
        panic!("expected a tuple index, got {:?}", index.kind);
    };
    assert!(matches!(&elems[0].kind, HExprKind::Const(Const::Int(1))));
    assert_eq!(slice_value_args(&elems[1]).len(), 3);

    let HExprKind::Subscript { index, .. } = &assigned(&hir, "z").kind else {
        panic!("expected a subscript, got {:?}", assigned(&hir, "z").kind);
    };
    let HExprKind::Tuple(elems) = &index.kind else {
        panic!("expected a tuple index, got {:?}", index.kind);
    };
    let args = slice_value_args(&elems[0]);
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::None)));
    assert!(matches!(&args[1].kind, HExprKind::Const(Const::None)));
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::Int(2))));
    assert!(matches!(&elems[1].kind, HExprKind::Const(Const::Ellipsis)));

    // A direct `a[start:stop:step]` keeps the native fast path.
    assert!(matches!(&assigned(&hir, "w").kind, HExprKind::Slice { .. }));
}

/// Split the tuple concatenation that a starred subscript index lowers to.
/// Returns the literal prefix elements in source order and the `tuple(...)`
/// operand that unpacks the starred value.
///
/// The lowering left-nests one `BinOp::Add` per chunk: `a[0, *xs]` becomes
/// `(Tuple([]) + Tuple([0])) + tuple(xs)`, so the literal prefix spans every
/// tuple on the left spine, not just the outermost left operand.
fn starred_index_concat(expr: &HExpr) -> (Vec<&HExpr>, &HExpr) {
    let HExprKind::Bin {
        op: BinOp::Add,
        lhs,
        rhs,
    } = &expr.kind
    else {
        panic!("expected tuple concatenation, got {:?}", expr.kind);
    };
    assert!(
        matches!(
            &rhs.kind,
            HExprKind::Call {
                callee: Callee::Builtin(Builtin::Tuple),
                args,
            } if args.len() == 1
        ),
        "expected the unpacked operand copied through tuple(), got {:?}",
        rhs.kind
    );
    let mut prefix = Vec::new();
    prefix_tuple_chunks(lhs, &mut prefix);
    (prefix, rhs)
}

/// Collect the elements of the literal tuple chunks along the left spine of a
/// tuple concatenation, in source order: the leftmost chunk is the deepest.
fn prefix_tuple_chunks<'a>(expr: &'a HExpr, out: &mut Vec<&'a HExpr>) {
    match &expr.kind {
        HExprKind::Bin {
            op: BinOp::Add,
            lhs,
            rhs,
        } => {
            prefix_tuple_chunks(lhs, out);
            prefix_tuple_chunks(rhs, out);
        }
        HExprKind::Tuple(elements) => out.extend(elements.iter()),
        _ => panic!("expected a literal tuple chunk, got {:?}", expr.kind),
    }
}

#[test]
fn starred_subscript_index_unpacks_the_whole_index() {
    // `a[*xs]` parses as a bare starred operand; `a[*xs,]` wraps it in a
    // one-element tuple. Both are the same index tuple `tuple(xs)`.
    let source = "\
xs = (1, 2)
a = {}
x = a[*xs]
y = a[0, *xs]
z = a[*xs,]
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::Subscript { index, .. } = &assigned(&hir, "x").kind else {
        panic!("expected a subscript, got {:?}", assigned(&hir, "x").kind);
    };
    let (prefix, unpacked) = starred_index_concat(index);
    assert!(
        prefix.is_empty(),
        "a leading star starts from an empty tuple, got {prefix:?}"
    );
    let HExprKind::Call { args, .. } = &unpacked.kind else {
        panic!("expected a tuple() call, got {:?}", unpacked.kind);
    };
    assert!(
        matches!(&args[0].kind, HExprKind::Local(_)),
        "the starred operand is the lowered name, got {:?}",
        args[0].kind
    );

    let HExprKind::Subscript { index, .. } = &assigned(&hir, "y").kind else {
        panic!("expected a subscript, got {:?}", assigned(&hir, "y").kind);
    };
    let (prefix, _) = starred_index_concat(index);
    assert_eq!(prefix.len(), 1);
    assert!(matches!(&prefix[0].kind, HExprKind::Const(Const::Int(0))));

    let HExprKind::Subscript { index, .. } = &assigned(&hir, "z").kind else {
        panic!("expected a subscript, got {:?}", assigned(&hir, "z").kind);
    };
    let (prefix, _) = starred_index_concat(index);
    assert!(
        prefix.is_empty(),
        "the trailing comma does not add an element, got {prefix:?}"
    );
}

#[test]
fn starred_subscript_index_keeps_plain_and_slice_tuple_indices() {
    let source = "\
xs = (1, 2)
a = {}
p = a[0, 1]
q = a[:, *xs]
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    // A tuple index without a star stays one literal tuple.
    let HExprKind::Subscript { index, .. } = &assigned(&hir, "p").kind else {
        panic!("expected a subscript, got {:?}", assigned(&hir, "p").kind);
    };
    let HExprKind::Tuple(elements) = &index.kind else {
        panic!("expected a plain tuple index, got {:?}", index.kind);
    };
    assert_eq!(elements.len(), 2);
    assert!(matches!(&elements[0].kind, HExprKind::Const(Const::Int(0))));
    assert!(matches!(&elements[1].kind, HExprKind::Const(Const::Int(1))));

    // Slice elements next to a star still become real slice values.
    let HExprKind::Subscript { index, .. } = &assigned(&hir, "q").kind else {
        panic!("expected a subscript, got {:?}", assigned(&hir, "q").kind);
    };
    let (prefix, _) = starred_index_concat(index);
    assert_eq!(prefix.len(), 1);
    let args = slice_value_args(prefix[0]);
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::None)));
    assert!(matches!(&args[1].kind, HExprKind::Const(Const::None)));
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::None)));
}

#[test]
fn starred_subscript_assignment_and_delete_targets_unpack_the_index() {
    let source = "\
xs = (0, 1)
a = {}
a[*xs] = 1
del a[*xs]
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let HStmtKind::SetSubscript { index, .. } = &hir.functions[0].body[2].kind else {
        panic!(
            "expected a subscript store, got {:?}",
            hir.functions[0].body[2].kind
        );
    };
    let (prefix, _) = starred_index_concat(index);
    assert!(prefix.is_empty());

    let HStmtKind::Delete { targets } = &hir.functions[0].body[3].kind else {
        panic!("expected a delete, got {:?}", hir.functions[0].body[3].kind);
    };
    let [HDeleteTarget::Subscript { index, .. }] = targets.as_slice() else {
        panic!("expected one subscript deletion, got {targets:?}");
    };
    let (prefix, _) = starred_index_concat(index);
    assert!(prefix.is_empty());
}

#[test]
fn slice_outside_a_subscript_is_rejected() {
    // The parser only produces slice syntax inside a subscript, so build the
    // illegal shape by lifting the parsed `1:2` node into a standalone
    // assignment value.
    let source = "a = 'abc'\nx = a[1:2]\n";
    let mut module = parse(source);
    let slice = match &module.body[1] {
        ast::Stmt::Assign(assign) => match &*assign.value {
            ast::Expr::Subscript(sub) => (*sub.slice).clone(),
            other => panic!("expected a subscript, got {other:?}"),
        },
        other => panic!("expected an assignment, got {other:?}"),
    };
    let ast::Stmt::Assign(assign) = &mut module.body[1] else {
        panic!("expected an assignment");
    };
    *assign.value = slice;
    let (_, _, diags) = lower_source(source, &module);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0].message,
        "slice syntax is only supported inside a subscript"
    );
}

#[test]
fn sema_accepts_slice_tuple_indices_on_native_sequences() {
    let source = "\
s = 'abc'
xs = [1, 2]
t = (1, 2)
raw = b'ab'
a = s[:, 1]
c = xs[:, 0]
d = t[0, :]
e = raw[::2, 0]
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    // A dynamic key has no static result type: the runtime raises the
    // CPython `TypeError` for the native sequences.
    for name in ["a", "c", "d", "e"] {
        assert_eq!(
            infer.resolve(assigned(&hir, name).ty),
            Ty::Concrete(Type::Any),
            "{name}"
        );
    }

    // A tuple index without a slice keeps the static integer check.
    let source = "xs = [1, 2]\ny = xs[1, 2]\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let errors = crate::sema::check(&hir, &mut infer, "test.py").expect_err("expected errors");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "list index must be int, not tuple");
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
fn augmented_slice_assignment_desugars_with_single_evaluation_temps() {
    // `xs[start:stop] += [9]` reads the slice once, applies the operator and
    // writes the updated value back. The object and every bound are parked in
    // hidden locals so a side-effecting operand runs exactly once; the
    // omitted step keeps the `None` sentinel.
    let source = "\
xs = [1, 2, 3]
xs[0:1] += [9]
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let locals = &hir.functions[0].locals;
    let then_body = subscript_augassign_block(&hir);
    assert_eq!(then_body.len(), 5, "{then_body:?}");

    let mut temps = Vec::new();
    for (offset, expected) in ["$aug0", "$aug1", "$aug2", "$aug3"].iter().enumerate() {
        let HStmtKind::Assign { target, .. } = &then_body[offset].kind else {
            panic!(
                "expected a parked operand assignment, got {:?}",
                then_body[offset].kind
            );
        };
        assert_eq!(locals[*target as usize].name, *expected);
        temps.push(*target);
    }
    // `xs` is read into the receiver temp; the bounds are the literal start,
    // the literal stop and the `None` step.
    let HStmtKind::Assign { value: object, .. } = &then_body[0].kind else {
        unreachable!()
    };
    assert!(
        matches!(&object.kind, HExprKind::Local(id) if locals[*id as usize].name == "xs"),
        "{:?}",
        object.kind
    );
    let HStmtKind::Assign { value: start, .. } = &then_body[1].kind else {
        unreachable!()
    };
    assert!(matches!(&start.kind, HExprKind::Const(Const::Int(0))));
    let HStmtKind::Assign { value: stop, .. } = &then_body[2].kind else {
        unreachable!()
    };
    assert!(matches!(&stop.kind, HExprKind::Const(Const::Int(1))));
    let HStmtKind::Assign { value: step, .. } = &then_body[3].kind else {
        unreachable!()
    };
    assert!(matches!(&step.kind, HExprKind::Const(Const::None)));

    let HStmtKind::SetSlice {
        object: set_object,
        start: set_start,
        stop: set_stop,
        step: set_step,
        value,
    } = &then_body[4].kind
    else {
        panic!("expected a slice store, got {:?}", then_body[4].kind);
    };
    assert_eq!(local_of(set_object), temps[0]);
    assert_eq!(local_of(set_start.as_ref().expect("start")), temps[1]);
    assert_eq!(local_of(set_stop.as_ref().expect("stop")), temps[2]);
    assert_eq!(local_of(set_step.as_ref().expect("step")), temps[3]);

    let HExprKind::Bin { op, lhs, rhs } = &value.kind else {
        panic!("expected a bin op, got {:?}", value.kind);
    };
    assert_eq!(*op, BinOp::Add);
    assert!(matches!(&rhs.kind, HExprKind::List(items) if items.len() == 1));
    let HExprKind::Slice {
        object: read_object,
        start: read_start,
        stop: read_stop,
        step: read_step,
    } = &lhs.kind
    else {
        panic!("expected a slice read, got {:?}", lhs.kind);
    };
    assert_eq!(local_of(read_object), temps[0]);
    assert_eq!(local_of(read_start.as_ref().expect("start")), temps[1]);
    assert_eq!(local_of(read_stop.as_ref().expect("stop")), temps[2]);
    assert_eq!(local_of(read_step.as_ref().expect("step")), temps[3]);

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
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
    use hydro_ir::ExprKind;

    let source = "s = 'abc'\nxs = [1, 2]\na = s[1:]\nb = xs[1:]\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let module = &program
        .functions
        .iter()
        .find(|f| f.name == MODULE_NAME)
        .expect("module function")
        .body;
    let type_of = |statement: &hydro_ir::Stmt| match &statement.kind {
        hydro_ir::StmtKind::Assign { value, .. } => match &value.kind {
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
fn async_comprehension_outside_an_async_function_is_rejected() {
    let source = "xs = [x async for x in range(3)]\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0].message,
        "asynchronous comprehension outside of an asynchronous function"
    );
    assert_eq!(diags[0].code, codes::CONTROL);
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
    use hydro_ir::ExprKind;

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
            hydro_ir::StmtKind::Assign { value, .. } => value.ty,
            other => panic!("expected an assignment, got {other:?}"),
        })
        .collect();
    assert_eq!(
        types,
        vec![Type::List, Type::Set, Type::Dict, Type::Any],
        "list/set/dict comprehensions stay eager containers; a generator \
         expression is a lazily called generator function"
    );
    assert!(
        matches!(&module[0].kind, hydro_ir::StmtKind::Assign { value, .. }
            if matches!(&value.kind, ExprKind::ListComp { .. }))
    );
    assert!(
        matches!(&module[1].kind, hydro_ir::StmtKind::Assign { value, .. }
            if matches!(&value.kind, ExprKind::SetComp { .. }))
    );
    assert!(
        matches!(&module[2].kind, hydro_ir::StmtKind::Assign { value, .. }
            if matches!(&value.kind, ExprKind::DictComp { .. }))
    );
    let hydro_ir::StmtKind::Assign { value, .. } = &module[3].kind else {
        panic!("expected a generator expression assignment");
    };
    let ExprKind::Call { callee, args } = &value.kind else {
        panic!("expected a generator call, got {:?}", value.kind);
    };
    assert!(
        matches!(callee, hydro_ir::Callee::Function(id)
            if program.functions[*id as usize].is_generator),
        "a generator expression calls a synthetic generator function"
    );
    assert_eq!(
        args.len(),
        1,
        "only the outermost iterable is evaluated eagerly"
    );
}

#[test]
fn generator_expression_captures_and_laziness_are_structural() {
    let source = "def f(xs, n):\n    return sum(x + n for x in xs if x)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let genexpr = program
        .functions
        .iter()
        .find(|function| function.name.ends_with("<genexpr>"))
        .expect("synthetic generator function");
    assert!(genexpr.is_generator);
    assert_eq!(
        genexpr.params.len(),
        2,
        "the hidden iterable plus the captured `n` (the eagerly evaluated \
         outermost iterable needs no capture)"
    );
    let hydro_ir::StmtKind::ForEach { body, .. } = &genexpr.body[0].kind else {
        panic!(
            "expected the outermost clause to be a for loop: {:?}",
            genexpr.body
        );
    };
    let hydro_ir::StmtKind::If { then_body, .. } = &body[0].kind else {
        panic!("expected the `if` filter inside the loop");
    };
    assert!(
        matches!(&then_body[0].kind, hydro_ir::StmtKind::Yield { .. }),
        "the element is yielded, not stored in a list: {:?}",
        then_body
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
fn lambda_defaults_lower_like_function_defaults() {
    let source = "f = lambda x=1: x\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let lambda = function(&hir, "<lambda>");
    assert_eq!(lambda.params.len(), 1);
    assert_eq!(lambda.params[0].default, Some(Const::Int(1)));
}

#[test]
fn lambda_rejects_variadic_parameters() {
    for (source, expected) in [
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

/// The binder function of the first capturing lambda, the lambda function it
/// closes over and the capture slots the binder forwards.
fn capturing_lambda(hir: &LHir) -> (&HFunction, &HFunction, Vec<LocalId>) {
    let binder = hir
        .functions
        .iter()
        .find(|function| function.name == "<lambda.bind>")
        .expect("a capturing lambda creates its closure");
    let HStmtKind::Closure {
        function, captures, ..
    } = &binder.body[0].kind
    else {
        panic!("expected the closure statement, got {:?}", binder.body[0]);
    };
    let lambda = &hir.functions[*function as usize];
    (lambda, binder, captures.clone())
}

#[test]
fn lambda_captures_enclosing_locals() {
    // A lambda closes over the enclosing function's bindings exactly like a
    // nested `def`: the captured names lead the synthetic function's
    // parameters and the closure value is created where the lambda is
    // evaluated.
    let source = "def shape(base, extra):\n    return lambda x: base + x + extra\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let (lambda, binder, forwarded) = capturing_lambda(&hir);
    assert_eq!(lambda.name, "<lambda>");
    assert_eq!(lambda.capture_count, 2);
    assert_eq!(
        lambda
            .params
            .iter()
            .map(|param| param.name.as_str())
            .collect::<Vec<_>>(),
        vec!["base", "extra", "x"]
    );
    assert_eq!(binder.params.len(), 2);
    assert_eq!(forwarded, vec![0, 1]);

    // The body reads the captured slots: `(base + x) + extra`. Nothing
    // rebinds them, so they are plain values here.
    let HStmtKind::Return(Some(body)) = &lambda.body[0].kind else {
        panic!("expected the lambda return, got {:?}", lambda.body[0]);
    };
    let HExprKind::Bin { lhs, rhs, .. } = &body.kind else {
        panic!("expected an addition, got {:?}", body.kind);
    };
    assert!(matches!(&rhs.kind, HExprKind::Local(1)), "{:?}", rhs.kind);
    let HExprKind::Bin { lhs, rhs, .. } = &lhs.kind else {
        panic!("expected an addition, got {:?}", lhs.kind);
    };
    assert!(matches!(&lhs.kind, HExprKind::Local(0)), "{:?}", lhs.kind);
    assert!(matches!(&rhs.kind, HExprKind::Local(2)), "{:?}", rhs.kind);

    // `shape` evaluates the lambda by calling the binder with both capture
    // slots; the binder builds the closure from them.
    let shape = function(&hir, "shape");
    let HStmtKind::Return(Some(value)) = &shape.body[0].kind else {
        panic!("expected the return, got {:?}", shape.body[0]);
    };
    let HExprKind::Call {
        callee: Callee::Function(binder_id),
        args,
    } = &value.kind
    else {
        panic!("expected a binder call, got {:?}", value.kind);
    };
    assert_eq!(
        *binder_id as usize,
        hir.functions
            .iter()
            .position(|function| function.name == "<lambda.bind>")
            .expect("binder")
    );
    assert_eq!(args.len(), 2);
    assert!(
        matches!(&args[0].kind, HExprKind::Local(0)),
        "{:?}",
        args[0].kind
    );
    assert!(
        matches!(&args[1].kind, HExprKind::Local(1)),
        "{:?}",
        args[1].kind
    );
}

#[test]
fn lambda_capture_reads_a_rebound_local_through_its_cell() {
    // CPython keeps the binding, not the value: reassigning `n` after the
    // lambda was created is visible to the closure, so the local is a cell.
    let source = "def counter():\n    n = 0\n    peek = lambda: n\n    n = 2\n    return peek()\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let lambda = function(&hir, "<lambda>");
    assert_eq!(lambda.capture_count, 1);
    let HStmtKind::Return(Some(body)) = &lambda.body[0].kind else {
        panic!("expected the lambda return, got {:?}", lambda.body[0]);
    };
    assert!(
        matches!(&body.kind, HExprKind::CellGet(0)),
        "the captured local must be read through its cell, got {:?}",
        body.kind
    );

    // The enclosing function owns the cell and stores through it, so the
    // lambda observes the later assignment.
    let counter = function(&hir, "counter");
    assert!(
        counter
            .body
            .iter()
            .any(|stmt| matches!(&stmt.kind, HStmtKind::CellInit { .. })),
        "{:?}",
        counter.body
    );
    assert!(
        counter
            .body
            .iter()
            .any(|stmt| matches!(&stmt.kind, HStmtKind::CellAssign { .. })),
        "{:?}",
        counter.body
    );
}

#[test]
fn nested_lambdas_close_over_the_same_binding() {
    let source = "def outer(base):\n    return lambda: (lambda: base + 1)()\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let lambdas: Vec<&HFunction> = hir
        .functions
        .iter()
        .filter(|function| function.name == "<lambda>")
        .collect();
    assert_eq!(lambdas.len(), 2, "{:?}", hir.functions.len());
    // The inner lambda captures the outer lambda's captured parameter, so an
    // intermediate scope passes the binding through.
    assert!(lambdas.iter().all(|lambda| lambda.capture_count == 1));
    assert_eq!(
        hir.functions
            .iter()
            .filter(|function| function.name == "<lambda.bind>")
            .count(),
        2
    );
}

#[test]
fn lambda_captures_a_class_instance() {
    let source = "\
class Point:
    def __init__(self, x):
        self.x = x

def make(point):
    return lambda: point.x
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let lambda = function(&hir, "<lambda>");
    assert_eq!(lambda.capture_count, 1);
    assert_eq!(lambda.params[0].name, "point");
    let HStmtKind::Return(Some(body)) = &lambda.body[0].kind else {
        panic!("expected the lambda return, got {:?}", lambda.body[0]);
    };
    let HExprKind::GetAttr { object, name } = &body.kind else {
        panic!("expected an attribute read, got {:?}", body.kind);
    };
    assert_eq!(name, "x");
    assert!(
        matches!(&object.kind, HExprKind::Local(0)),
        "the instance must come from the capture slot, got {:?}",
        object.kind
    );
}

#[test]
fn module_level_lambdas_read_module_globals() {
    // A module body is not a closure scope: the lambda reads the global table,
    // and the module body mirrors the binding for it.
    let source = "x = 1\nf = lambda: x\ny = f()\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let lambda = function(&hir, "<lambda>");
    assert_eq!(lambda.capture_count, 0);
    let HStmtKind::Return(Some(body)) = &lambda.body[0].kind else {
        panic!("expected the lambda return, got {:?}", lambda.body[0]);
    };
    assert!(
        matches!(
            &body.kind,
            HExprKind::GlobalGet { module, name } if module == "__main__" && name == "x"
        ),
        "a module-level lambda reads the module global, got {:?}",
        body.kind
    );
    assert!(
        binds_module_global(&hir.functions[0].body, "x"),
        "the assignment must be mirrored for the lambda: {:?}",
        hir.functions[0].body
    );
}

#[test]
fn lambda_can_still_call_its_parameters_and_named_functions() {
    let source =
        "def double(value):\n    return value * 2\nf = lambda fn: fn(1)\ng = lambda: double(2)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn dynamic_module_member_imports_bind_at_runtime() {
    // NumPy's `numerictypes` installs its scalar classes by writing
    // `globals()[key] = ...`, so `arrayprint` imports names no static export
    // table can list. Their reads must bind to the source module's runtime
    // global table instead of failing the check.
    let source =
        "from numpy._core.numerictypes import complex128, flexible\nx = complex128\ny = flexible(1)\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    lowerer.set_user_modules(HashMap::from([(
        "numpy._core.numerictypes".to_string(),
        dynamic_user_module(
            "numpy._core.numerictypes",
            &[("ScalarType", UserExport::Global)],
        ),
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");

    assert!(
        matches!(
            &assigned(&hir, "x").kind,
            HExprKind::GlobalGet { module, name }
                if module == "numpy._core.numerictypes" && name == "complex128"
        ),
        "an unknown name of a dynamic module reads its global table: {:?}",
        assigned(&hir, "x").kind
    );
    let HExprKind::CallKw { callee, .. } = &assigned(&hir, "y").kind else {
        panic!("expected a call, got {:?}", assigned(&hir, "y").kind);
    };
    assert!(
        matches!(
            &callee.kind,
            HExprKind::GlobalGet { module, name }
                if module == "numpy._core.numerictypes" && name == "flexible"
        ),
        "a dynamic callee is read from the module table too: {:?}",
        callee.kind
    );
}

#[test]
fn dynamic_package_self_import_binds_at_runtime() {
    // A package `__init__.py` that installs names through `globals()` may
    // import its own attributes with `from . import name`; the name is not a
    // static export, so it reads the package's global table.
    let source = "globals()['late'] = 3\nfrom . import late\nx = late\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "pkg/__init__.py", &mut infer);
    lowerer.set_dunder_name("pkg");
    lowerer.set_package("pkg");
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        matches!(
            &assigned(&hir, "x").kind,
            HExprKind::GlobalGet { module, name } if module == "pkg" && name == "late"
        ),
        "the package reads its own dynamic attribute: {:?}",
        assigned(&hir, "x").kind
    );
}

#[test]
fn ordinary_module_member_typos_still_fail_at_compile_time() {
    // A module that never writes its namespace keeps the static diagnostic.
    let source = "from helper import missing\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    lowerer.set_user_modules(HashMap::from([(
        "helper".to_string(),
        user_module("helper", &[("present", UserExport::Global)]),
    )]));
    let _ = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::ATTRIBUTE);
    assert!(
        diags[0]
            .message
            .contains("does not export a function named 'missing'"),
        "{diags:?}"
    );
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
fn double_star_calls_use_the_runtime_flattener() {
    let source = "def f(**kwargs):\n    return kwargs\nx = f(**{\"a\": 1})\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::CallFull {
        args,
        star,
        double_star,
        ..
    } = &assigned(&hir, "x").kind
    else {
        panic!(
            "expected a dynamically bound call, got {:?}",
            assigned(&hir, "x").kind
        );
    };
    assert!(args.is_empty());
    assert!(star.is_none());
    assert!(double_star.is_some());
}

#[test]
fn keyword_call_on_a_function_value_uses_the_runtime_binder() {
    let source = "def g(f):\n    return f(a=1)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    let HStmtKind::Return(Some(value)) = &g.body[0].kind else {
        panic!("expected a return, got {:?}", g.body[0].kind);
    };
    let HExprKind::CallKw {
        callee, keywords, ..
    } = &value.kind
    else {
        panic!("expected a keyword call, got {:?}", value.kind);
    };
    assert!(matches!(&callee.kind, HExprKind::Local(_)));
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "a");
}

#[test]
fn keyword_call_on_a_dynamic_callee_keeps_the_keyword_name() {
    // A callee with no static signature is called through the runtime binder,
    // never through a deferred call that would drop the keyword and raise.
    let source = "\
def h(g):
    return g(a=1)
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let h = function(&hir, "h");
    let HStmtKind::Return(Some(value)) = &h.body[0].kind else {
        panic!("expected a return, got {:?}", h.body[0].kind);
    };
    let HExprKind::CallKw {
        callee, keywords, ..
    } = &value.kind
    else {
        panic!("expected a keyword call, got {:?}", value.kind);
    };
    assert!(
        matches!(&callee.kind, HExprKind::Local(_)),
        "callee must be the runtime function value, got {:?}",
        callee.kind
    );
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "a");
}

#[test]
fn double_star_forwarding_keeps_the_mapping_operand() {
    let source = "\
def g(f, kwargs):
    return f(1, **kwargs)
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    let HStmtKind::Return(Some(value)) = &g.body[0].kind else {
        panic!("expected a return, got {:?}", g.body[0].kind);
    };
    let HExprKind::CallFull {
        args,
        star,
        keywords,
        double_star,
        ..
    } = &value.kind
    else {
        panic!("expected a full call, got {:?}", value.kind);
    };
    assert_eq!(args.len(), 1);
    assert!(star.is_none());
    assert!(keywords.is_empty());
    assert!(double_star.is_some());
}

#[test]
fn starred_call_with_keywords_keeps_both_operand_groups() {
    let source = "\
def g(f, args):
    return f(*args, b=2)
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    let HStmtKind::Return(Some(value)) = &g.body[0].kind else {
        panic!("expected a return, got {:?}", g.body[0].kind);
    };
    let HExprKind::CallFull {
        args,
        star,
        keywords,
        double_star,
        ..
    } = &value.kind
    else {
        panic!("expected a full call, got {:?}", value.kind);
    };
    assert!(args.is_empty());
    assert!(star.is_some());
    assert!(double_star.is_none());
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "b");
}

#[test]
fn method_double_star_call_lowers_to_the_method_binder() {
    let source = "\
class C:
    def m(self, a):
        return a
c = C()
x = c.m(**{\"a\": 1})
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::MethodCall {
        args, double_star, ..
    } = &assigned(&hir, "x").kind
    else {
        panic!("expected a method call, got {:?}", assigned(&hir, "x").kind);
    };
    assert!(args.is_empty());
    assert!(double_star.is_some());
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
        bases,
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
    assert!(bases.is_empty(), "{bases:?}");
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
        bases,
        ..
    } = &hir.functions[0].body[1].kind
    else {
        panic!(
            "expected class D with a base, got {:?}",
            hir.functions[0].body[1].kind
        );
    };
    let base = bases.first().expect("class D base");
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
        bases: e_bases,
        ..
    } = &hir.functions[0].body[1].kind
    else {
        panic!("expected class E, got {:?}", hir.functions[0].body[1].kind);
    };
    let e_base = e_bases.first().expect("class E base");
    assert_eq!(e_name, "E");
    assert!(
        matches!(&e_base.kind, HExprKind::ExceptionClass(ExcKind::Exception)),
        "expected the Exception class as base, got {e_base:?}"
    );

    let HStmtKind::ClassDef {
        name: f_name,
        bases: f_bases,
        ..
    } = &hir.functions[0].body[2].kind
    else {
        panic!("expected class F, got {:?}", hir.functions[0].body[2].kind);
    };
    let f_base = f_bases.first().expect("class F base");
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
    let source = "class C:\n    mark = 7\n    ratio: int = 1\n";
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
    assert_eq!(names, vec!["mark", "ratio"]);
    assert!(matches!(
        &attributes[0].1.kind,
        HExprKind::Const(Const::Int(7))
    ));
    assert!(matches!(
        &attributes[1].1.kind,
        HExprKind::Const(Const::Int(1))
    ));

    crate::sema::check(&hir, &mut infer, "test.py").expect("valid program");
    let program = crate::sema::emit(&hir, &mut infer);
    let hydro_ir::StmtKind::ClassDef {
        attributes: emitted,
        ..
    } = &program.functions[0].body[0].kind
    else {
        panic!("expected a class statement");
    };
    assert_eq!(emitted.len(), 2);
    assert_eq!(emitted[0].name, "mark");
}

#[test]
fn chained_class_attribute_assignment_binds_every_name_to_one_value() {
    // `a = b = c = value()` evaluates the right-hand side once, ahead of the
    // class statement, and every name publishes the same parked value. The
    // next attribute reads the earlier names through that value.
    let source = "\
def value():
    return 1
class C:
    a = b = c = value()
    d = a + b
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let locals = &hir.functions[0].locals;
    let block = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::If { then_body, .. }
                if then_body.iter().any(
                    |inner| matches!(&inner.kind, HStmtKind::ClassDef { name, .. } if name == "C"),
                ) =>
            {
                Some(then_body)
            }
            _ => None,
        })
        .expect("class C should be wrapped with its parked values");
    assert_eq!(block.len(), 3, "{block:?}");
    let HStmtKind::Assign { target: shared, .. } = &block[0].kind else {
        panic!(
            "expected the shared value assignment, got {:?}",
            block[0].kind
        );
    };
    assert_eq!(locals[*shared as usize].name, "$classattr0");
    assert!(
        matches!(
            &block[0].kind,
            HStmtKind::Assign { value, .. }
                if matches!(
                    &value.kind,
                    HExprKind::Call { .. } | HExprKind::IndirectCall { .. }
                )
        ),
        "the right-hand side runs in one statement: {:?}",
        block[0].kind
    );
    let HStmtKind::Assign {
        target: derived, ..
    } = &block[1].kind
    else {
        panic!(
            "expected the derived attribute assignment, got {:?}",
            block[1].kind
        );
    };
    assert_eq!(locals[*derived as usize].name, "$classattr1");
    let HStmtKind::ClassDef { attributes, .. } = &block[2].kind else {
        panic!("expected a class statement, got {:?}", block[2].kind);
    };
    let names: Vec<&str> = attributes.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c", "d"]);
    for (_, value) in &attributes[..3] {
        assert_eq!(local_of(value), *shared, "{value:?}");
    }
    assert_eq!(local_of(&attributes[3].1), *derived);
    // The derived attribute is parked like the shared one, so the sum itself
    // lives in the second prelude statement and `d` only publishes it. Both
    // operands resolve `a` and `b` to the one parked value.
    let HStmtKind::Assign { value: sum, .. } = &block[1].kind else {
        panic!(
            "expected the derived attribute assignment, got {:?}",
            block[1].kind
        );
    };
    let HExprKind::Bin { lhs, rhs, .. } = &sum.kind else {
        panic!(
            "expected `a + b` to add the parked value, got {:?}",
            sum.kind
        );
    };
    assert_eq!(local_of(lhs), *shared);
    assert_eq!(local_of(rhs), *shared);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn chained_class_assignment_uses_the_namespace_on_the_dynamic_path() {
    // A class body with a non-name assignment target runs against the runtime
    // namespace, where a chain stores each name from one shared value.
    let source = "\
class C:
    items = [1, 2]
    items[0] = 9
    a = b = items
";
    let module = parse(source);
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    let hir = lowerer.lower_module(&module);
    let diags = std::mem::take(&mut lowerer.diags);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        class_plan(&lowerer, &module, "C").dynamic,
        "a subscript target needs the runtime namespace"
    );
    drop(lowerer);
    let a = namespace_store(&hir.functions[0].body, "a").expect("namespace store for a");
    let b = namespace_store(&hir.functions[0].body, "b").expect("namespace store for b");
    assert_eq!(local_of(a), local_of(b));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
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
    let module = parse(source);
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    let hir = lowerer.lower_module(&module);
    let diags = std::mem::take(&mut lowerer.diags);
    assert!(diags.is_empty(), "{diags:?}");
    // A lone descriptor-factory decorator (and the known no-op markers) is
    // fully described by the binding kind: the class keeps the static
    // `ClassDef` shape instead of executing its body through a namespace.
    assert!(
        !class_plan(&lowerer, &module, "C").dynamic,
        "binding-kind decorators keep the static class path"
    );
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

/// The value stored in a dynamically-built class namespace for `name`, with
/// the `if True:`/control-flow wrappers peeled away.
fn namespace_store<'a>(stmts: &'a [HStmt], name: &str) -> Option<&'a HExpr> {
    for stmt in stmts {
        match &stmt.kind {
            HStmtKind::SetSubscript { index, value, .. } => {
                if matches!(&index.kind, HExprKind::Const(Const::Str(key)) if key == name) {
                    return Some(value);
                }
            }
            HStmtKind::If {
                then_body,
                else_body,
                ..
            }
            | HStmtKind::While {
                body: then_body,
                else_body,
                ..
            } => {
                if let Some(value) =
                    namespace_store(then_body, name).or_else(|| namespace_store(else_body, name))
                {
                    return Some(value);
                }
            }
            HStmtKind::ForRange {
                body, else_body, ..
            }
            | HStmtKind::ForEach {
                body, else_body, ..
            } => {
                if let Some(value) =
                    namespace_store(body, name).or_else(|| namespace_store(else_body, name))
                {
                    return Some(value);
                }
            }
            HStmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => {
                let found = namespace_store(body, name)
                    .or_else(|| {
                        handlers
                            .iter()
                            .find_map(|handler| namespace_store(&handler.body, name))
                    })
                    .or_else(|| namespace_store(else_body, name))
                    .or_else(|| namespace_store(finally_body, name));
                if found.is_some() {
                    return found;
                }
            }
            _ => {}
        }
    }
    None
}

/// The plan registered for the `class` statement in `module.body`.
fn class_plan<'a>(lowerer: &'a Lowerer<'_>, module: &ast::ModModule, name: &str) -> &'a ClassPlan {
    let class = module
        .body
        .iter()
        .find_map(|stmt| match stmt {
            ast::Stmt::ClassDef(class) if class.name.as_str() == name => Some(class),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no class '{name}'"));
    lowerer
        .class_defs
        .get(&(u32::from(class.range.start()) as usize))
        .unwrap_or_else(|| panic!("class '{name}' was not registered"))
}

/// Names of a planned method's runtime decorators, outermost first.
fn planned_decorator_names(method: &PlannedMethod) -> Vec<String> {
    method
        .decorators
        .iter()
        .map(|decorator| decorator_final_name(decorator).unwrap_or_default())
        .collect()
}

#[test]
fn method_decorators_apply_bottom_up_and_keep_classmethod_binding() {
    let source = "\
def logged(fn=None):
    def decorate(inner):
        return inner
    if fn is None:
        return decorate
    return fn

class C:
    @logged
    @classmethod
    def decorated_classmethod(cls):
        return 1

    @classmethod
    @logged
    def classmethod_last(cls):
        return 2

    @logged()
    def with_arguments(self):
        return 3

    @staticmethod
    @logged
    def decorated_static():
        return 4
";
    let module = parse(source);
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    let hir = lowerer.lower_module(&module);
    let diags = std::mem::take(&mut lowerer.diags);
    assert!(diags.is_empty(), "{diags:?}");

    let plan = class_plan(&lowerer, &module, "C");
    assert!(plan.dynamic, "decorated methods need the runtime body");
    let method = |name: &str| {
        plan.methods
            .iter()
            .find(|method| method.name == name)
            .unwrap_or_else(|| panic!("no method '{name}'"))
    };
    assert_eq!(
        planned_decorator_names(method("decorated_classmethod")),
        vec!["logged", "classmethod"]
    );
    assert_eq!(
        method("decorated_classmethod").kind,
        MethodKind::Classmethod
    );
    assert_eq!(
        planned_decorator_names(method("classmethod_last")),
        vec!["classmethod", "logged"]
    );
    assert_eq!(
        planned_decorator_names(method("with_arguments")),
        vec!["logged"]
    );
    assert_eq!(
        planned_decorator_names(method("decorated_static")),
        vec!["staticmethod", "logged"]
    );

    // `@logged @classmethod` applies `@logged` to the classmethod object:
    // the stored value is `logged(classmethod(function))`.
    let store = namespace_store(&hir.functions[0].body, "decorated_classmethod")
        .expect("class namespace stores the decorated method");
    let HExprKind::IndirectCall { args, .. } = &store.kind else {
        panic!("expected the decorator call, got {:?}", store.kind);
    };
    assert_eq!(args.len(), 1);
    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Classmethod),
        args: inner,
    } = &args[0].kind
    else {
        panic!(
            "expected classmethod(...) below @logged, got {:?}",
            args[0].kind
        );
    };
    assert!(matches!(inner[0].kind, HExprKind::FunctionRef(_)));

    // `@classmethod @logged` stores `classmethod(logged(function))`.
    let store = namespace_store(&hir.functions[0].body, "classmethod_last")
        .expect("class namespace stores the decorated method");
    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Classmethod),
        args,
    } = &store.kind
    else {
        panic!("expected classmethod(...) outermost, got {:?}", store.kind);
    };
    assert!(matches!(args[0].kind, HExprKind::IndirectCall { .. }));

    // `@logged()` evaluates its call expression once and applies the result to
    // the function; it is not lowered as `logged(function)`.
    let store = namespace_store(&hir.functions[0].body, "with_arguments")
        .expect("class namespace stores the argument-decorated method");
    let HExprKind::IndirectCall { callee, args, .. } = &store.kind else {
        panic!("expected logged()(function), got {:?}", store.kind);
    };
    assert!(matches!(&callee.kind, HExprKind::Local(_)));
    assert!(matches!(args[0].kind, HExprKind::FunctionRef(_)));
}

#[test]
fn method_decorator_expressions_evaluate_top_down_and_apply_bottom_up() {
    let source = "\
def outer(fn):
    return fn

def inner(fn):
    return fn

class C:
    @outer
    @inner
    def method(self):
        return 0
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let module = &hir.functions[0];

    // CPython evaluates `outer` before `inner`, so their hidden locals are
    // emitted in that order.
    fn collect_decorator_assigns(stmts: &[HStmt], locals: &[HLocal], out: &mut Vec<LocalId>) {
        for stmt in stmts {
            match &stmt.kind {
                HStmtKind::Assign { target, .. }
                    if locals[*target as usize].name.starts_with("$decor") =>
                {
                    out.push(*target);
                }
                HStmtKind::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    collect_decorator_assigns(then_body, locals, out);
                    collect_decorator_assigns(else_body, locals, out);
                }
                _ => {}
            }
        }
    }
    let mut evaluated = Vec::new();
    collect_decorator_assigns(&module.body, &module.locals, &mut evaluated);
    let names: Vec<&str> = evaluated
        .iter()
        .map(|local| module.locals[*local as usize].name.as_str())
        .collect();
    assert_eq!(names, vec!["$decor0", "$decor1"]);

    // The function is transformed bottom-up: the last-evaluated decorator
    // (`inner`, `$decor1`) is applied first, then `outer` (`$decor0`).
    let store = namespace_store(&module.body, "method").expect("class namespace store");
    let HExprKind::IndirectCall { callee, args, .. } = &store.kind else {
        panic!("expected outer(inner(function)), got {:?}", store.kind);
    };
    let HExprKind::Local(outer_local) = &callee.kind else {
        panic!(
            "expected the evaluated 'outer' local, got {:?}",
            callee.kind
        );
    };
    assert_eq!(module.locals[*outer_local as usize].name, "$decor0");
    let HExprKind::IndirectCall {
        callee,
        args: inner_args,
        ..
    } = &args[0].kind
    else {
        panic!("expected the 'inner' application, got {:?}", args[0].kind);
    };
    let HExprKind::Local(inner_local) = &callee.kind else {
        panic!(
            "expected the evaluated 'inner' local, got {:?}",
            callee.kind
        );
    };
    assert_eq!(module.locals[*inner_local as usize].name, "$decor1");
    assert!(matches!(inner_args[0].kind, HExprKind::FunctionRef(_)));
}

#[test]
fn decorated_init_defers_constructor_binding_to_runtime() {
    let source = "\
def logged(fn):
    return fn

class C:
    @logged
    def __init__(self, value):
        self.value = value
";
    let module = parse(source);
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    let hir = lowerer.lower_module(&module);
    let diags = std::mem::take(&mut lowerer.diags);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        !lowerer.class_inits.contains_key("C"),
        "a decorated __init__ cannot be called through the static constructor path"
    );
    let store = namespace_store(&hir.functions[0].body, "__init__")
        .expect("class namespace stores the decorated initializer");
    assert!(
        matches!(store.kind, HExprKind::IndirectCall { .. }),
        "{:?}",
        store.kind
    );
}

#[test]
fn nested_class_methods_capture_enclosing_locals() {
    let source = "\
def outer():
    base = 1
    class Inner:
        def get(self):
            return base
        def bump(self):
            nonlocal base
            base += 1
    return Inner
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let outer = function(&hir, "outer");
    // `base` is read and rebound through the nested class, so the enclosing
    // function owns a cell for it.
    assert!(
        outer
            .body
            .iter()
            .any(|stmt| matches!(&stmt.kind, HStmtKind::CellInit { .. })),
        "expected a cell for 'base': {:?}",
        outer.body
    );
    let capture = function(&hir, "Inner.get");
    assert_eq!(capture.capture_count, 1);
    assert_eq!(capture.params[0].name, "base");
    let bump = function(&hir, "Inner.bump");
    assert_eq!(bump.capture_count, 1);
    assert_eq!(bump.params[0].name, "base");
    // The class itself is bound to the function's local slot, not a global.
    assert!(
        outer
            .body
            .iter()
            .any(|stmt| matches!(&stmt.kind, HStmtKind::If { .. })),
        "expected the dynamic class statement: {:?}",
        outer.body
    );
    assert!(!outer
        .body
        .iter()
        .any(|stmt| matches!(&stmt.kind, HStmtKind::GlobalSet { .. })));
}

#[test]
fn setattr_and_delattr_lower_to_builtins_calls() {
    let source = "\
class C:
    pass

c = C()
setattr(c, 'value', 1)
delattr(c, 'value')
alias = setattr
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let mut setattr_calls = 0;
    let mut delattr_calls = 0;
    for stmt in &hir.functions[0].body {
        let HStmtKind::Expr(expr) = &stmt.kind else {
            continue;
        };
        let HExprKind::ModuleCall {
            module, function, ..
        } = &expr.kind
        else {
            continue;
        };
        assert_eq!(module, "builtins");
        match function.as_str() {
            "setattr" => setattr_calls += 1,
            "delattr" => delattr_calls += 1,
            other => panic!("unexpected module call '{other}'"),
        }
    }
    assert_eq!(
        (setattr_calls, delattr_calls),
        (1, 1),
        "{:?}",
        hir.functions[0].body
    );
    let alias = assigned(&hir, "alias");
    assert!(
        matches!(
            &alias.kind,
            HExprKind::ModuleAttr { module, name } if module == "builtins" && name == "setattr"
        ),
        "{:?}",
        alias.kind
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
        bases,
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
    assert_eq!(bases.len(), 1, "{bases:?}");
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
fn rich_repr_class_decorator_is_diagnosed() {
    // `@rich_repr` installs a generated `__repr__` in CPython. Hydro cannot
    // synthesize that method, so the frontend reports it instead of silently
    // dropping the decorator.
    let source = "\
@rich_repr
class C:
    pass
@rich.repr.rich_repr
class D:
    pass
";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 2, "{diags:?}");
    assert!(
        diags.iter().all(|diag| diag.message.contains("rich_repr")),
        "{diags:?}"
    );
}

#[test]
fn unresolved_class_decorator_name_is_diagnosed() {
    // Class decorators are now applied at runtime, so an unknown name is an
    // ordinary unresolved-name diagnostic instead of a blanket rejection.
    let source = "@dec\nclass C:\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("not defined"),
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
fn async_generator_keeps_both_function_flags() {
    let source = "async def g():\n    yield 1\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    assert!(g.is_async);
    assert!(g.is_generator);
    assert!(matches!(
        &g.body[0].kind,
        HStmtKind::Yield { value: Some(_) }
    ));
}

/// Unwraps the always-true `if` block that keeps a multi-statement lowering a
/// single HIR statement.
fn block_body(stmt: &HStmt) -> &[HStmt] {
    let HStmtKind::If {
        cond,
        then_body,
        else_body,
    } = &stmt.kind
    else {
        panic!("expected an always-true block wrapper, got {:?}", stmt.kind);
    };
    assert!(matches!(cond.kind, HExprKind::Const(Const::Bool(true))));
    assert!(else_body.is_empty());
    then_body
}

/// Method name called by an `await obj.method(...)` expression.
fn awaited_method(expr: &HExpr, expected: &str) {
    let HExprKind::Await { value } = &expr.kind else {
        panic!("expected await, got {:?}", expr.kind);
    };
    let HExprKind::MethodCall { method, .. } = &value.kind else {
        panic!("expected a method call, got {:?}", value.kind);
    };
    assert_eq!(method, expected);
}

#[test]
fn async_for_awaits_anext_and_catches_stop_async_iteration() {
    let source = "async def f(it):\n    async for x in it:\n        print(x)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let stmts = block_body(&f.body[0]);

    // `$aiter = it.__aiter__()`
    let HStmtKind::Assign { target, value } = &stmts[0].kind else {
        panic!("expected the iterator binding, got {:?}", stmts[0].kind);
    };
    assert!(f.locals[*target as usize].name.starts_with("$aiter"));
    let HExprKind::MethodCall { method, args, .. } = &value.kind else {
        panic!("expected __aiter__(), got {:?}", value.kind);
    };
    assert_eq!(method, "__aiter__");
    assert!(args.is_empty());

    // `while True: try: x = await $aiter.__anext__() except ...: break`
    let HStmtKind::While {
        cond,
        body,
        else_body,
    } = &stmts[1].kind
    else {
        panic!("expected the iteration loop, got {:?}", stmts[1].kind);
    };
    assert!(matches!(cond.kind, HExprKind::Const(Const::Bool(true))));
    assert!(else_body.is_empty());
    let HStmtKind::Try {
        body: fetch,
        handlers,
        else_body: try_else,
        finally_body,
    } = &body[0].kind
    else {
        panic!("expected the __anext__ try, got {:?}", body[0].kind);
    };
    assert!(try_else.is_empty());
    assert!(finally_body.is_empty());
    assert_eq!(handlers.len(), 1);
    assert_eq!(handlers[0].exc, Some(ExcKind::StopAsyncIteration));
    assert_eq!(
        handlers[0].exc.map(ExcKind::tag),
        Some(18),
        "StopAsyncIteration keeps the stable tag"
    );
    assert!(matches!(
        handlers[0].body.as_slice(),
        [HStmt {
            kind: HStmtKind::Break,
            ..
        }]
    ));

    let HStmtKind::Assign { target, value } = &fetch[0].kind else {
        panic!("expected the __anext__ assignment, got {:?}", fetch[0].kind);
    };
    assert_eq!(f.locals[*target as usize].name, "x");
    let HExprKind::Await { value } = &value.kind else {
        panic!("expected the awaited __anext__(), got {:?}", value.kind);
    };
    let HExprKind::MethodCall { method, .. } = &value.kind else {
        panic!("expected __anext__(), got {:?}", value.kind);
    };
    assert_eq!(method, "__anext__");

    // The body runs after the fetch.
    assert!(matches!(&body[1].kind, HStmtKind::Expr(_)));
}

#[test]
fn async_for_else_runs_only_after_normal_exhaustion() {
    let source =
        "async def f(it):\n    async for x in it:\n        print(x)\n    else:\n        print(0)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let stmts = block_body(&f.body[0]);
    assert_eq!(stmts.len(), 4, "{stmts:?}");

    // `$aloop = False` runs once, before the iterator is created.
    let HStmtKind::Assign { target, value } = &stmts[0].kind else {
        panic!("expected the exhaustion flag, got {:?}", stmts[0].kind);
    };
    assert!(f.locals[*target as usize].name.starts_with("$aloop"));
    assert!(matches!(value.kind, HExprKind::Const(Const::Bool(false))));

    // Exhaustion sets the flag before leaving the loop.
    let HStmtKind::While { body, .. } = &stmts[2].kind else {
        panic!("expected the iteration loop, got {:?}", stmts[2].kind);
    };
    let HStmtKind::Try { handlers, .. } = &body[0].kind else {
        panic!("expected the __anext__ try, got {:?}", body[0].kind);
    };
    assert_eq!(handlers[0].exc, Some(ExcKind::StopAsyncIteration));
    let HStmtKind::Assign {
        target: flag_target,
        value: flag_value,
    } = &handlers[0].body[0].kind
    else {
        panic!("expected the exhaustion flag assignment");
    };
    assert_eq!(flag_target, target);
    assert!(matches!(
        flag_value.kind,
        HExprKind::Const(Const::Bool(true))
    ));
    assert!(matches!(handlers[0].body[1].kind, HStmtKind::Break));

    // `if $aloop: <else body>` follows the loop, outside it.
    let HStmtKind::If {
        cond,
        then_body,
        else_body,
    } = &stmts[3].kind
    else {
        panic!("expected the else guard, got {:?}", stmts[3].kind);
    };
    assert!(matches!(cond.kind, HExprKind::Local(local) if local == *target));
    assert!(else_body.is_empty());
    assert!(matches!(then_body[0].kind, HStmtKind::Expr(_)));
}

#[test]
fn async_with_awaits_aenter_and_guards_aexit() {
    let source = "async def f(cm):\n    async with cm as value:\n        print(value)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let stmts = block_body(&f.body[0]);
    assert_eq!(stmts.len(), 4, "{stmts:?}");

    // `$with = cm`, `$aexc = None`, `value = await $with.__aenter__()`.
    let HStmtKind::Assign {
        target: manager, ..
    } = &stmts[0].kind
    else {
        panic!("expected the manager binding, got {:?}", stmts[0].kind);
    };
    assert!(f.locals[*manager as usize].name.starts_with("$with"));
    let HStmtKind::Assign { target: exc, value } = &stmts[1].kind else {
        panic!("expected the exception slot, got {:?}", stmts[1].kind);
    };
    assert!(f.locals[*exc as usize].name.starts_with("$aexc"));
    assert!(matches!(value.kind, HExprKind::Const(Const::None)));
    let HStmtKind::Assign {
        target: bound,
        value,
    } = &stmts[2].kind
    else {
        panic!("expected the as binding, got {:?}", stmts[2].kind);
    };
    assert_eq!(f.locals[*bound as usize].name, "value");
    awaited_method(value, "__aenter__");

    let HStmtKind::Try {
        body,
        handlers,
        else_body,
        finally_body,
    } = &stmts[3].kind
    else {
        panic!("expected the __aexit__ try, got {:?}", stmts[3].kind);
    };
    assert!(matches!(body[0].kind, HStmtKind::Expr(_)));
    assert!(else_body.is_empty());
    assert_eq!(handlers.len(), 1);
    assert_eq!(handlers[0].exc, Some(ExcKind::BaseException));
    assert_eq!(handlers[0].binding, Some(*exc));

    // `if not await __aexit__(type(exc), exc, None): raise`
    let HStmtKind::If {
        cond, then_body, ..
    } = &handlers[0].body[0].kind
    else {
        panic!("expected the suppression check");
    };
    let HExprKind::Un { op, operand } = &cond.kind else {
        panic!("expected `not __aexit__(...)`, got {:?}", cond.kind);
    };
    assert_eq!(*op, hydro_ir::UnOp::Not);
    awaited_method(operand, "__aexit__");
    let HExprKind::Await { value: exit } = &operand.kind else {
        panic!("expected an await");
    };
    let HExprKind::MethodCall { args, .. } = &exit.kind else {
        panic!("expected a method call");
    };
    assert_eq!(args.len(), 3);
    assert!(matches!(
        &args[0].kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Type),
            ..
        }
    ));
    assert!(matches!(&args[1].kind, HExprKind::Local(local) if local == exc));
    assert!(matches!(&args[2].kind, HExprKind::Const(Const::None)));
    assert!(matches!(
        &then_body[0].kind,
        HStmtKind::Raise { exc: None, .. }
    ));

    // `finally: if $aexc is None: await __aexit__(None, None, None)`
    assert_eq!(finally_body.len(), 1);
    let HStmtKind::If {
        cond, then_body, ..
    } = &finally_body[0].kind
    else {
        panic!("expected the normal-exit guard");
    };
    let HExprKind::Bin { op, lhs, rhs } = &cond.kind else {
        panic!("expected `$aexc is None`, got {:?}", cond.kind);
    };
    assert_eq!(*op, BinOp::Is);
    assert!(matches!(&lhs.kind, HExprKind::Local(local) if local == exc));
    assert!(matches!(&rhs.kind, HExprKind::Const(Const::None)));
    let HStmtKind::Expr(exit) = &then_body[0].kind else {
        panic!("expected the normal exit call");
    };
    let HExprKind::Await { value } = &exit.kind else {
        panic!("expected an await");
    };
    let HExprKind::MethodCall { method, args, .. } = &value.kind else {
        panic!("expected a method call");
    };
    assert_eq!(method, "__aexit__");
    assert_eq!(args.len(), 3);
    assert!(args
        .iter()
        .all(|arg| matches!(arg.kind, HExprKind::Const(Const::None))));
}

#[test]
fn async_with_stacks_managers_in_reverse_order() {
    let source = "async def f(a, b):\n    async with a as x, b as y:\n        pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let stmts = block_body(&f.body[0]);

    // The outer manager is `a` (first written, first entered).
    let HStmtKind::Assign { value, .. } = &stmts[0].kind else {
        panic!("expected the first manager binding");
    };
    let HExprKind::Local(first_manager) = &value.kind else {
        panic!("expected `a`, got {:?}", value.kind);
    };
    let first_param = f
        .locals
        .iter()
        .position(|local| local.name == "a")
        .expect("parameter a");
    assert_eq!(*first_manager as usize, first_param);

    // The inner Try's body begins with the second manager's prelude.
    let HStmtKind::Try { body, .. } = &stmts[stmts.len() - 1].kind else {
        panic!("expected the outer try");
    };
    let HStmtKind::Assign { value, .. } = &body[0].kind else {
        panic!("expected the second manager binding");
    };
    let HExprKind::Local(second_manager) = &value.kind else {
        panic!("expected `b`, got {:?}", value.kind);
    };
    let second_param = f
        .locals
        .iter()
        .position(|local| local.name == "b")
        .expect("parameter b");
    assert_eq!(*second_manager as usize, second_param);
}

/// True when `stmts` or any statement nested inside them matches `pred`.
fn nested_stmt_any(stmts: &[HStmt], pred: &dyn Fn(&HStmtKind) -> bool) -> bool {
    stmts.iter().any(|stmt| {
        if pred(&stmt.kind) {
            return true;
        }
        match &stmt.kind {
            HStmtKind::If {
                then_body,
                else_body,
                ..
            } => nested_stmt_any(then_body, pred) || nested_stmt_any(else_body, pred),
            HStmtKind::While {
                body, else_body, ..
            }
            | HStmtKind::ForEach {
                body, else_body, ..
            }
            | HStmtKind::ForRange {
                body, else_body, ..
            } => nested_stmt_any(body, pred) || nested_stmt_any(else_body, pred),
            HStmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => {
                nested_stmt_any(body, pred)
                    || handlers
                        .iter()
                        .any(|handler| nested_stmt_any(&handler.body, pred))
                    || nested_stmt_any(else_body, pred)
                    || nested_stmt_any(finally_body, pred)
            }
            _ => false,
        }
    })
}

fn catches_stop_async_iteration(kind: &HStmtKind) -> bool {
    matches!(
        kind,
        HStmtKind::Try { handlers, .. }
            if handlers
                .iter()
                .any(|handler| handler.exc == Some(ExcKind::StopAsyncIteration))
    )
}

#[test]
fn async_list_comprehension_lowers_to_an_awaited_coroutine() {
    let source = "async def f(it):\n    return [x async for x in it]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let HStmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!(
            "expected the returned comprehension, got {:?}",
            f.body[0].kind
        );
    };
    assert_eq!(value.ty, Ty::Concrete(Type::List));
    let HExprKind::Await { value: call } = &value.kind else {
        panic!("expected an awaited helper, got {:?}", value.kind);
    };
    let HExprKind::Call {
        callee: Callee::Function(id),
        args,
    } = &call.kind
    else {
        panic!("expected a helper call, got {:?}", call.kind);
    };
    assert_eq!(args.len(), 1, "the outermost iterable is passed eagerly");

    let helper = &hir.functions[*id as usize];
    assert_eq!(helper.name, "<asynccomp>");
    assert!(helper.is_async);
    assert!(!helper.is_generator);
    assert!(matches!(
        helper.body.first().map(|stmt| &stmt.kind),
        Some(HStmtKind::Assign { .. })
    ));
    assert!(matches!(
        helper.body.last().map(|stmt| &stmt.kind),
        Some(HStmtKind::Return(Some(_)))
    ));
    assert!(
        nested_stmt_any(&helper.body, &catches_stop_async_iteration),
        "the helper handles the end of the async iterator"
    );
    assert!(nested_stmt_any(&helper.body, &|kind| matches!(
        kind,
        HStmtKind::Expr(HExpr {
            kind: HExprKind::MethodCall { method, .. },
            ..
        }) if method == "append"
    )));
    assert_eq!(helper.ret_ty, Ty::Concrete(Type::Any));
}

#[test]
fn async_generator_expression_is_an_async_generator() {
    let source = "async def f(it):\n    return (x async for x in it)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let HStmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!("expected the returned generator expression");
    };
    let HExprKind::Call {
        callee: Callee::Function(id),
        ..
    } = &value.kind
    else {
        panic!("expected the generator function call, got {:?}", value.kind);
    };
    let helper = &hir.functions[*id as usize];
    assert_eq!(helper.name, "<genexpr>");
    assert!(helper.is_async);
    assert!(helper.is_generator, "async generator expressions stay lazy");
    assert!(nested_stmt_any(&helper.body, &|kind| matches!(
        kind,
        HStmtKind::Yield { .. }
    )));
    assert!(nested_stmt_any(&helper.body, &catches_stop_async_iteration));
}

#[test]
fn generator_expression_with_await_is_an_async_generator() {
    let source =
        "import asyncio\nasync def f(xs):\n    return (await asyncio.sleep(0) for x in xs)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    let HStmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!("expected the returned generator expression");
    };
    let HExprKind::Call {
        callee: Callee::Function(id),
        ..
    } = &value.kind
    else {
        panic!("expected the generator function call, got {:?}", value.kind);
    };
    let helper = &hir.functions[*id as usize];
    assert_eq!(helper.name, "<genexpr>");
    assert!(helper.is_async, "a frame that awaits is an async generator");
    assert!(helper.is_generator);
    assert!(nested_stmt_any(&helper.body, &|kind| matches!(
        kind,
        HStmtKind::Yield { value: Some(value) }
            if matches!(value.kind, HExprKind::Await { .. })
    )));
}

#[test]
fn await_inside_a_list_comprehension_stays_in_the_enclosing_coroutine() {
    let source =
        "import asyncio\nasync def f(xs):\n    return [await asyncio.sleep(0) for x in xs]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    // No synthetic coroutine: the comprehension is eager and its element
    // awaits directly in `f`.
    assert_eq!(hir.functions.len(), 2, "only the module body and f");
    let f = function(&hir, "f");
    let HStmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!("expected the returned list comprehension");
    };
    let HExprKind::ListComp { element, .. } = &value.kind else {
        panic!("expected a list comprehension, got {:?}", value.kind);
    };
    assert!(matches!(element.kind, HExprKind::Await { .. }));
    assert!(hir
        .functions
        .iter()
        .all(|function| function.name != "<asynccomp>"));
}

#[test]
fn async_comprehension_in_a_sync_function_is_rejected() {
    let source = "def f(it):\n    return [x async for x in it]\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0].message,
        "asynchronous comprehension outside of an asynchronous function"
    );
    assert_eq!(diags[0].code, codes::CONTROL);
}

#[test]
fn async_for_and_with_outside_async_functions_are_rejected() {
    for (source, expected) in [
        (
            "async for x in y:\n    pass\n",
            "'async for' is only allowed inside async functions",
        ),
        (
            "async with y:\n    pass\n",
            "'async with' is only allowed inside async functions",
        ),
    ] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert_eq!(diags.len(), 1, "{source}: {diags:?}");
        assert_eq!(diags[0].message, expected, "{source}");
        assert_eq!(diags[0].code, codes::CONTROL, "{source}");
    }
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

/// Asserts that `expr` calls the function value held in `local`'s closure
/// cell. A plain positional call to a name bound to a function value lowers
/// either to the historic `IndirectCall` node or to the keyword-capable
/// `CallKw` node with no keywords; both evaluate the callee as the `CellGet`
/// read. A statically linked callee keeps the direct `Call` shape.
fn assert_call_through_cell(expr: &HExpr, local: LocalId, context: &str) {
    let callee = match &expr.kind {
        HExprKind::IndirectCall { callee, .. } => callee,
        HExprKind::CallKw {
            callee, keywords, ..
        } if keywords.is_empty() => callee,
        HExprKind::Call { .. } => return,
        other => panic!("{context}: expected a call, got {other:?}"),
    };
    assert!(
        matches!(callee.kind, HExprKind::CellGet(slot) if slot == local),
        "{context}: the call must read the cell, got {:?}",
        callee.kind
    );
}

#[test]
fn recursive_nested_function_shares_a_cell_with_its_own_closure() {
    let source = "\
def make(n):
    def fact(k):
        if k <= 1:
            return 1
        return k * fact(k - 1)
    return fact(n)
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let outer = function(&hir, "make");
    let fact_local = outer
        .locals
        .iter()
        .position(|local| local.name == "fact")
        .expect("nested function local fact") as LocalId;

    // The `def` binding is a cell allocated at entry, so the closure can
    // capture it before it holds anything and the definition fills it in.
    assert!(
        matches!(
            &outer.body[0].kind,
            HStmtKind::CellInit { target, value } if *target == fact_local && value.is_none()
        ),
        "expected the entry cell for fact, got {:?}",
        outer.body[0].kind
    );

    let HStmtKind::If { then_body, .. } = &outer.body[1].kind else {
        panic!(
            "expected the definition block, got {:?}",
            outer.body[1].kind
        );
    };
    let HStmtKind::Closure {
        target,
        function: fid,
        captures,
    } = &then_body[0].kind
    else {
        panic!("expected the closure, got {:?}", then_body[0].kind);
    };
    assert_ne!(
        *target, fact_local,
        "the closure value must not overwrite the cell in its slot"
    );
    assert_eq!(captures, &vec![fact_local]);
    assert!(
        matches!(&then_body[1].kind, HStmtKind::CellAssign { target, .. } if *target == fact_local),
        "the definition must store through the cell, got {:?}",
        then_body[1].kind
    );

    // The enclosing scope reads the recursive name through the same cell.
    let HStmtKind::Return(Some(value)) = &outer.body[2].kind else {
        panic!("expected the returned call, got {:?}", outer.body[2].kind);
    };
    assert_call_through_cell(value, fact_local, "enclosing recursive call");

    // Inside the nested function the recursive call reads the captured cell.
    let fact = &hir.functions[*fid as usize];
    assert_eq!(fact.capture_count, 1);
    assert_eq!(fact.params[0].name, "fact");
    let HStmtKind::Return(Some(value)) = &fact.body[1].kind else {
        panic!("expected the recursive return, got {:?}", fact.body[1].kind);
    };
    let HExprKind::Bin { rhs, .. } = &value.kind else {
        panic!("expected the recursive product, got {:?}", value.kind);
    };
    assert_call_through_cell(rhs, fact.params[0].local, "recursive call");
}

#[test]
fn recursive_nested_function_observes_the_binding_after_the_outer_returns() {
    let source = "\
def build():
    def countdown(n):
        if n <= 0:
            return 0
        return countdown(n - 1) + 1
    return countdown
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let outer = function(&hir, "build");
    let countdown_local = outer
        .locals
        .iter()
        .position(|local| local.name == "countdown")
        .expect("nested function local countdown") as LocalId;
    assert!(
        matches!(&outer.body[0].kind, HStmtKind::CellInit { target, .. } if *target == countdown_local),
        "expected the entry cell, got {:?}",
        outer.body[0].kind
    );

    // The closure captures the cell itself, and returning the function reads
    // that same cell, so the value created while the outer frame was alive
    // stays reachable after it returns.
    let HStmtKind::If { then_body, .. } = &outer.body[1].kind else {
        panic!(
            "expected the definition block, got {:?}",
            outer.body[1].kind
        );
    };
    let HStmtKind::Closure { captures, .. } = &then_body[0].kind else {
        panic!("expected the closure, got {:?}", then_body[0].kind);
    };
    assert_eq!(captures, &vec![countdown_local]);

    let HStmtKind::Return(Some(value)) = &outer.body[2].kind else {
        panic!(
            "expected the returned function, got {:?}",
            outer.body[2].kind
        );
    };
    assert!(
        matches!(value.kind, HExprKind::CellGet(local) if local == countdown_local),
        "returning the recursive function must read the cell, got {:?}",
        value.kind
    );

    let countdown = function(&hir, "build.countdown");
    assert_eq!(countdown.capture_count, 1);
    assert_eq!(countdown.params[0].name, "countdown");
}

#[test]
fn sibling_nested_functions_support_mutual_recursion() {
    let source = "\
def outer(n):
    def even(k):
        if k == 0:
            return True
        return odd(k - 1)
    def odd(k):
        if k == 0:
            return False
        return even(k - 1)
    return even(n)
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let outer = function(&hir, "outer");
    let local = |name: &str| {
        outer
            .locals
            .iter()
            .position(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("no local '{name}'")) as LocalId
    };
    let even_local = local("even");
    let odd_local = local("odd");

    // Both bindings are cells created at entry, so each closure captures the
    // sibling's cell before either `def` ran.
    assert!(
        matches!(&outer.body[0].kind, HStmtKind::CellInit { target, value } if *target == even_local && value.is_none()),
        "expected the entry cell for even, got {:?}",
        outer.body[0].kind
    );
    assert!(
        matches!(&outer.body[1].kind, HStmtKind::CellInit { target, value } if *target == odd_local && value.is_none()),
        "expected the entry cell for odd, got {:?}",
        outer.body[1].kind
    );

    let HStmtKind::If { then_body, .. } = &outer.body[2].kind else {
        panic!("expected the even definition, got {:?}", outer.body[2].kind);
    };
    let HStmtKind::Closure { captures, .. } = &then_body[0].kind else {
        panic!("expected the even closure, got {:?}", then_body[0].kind);
    };
    assert_eq!(captures, &vec![odd_local]);

    let HStmtKind::If { then_body, .. } = &outer.body[3].kind else {
        panic!("expected the odd definition, got {:?}", outer.body[3].kind);
    };
    let HStmtKind::Closure { captures, .. } = &then_body[0].kind else {
        panic!("expected the odd closure, got {:?}", then_body[0].kind);
    };
    assert_eq!(captures, &vec![even_local]);

    let even = function(&hir, "outer.even");
    assert_eq!(even.capture_count, 1);
    assert_eq!(even.params[0].name, "odd");
    let odd = function(&hir, "outer.odd");
    assert_eq!(odd.capture_count, 1);
    assert_eq!(odd.params[0].name, "even");

    let HStmtKind::Return(Some(value)) = &even.body[1].kind else {
        panic!("expected the sibling call, got {:?}", even.body[1].kind);
    };
    assert_call_through_cell(value, even.params[0].local, "sibling call");
}

#[test]
fn recursive_nested_function_captures_cells_and_values_together() {
    let source = "\
def outer(step):
    count = 0
    def add(n):
        nonlocal count
        count += 1
        if n <= 0:
            return count + step
        return add(n - 1)
    return add
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let outer = function(&hir, "outer");
    let local = |name: &str| {
        outer
            .locals
            .iter()
            .position(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("no local '{name}'")) as LocalId
    };
    let step_local = local("step");
    let count_local = local("count");
    let add_local = local("add");

    // `count` and the recursive name `add` are cells; `step` is read-only and
    // keeps by-value capture, exactly like a closure without recursion.
    assert!(
        matches!(&outer.body[0].kind, HStmtKind::CellInit { target, value } if *target == count_local && value.is_none()),
        "expected the entry cell for count, got {:?}",
        outer.body[0].kind
    );
    assert!(
        matches!(&outer.body[1].kind, HStmtKind::CellInit { target, value } if *target == add_local && value.is_none()),
        "expected the entry cell for add, got {:?}",
        outer.body[1].kind
    );
    let HStmtKind::If { then_body, .. } = &outer.body[3].kind else {
        panic!(
            "expected the definition block, got {:?}",
            outer.body[3].kind
        );
    };
    let HStmtKind::Closure { captures, .. } = &then_body[0].kind else {
        panic!("expected the closure, got {:?}", then_body[0].kind);
    };
    assert_eq!(captures, &vec![count_local, step_local, add_local]);

    let add = function(&hir, "outer.add");
    assert_eq!(add.capture_count, 3);
    let names: Vec<&str> = add.params.iter().map(|param| param.name.as_str()).collect();
    // The capture parameters come first in first-use order. `n` is `add`'s own
    // positional parameter and follows them; the recursive name is reached
    // through the captured cell, never through that parameter.
    assert_eq!(names, vec!["count", "step", "add", "n"]);
    let capture_names: Vec<&str> = names[..add.capture_count].to_vec();
    assert_eq!(capture_names, vec!["count", "step", "add"]);
    assert!(matches!(&add.body[0].kind, HStmtKind::CellAssign { .. }));

    let HStmtKind::If { then_body, .. } = &add.body[1].kind else {
        panic!("expected the base-case branch, got {:?}", add.body[1].kind);
    };
    let HStmtKind::Return(Some(value)) = &then_body[0].kind else {
        panic!("expected the base-case return, got {:?}", then_body[0].kind);
    };
    let HExprKind::Bin { lhs, rhs, .. } = &value.kind else {
        panic!("expected the base-case sum, got {:?}", value.kind);
    };
    assert!(matches!(lhs.kind, HExprKind::CellGet(_)), "{:?}", lhs.kind);
    assert!(
        matches!(rhs.kind, HExprKind::Local(local) if local == add.params[1].local),
        "the read-only capture keeps by-value semantics, got {:?}",
        rhs.kind
    );
}

#[test]
fn recursive_nested_function_in_a_loop_gets_a_fresh_cell_per_iteration() {
    let source = "\
def outer(n):
    fns = []
    for i in range(n):
        def step(k):
            if k <= 0:
                return i
            return step(k - 1)
        fns.append(step)
    return fns
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let outer = function(&hir, "outer");
    let local = |name: &str| {
        outer
            .locals
            .iter()
            .position(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("no local '{name}'")) as LocalId
    };
    let i_local = local("i");
    let step_local = local("step");

    // The loop body re-creates the recursion cell, so a closure from one
    // iteration never shares its binding with the next iteration.
    let HStmtKind::ForRange { body, .. } = &outer.body[2].kind else {
        panic!("expected the range loop, got {:?}", outer.body[2].kind);
    };
    assert!(
        matches!(&body[0].kind, HStmtKind::CellInit { target, value } if *target == step_local && value.is_none()),
        "expected a fresh cell at the top of the iteration, got {:?}",
        body[0].kind
    );
    let HStmtKind::If { then_body, .. } = &body[1].kind else {
        panic!("expected the definition block, got {:?}", body[1].kind);
    };
    let HStmtKind::Closure { captures, .. } = &then_body[0].kind else {
        panic!("expected the step closure, got {:?}", then_body[0].kind);
    };
    // `i` stays a by-value capture of the current iteration; `step` is the
    // per-iteration cell the recursive call reads.
    assert_eq!(captures, &vec![i_local, step_local]);

    let step = function(&hir, "outer.step");
    assert_eq!(step.capture_count, 2);
    let names: Vec<&str> = step
        .params
        .iter()
        .map(|param| param.name.as_str())
        .collect();
    assert_eq!(names, vec!["i", "step", "k"]);
}

#[test]
fn recursive_reference_to_an_unbound_name_is_a_name_error() {
    let source =
        "def outer():\n    def inner(n):\n        return missing(n - 1)\n    return inner\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("name 'missing' is not defined"),
        "{diags:?}"
    );
    assert_eq!(diags[0].code, codes::NAME);
}

#[test]
fn nested_function_rebinding_its_own_name_does_not_capture_a_cell() {
    let source =
        "def outer(v):\n    def inner(n):\n        inner = n\n        return inner\n    return inner\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let outer = function(&hir, "outer");
    let inner_local = outer
        .locals
        .iter()
        .position(|local| local.name == "inner")
        .expect("nested function local inner") as LocalId;
    // The body never reads the enclosing binding, so no cell is needed and the
    // closure value goes straight into the slot.
    assert!(
        matches!(&outer.body[0].kind, HStmtKind::Closure { target, .. } if *target == inner_local),
        "a non-recursive definition keeps the plain local, got {:?}",
        outer.body[0].kind
    );
    let inner = function(&hir, "outer.inner");
    assert_eq!(inner.capture_count, 0);
}

#[test]
fn recursive_nested_function_with_a_global_binding_still_reports_the_slot_error() {
    // `global helper` gives the enclosing function no slot for the `def`, so
    // recursion support cannot invent one; the declaration error stands.
    let source = "def outer():\n    global helper\n    def helper(n):\n        return helper(n - 1)\n    return helper\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]
            .message
            .contains("nested function 'helper' has no local slot"),
        "{diags:?}"
    );
    assert_eq!(diags[0].code, codes::DECL);
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
fn captured_mutations_lower_to_shared_cells() {
    let source = "\ndef outer():\n    x = 0\n    def inc():\n        nonlocal x\n        x += 1\n    def get():\n        return x\n    return inc, get\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let outer = function(&hir, "outer");
    let x_local = outer
        .locals
        .iter()
        .position(|local| local.name == "x")
        .expect("outer x") as LocalId;

    // The first statement allocates the cell that both closures share.
    assert!(
        matches!(&outer.body[0].kind, HStmtKind::CellInit { target, .. } if *target == x_local),
        "expected a cell init, got {:?}",
        outer.body[0].kind
    );
    assert!(matches!(
        &outer.body[1].kind,
        HStmtKind::CellAssign { target, .. } if *target == x_local
    ));

    let HStmtKind::Closure { captures, .. } = &outer.body[2].kind else {
        panic!("expected inc closure, got {:?}", outer.body[2].kind);
    };
    assert_eq!(captures, &vec![x_local]);
    let HStmtKind::Closure { captures, .. } = &outer.body[3].kind else {
        panic!("expected get closure, got {:?}", outer.body[3].kind);
    };
    assert_eq!(captures, &vec![x_local]);

    let inc = function(&hir, "outer.inc");
    assert!(matches!(&inc.body[0].kind, HStmtKind::CellAssign { .. }));
    let get = function(&hir, "outer.get");
    let HStmtKind::Return(Some(value)) = &get.body[0].kind else {
        panic!("expected get return, got {:?}", get.body[0].kind);
    };
    assert!(matches!(value.kind, HExprKind::CellGet(_)));
}

#[test]
fn nonlocal_without_an_enclosing_binding_is_rejected() {
    let source = "\ndef outer():\n    def inner():\n        nonlocal absent\n        absent = 1\n    return inner\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]
            .message
            .contains("no binding for nonlocal 'absent' found"),
        "{diags:?}"
    );
}

#[test]
fn nonlocal_conflicting_with_global_is_rejected() {
    let source = "\ndef outer():\n    x = 1\n    def inner():\n        global x\n        nonlocal x\n    return inner\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("name 'x' is nonlocal and global"),
        "{diags:?}"
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
            "hydro-frontend-ext-{}-{test_name}-{unique}",
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
fn relative_extension_submodule_inside_try_binds_the_loader() {
    // `from . import ext` inside a module-level `try` resolves the extension
    // from the package root (the source file lives inside the package) and
    // binds the lazy loader; the extension is never loaded at compile time, so
    // a load failure stays catchable by the handler.
    let dir = ExtensionDir::new("relative-try-ext");
    let so = dir.add_extension("pkg/ext.abi3.so");
    let expected = fs::canonicalize(&so)
        .expect("canonical extension path")
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(dir.path.join("pkg")).expect("create package dir");
    let source = "try:\n    from . import ext\nexcept ImportError:\n    pass\n\ndef get():\n    return ext.value()\n";
    let filename = dir.path.join("pkg").join("__init__.py");
    fs::write(&filename, source).expect("write package init");

    let mut infer = Infer::new();
    let filename_display = filename.to_string_lossy();
    let mut lowerer = Lowerer::new(source, &filename_display, &mut infer);
    lowerer.set_dunder_name("pkg");
    lowerer.set_package("pkg");
    lowerer.set_deferred_imports(std::collections::HashSet::from(["pkg".to_string()]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");

    let body = function_body(&hir, "get");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    assert_eq!(extension_callee(value), (expected.as_str(), "value"));
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
fn extension_alias_used_as_value_is_a_module() {
    let dir = ExtensionDir::new("alias-value");
    let so = dir.add_extension("mod_ext.abi3.so");
    let expected_path = fs::canonicalize(&so)
        .expect("canonical extension path")
        .to_string_lossy()
        .into_owned();
    let source = "import mod_ext as ext\nx = ext\n";
    let (hir, _, diags) = lower_source_in(&dir.path, source);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::ExtensionModuleRef { module, path } => {
            assert_eq!(
                (module.as_str(), path.as_str()),
                ("mod_ext", expected_path.as_str())
            );
        }
        other => panic!("expected an extension module value, got {other:?}"),
    }
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
fn extension_star_import_binds_all_names_and_falls_back() {
    // A module-level star import of a compiled extension binds the importing
    // unit's literal `__all__` names and leaves every other read as a lazy
    // extension member lookup; nothing is rejected or silently dropped.
    let dir = ExtensionDir::new("star");
    let so = dir.add_extension("mod_ext.abi3.so");
    let expected = fs::canonicalize(&so).expect("canonical extension path");
    let expected_path = expected.to_string_lossy().into_owned();
    let source = r#"__all__ = ['array', 'add']
from mod_ext import *

x = array
y = add(2, 3)
z = helper
"#;
    let (hir, _, diags) = lower_source_in(&dir.path, source);
    assert!(diags.is_empty(), "{diags:?}");

    match &assigned(&hir, "x").kind {
        HExprKind::ExtensionAttr { path, member } => {
            assert_eq!(
                (path.as_str(), member.as_str()),
                (expected_path.as_str(), "array")
            );
        }
        other => panic!("expected an extension member read, got {other:?}"),
    }
    assert_eq!(
        extension_callee(assigned(&hir, "y")),
        (expected_path.as_str(), "add")
    );
    match &assigned(&hir, "z").kind {
        HExprKind::ExtensionAttr { path, member } => {
            assert_eq!(
                (path.as_str(), member.as_str()),
                (expected_path.as_str(), "helper")
            );
        }
        other => panic!("expected the lazy fallback, got {other:?}"),
    }
}

#[test]
fn module_level_try_extension_star_import_is_legal() {
    // A module-level `try` body still runs at module level, so the wildcard
    // form is allowed there; unresolvable names keep the lazy extension
    // fallback instead of a diagnostic.
    let dir = ExtensionDir::new("star-try");
    let so = dir.add_extension("mod_ext.abi3.so");
    let expected = fs::canonicalize(&so).expect("canonical extension path");
    let expected_path = expected.to_string_lossy().into_owned();
    let source = "try:\n    from mod_ext import *\nexcept ImportError:\n    pass\n\nx = helper\n";
    let (hir, _, diags) = lower_source_in(&dir.path, source);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::ExtensionAttr { path, member } => {
            assert_eq!(
                (path.as_str(), member.as_str()),
                (expected_path.as_str(), "helper")
            );
        }
        other => panic!("expected the lazy extension fallback, got {other:?}"),
    }
}

#[test]
fn nested_extension_star_import_is_rejected() {
    // Python only allows `import *` at module level; a function-local star
    // import is a syntax error regardless of the module kind.
    let dir = ExtensionDir::new("nested-star");
    dir.add_extension("mod_ext.abi3.so");
    let source = "def f():\n    from mod_ext import *\n";
    let (_, _, diags) = lower_source_in(&dir.path, source);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::SYNTAX);
    assert_eq!(diags[0].message, "import * only allowed at module level");
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
        "HYDRO_SITE_PACKAGES must override auto-detected venv site-packages"
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
        HExprKind::Const(hydro_ir::Const::Str(name)) => assert_eq!(name, "EDOM"),
        other => panic!("expected a string constant, got {other:?}"),
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn module_alias_lowers_to_a_first_class_module_value() {
    // A compiled user module is a real runtime value: `x = helper` binds the
    // same module object `sys.modules['helper']` holds, so `getattr(helper,
    // name)` and module identity work.
    let source = "import helper\nx = helper\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    lowerer.set_user_modules(HashMap::from([(
        "helper".to_string(),
        UserModule {
            name: "helper".to_string(),
            exports: HashMap::new(),
            all: None,
            dynamic_globals: false,
            extension_stars: Vec::new(),
        },
    )]));
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::ModuleRef { module } => assert_eq!(module, "helper"),
        other => panic!("expected a module reference, got {other:?}"),
    }
}

#[test]
fn extension_module_alias_is_a_first_class_value() {
    // `import mod_ext as ext` binds a real module object: it can be passed to
    // `getattr` (with and without a default), stored, put in a container,
    // returned and compared for identity.
    let dir = ExtensionDir::new("module-value");
    let so = dir.add_extension("mod_ext.abi3.so");
    let expected_path = fs::canonicalize(&so)
        .expect("canonical extension path")
        .to_string_lossy()
        .into_owned();

    let source = r#"import mod_ext as ext
value = getattr(ext, "tag", None)
stored = ext
box = [ext]
def returned():
    return ext
same = ext is ext
"#;
    let (hir, mut infer, diags) = lower_source_in(&dir.path, source);
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::Call { callee, args } = &assigned(&hir, "value").kind else {
        panic!(
            "expected a getattr call, got {:?}",
            assigned(&hir, "value").kind
        );
    };
    assert_eq!(*callee, Callee::Builtin(Builtin::Getattr));
    assert_eq!(args.len(), 3);
    match &args[0].kind {
        HExprKind::ExtensionModuleRef { module, path } => {
            assert_eq!(
                (module.as_str(), path.as_str()),
                ("mod_ext", expected_path.as_str())
            );
        }
        other => panic!("expected an extension module value, got {other:?}"),
    }

    match &assigned(&hir, "stored").kind {
        HExprKind::ExtensionModuleRef { module, path } => {
            assert_eq!(
                (module.as_str(), path.as_str()),
                ("mod_ext", expected_path.as_str()),
                "`stored = ext` must bind the module value"
            );
        }
        other => panic!("expected a module value, got {other:?}"),
    }

    let HExprKind::List(items) = &assigned(&hir, "box").kind else {
        panic!(
            "expected a list literal, got {:?}",
            assigned(&hir, "box").kind
        );
    };
    assert!(matches!(
        &items[0].kind,
        HExprKind::ExtensionModuleRef { module, .. } if module == "mod_ext"
    ));

    let HStmtKind::Return(Some(value)) = &function_body(&hir, "returned")[0].kind else {
        panic!("expected a return statement");
    };
    assert!(matches!(
        &value.kind,
        HExprKind::ExtensionModuleRef { module, .. } if module == "mod_ext"
    ));

    assert!(matches!(
        &assigned(&hir, "same").kind,
        HExprKind::Bin { op: BinOp::Is, .. }
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn user_module_values_store_return_and_compare_by_identity() {
    // A compiled user module is equally ordinary: storing it, returning it
    // from a function and comparing two reads must all lower to module values.
    let source = r#"import helper
stored = helper
box = (helper,)
def returned():
    return helper
def same(a, b):
    return a is b
"#;
    let mut infer = Infer::new();
    let mut lowerer = lowerer_with_user_module(source, &mut infer, "helper", HashMap::new());
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");

    assert!(matches!(
        &assigned(&hir, "stored").kind,
        HExprKind::ModuleRef { module } if module == "helper"
    ));
    let HExprKind::Tuple(items) = &assigned(&hir, "box").kind else {
        panic!(
            "expected a tuple literal, got {:?}",
            assigned(&hir, "box").kind
        );
    };
    assert!(matches!(
        &items[0].kind,
        HExprKind::ModuleRef { module } if module == "helper"
    ));

    let returned = function_body(&hir, "returned");
    let HStmtKind::Return(Some(value)) = &returned[0].kind else {
        panic!("expected a return statement");
    };
    assert!(matches!(
        &value.kind,
        HExprKind::ModuleRef { module } if module == "helper"
    ));
    let same = function_body(&hir, "same");
    let HStmtKind::Return(Some(value)) = &same[0].kind else {
        panic!("expected a return statement");
    };
    assert!(matches!(&value.kind, HExprKind::Bin { op: BinOp::Is, .. }));
}

#[test]
fn native_module_value_keeps_its_identity_in_expressions() {
    // `m = errno; same = m is errno` keeps both operands as module values; the
    // runtime registry returns one object for both reads.
    let source = "import errno\nm = errno\nsame = m is errno\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "m").kind,
        HExprKind::ModuleRef { module } if module == "errno"
    ));
    let HExprKind::Bin { op, lhs, rhs } = &assigned(&hir, "same").kind else {
        panic!(
            "expected an identity comparison, got {:?}",
            assigned(&hir, "same").kind
        );
    };
    assert_eq!(*op, BinOp::Is);
    let HExprKind::Local(local) = &lhs.kind else {
        panic!("expected a read of the module variable, got {:?}", lhs.kind);
    };
    assert_eq!(hir.functions[0].locals[*local as usize].name, "m");
    assert!(matches!(
        &rhs.kind,
        HExprKind::ModuleRef { module } if module == "errno"
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn nested_native_module_is_a_first_class_value() {
    // `os.path` is a module in its own right: using it as a value must bind
    // the nested module, not report that modules are unsupported.
    let source = "import os\np = os.path\nq = os.path.join\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "p").kind,
        HExprKind::ModuleRef { module } if module == "os.path"
    ));
    assert!(matches!(
        &assigned(&hir, "q").kind,
        HExprKind::ModuleAttr { module, name }
            if module == "os.path" && name == "join"
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn dynamic_attribute_read_on_a_module_variable_is_getattr() {
    // Once a module is stored in a variable, `m.attr` cannot use the static
    // module path: it lowers to the dynamic attribute read.
    let source = "import helper\nm = helper\ndef read():\n    return m.value\n";
    let mut infer = Infer::new();
    let mut lowerer = lowerer_with_user_module(source, &mut infer, "helper", HashMap::new());
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    let body = function_body(&hir, "read");
    let HStmtKind::Return(Some(value)) = &body[0].kind else {
        panic!("expected a return statement");
    };
    let HExprKind::GetAttr { object, name } = &value.kind else {
        panic!("expected a dynamic attribute read, got {:?}", value.kind);
    };
    assert_eq!(name, "value");
    let reads_module_value = matches!(
        &object.kind,
        HExprKind::GlobalGet { module, name } if module == "__main__" && name == "m"
    ) || matches!(
        &object.kind,
        HExprKind::ModuleRef { module } if module == "helper"
    );
    assert!(
        reads_module_value,
        "expected the stored module value, got {:?}",
        object.kind
    );
}

/// Builds a lowerer whose only visible user module is `name` with `exports`.
fn lowerer_with_user_module<'a>(
    source: &'a str,
    infer: &'a mut Infer,
    name: &str,
    exports: HashMap<String, UserExport>,
) -> Lowerer<'a> {
    let mut lowerer = Lowerer::new(source, "test.py", infer);
    lowerer.set_user_modules(HashMap::from([(
        name.to_string(),
        UserModule {
            name: name.to_string(),
            exports,
            all: None,
            dynamic_globals: false,
            extension_stars: Vec::new(),
        },
    )]));
    lowerer
}

#[test]
fn imported_module_globals_read_through_the_module_value() {
    // `np.__version__` is not in the static export table: it is a module
    // global of the imported module at runtime, so the attribute becomes a
    // module lookup that raises `AttributeError` when it is really absent.
    let source = "import numpy as np\na = np.__version__\n";
    let mut infer = Infer::new();
    let mut lowerer = lowerer_with_user_module(source, &mut infer, "numpy", HashMap::new());
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "a").kind {
        HExprKind::ModuleAttr { module, name } => {
            assert_eq!((module.as_str(), name.as_str()), ("numpy", "__version__"))
        }
        other => panic!("expected a runtime module lookup, got {other:?}"),
    }
}

#[test]
fn unknown_user_module_attributes_call_dynamically() {
    let source = "import numpy as np\nb = np.linspace(0, 1)\n";
    let mut infer = Infer::new();
    let mut lowerer = lowerer_with_user_module(source, &mut infer, "numpy", HashMap::new());
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "b").kind {
        HExprKind::IndirectCall { callee, args, .. } => {
            assert_eq!(args.len(), 2);
            match &callee.kind {
                HExprKind::ModuleAttr { module, name } => {
                    assert_eq!((module.as_str(), name.as_str()), ("numpy", "linspace"))
                }
                other => panic!("expected a module attribute callee, got {other:?}"),
            }
        }
        other => panic!("expected a dynamic module call, got {other:?}"),
    }
}

#[test]
fn known_user_module_members_keep_their_static_path() {
    let source = "import helper\nx = helper.add(1, 2)\ny = helper.VALUE\n";
    let mut infer = Infer::new();
    let mut lowerer = lowerer_with_user_module(
        source,
        &mut infer,
        "helper",
        HashMap::from([
            ("add".to_string(), UserExport::Function(1)),
            ("VALUE".to_string(), UserExport::Global),
        ]),
    );
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::Call { callee, args } => {
            assert_eq!(*callee, Callee::Function(1));
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected a direct function call, got {other:?}"),
    }
    match &assigned(&hir, "y").kind {
        HExprKind::GlobalGet { module, name } => {
            assert_eq!((module.as_str(), name.as_str()), ("helper", "VALUE"))
        }
        other => panic!("expected a module global read, got {other:?}"),
    }
}

#[test]
fn user_module_member_keyword_calls_bind_at_runtime() {
    // A statically known member called with keywords binds through the
    // member's runtime value: the function reference for a function export,
    // the module-qualified global for a value export. The keyword names must
    // survive instead of being dropped by a deferred call.
    let source = "\
import helper
x = helper.add(1, addend=2)
y = helper.make(value=3)
";
    let mut infer = Infer::new();
    let mut lowerer = lowerer_with_user_module(
        source,
        &mut infer,
        "helper",
        HashMap::from([
            ("add".to_string(), UserExport::Function(1)),
            ("make".to_string(), UserExport::Global),
        ]),
    );
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "x").kind {
        HExprKind::CallKw {
            callee,
            args,
            keywords,
        } => {
            assert!(matches!(&callee.kind, HExprKind::FunctionRef(1)));
            assert_eq!(args.len(), 1);
            assert_eq!(keywords.len(), 1);
            assert_eq!(keywords[0].0, "addend");
        }
        other => panic!("expected a keyword call on the function, got {other:?}"),
    }
    match &assigned(&hir, "y").kind {
        HExprKind::CallKw {
            callee, keywords, ..
        } => {
            assert!(matches!(
                &callee.kind,
                HExprKind::GlobalGet { module, name } if module == "helper" && name == "make"
            ));
            assert_eq!(keywords.len(), 1);
            assert_eq!(keywords[0].0, "value");
        }
        other => panic!("expected a keyword call on the global, got {other:?}"),
    }
}

#[test]
fn imported_units_mirror_their_import_bindings() {
    // A package `__init__.py` re-exports what it imported; importers read
    // those names through the package module value, so the bindings must land
    // in the runtime global table.
    let source = "from helper import VALUE, add\n";
    let mut infer = Infer::new();
    let mut lowerer = lowerer_with_user_module(
        source,
        &mut infer,
        "helper",
        HashMap::from([
            ("add".to_string(), UserExport::Function(1)),
            ("VALUE".to_string(), UserExport::Global),
        ]),
    );
    lowerer.set_dunder_name("pkg");
    lowerer.set_mirror_all_globals(true);
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    // Multiple bindings from one statement are wrapped in an `if True:` block
    // so they keep the import statement's position.
    let mut stores: Vec<&HStmt> = Vec::new();
    for stmt in &hir.functions[0].body {
        match &stmt.kind {
            HStmtKind::GlobalSet { .. } => stores.push(stmt),
            HStmtKind::If { then_body, .. } => stores.extend(then_body.iter()),
            _ => {}
        }
    }
    let mut saw_value = false;
    let mut saw_function = false;
    for stmt in stores {
        let HStmtKind::GlobalSet {
            module,
            name,
            value,
        } = &stmt.kind
        else {
            panic!("unexpected non-store statement {:?}", stmt.kind);
        };
        assert_eq!(module, "pkg");
        match name.as_str() {
            "VALUE" => {
                saw_value = true;
                assert!(matches!(
                    &value.kind,
                    HExprKind::GlobalGet { module, name }
                        if module == "helper" && name == "VALUE"
                ));
            }
            "add" => {
                saw_function = true;
                assert!(matches!(&value.kind, HExprKind::FunctionRef(1)));
            }
            // Imported units always register their module name so the runtime
            // module registry can resolve them, including empty submodules.
            "__name__" => {
                assert!(
                    matches!(
                        &value.kind,
                        HExprKind::Local(_) | HExprKind::Const(Const::Str(_))
                    ),
                    "unexpected __name__ binding: {value:?}"
                );
            }
            // Imported units publish `__doc__` too: an importer resolves
            // `from module import __doc__` through the runtime global table.
            // This source has no docstring, so the binding is `None`.
            "__doc__" => {
                assert!(
                    matches!(&value.kind, HExprKind::Local(_)),
                    "unexpected __doc__ binding: {value:?}"
                );
            }
            other => panic!("unexpected mirrored binding '{other}'"),
        }
    }
    assert!(
        saw_value && saw_function,
        "import bindings were not mirrored: {:?}",
        hir.functions[0].body
    );
}

#[test]
fn imported_units_publish_their_module_docstring() {
    // `from module import __doc__` is a real module attribute in CPython: the
    // initializer stores the docstring in the runtime global table even when
    // the module body never reads its own `__doc__`.
    let source = "\"\"\"Module documentation.\"\"\"\n\ndef add(a, b):\n    return a + b\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "helper.py", &mut infer);
    lowerer.set_mirror_all_globals(true);
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        matches!(
            &assigned(&hir, "__doc__").kind,
            HExprKind::Const(Const::Str(text)) if text == "Module documentation."
        ),
        "the leading string literal is the module docstring: {:?}",
        assigned(&hir, "__doc__")
    );
    assert!(
        hir.functions[0].body.iter().any(|stmt| matches!(
            &stmt.kind,
            HStmtKind::GlobalSet { name, .. } if name == "__doc__"
        )),
        "the docstring must be mirrored into the global table: {:?}",
        hir.functions[0].body
    );
}

#[test]
fn imported_units_without_a_docstring_publish_none() {
    // CPython gives every module a `__doc__`: `None` when the body has no
    // docstring. Importers must resolve it instead of reporting a missing
    // export.
    let source = "VALUE = 1\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "helper.py", &mut infer);
    lowerer.set_mirror_all_globals(true);
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        matches!(
            &assigned(&hir, "__doc__").kind,
            HExprKind::Const(Const::None)
        ),
        "an undocumented module binds `__doc__` to None: {:?}",
        assigned(&hir, "__doc__")
    );
}

#[test]
fn module_docstring_reexport_uses_the_imported_value() {
    // NumPy's `numpy/lib/array_utils.py` re-exports another module's docstring
    // with `from ._array_utils_impl import __doc__`. CPython sets the module
    // docstring first and the import statement replaces it, so the imported
    // value is the one every read observes; the injected dunder must not
    // shadow it.
    let source = "\"\"\"Wrapper documentation.\"\"\"\nfrom helper import __doc__\n";
    let mut infer = Infer::new();
    let mut lowerer = lowerer_with_user_module(
        source,
        &mut infer,
        "helper",
        HashMap::from([("__doc__".to_string(), UserExport::Global)]),
    );
    lowerer.set_dunder_name("wrapper");
    lowerer.set_mirror_all_globals(true);
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    // No local `__doc__` slot is injected, so module-level reads resolve to the
    // imported module instead of the shadowing local.
    assert!(
        !hir.functions[0]
            .locals
            .iter()
            .any(|local| local.name == "__doc__"),
        "the imported docstring must not be shadowed by an injected local"
    );
    // The import statement mirrors the re-exported value: `wrapper.__doc__`
    // reads `helper.__doc__`.
    assert!(
        hir.functions[0].body.iter().any(|stmt| matches!(
            &stmt.kind,
            HStmtKind::GlobalSet {
                name,
                value,
                module: _
            } if name == "__doc__"
                    && matches!(
                        &value.kind,
                        HExprKind::GlobalGet { module, name }
                            if module == "helper" && name == "__doc__"
                    )
        )),
        "the re-exported docstring must be mirrored: {:?}",
        hir.functions[0].body
    );
}

#[test]
fn id_is_a_first_class_builtin() {
    let source = "a = id(1)\nf = id\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    match &assigned(&hir, "a").kind {
        HExprKind::Call { callee, args } => {
            assert_eq!(*callee, Callee::Builtin(Builtin::Id));
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected a direct builtin call, got {other:?}"),
    }
    assert!(matches!(
        &assigned(&hir, "f").kind,
        HExprKind::BuiltinRef(Builtin::Id)
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());

    // Arity is checked like every other builtin.
    let source = "x = id()\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        crate::sema::check(&hir, &mut infer, "test.py").is_err(),
        "id() without an argument must be rejected"
    );
}

#[test]
fn unittest_test_case_is_accepted_as_a_class_base() {
    let source =
        "import unittest\nclass T(unittest.TestCase):\n    def test_errno(self):\n        pass\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::ClassDef { bases, .. } = &hir.functions[0].body[0].kind else {
        panic!(
            "expected a class with a base, got {:?}",
            hir.functions[0].body[0].kind
        );
    };
    let base = bases.first().expect("class T base");
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
            HStmtKind::ClassDef { name, bases, .. } if name == "T" => Some(bases.first()),
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

    let HStmtKind::ClassDef { bases, .. } = hir.functions[0]
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
            bases.first().map(|base| &base.kind),
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
fn import_keywords_lower_through_the_keyword_binder() {
    // `encodings/__init__.py` calls `__import__('encodings.' + modname,
    // fromlist=_import_tail, level=0)`; every CPython 3.14 parameter name must
    // bind instead of being rejected.
    let source = "mod = __import__('os.path', fromlist=['*'], level=0)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::CallKw {
        callee,
        args,
        keywords,
    } = &assigned(&hir, "mod").kind
    else {
        panic!(
            "expected a keyword __import__ call, got {:?}",
            assigned(&hir, "mod")
        );
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::BuiltinRef(Builtin::Import)
    ));
    assert_eq!(args.len(), 1, "the positional name stays positional");
    assert_eq!(
        keywords
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["fromlist", "level"]
    );
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn import_rejects_unknown_keyword() {
    let source = "mod = __import__('os', nope=1)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{source}: {diags:?}");
    assert!(
        diags[0]
            .message
            .contains("__import__() got an unexpected keyword argument 'nope'"),
        "{source}: {diags:?}"
    );
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
fn tuple_displays_expand_starred_operands_in_order() {
    let source = "xs = (1, 2)\ny = ('a', *xs, 'b')\nz = (*xs,)\nw = (1, *xs, 2, *xs)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for name in ["y", "z", "w"] {
        let HExprKind::Bin { op: BinOp::Add, .. } = &assigned(&hir, name).kind else {
            panic!("expected tuple concatenation for {name}");
        };
        assert_eq!(assigned(&hir, name).ty, Ty::Concrete(Type::Tuple));
    }
    // `(*xs,)` copies the operand through `tuple(xs)`; the plain chunks are
    // evaluated before the starred operand in source order.
    let HExprKind::Bin {
        op: BinOp::Add,
        rhs,
        ..
    } = &assigned(&hir, "z").kind
    else {
        unreachable!()
    };
    assert!(matches!(
        &rhs.kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Tuple),
            args,
        } if args.len() == 1
    ));
    let HExprKind::Bin {
        op: BinOp::Add,
        lhs,
        ..
    } = &assigned(&hir, "y").kind
    else {
        unreachable!()
    };
    let HExprKind::Bin {
        op: BinOp::Add,
        lhs,
        rhs,
        ..
    } = &lhs.kind
    else {
        panic!("expected the starred operand before the trailing chunk");
    };
    assert!(matches!(
        &rhs.kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Tuple),
            args,
        } if args.len() == 1
    ));
    let HExprKind::Bin {
        op: BinOp::Add,
        rhs,
        ..
    } = &lhs.kind
    else {
        panic!("expected the leading plain chunk before the starred operand");
    };
    assert!(matches!(&rhs.kind, HExprKind::Tuple(elems) if elems.len() == 1));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn set_displays_expand_starred_operands_in_order() {
    let source = "xs = {1, 2}\ny = {3, *xs, 4}\nz = {*xs}\nr = {*range(3)}\nw = {1, *xs, *xs}\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for name in ["y", "w"] {
        let HExprKind::Bin {
            op: BinOp::BitOr, ..
        } = &assigned(&hir, name).kind
        else {
            panic!("expected a set union for {name}");
        };
        assert_eq!(assigned(&hir, name).ty, Ty::Concrete(Type::Set));
    }
    // A leading star needs no empty-set seed: `set(list(xs))` already copies.
    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Set),
        args,
    } = &assigned(&hir, "z").kind
    else {
        panic!(
            "expected set(list(xs)) for a leading star, got {:?}",
            assigned(&hir, "z").kind
        );
    };
    assert_eq!(args.len(), 1);
    assert!(matches!(
        &args[0].kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::List),
            ..
        }
    ));
    // Any iterable, including a range, is accepted after `*`.
    assert!(matches!(
        &assigned(&hir, "r").kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Set),
            args,
        } if args.len() == 1
    ));
    let HExprKind::Bin {
        op: BinOp::BitOr,
        lhs,
        ..
    } = &assigned(&hir, "y").kind
    else {
        unreachable!()
    };
    assert!(matches!(
        &lhs.kind,
        HExprKind::Bin {
            op: BinOp::BitOr,
            ..
        }
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn starred_displays_of_non_iterables_are_rejected() {
    let source = "a = (1, *1)\nb = {*1}\nc = [*1]\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    // The starred set operand is materialised through `list(...)`; inference
    // rejects a statically known non-iterable before the runtime can raise.
    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Set),
        args,
    } = &assigned(&hir, "b").kind
    else {
        panic!("expected set(list(1)) for a starred set display");
    };
    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::List),
        args,
    } = &args[0].kind
    else {
        panic!("expected the starred set operand to be materialised as a list");
    };
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::Int(1))));
    let diags = crate::sema::check(&hir, &mut infer, "test.py")
        .expect_err("starred non-iterable operands must be rejected");
    assert!(
        diags
            .iter()
            .any(|diag| diag.message.contains("argument must be iterable")),
        "{diags:?}"
    );
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
        ..
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

    // A positional operand before the star and a `**mapping` are both part of
    // the extended method ABI now.
    for source in [
        "def f(t, a):\n    return t.add_row(1, *a)\n",
        "def f(t, a, b):\n    return t.add_row(*a, **b)\n",
    ] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert!(diags.is_empty(), "{source}: {diags:?}");
    }
    // Two starred operands still cannot be represented.
    let source = "def f(t, a, b):\n    return t.add_row(*a, *b)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{source}: {diags:?}");
    assert!(diags[0].message.contains("only one starred argument"));
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

    // An enclosing local is a closure capture, not an error.
    let source = "def outer():\n    x = 1\n    return lambda: x + 1\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(function(&hir, "<lambda>").capture_count, 1);
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
        hydro_ir::StmtKind::Assign { value, .. }
            if matches!(
                &value.kind,
                hydro_ir::ExprKind::Call {
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
            hydro_ir::StmtKind::Assign { value, .. }
                if matches!(
                    &value.kind,
                    hydro_ir::ExprKind::IndirectCall { star: Some(_), .. }
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
fn matrix_multiplication_lowers_as_a_binary_operator() {
    let source = "a = 2 @ 3\nb = 2.0 @ 3\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for name in ["a", "b"] {
        let HExprKind::Bin { op, .. } = &assigned(&hir, name).kind else {
            panic!("expected a binary expression for {name}");
        };
        assert_eq!(*op, BinOp::MatMul);
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let a_ty = program.functions[0]
        .locals
        .iter()
        .find(|local| local.name == "a")
        .map(|local| local.ty);
    assert_eq!(a_ty, Some(Type::Int));
}

#[test]
fn matrix_multiplication_of_class_instances_stays_dynamic() {
    let source = "class M:\n    def __matmul__(self, other):\n        return 1\n\
                  class R:\n    def __rmatmul__(self, other):\n        return 2\n\
                  a = M() @ M()\nb = 1 @ R()\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    for name in ["a", "b"] {
        let HExprKind::Bin { op, .. } = &assigned(&hir, name).kind else {
            panic!("expected a binary expression for {name}");
        };
        assert_eq!(*op, BinOp::MatMul);
    }
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn augmented_matrix_multiplication_lowers_the_in_place_operator() {
    let source = "x = 1\nx @= 2\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::AugAssign { op, target, .. } = &hir.functions[0].body[1].kind else {
        panic!(
            "expected an augmented assignment, got {:?}",
            hir.functions[0].body[1]
        );
    };
    assert_eq!(*op, BinOp::MatMulInPlace);
    assert_eq!(*target, 0);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn raise_with_keyword_exception_constructor_raises_the_value() {
    let source = "def f(x):\n    raise AttributeError(x, name=None)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let raise = function(&hir, "f")
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Raise { exc, msg, cause } => Some((*exc, msg, cause)),
            _ => None,
        })
        .expect("raise statement");
    assert_eq!(raise.0, None, "the value form replaces the tag form");
    assert!(raise.2.is_none());
    let HExprKind::CallKw {
        callee,
        args,
        keywords,
    } = &raise.1.as_ref().expect("raised value").kind
    else {
        panic!(
            "expected the constructor call, got {:?}",
            raise.1.as_ref().map(|expr| &expr.kind)
        );
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::ExceptionClass(ExcKind::AttributeError)
    ));
    assert_eq!(args.len(), 1);
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "name");
    assert!(matches!(&keywords[0].1.kind, HExprKind::Const(Const::None)));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let function = program.functions.iter().find(|f| f.name == "f").expect("f");
    let raise = function
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            hydro_ir::StmtKind::Raise { exc, msg, .. } => Some((*exc, msg)),
            _ => None,
        })
        .expect("IR raise");
    assert_eq!(raise.0, None);
    assert!(matches!(
        raise.1.as_ref().map(|value| &value.kind),
        Some(hydro_ir::ExprKind::CallKw { .. })
    ));
}

#[test]
fn keyword_exception_constructor_is_a_dynamic_expression() {
    let source = "e = AttributeError('boom', name=None)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::CallKw {
        callee, keywords, ..
    } = &assigned(&hir, "e").kind
    else {
        panic!(
            "expected a keyword constructor call, got {:?}",
            assigned(&hir, "e")
        );
    };
    assert!(matches!(
        &callee.kind,
        HExprKind::ExceptionClass(ExcKind::AttributeError)
    ));
    assert_eq!(keywords.len(), 1);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
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
            hydro_ir::StmtKind::Unpack { targets, .. } => Some(targets),
            _ => None,
        })
        .expect("emitted unpack");
    assert!(matches!(
        emitted[2],
        hydro_ir::UnpackTarget::Star(ref inner)
            if matches!(**inner, hydro_ir::UnpackTarget::Name(_))
    ));
}

#[test]
fn nested_starred_unpack_targets_are_accepted() {
    let source = "(a, *b), c = ((1, 2, 3), 4)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let targets = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Unpack { targets, .. } => Some(targets),
            _ => None,
        })
        .expect("unpack statement");
    let HUnpackTarget::Nested(inner) = &targets[0] else {
        panic!("expected a nested target, got {:?}", targets[0]);
    };
    assert!(matches!(inner[0], HUnpackTarget::Name(_)));
    assert!(matches!(inner[1], HUnpackTarget::Star(_)));
    assert!(matches!(targets[1], HUnpackTarget::Name(_)));

    // Every sequence level owns its star; list targets behave like tuples.
    let source = "(a, *b), (*c, d) = ((1, 2, 3), (4, 5, 6))\n\
                  [e, *f] = [1, 2, 3]\n\
                  [*g, h] = [1, 2, 3]\n\
                  for (i, *j), (*k, l) in [((1, 2), (3, 4))]:\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn multiple_starred_unpack_targets_are_rejected_per_level() {
    for source in [
        "*a, *b = (1, 2)\n",
        "(a, *b), (*c, *d) = ((1, 2), (3, 4))\n",
        "for *a, *b in [(1, 2)]:\n    pass\n",
        "xs = [1 for *a, *b in [(1, 2)]]\n",
    ] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert!(
            diags
                .iter()
                .any(|diag| diag.message.contains("multiple starred unpacking targets")),
            "{source}: {diags:?}"
        );
    }
}

#[test]
fn inspect_starred_unpack_patterns_lower() {
    let source = "\
frame = 1
lineno = 2
positions = [3, 4]
frame, *positions = (frame, lineno, *positions[1:])
frame, *positions = (frame, *positions)
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
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
                op: hydro_ir::UnOp::Invert,
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
        op: hydro_ir::BoolOp::And,
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

#[test]
fn integer_literals_choose_tagged_or_bigint_constants() {
    let source = "small = 7\n\
                  big = 1152921504606846976\n\
                  neg = -123456789012345678901234567890\n\
                  lazy = 10 - 3\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    assert!(matches!(
        &assigned(&hir, "small").kind,
        HExprKind::Const(Const::Int(7))
    ));
    assert_eq!(assigned(&hir, "small").ty, Ty::Concrete(Type::Int));

    assert!(matches!(
        &assigned(&hir, "big").kind,
        HExprKind::Const(Const::BigInt(text)) if text == "1152921504606846976"
    ));
    assert_eq!(assigned(&hir, "big").ty, Ty::Concrete(Type::BigInt));

    assert!(matches!(
        &assigned(&hir, "neg").kind,
        HExprKind::Const(Const::BigInt(text))
            if text == "-123456789012345678901234567890"
    ));
    assert_eq!(assigned(&hir, "neg").ty, Ty::Concrete(Type::BigInt));

    assert!(matches!(
        &assigned(&hir, "lazy").kind,
        HExprKind::Const(Const::Int(7))
    ));
}

#[test]
fn bigint_defaults_keep_their_sign_and_type() {
    let source =
        "def f(a=1152921504606846976, b=-123456789012345678901234567890, c=3):\n    pass\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let f = function(&hir, "f");
    assert_eq!(
        f.params[0].default,
        Some(Const::BigInt("1152921504606846976".to_string()))
    );
    assert_eq!(
        f.params[1].default,
        Some(Const::BigInt("-123456789012345678901234567890".to_string()))
    );
    assert_eq!(f.params[2].default, Some(Const::Int(3)));
}

#[test]
fn negating_a_bigint_literal_that_fits_the_tagged_range_becomes_int() {
    let source = "x = -1152921504606846976\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "x").kind,
        HExprKind::Const(Const::Int(v)) if *v == -1152921504606846976
    ));
    assert_eq!(assigned(&hir, "x").ty, Ty::Concrete(Type::Int));
}

// ---------------------------------------------------------------------------
// PEP 701 f-strings and PEP 750 template strings
// ---------------------------------------------------------------------------

/// Parse a module and keep the template side table.
fn parse_with_templates(source: &str) -> (ast::ModModule, Vec<TemplateString>) {
    match rustpython_parser::parse_module_with_templates(source, "test.py") {
        Ok(parsed) => match parsed.module {
            ast::Mod::Module(module) => (module, parsed.templates),
            _ => panic!("expected a module"),
        },
        Err(error) => panic!("parse failed: {}", error.error),
    }
}

fn lower_source_with_templates(source: &str) -> (LHir, Infer, Vec<Diagnostic>) {
    let (module, templates) = parse_with_templates(source);
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    lowerer.set_templates(&templates);
    let hir = lowerer.lower_module(&module);
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    (hir, infer, diags)
}

#[test]
fn pep701_same_quote_and_nested_fstrings_lower() {
    // Quote reuse inside a replacement field (PEP 701).
    let source = "x = f\"{\"hi\"}\"\n";
    let (hir, _, diags) = lower_source_with_templates(source);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(assigned(&hir, "x").ty, Ty::Concrete(Type::Str));

    // A nested f-string in a replacement field.
    let source = "y = 'a'\nx = f\"{f'{y}'}\"\n";
    let (hir, _, diags) = lower_source_with_templates(source);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(assigned(&hir, "x").ty, Ty::Concrete(Type::Str));
}

#[test]
fn simple_template_literal_lowers_to_a_template() {
    let source = "name = 'world'\nx = t\"hello {name}\"\n";
    let (hir, _, diags) = lower_source_with_templates(source);
    assert!(diags.is_empty(), "{diags:?}");

    let value = assigned(&hir, "x");
    assert_eq!(value.ty, Ty::Concrete(Type::Any));
    let HExprKind::Template {
        strings,
        interpolations,
    } = &value.kind
    else {
        panic!("expected a template expression, got {:?}", value.kind);
    };
    assert_eq!(strings, &vec!["hello ".to_string(), String::new()]);
    assert_eq!(interpolations.len(), 1);
    assert_eq!(interpolations[0].expression, "name");
    assert_eq!(interpolations[0].conversion, 0);
    assert!(interpolations[0].format_spec.is_none());
    assert!(!interpolations[0].self_documenting);
}

#[test]
fn template_conversions_are_preserved() {
    let source = "name = 'w'\nx = t\"{name!r}{name!s}{name!a}\"\n";
    let (hir, _, diags) = lower_source_with_templates(source);
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::Template { interpolations, .. } = &assigned(&hir, "x").kind else {
        panic!("expected a template expression");
    };
    let conversions: Vec<u8> = interpolations
        .iter()
        .map(|interpolation| interpolation.conversion)
        .collect();
    assert_eq!(conversions, vec![2, 1, 3]);
}

#[test]
fn template_format_spec_lowers_to_an_expression() {
    let source = "n = 42\nx = t\"{n:>10}\"\n";
    let (hir, _, diags) = lower_source_with_templates(source);
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::Template { interpolations, .. } = &assigned(&hir, "x").kind else {
        panic!("expected a template expression");
    };
    let spec = interpolations[0].format_spec.as_ref().expect("format spec");
    assert!(matches!(&spec.kind, HExprKind::Const(Const::Str(text)) if text == ">10"));
}

#[test]
fn template_self_documenting_keeps_text_and_repr_conversion() {
    let source = "n = 42\nx = t\"{n=}\"\n";
    let (hir, _, diags) = lower_source_with_templates(source);
    assert!(diags.is_empty(), "{diags:?}");
    let HExprKind::Template {
        strings,
        interpolations,
    } = &assigned(&hir, "x").kind
    else {
        panic!("expected a template expression");
    };
    // `t"{n=}"` is `strings=('n=', '')` with an implicit `!r` conversion.
    assert_eq!(strings, &vec!["n=".to_string(), String::new()]);
    assert_eq!(interpolations[0].expression, "n");
    assert_eq!(interpolations[0].conversion, 2);
    assert!(interpolations[0].self_documenting);
}

#[test]
fn nested_templates_are_preserved() {
    let source = "name = 'w'\nx = t\"outer {t'inner {name}'}\"\n";
    let (module, templates) = parse_with_templates(source);
    assert_eq!(templates.len(), 2, "outer and nested literal");
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "test.py", &mut infer);
    lowerer.set_templates(&templates);
    let hir = lowerer.lower_module(&module);
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");

    let HExprKind::Template { interpolations, .. } = &assigned(&hir, "x").kind else {
        panic!("expected a template expression");
    };
    assert!(
        matches!(&interpolations[0].value.kind, HExprKind::Template { .. }),
        "nested template must stay a template, got {:?}",
        interpolations[0].value.kind
    );
}

#[test]
fn template_spans_cover_the_literal_and_expression() {
    let source = "x = t\"a{b}c\"\n";
    let (_, templates) = parse_with_templates(source);
    assert_eq!(templates.len(), 1);
    let template = &templates[0];
    let literal = &source[template.range.start().to_usize()..template.range.end().to_usize()];
    assert_eq!(literal, "t\"a{b}c\"");
    let interpolation = &template.interpolations[0];
    let expression = &source[interpolation.expression_range.start().to_usize()
        ..interpolation.expression_range.end().to_usize()];
    assert_eq!(expression, "b");
    let field =
        &source[interpolation.range.start().to_usize()..interpolation.range.end().to_usize()];
    assert_eq!(field, "{b}");
    assert_eq!(
        template
            .strings
            .iter()
            .map(|part| part.value.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "c"]
    );
}

#[test]
fn malformed_templates_are_syntax_errors() {
    for source in [
        "x = t\"{name\"\n",
        "x = t\"name}\"\n",
        "x = t\"{a}\" \"b\"\n",
    ] {
        let parsed = rustpython_parser::parse_module_with_templates(source, "test.py");
        assert!(parsed.is_err(), "expected a syntax error for {source:?}");
        let diagnostics = crate::check_source(source, "test.py")
            .expect_err("check_source must reject malformed templates");
        assert_eq!(diagnostics[0].code, crate::diag::codes::SYNTAX);
    }
}

#[test]
fn stdlib_template_and_pep701_lines_compile() {
    // `Lib/annotationlib.py`: `_Template = type(t"")`.
    let source = "_Template = type(t\"\")\n";
    let (hir, _, diags) = lower_source_with_templates(source);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        &assigned(&hir, "_Template").kind,
        HExprKind::Call { .. }
    ));

    // `Lib/traceback.py` (`_extract_from_extended_frame_gen`): an f-string
    // whose replacement field contains string literals and a conditional.
    let source = "def f(count):\n    return f'  [Previous line repeated {count} more time{\"s\" if count > 1 else \"\"}]\\n'\n";
    let (hir, _, diags) = lower_source_with_templates(source);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(function(&hir, "f").body.len(), 1);
}

/// Expression statements of the module body, in order.
fn module_exprs(hir: &LHir) -> Vec<&HExpr> {
    hir.functions[0]
        .body
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            HStmtKind::Expr(value) => Some(value),
            _ => None,
        })
        .collect()
}

fn lower_ok(source: &str) -> LHir {
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    hir
}

#[test]
fn positional_only_parameters_are_recorded_and_bound() {
    let source = "\
def f(x, /, y, *args, z=1, **kw):
    return (x, y, args, z, kw)
f(1, 2, 3, z=4, extra=5)
";
    let hir = lower_ok(source);
    let kinds: Vec<ParamKind> = function(&hir, "f").params.iter().map(|p| p.kind).collect();
    assert_eq!(
        kinds,
        vec![
            ParamKind::PosOnly,
            ParamKind::Positional,
            ParamKind::VarArg,
            ParamKind::KwOnly,
            ParamKind::KwArg,
        ]
    );
    // A keyword name that matches a positional-only parameter is forwarded to
    // `**kwargs`; the positional-only slot still needs a positional value.
    lower_ok("def f(x=0, /, **kw):\n    return kw\nf(x=1)\n");
    let source = "def f(x, /, **kw):\n    return kw\nf(x=1)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]
            .message
            .contains("missing required positional argument 'x'"),
        "{diags:?}"
    );
    // Without `**kwargs`, the keyword name is not bindable at all.
    let source = "def f(x=0, /):\n    return x\nf(x=1)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].message.contains("unexpected keyword argument 'x'"),
        "{diags:?}"
    );
    // Positional filling still sees the positional-only slot.
    lower_ok("def f(x, /, y):\n    return x + y\nf(1, 2)\n");
}

#[test]
fn star_and_double_star_calls_lower_to_the_runtime_flattener() {
    let source = "\
def f(a, b=0, **kw):
    return a
xs = [1]
kw = {'b': 2}
obj = None
f(0, *xs, b=3, **kw)
obj.m(0, *xs, b=3, **kw)
";
    let hir = lower_ok(source);
    let exprs = module_exprs(&hir);
    let call = exprs
        .iter()
        .find(|expr| matches!(expr.kind, HExprKind::CallFull { .. }))
        .expect("function call with * and ** lowers to CallFull");
    let HExprKind::CallFull {
        args,
        star,
        keywords,
        double_star,
        ..
    } = &call.kind
    else {
        unreachable!()
    };
    assert_eq!(args.len(), 1);
    assert!(star.is_some());
    assert_eq!(keywords.len(), 1);
    assert!(double_star.is_some());
    let method = exprs
        .iter()
        .find(|expr| matches!(expr.kind, HExprKind::MethodCall { .. }))
        .expect("method call with * and ** lowers to MethodCall");
    let HExprKind::MethodCall {
        star,
        keywords,
        double_star,
        ..
    } = &method.kind
    else {
        unreachable!()
    };
    assert!(star.is_some());
    assert_eq!(keywords.len(), 1);
    assert!(double_star.is_some());
}

#[test]
fn match_patterns_lower_to_guarded_branches() {
    let source = "\
class Point:
    x = 0
    y = 0
def classify(value):
    match value:
        case 0:
            return 'zero'
        case [a, *rest]:
            return (a, rest)
        case {'name': name, **extra}:
            return (name, extra)
        case Point(x=px, y=py) as point:
            return (px, py, point)
        case 1 | 2:
            return 'small'
        case other if other > 10:
            return 'big'
        case _:
            return 'other'
";
    let hir = lower_ok(source);
    let body = &function(&hir, "classify").body;
    assert!(
        body.iter()
            .any(|stmt| matches!(stmt.kind, HStmtKind::If { .. })),
        "match must lower to guarded branches: {body:#?} functions={:?}",
        hir.functions
            .iter()
            .map(|f| (f.name.clone(), f.body.len()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn malformed_match_patterns_are_diagnosed() {
    for (source, needle) in [
        (
            "def f(v):\n    match v:\n        case x:\n            pass\n        case _:\n            pass\n",
            "unreachable",
        ),
        (
            "def f(v):\n    match v:\n        case 1 | x:\n            pass\n",
            "bind different names",
        ),
        (
            "def f(v):\n    match v:\n        case [x, x]:\n            pass\n",
            "multiple assignments",
        ),
    ] {
        let (_, _, diags) = lower_source(source, &parse(source));
        assert!(!diags.is_empty(), "expected a diagnostic for {source:?}");
        assert!(
            diags.iter().any(|diag| diag.message.contains(needle)),
            "expected {needle:?} in {diags:?}"
        );
    }
}

#[test]
fn except_star_lowers_to_an_explicit_unsupported_error() {
    let source = "\
def f():
    try:
        raise ValueError('x')
    except* ValueError as group:
        return group
";
    let hir = lower_ok(source);
    let body = &function(&hir, "f").body;
    let try_stmt = body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Try { handlers, .. } => Some(handlers),
            _ => None,
        })
        .expect("except* lowers to a Try with one explicit handler");
    assert_eq!(try_stmt.len(), 1);
    assert!(matches!(
        try_stmt[0].body[0].kind,
        HStmtKind::Raise {
            exc: Some(ExcKind::NotImplementedError),
            ..
        }
    ));
}

#[test]
fn function_and_class_decorators_apply_bottom_up() {
    let source = "\
def first(fn):
    return fn
def second(fn):
    return fn
@first
@second
def target():
    return 1
@first
class C:
    pass
";
    let hir = lower_ok(source);
    let body = &hir.functions[0].body;
    let mut decorator_calls = 0;
    let mut saw_function_ref = false;
    for stmt in body {
        if let HStmtKind::Assign { value, .. } = &stmt.kind {
            match &value.kind {
                HExprKind::FunctionRef(_) => saw_function_ref = true,
                HExprKind::IndirectCall { .. } => decorator_calls += 1,
                _ => {}
            }
        }
    }
    assert!(saw_function_ref, "the function value must be created");
    // Two decorators on the function and one on the class.
    assert_eq!(decorator_calls, 3, "{body:?}");
    assert!(
        body.iter()
            .any(|stmt| matches!(stmt.kind, HStmtKind::ClassDef { .. })),
        "the class itself must still be created"
    );
}

#[test]
fn class_bases_metaclass_and_slots_are_lowered() {
    let source = "\
class A:
    pass
class B:
    pass
class C(A, B, metaclass=type):
    pass
";
    let hir = lower_ok(source);
    let class = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::ClassDef { name, .. } if name == "C" => Some(&stmt.kind),
            _ => None,
        })
        .expect("class C");
    let HStmtKind::ClassDef {
        bases, metaclass, ..
    } = class
    else {
        unreachable!()
    };
    assert_eq!(bases.len(), 2);
    assert!(metaclass.is_some());

    // `__slots__` is a class attribute: the runtime parses it into the class's
    // slot table (`value::parse_slots`), so the frontend must accept it.
    let source = "class D:\n    __slots__ = ('x',)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let class = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::ClassDef {
                name, attributes, ..
            } if name == "D" => Some(attributes),
            _ => None,
        })
        .expect("class D");
    assert!(class.iter().any(|(name, _)| name == "__slots__"));
}

/// The `HStmtKind::ClassDef` for `name`, descending into module-level
/// control-flow blocks (and the `if True:` wrappers class lowering emits).
fn class_def_in<'a>(stmts: &'a [HStmt], name: &str) -> Option<&'a HStmtKind> {
    for stmt in stmts {
        match &stmt.kind {
            HStmtKind::ClassDef {
                name: class_name, ..
            } if class_name.as_str() == name => return Some(&stmt.kind),
            HStmtKind::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(found) =
                    class_def_in(then_body, name).or_else(|| class_def_in(else_body, name))
                {
                    return Some(found);
                }
            }
            HStmtKind::While {
                body, else_body, ..
            }
            | HStmtKind::ForRange {
                body, else_body, ..
            }
            | HStmtKind::ForEach {
                body, else_body, ..
            } => {
                if let Some(found) =
                    class_def_in(body, name).or_else(|| class_def_in(else_body, name))
                {
                    return Some(found);
                }
            }
            HStmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => {
                let found = class_def_in(body, name)
                    .or_else(|| {
                        handlers
                            .iter()
                            .find_map(|handler| class_def_in(&handler.body, name))
                    })
                    .or_else(|| class_def_in(else_body, name))
                    .or_else(|| class_def_in(finally_body, name));
                if found.is_some() {
                    return found;
                }
            }
            _ => {}
        }
    }
    None
}

/// The first `HStmtKind::Unpack` reachable from `stmts`, descending into
/// control-flow blocks (class lowering parks attribute values in an
/// `if True:` wrapper).
fn first_unpack(stmts: &[HStmt]) -> Option<&[HUnpackTarget]> {
    for stmt in stmts {
        match &stmt.kind {
            HStmtKind::Unpack { targets, .. } => return Some(targets),
            HStmtKind::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(found) = first_unpack(then_body).or_else(|| first_unpack(else_body)) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

#[test]
fn module_level_unpack_assignments_bind_module_globals() {
    let source = "\
CO_OPTIMIZED, (CO_NEWLOCALS, *rest) = 1, (2, 4, 8)
def flags():
    return CO_OPTIMIZED, CO_NEWLOCALS, rest
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let body = &hir.functions[0].body;
    assert!(
        matches!(body[0].kind, HStmtKind::Unpack { .. }),
        "expected an unpacking assignment: {:?}",
        body[0].kind
    );
    let mirrored: Vec<&str> = body
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            HStmtKind::GlobalSet { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        mirrored,
        ["CO_OPTIMIZED", "CO_NEWLOCALS", "rest"],
        "every unpacked name must be mirrored, in binding order"
    );
    let HStmtKind::Return(Some(expr)) = &function(&hir, "flags").body[0].kind else {
        panic!("expected a return statement");
    };
    let HExprKind::Tuple(items) = &expr.kind else {
        panic!("expected a tuple, got {:?}", expr.kind);
    };
    let names: Vec<&str> = items
        .iter()
        .map(|item| match &item.kind {
            HExprKind::GlobalGet { name, .. } => name.as_str(),
            other => panic!("expected a global read, got {other:?}"),
        })
        .collect();
    assert_eq!(names, ["CO_OPTIMIZED", "CO_NEWLOCALS", "rest"]);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn class_header_keywords_evaluate_between_bases_and_the_body() {
    let source = "\
def base():
    return object
def flag():
    return True
def mark():
    return 1
class C(base(), _root=flag()):
    value = mark()
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let locals = &hir.functions[0].locals;
    let then_body = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::If { then_body, .. }
                if then_body.iter().any(
                    |inner| matches!(&inner.kind, HStmtKind::ClassDef { name, .. } if name == "C"),
                ) =>
            {
                Some(then_body)
            }
            _ => None,
        })
        .expect("the keyword-bearing class is wrapped in a statement block");
    assert_eq!(then_body.len(), 3, "{then_body:?}");
    let HStmtKind::Assign {
        target: base_target,
        value: base,
    } = &then_body[0].kind
    else {
        panic!("expected the base temp, got {:?}", then_body[0].kind);
    };
    assert!(locals[*base_target as usize].name.starts_with("$classbase"));
    // A header expression is an ordinary expression evaluated when the class
    // statement runs, so a call to a known function stays a direct call.
    assert!(matches!(
        &base.kind,
        HExprKind::Call {
            callee: Callee::Function(id),
            args,
        } if args.is_empty() && hir.functions[*id as usize].name == "base"
    ));
    let HStmtKind::Assign {
        target: keyword_target,
        value: keyword,
    } = &then_body[1].kind
    else {
        panic!("expected the keyword temp, got {:?}", then_body[1].kind);
    };
    assert!(locals[*keyword_target as usize]
        .name
        .starts_with("$classkw"));
    assert!(matches!(
        &keyword.kind,
        HExprKind::Call {
            callee: Callee::Function(id),
            args,
        } if args.is_empty() && hir.functions[*id as usize].name == "flag"
    ));
    let HStmtKind::ClassDef {
        bases,
        metaclass,
        methods,
        attributes,
        ..
    } = &then_body[2].kind
    else {
        panic!("expected the class definition, got {:?}", then_body[2].kind);
    };
    assert_eq!(bases.len(), 1);
    assert!(matches!(
        &bases[0].kind,
        HExprKind::Local(local) if local == base_target
    ));
    assert!(metaclass.is_none());
    assert!(methods.is_empty());
    assert_eq!(
        attributes
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["value"]
    );
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn keyword_bearing_dynamic_class_evaluates_keywords_before_the_body() {
    let source = "\
def flag():
    return True
class C(object, total=flag()):
    if flag():
        pass
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let locals = &hir.functions[0].locals;
    let then_body = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::If { then_body, .. }
                if matches!(
                    then_body.first().map(|first| &first.kind),
                    Some(HStmtKind::Assign { target, .. })
                        if locals[*target as usize].name.starts_with("$classbase")
                ) =>
            {
                Some(then_body)
            }
            _ => None,
        })
        .expect("the dynamic class is wrapped in a statement block");
    let assigned_name = |stmt: &HStmt| match &stmt.kind {
        HStmtKind::Assign { target, .. } => locals[*target as usize].name.as_str(),
        other => panic!("expected an assignment, got {other:?}"),
    };
    assert!(assigned_name(&then_body[0]).starts_with("$classbase"));
    assert!(assigned_name(&then_body[1]).starts_with("$classkw"));
    assert!(assigned_name(&then_body[2]).starts_with("$classns"));
}

#[test]
fn class_keywords_with_an_explicit_metaclass_are_rejected() {
    let source = "class C(metaclass=type, order=True):\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags
            .iter()
            .any(|diag| diag.message.contains("cannot be forwarded")),
        "{diags:?}"
    );
    let source = "class C(**options):\n    pass\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(
        diags.iter().any(|diag| diag.message.contains("'**'")),
        "{diags:?}"
    );
}

#[test]
fn module_level_binding_decorators_apply_at_runtime() {
    fn has_builtin(stmts: &[HStmt], builtin: Builtin) -> bool {
        stmts.iter().any(|stmt| match &stmt.kind {
            HStmtKind::Assign { value, .. } => {
                matches!(value.kind, HExprKind::BuiltinRef(actual) if actual == builtin)
            }
            HStmtKind::If {
                then_body,
                else_body,
                ..
            } => has_builtin(then_body, builtin) || has_builtin(else_body, builtin),
            _ => false,
        })
    }
    for (decorator, builtin) in [
        ("classmethod", Builtin::Classmethod),
        ("staticmethod", Builtin::Staticmethod),
        ("property", Builtin::Property),
    ] {
        let source =
            format!("if True:\n    @{decorator}\n    def hook(cls):\n        return cls\n");
        let (hir, _, diags) = lower_source(&source, &parse(&source));
        assert!(diags.is_empty(), "{decorator}: {diags:?}");
        assert!(
            has_builtin(&hir.functions[0].body, builtin),
            "{decorator}: the conditional definition must evaluate @{decorator} at runtime: {:?}",
            hir.functions[0].body
        );
        assert!(
            has_global_set(&hir.functions[0].body),
            "{decorator}: the decorated conditional definition mirrors its result"
        );
    }
}

#[test]
fn class_in_module_level_conditional_keeps_binding_kinds() {
    let source = "\
if True:
    class C:
        @classmethod
        def build(cls):
            return cls
        @staticmethod
        def helper(x):
            return x
        @property
        def value(self):
            return 1
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let Some(HStmtKind::ClassDef { methods, .. }) = class_def_in(&hir.functions[0].body, "C")
    else {
        panic!("class C was not lowered");
    };
    let kinds: Vec<(&str, MethodKind)> = methods
        .iter()
        .map(|(name, _, kind)| (name.as_str(), *kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("build", MethodKind::Classmethod),
            ("helper", MethodKind::Staticmethod),
            ("value", MethodKind::Property),
        ]
    );
}

#[test]
fn bytearray_and_memoryview_lower_as_builtin_callables() {
    let source = "\
a = bytearray()
b = bytearray(4)
c = bytearray('hi', 'utf-8')
m = memoryview(b)
f = bytearray
";
    let hir = lower_ok(source);
    for (name, builtin) in [
        ("a", Builtin::ByteArray),
        ("b", Builtin::ByteArray),
        ("c", Builtin::ByteArray),
        ("m", Builtin::MemoryView),
    ] {
        let HExprKind::Call {
            callee: Callee::Builtin(actual),
            ..
        } = &assigned(&hir, name).kind
        else {
            panic!(
                "expected a builtin call for {name}: {:?}",
                assigned(&hir, name).kind
            );
        };
        assert_eq!(*actual, builtin, "{name}");
    }
    assert!(matches!(
        &assigned(&hir, "f").kind,
        HExprKind::BuiltinRef(Builtin::ByteArray)
    ));
}

#[test]
fn dynamic_module_constant_members_are_callable_values() {
    let source = "\
from _weakref import ref
class Holder:
    pass
item = Holder()
callback = None
x = ref(item, callback)
y = ref(item)
";
    let hir = lower_ok(source);
    for name in ["x", "y"] {
        let HExprKind::IndirectCall { callee, args, .. } = &assigned(&hir, name).kind else {
            panic!(
                "expected an indirect call for {name}: {:?}",
                assigned(&hir, name).kind
            );
        };
        assert!(matches!(
            &callee.kind,
            HExprKind::ModuleAttr { module, name } if module == "_weakref" && name == "ref"
        ));
        assert!(!args.is_empty());
    }
}

#[test]
fn class_body_method_aliases_become_method_entries() {
    let source = "\
class S:
    def difference(self, other):
        return other
    __sub__ = difference
    def __rsub__(self, other):
        return self.difference(other)
";
    let hir = lower_ok(source);
    let HStmtKind::ClassDef {
        methods,
        attributes,
        ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!("expected class S");
    };
    assert!(attributes.is_empty(), "{attributes:?}");
    let difference = methods
        .iter()
        .find(|(name, _, _)| name == "difference")
        .map(|(_, function, _)| *function)
        .expect("difference method");
    let alias = methods
        .iter()
        .find(|(name, _, _)| name == "__sub__")
        .map(|(_, function, kind)| (*function, *kind))
        .expect("__sub__ alias");
    assert_eq!(alias.0, difference, "the alias reuses the method function");
    assert_eq!(alias.1, MethodKind::Normal);
}

#[test]
fn class_body_unpacking_assignment_binds_class_attributes() {
    // NumPy's `mixins.py` builds its operators in one call and names them
    // through a tuple target. Every name is a class attribute, the right-hand
    // side runs once, and a later attribute reads the unpacked name.
    let source = "\
def numeric_methods():
    return 1, 2, 3
class DataOps:
    __add__, __radd__, __iadd__ = numeric_methods()
    primary = __add__
";
    let hir = lower_ok(source);
    // The body parks the right-hand side, unpacks it into hidden locals and
    // only then publishes the class.
    let then_body = hir.functions[0]
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::If { then_body, .. } => {
                let wraps_class = then_body.iter().any(|inner| {
                    matches!(&inner.kind, HStmtKind::ClassDef { name, .. } if name == "DataOps")
                });
                wraps_class.then_some(then_body)
            }
            _ => None,
        })
        .expect("class-attribute wrapper");
    assert_eq!(then_body.len(), 4, "{then_body:?}");
    assert!(
        matches!(then_body[0].kind, HStmtKind::Assign { .. }),
        "the right-hand side is parked once: {:?}",
        then_body[0].kind
    );
    let HStmtKind::Unpack { targets, value } = &then_body[1].kind else {
        panic!("expected the unpack, got {:?}", then_body[1].kind);
    };
    let bound: Vec<LocalId> = targets
        .iter()
        .map(|target| match target {
            HUnpackTarget::Name(id) => *id,
            other => panic!("expected a name target, got {other:?}"),
        })
        .collect();
    assert_eq!(bound.len(), 3);
    assert!(
        matches!(value.kind, HExprKind::Local(_) | HExprKind::Call { .. }),
        "the unpack reads the parked value: {value:?}"
    );
    assert!(matches!(&then_body[3].kind, HStmtKind::ClassDef { .. }));

    let HStmtKind::ClassDef {
        attributes,
        methods,
        ..
    } = class_def_in(&hir.functions[0].body, "DataOps").expect("class DataOps")
    else {
        unreachable!()
    };
    assert!(methods.is_empty(), "{methods:?}");
    let names: Vec<&str> = attributes.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["__add__", "__radd__", "__iadd__", "primary"]);
    for ((name, value), bound_local) in attributes[..3].iter().zip(&bound) {
        match &value.kind {
            HExprKind::Local(id) => assert_eq!(id, bound_local, "{name}"),
            other => panic!("expected the unpacked local for {name}, got {other:?}"),
        }
    }
    // `primary = __add__` reads the unpacked name: its parked value is the
    // local the first element binds.
    assert!(
        then_body.iter().any(|stmt| matches!(
            &stmt.kind,
            HStmtKind::Assign { value, .. }
                if matches!(&value.kind, HExprKind::Local(id) if *id == bound[0])
        )),
        "a later class attribute reads the unpacked name: {then_body:?}"
    );
}

#[test]
fn class_body_nested_and_starred_unpack_targets_bind_attributes() {
    let source = "\
class C:
    head, (second, *rest) = 1, (2, 3, 4)
    tail = rest
";
    let hir = lower_ok(source);
    let targets = first_unpack(&hir.functions[0].body).expect("unpack in the class body");
    assert_eq!(targets.len(), 2, "{targets:?}");
    assert!(matches!(&targets[0], HUnpackTarget::Name(_)));
    let HUnpackTarget::Nested(nested) = &targets[1] else {
        panic!("expected a nested target, got {:?}", targets[1]);
    };
    assert!(matches!(&nested[0], HUnpackTarget::Name(_)));
    let HUnpackTarget::Star(inner) = &nested[1] else {
        panic!("expected a starred target, got {:?}", nested[1]);
    };
    let HUnpackTarget::Name(rest) = &**inner else {
        panic!("expected a starred name target, got {inner:?}");
    };

    let HStmtKind::ClassDef { attributes, .. } =
        class_def_in(&hir.functions[0].body, "C").expect("class C")
    else {
        unreachable!()
    };
    let names: Vec<&str> = attributes.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["head", "second", "rest", "tail"]);
    // `tail = rest` reads the starred binding: the parked value of the later
    // attribute is the local the star binds.
    assert!(
        hir.functions[0].body.iter().any(|stmt| {
            matches!(
                &stmt.kind,
                HStmtKind::If { then_body, .. }
                    if then_body.iter().any(|inner| matches!(
                        &inner.kind,
                        HStmtKind::Assign { value, .. }
                            if matches!(&value.kind, HExprKind::Local(id) if *id == *rest)
                    ))
            )
        }),
        "the starred binding reaches later attributes"
    );
}

#[test]
fn class_body_chained_and_unpacking_assignment_binds_every_name() {
    let source = "class C:\n    whole = first, second = (1, 2)\n    later = whole\n";
    let hir = lower_ok(source);
    let targets = first_unpack(&hir.functions[0].body).expect("unpack in the class body");
    assert_eq!(targets.len(), 2, "{targets:?}");
    let HStmtKind::ClassDef { attributes, .. } =
        class_def_in(&hir.functions[0].body, "C").expect("class C")
    else {
        unreachable!()
    };
    let names: Vec<&str> = attributes.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["whole", "first", "second", "later"]);
    let local = |index: usize| match &attributes[index].1.kind {
        HExprKind::Local(id) => *id,
        other => panic!(
            "expected a local for {}, got {other:?}",
            attributes[index].0
        ),
    };
    assert_ne!(local(0), local(1), "the sequence and its elements differ");
    assert_ne!(local(1), local(2));
    // `later = whole` reads the chained target's parked value.
    assert!(
        hir.functions[0].body.iter().any(|stmt| {
            matches!(
                &stmt.kind,
                HStmtKind::If { then_body, .. }
                    if then_body.iter().any(|inner| matches!(
                        &inner.kind,
                        HStmtKind::Assign { value, .. }
                            if matches!(&value.kind, HExprKind::Local(id) if *id == local(0))
                    ))
            )
        }),
        "`whole` keeps the sequence the chained target parked"
    );
}

#[test]
fn class_body_unpacking_assignment_coexists_with_slots() {
    let source = "\
class C:
    __slots__ = ('x',)
    left, right = 1, 2
";
    let hir = lower_ok(source);
    let HStmtKind::ClassDef { attributes, .. } =
        class_def_in(&hir.functions[0].body, "C").expect("class C")
    else {
        unreachable!()
    };
    let names: Vec<&str> = attributes.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["__slots__", "left", "right"]);
}

#[test]
fn class_body_unpacking_assignment_in_a_dynamic_body_writes_the_namespace() {
    // A class registered inside a function always runs against the runtime
    // namespace dict, so the unpacked names must store through it.
    let source = "\
def make():
    class C:
        a, *rest = 1, 2, 3
        first = a
    return C
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let targets = first_unpack(function_body(&hir, "make")).expect("unpack");
    assert_eq!(targets.len(), 2, "{targets:?}");
    let HUnpackTarget::Subscript { object, index } = &targets[0] else {
        panic!("expected a namespace store, got {:?}", targets[0]);
    };
    assert!(matches!(object.kind, HExprKind::Local(_)), "{object:?}");
    assert!(
        matches!(&index.kind, HExprKind::Const(Const::Str(name)) if name == "a"),
        "{index:?}"
    );
    let HUnpackTarget::Star(inner) = &targets[1] else {
        panic!("expected a starred target, got {:?}", targets[1]);
    };
    let HUnpackTarget::Subscript { index, .. } = &**inner else {
        panic!("expected a namespace store, got {inner:?}");
    };
    assert!(
        matches!(&index.kind, HExprKind::Const(Const::Str(name)) if name == "rest"),
        "{index:?}"
    );
}

#[test]
fn class_body_unpacking_assignment_in_a_namespace_body_binds_names() {
    // Control flow sends the body to the runtime namespace even at module
    // level; an attribute element of the unpacking target stays a normal
    // store, and a later attribute reads the unpacked name.
    let source = "\
holder = [0]
class C:
    if True:
        left, holder[0] = 1, 2
    both = left
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let targets = first_unpack(&hir.functions[0].body).expect("unpack");
    let HUnpackTarget::Subscript { object, index } = &targets[0] else {
        panic!("expected a namespace store, got {:?}", targets[0]);
    };
    assert!(matches!(object.kind, HExprKind::Local(_)), "{object:?}");
    assert!(
        matches!(&index.kind, HExprKind::Const(Const::Str(name)) if name == "left"),
        "{index:?}"
    );
    let HUnpackTarget::Subscript { object, index } = &targets[1] else {
        panic!("expected a subscript store, got {:?}", targets[1]);
    };
    assert!(matches!(&object.kind, HExprKind::Local(_)), "{object:?}");
    assert!(
        matches!(&index.kind, HExprKind::Const(Const::Int(0))),
        "{index:?}"
    );
    crate::sema::check(&hir, &mut infer, "test.py").expect("valid program");
}

#[test]
fn class_body_names_prefer_the_class_namespace() {
    let source = "\
value = 1
class C:
    value = 2
    copy = value
";
    let hir = lower_ok(source);
    let HStmtKind::ClassDef { attributes, .. } = &hir.functions[0].body[1].kind else {
        panic!("expected class C");
    };
    let copy = attributes
        .iter()
        .find(|(name, _)| name == "copy")
        .map(|(_, value)| value)
        .expect("copy attribute");
    assert!(
        matches!(copy.kind, HExprKind::Const(Const::Int(2))),
        "{copy:?}"
    );
}

#[test]
fn zero_argument_super_supports_attributes_and_methods() {
    let source = "\
class A:
    def who(self):
        return 'a'
    def __init__(self):
        self.tag = 1
class B:
    def who(self):
        return 'b'
class C(A, B):
    def who(self):
        first = super().who()
        other = super().tag
        return first, other
    def __init__(self):
        super().__init__()
";
    let hir = lower_ok(source);
    let body = &function(&hir, "C.who").body;
    // `super().who()` and `super().tag` both build the proxy through
    // `self.$super_new(<defining class>)`.
    fn count_expr(expr: &HExpr, predicate: &dyn Fn(&HExprKind) -> bool) -> usize {
        let mut count = usize::from(predicate(&expr.kind));
        match &expr.kind {
            HExprKind::Bin { lhs, rhs, .. } => {
                count += count_expr(lhs, predicate) + count_expr(rhs, predicate);
            }
            HExprKind::BoolOp { lhs, rhs, .. } => {
                count += count_expr(lhs, predicate) + count_expr(rhs, predicate);
            }
            HExprKind::Un { operand, .. } => count += count_expr(operand, predicate),
            HExprKind::IfExp { test, then, orelse } => {
                count += count_expr(test, predicate)
                    + count_expr(then, predicate)
                    + count_expr(orelse, predicate);
            }
            HExprKind::List(items) | HExprKind::Tuple(items) | HExprKind::Set(items) => {
                count += items
                    .iter()
                    .map(|item| count_expr(item, predicate))
                    .sum::<usize>();
            }
            HExprKind::Dict(pairs) => {
                count += pairs
                    .iter()
                    .map(|(key, value)| count_expr(key, predicate) + count_expr(value, predicate))
                    .sum::<usize>();
            }
            HExprKind::Subscript { object, index } => {
                count += count_expr(object, predicate) + count_expr(index, predicate);
            }
            HExprKind::GetAttr { object, .. } => count += count_expr(object, predicate),
            HExprKind::MethodCall {
                object,
                args,
                star,
                keywords,
                double_star,
                ..
            } => {
                count += count_expr(object, predicate)
                    + args
                        .iter()
                        .map(|arg| count_expr(arg, predicate))
                        .sum::<usize>()
                    + star
                        .as_deref()
                        .map_or(0, |star| count_expr(star, predicate))
                    + keywords
                        .iter()
                        .map(|(_, value)| count_expr(value, predicate))
                        .sum::<usize>()
                    + double_star
                        .as_deref()
                        .map_or(0, |value| count_expr(value, predicate));
            }
            HExprKind::IndirectCall { callee, args, star } => {
                count += count_expr(callee, predicate)
                    + args
                        .iter()
                        .map(|arg| count_expr(arg, predicate))
                        .sum::<usize>()
                    + star
                        .as_deref()
                        .map_or(0, |star| count_expr(star, predicate));
            }
            HExprKind::CallKw {
                callee,
                args,
                keywords,
            } => {
                count += count_expr(callee, predicate)
                    + args
                        .iter()
                        .map(|arg| count_expr(arg, predicate))
                        .sum::<usize>()
                    + keywords
                        .iter()
                        .map(|(_, value)| count_expr(value, predicate))
                        .sum::<usize>();
            }
            HExprKind::CallFull {
                callee,
                args,
                star,
                keywords,
                double_star,
            } => {
                count += count_expr(callee, predicate)
                    + args
                        .iter()
                        .map(|arg| count_expr(arg, predicate))
                        .sum::<usize>()
                    + star
                        .as_deref()
                        .map_or(0, |star| count_expr(star, predicate))
                    + keywords
                        .iter()
                        .map(|(_, value)| count_expr(value, predicate))
                        .sum::<usize>()
                    + double_star
                        .as_deref()
                        .map_or(0, |value| count_expr(value, predicate));
            }
            _ => {}
        }
        count
    }
    let statements_count = |predicate: &dyn Fn(&HExprKind) -> bool| {
        body.iter()
            .map(|stmt| match &stmt.kind {
                HStmtKind::Assign { value, .. } | HStmtKind::Expr(value) => {
                    count_expr(value, predicate)
                }
                HStmtKind::Return(Some(value)) => count_expr(value, predicate),
                _ => 0,
            })
            .sum::<usize>()
    };
    let proxies = statements_count(&|kind| {
        matches!(
            kind,
            HExprKind::MethodCall {
                method,
                args,
                ..
            } if method == "$super_new" && args.len() == 1
        )
    });
    assert_eq!(proxies, 2, "expected a super proxy per access: {body:#?}");
    let method_calls = statements_count(
        &|kind| matches!(kind, HExprKind::MethodCall { method, .. } if method == "who"),
    );
    assert_eq!(method_calls, 1, "{body:#?}");
    let getattr =
        statements_count(&|kind| matches!(kind, HExprKind::GetAttr { name, .. } if name == "tag"));
    assert_eq!(getattr, 1, "{body:#?}");
    // `super().__init__()` keeps the keyword-capable initializer helper.
    let init = &function(&hir, "C.__init__").body;
    assert!(init.iter().any(|stmt| {
        matches!(
            &stmt.kind,
            HStmtKind::Expr(HExpr {
                kind: HExprKind::MethodCall { method, .. },
                ..
            }) if method == "$super_init"
        )
    }));
}

#[test]
fn decorated_functions_keep_name_access_and_indirect_calls() {
    let source = "\
def deco(tag):
    def apply(fn):
        def wrapper(*args, **kwargs):
            return tag + fn(*args, **kwargs)
        return wrapper
    return apply
@deco('[')
def add(a, /, b, *, c=0):
    return a + b + c
name = add.__name__
result = add(1, 2, c=3)
";
    let hir = lower_ok(source);
    let name = assigned(&hir, "name");
    assert!(matches!(&name.kind, HExprKind::GetAttr { name, .. } if name == "__name__"));
    // The call goes through the decorated binding, never the raw function id.
    let result = assigned(&hir, "result");
    let callee = match &result.kind {
        HExprKind::CallKw { callee, .. } | HExprKind::CallFull { callee, .. } => callee,
        other => panic!("expected a call through the decorated binding, got {other:?}"),
    };
    assert!(
        matches!(&callee.kind, HExprKind::Local(_)),
        "the decorated binding must be called, got {callee:?}"
    );
}

#[test]
fn three_level_decorator_supports_keyword_calls() {
    // Regression: the returned `wrapper(*args, **kwargs)` closure captures
    // `tag` and `fn`; a keyword call on the decorated name must bind against
    // `wrapper`'s own signature, never the captures of `apply`.
    let source = "\
def deco2(tag):
    def apply(fn):
        def wrapper(*args, **kwargs):
            return tag + str(fn(*args, **kwargs))
        return wrapper
    return apply
@deco2('x')
def mul(a, b=0):
    return a * b
first = mul(2, 3)
second = mul(2, b=3)
";
    let hir = lower_ok(source);
    let wrapper = function(&hir, "deco2.apply.wrapper");
    assert_eq!(wrapper.capture_count, 2, "tag and fn are captures");
    assert_eq!(wrapper.params[0].name, "tag");
    assert_eq!(wrapper.params[1].name, "fn");
    let kinds: Vec<ParamKind> = wrapper.params.iter().map(|param| param.kind).collect();
    assert_eq!(
        &kinds[2..],
        &[ParamKind::VarArg, ParamKind::KwArg],
        "wrapper keeps its own *args/**kwargs signature"
    );
    // Both calls go through the decorated module binding.
    for name in ["first", "second"] {
        let call = assigned(&hir, name);
        let callee = match &call.kind {
            HExprKind::CallKw { callee, .. }
            | HExprKind::CallFull { callee, .. }
            | HExprKind::IndirectCall { callee, .. } => callee,
            other => panic!("expected an indirect call for {name}, got {other:?}"),
        };
        assert!(matches!(&callee.kind, HExprKind::Local(_)), "{callee:?}");
    }
    let HExprKind::CallKw { keywords, .. } = &assigned(&hir, "second").kind else {
        panic!("expected a keyword call");
    };
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "b");
}

#[test]
fn descriptor_wrappers_work_as_builtin_values_and_class_entries() {
    let source = "\
from types import GenericAlias
class C:
    def plain(self):
        return 1
    def getter(self):
        return 2
    cm = classmethod(plain)
    sm = staticmethod(plain)
    pr = property(getter)
    __class_getitem__ = classmethod(GenericAlias)
a = classmethod
b = staticmethod
c = property
";
    let hir = lower_ok(source);
    let HStmtKind::ClassDef {
        methods,
        attributes,
        ..
    } = &hir.functions[0].body[0].kind
    else {
        panic!("expected class C");
    };
    let plain = methods
        .iter()
        .find(|(name, _, _)| name == "plain")
        .map(|(_, function, _)| *function)
        .expect("plain method");
    let getter = methods
        .iter()
        .find(|(name, _, _)| name == "getter")
        .map(|(_, function, _)| *function)
        .expect("getter method");
    let entry = |name: &str| {
        methods
            .iter()
            .find(|(candidate, _, _)| candidate == name)
            .map(|(_, function, kind)| (*function, *kind))
            .unwrap_or_else(|| panic!("missing method entry {name}"))
    };
    assert_eq!(entry("cm"), (plain, MethodKind::Classmethod));
    assert_eq!(entry("sm"), (plain, MethodKind::Staticmethod));
    assert_eq!(entry("pr"), (getter, MethodKind::Property));
    // A wrapped class value is not an earlier method, so it stays a class
    // attribute holding the descriptor object built by the runtime.
    let class_getitem = attributes
        .iter()
        .find(|(name, _)| name == "__class_getitem__")
        .map(|(_, value)| value)
        .expect("__class_getitem__ attribute");
    assert!(
        matches!(
            &class_getitem.kind,
            HExprKind::Call {
                callee: Callee::Builtin(Builtin::Classmethod),
                args,
            } if args.len() == 1
        ),
        "{class_getitem:?}"
    );
    for (name, expected) in [
        ("a", Builtin::Classmethod),
        ("b", Builtin::Staticmethod),
        ("c", Builtin::Property),
    ] {
        assert!(
            matches!(&assigned(&hir, name).kind, HExprKind::BuiltinRef(actual) if *actual == expected),
            "{name}: {:?}",
            assigned(&hir, name).kind
        );
    }
}

#[test]
fn property_accepts_named_components() {
    let source = "\
class C:
    def getter(self):
        return 1
    def setter(self, value):
        pass
    value = property(fget=getter, fset=setter, doc='value')
";
    let hir = lower_ok(source);
    let HStmtKind::ClassDef { attributes, .. } = &hir.functions[0].body[0].kind else {
        panic!("expected class C");
    };
    let value = attributes
        .iter()
        .find(|(name, _)| name == "value")
        .map(|(_, value)| value)
        .expect("value attribute");
    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Property),
        args,
    } = &value.kind
    else {
        panic!("expected a property() call: {value:?}");
    };
    assert_eq!(args.len(), 4, "fget, fset, fdel=None, doc");
    assert!(matches!(args[2].kind, HExprKind::Const(Const::None)));
}

#[test]
fn descriptor_builtin_arity_is_checked() {
    for source in [
        "x = classmethod()\n",
        "x = staticmethod(1, 2)\n",
        "x = property()\n",
        "x = property(1, 2, 3, 4, 5)\n",
    ] {
        let diags = crate::check_source(source, "test.py")
            .expect_err("invalid descriptor call must be rejected");
        assert!(!diags.is_empty(), "{source}");
    }
}

#[test]
fn print_accepts_its_cpython_keywords() {
    let source = "\
import sys
f = sys.stdout
print('x')
print('x', file=f, end='', sep='-', flush=True)
items = ['a', 'b']
print(*items, sep=',')
p = print
p('y', file=f)
";
    let hir = lower_ok(source);
    let exprs = module_exprs(&hir);
    // `print('x')` stays the plain positional builtin call.
    assert!(exprs.iter().any(|expr| matches!(
        &expr.kind,
        HExprKind::Call {
            callee: Callee::Builtin(Builtin::Print),
            ..
        }
    )));
    // The keyword form goes through the runtime keyword binder with the
    // keyword values in source order.
    let keyworded = exprs
        .iter()
        .find(|expr| matches!(expr.kind, HExprKind::CallKw { .. }))
        .expect("keyworded print");
    let HExprKind::CallKw {
        callee,
        args,
        keywords,
    } = &keyworded.kind
    else {
        unreachable!()
    };
    assert!(matches!(callee.kind, HExprKind::BuiltinRef(Builtin::Print)));
    assert_eq!(args.len(), 1);
    let names: Vec<&str> = keywords.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["file", "end", "sep", "flush"]);
    // A first-class `p = print; p(...)` call binds keywords through the value.
    assert!(
        exprs.iter().any(|expr| matches!(
            &expr.kind,
            HExprKind::CallKw { callee, .. }
                if matches!(&callee.kind, HExprKind::Local(_))
        )),
        "first-class print keyword call"
    );
    // A starred positional operand with a keyword uses the general flattener.
    assert!(exprs.iter().any(|expr| matches!(
        &expr.kind,
        HExprKind::CallFull { callee, .. }
            if matches!(&callee.kind, HExprKind::BuiltinRef(Builtin::Print))
    )));
}

#[test]
fn print_rejects_unknown_keywords() {
    let source = "print('x', bad=1)\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]
            .message
            .contains("unexpected keyword argument 'bad'"),
        "{diags:?}"
    );
}

#[test]
fn globals_call_carries_the_owning_module_name() {
    let source = "def read():\n    return globals()\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let HStmtKind::Return(Some(value)) = &hir.functions[1].body[0].kind else {
        panic!("expected a return, got {:?}", hir.functions[1].body[0]);
    };
    let HExprKind::Call {
        callee: Callee::Builtin(Builtin::Globals),
        args,
    } = &value.kind
    else {
        panic!("expected a globals() call, got {:?}", value.kind);
    };
    // The module name is compile-time knowledge: it is the call's single
    // hidden argument instead of a runtime frame lookup.
    assert_eq!(args.len(), 1, "{args:?}");
    assert!(matches!(
        &args[0].kind,
        HExprKind::Const(Const::Str(name)) if name == "__main__"
    ));
    crate::sema::check(&hir, &mut infer, "test.py").expect("valid program");
}

#[test]
fn inspect_style_dynamic_globals_compile_unknown_module_reads() {
    // CPython's inspect.py installs its `CO_*` constants by writing through
    // `globals()`, then reads them inside functions. The reads have no static
    // binding, so they must become runtime global lookups.
    let source = "\
mod_dict = globals()
for flag, value in [('GENERATOR', 32)]:
    mod_dict['CO_' + flag] = value
del flag, value, mod_dict

def is_generator_function(obj):
    return CO_GENERATOR
";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let read = function(&hir, "is_generator_function");
    let HStmtKind::Return(Some(value)) = &read.body[0].kind else {
        panic!("expected a return, got {:?}", read.body[0]);
    };
    assert!(
        matches!(
            &value.kind,
            HExprKind::GlobalGet { module, name }
                if module == "__main__" && name == "CO_GENERATOR"
        ),
        "the dynamically installed name must be read from the module table, got {:?}",
        value.kind
    );
    crate::sema::check(&hir, &mut infer, "test.py").expect("dynamic global read");
}

#[test]
fn dynamic_global_alias_from_vars_allows_unknown_reads() {
    let source = "namespace = vars()\nnamespace['injected'] = 1\ndef read():\n    return injected\ndef invoke(obj):\n    return hook(obj)\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let read = function(&hir, "read");
    let HStmtKind::Return(Some(value)) = &read.body[0].kind else {
        panic!("expected a return, got {:?}", read.body[0]);
    };
    assert!(
        matches!(
            &value.kind,
            HExprKind::GlobalGet { module, name }
                if module == "__main__" && name == "injected"
        ),
        "an alias of vars() is dynamic too, got {:?}",
        value.kind
    );
    // A dynamically installed callable is resolved the same way: the callee
    // is a module global read, not a compile-time "not defined" error.
    let invoke = function(&hir, "invoke");
    let HStmtKind::Return(Some(value)) = &invoke.body[0].kind else {
        panic!("expected a return, got {:?}", invoke.body[0]);
    };
    let HExprKind::IndirectCall { callee, .. } = &value.kind else {
        panic!("expected an indirect call, got {:?}", value.kind);
    };
    assert!(
        matches!(
            &callee.kind,
            HExprKind::GlobalGet { module, name }
                if module == "__main__" && name == "hook"
        ),
        "the dynamic callee must be a global read, got {:?}",
        callee.kind
    );
}

#[test]
fn undefined_name_in_an_ordinary_module_still_errors() {
    let source = "value = 1\ndef read():\n    return valeu\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::NAME);
    assert_eq!(diags[0].message, "name 'valeu' is not defined");
}

#[test]
fn dynamic_globals_do_not_hide_local_scope_errors() {
    // `item` is a module local no function can see; reaching it from `read`
    // stays a compile-time scope error even though the module writes through
    // `globals()`.
    let source =
        "namespace = globals()\nfor item in [1]:\n    pass\ndef read():\n    return item\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::NAME);
    assert!(
        diags[0]
            .message
            .contains("is not visible inside a function"),
        "{diags:?}"
    );
}

#[test]
fn class_body_statements_run_against_a_runtime_namespace() {
    let source = "class C:\n    x = 1\n    del x\n    if True:\n        y = 2\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    // A body that executes statements lowers to the namespace sequence
    // wrapper rather than the static `class` statement.
    assert!(
        matches!(&hir.functions[0].body[0].kind, HStmtKind::If { .. }),
        "expected the dynamic class wrapper, got {:?}",
        hir.functions[0].body[0].kind
    );
    crate::sema::check(&hir, &mut infer, "test.py").expect("valid program");
}

#[test]
fn conditional_module_definitions_bind_through_the_global_table() {
    let source = "\
if True:
    def f():
        return 1
try:
    def g():
        return 2
except Exception:
    pass
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(hir.functions.iter().any(|function| function.name == "f"));
    assert!(hir.functions.iter().any(|function| function.name == "g"));
    assert!(has_global_set(&hir.functions[0].body));

    // The definitions are conditional, so they never enter the static
    // function table: calls resolve through the runtime binding.
    let call = "if True:\n    def f():\n        return 1\nx = f()\n";
    let (hir, _, diags) = lower_source(call, &parse(call));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(matches!(
        assigned(&hir, "x").kind,
        HExprKind::IndirectCall { .. }
    ));
}

#[test]
fn conditional_module_class_definitions_are_registered() {
    let source = "\
if True:
    class C:
        def m(self):
            return 1
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(hir.functions.iter().any(|function| function.name == "C.m"));
    fn has_class(stmts: &[HStmt]) -> bool {
        stmts.iter().any(|stmt| match &stmt.kind {
            HStmtKind::ClassDef { name, .. } if name == "C" => true,
            HStmtKind::If {
                then_body,
                else_body,
                ..
            } => has_class(then_body) || has_class(else_body),
            _ => false,
        })
    }
    assert!(has_class(&hir.functions[0].body));
}

#[test]
fn conditional_module_class_constructors_bind_at_runtime() {
    // A class defined inside module-level control flow is created only when
    // that block runs, so its constructor calls must not bind a compile-time
    // `__init__` signature. `typing_extensions` defines its 3.9 alias fallback
    // in a dead `if not hasattr(typing, 'Concatenate')` branch and calls it
    // with keyword-only arguments its `__init__` does not declare; the call
    // has to reach the runtime class instead of being rejected statically.
    let source = "\
if True:
    class C:
        def __init__(self, origin, args):
            self.origin = origin


def make():
    return C(1, (2,), _typevar_types=(int,))
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let make = hir
        .functions
        .iter()
        .find(|function| function.name == "make")
        .expect("make must be lowered");
    fn has_call_kw(stmts: &[HStmt]) -> bool {
        stmts.iter().any(|stmt| match &stmt.kind {
            HStmtKind::Return(Some(value)) => match &value.kind {
                HExprKind::CallKw {
                    callee, keywords, ..
                } => {
                    matches!(&callee.kind, HExprKind::GlobalGet { name, .. } if name == "C")
                        && keywords.iter().any(|(name, _)| name == "_typevar_types")
                }
                _ => false,
            },
            HStmtKind::If {
                then_body,
                else_body,
                ..
            } => has_call_kw(then_body) || has_call_kw(else_body),
            HStmtKind::While {
                body, else_body, ..
            }
            | HStmtKind::ForRange {
                body, else_body, ..
            }
            | HStmtKind::ForEach {
                body, else_body, ..
            } => has_call_kw(body) || has_call_kw(else_body),
            HStmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => {
                has_call_kw(body)
                    || handlers.iter().any(|handler| has_call_kw(&handler.body))
                    || has_call_kw(else_body)
                    || has_call_kw(finally_body)
            }
            _ => false,
        })
    }
    assert!(
        has_call_kw(&make.body),
        "the conditional class constructor must lower as a runtime keyword call: {:?}",
        make.body
    );
}

#[test]
fn hasattr_of_native_module_decides_static_if_branches() {
    // `builtins.sentinel` only exists on 3.15, but Hydro targets 3.14: the
    // guard is false and the dead branch must not be typed (reading the
    // missing member would be a compile error). `typing.Concatenate` is in the
    // native spec, so the opposite branch is dead.
    let source = "\
import builtins
import os
import typing
if hasattr(builtins, \"sentinel\"):
    sentinel = builtins.sentinel
else:
    sentinel = None
if hasattr(typing, \"Concatenate\"):
    concat = 1
else:
    concat = 2
if not hasattr(builtins, \"sentinel\"):
    fallback = 3
else:
    fallback = 4
if hasattr(os, \"path\"):
    nested = 1
else:
    nested = 2
";
    let hir = lower_ok(source);
    let locals = &hir.functions[0].locals;
    fn assigned_name<'a>(stmt: &'a HStmt, locals: &'a [HLocal]) -> &'a str {
        match &stmt.kind {
            HStmtKind::Assign { target, .. } => locals[*target as usize].name.as_str(),
            other => panic!("expected an assignment, got {other:?}"),
        }
    }
    let ifs: Vec<&HStmt> = hir.functions[0]
        .body
        .iter()
        .filter(|stmt| matches!(stmt.kind, HStmtKind::If { .. }))
        .collect();
    assert_eq!(ifs.len(), 4, "{:?}", hir.functions[0].body);

    let HStmtKind::If {
        cond,
        then_body,
        else_body,
    } = &ifs[0].kind
    else {
        unreachable!()
    };
    assert!(matches!(cond.kind, HExprKind::Const(Const::Bool(false))));
    assert!(then_body.is_empty(), "{then_body:?}");
    assert_eq!(else_body.len(), 1);
    let HStmtKind::Assign { value, .. } = &else_body[0].kind else {
        unreachable!()
    };
    assert!(matches!(value.kind, HExprKind::Const(Const::None)));
    assert_eq!(assigned_name(&else_body[0], locals), "sentinel");

    let HStmtKind::If {
        cond,
        then_body,
        else_body,
    } = &ifs[1].kind
    else {
        unreachable!()
    };
    assert!(matches!(cond.kind, HExprKind::Const(Const::Bool(true))));
    assert_eq!(then_body.len(), 1);
    assert!(else_body.is_empty(), "{else_body:?}");
    assert_eq!(assigned_name(&then_body[0], locals), "concat");

    let HStmtKind::If {
        cond,
        then_body,
        else_body,
    } = &ifs[2].kind
    else {
        unreachable!()
    };
    assert!(matches!(cond.kind, HExprKind::Const(Const::Bool(true))));
    assert!(!then_body.is_empty(), "{then_body:?}");
    assert!(else_body.is_empty(), "{else_body:?}");
    assert_eq!(assigned_name(&then_body[0], locals), "fallback");

    // A nested native submodule counts as an attribute (`os.path`).
    let HStmtKind::If {
        cond,
        then_body,
        else_body,
    } = &ifs[3].kind
    else {
        unreachable!()
    };
    assert!(matches!(cond.kind, HExprKind::Const(Const::Bool(true))));
    assert_eq!(then_body.len(), 1);
    assert!(else_body.is_empty(), "{else_body:?}");
    assert_eq!(assigned_name(&then_body[0], locals), "nested");
}

#[test]
fn hasattr_of_user_module_and_computed_names_stays_dynamic() {
    // A user module may gain attributes at runtime, so its guard must keep
    // both branches. The same holds for a computed attribute name, and for
    // `errno`, whose platform names the spec table does not enumerate.
    let module = parse(
        "\
if hasattr(numpy, \"attrs\"):
    a = 1
else:
    a = 2
if not hasattr(numpy, \"attrs\"):
    b = 1
else:
    b = 2
if hasattr(builtins, name):
    c = 1
else:
    c = 2
if hasattr(errno, \"ENETUNREACH\"):
    d = 1
else:
    d = 2
",
    );
    for stmt in &module.body {
        let ast::Stmt::If(s) = stmt else {
            panic!("expected an if statement");
        };
        assert_eq!(static_if_branch(&s.test, "test"), None);
    }
}

#[test]
fn class_body_delete_binds_and_missing_names_raise_name_error() {
    let source = "\
class C:
    x = 1
    del x
    del missing
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    fn contains_name_error(stmts: &[HStmt]) -> bool {
        stmts.iter().any(|stmt| match &stmt.kind {
            HStmtKind::Raise {
                exc: Some(ExcKind::NameError),
                ..
            } => true,
            HStmtKind::If {
                then_body,
                else_body,
                ..
            } => contains_name_error(then_body) || contains_name_error(else_body),
            HStmtKind::While {
                body, else_body, ..
            }
            | HStmtKind::ForRange {
                body, else_body, ..
            }
            | HStmtKind::ForEach {
                body, else_body, ..
            } => contains_name_error(body) || contains_name_error(else_body),
            HStmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => {
                contains_name_error(body)
                    || handlers
                        .iter()
                        .any(|handler| contains_name_error(&handler.body))
                    || contains_name_error(else_body)
                    || contains_name_error(finally_body)
            }
            _ => false,
        })
    }
    assert!(contains_name_error(&hir.functions[0].body));
}

#[test]
fn builtins_module_exposes_dict_eval_exec_globals_and_locals() {
    let source = "\
import builtins
d = builtins.__dict__
e = builtins.eval
x = builtins.exec
g = builtins.globals
l = builtins.locals
";
    let hir = lower_ok(source);
    for (name, member) in [
        ("d", "__dict__"),
        ("e", "eval"),
        ("x", "exec"),
        ("g", "globals"),
        ("l", "locals"),
    ] {
        match &assigned(&hir, name).kind {
            HExprKind::ModuleAttr { module, name } => {
                assert_eq!((module.as_str(), name.as_str()), ("builtins", member));
            }
            other => panic!("expected module attribute for {name}, got {other:?}"),
        }
    }
}

#[test]
fn eval_keyword_arguments_are_folded_into_positional_slots() {
    let source = "\
a = eval('x + 1', globals={'x': 1}, locals={})
b = eval('x', locals={'x': 2})
";
    let hir = lower_ok(source);
    for name in ["a", "b"] {
        match &assigned(&hir, name).kind {
            HExprKind::Call {
                callee: Callee::Builtin(Builtin::Eval),
                args,
            } => {
                assert_eq!(args.len(), 3, "{name}");
                if name == "b" {
                    assert!(matches!(&args[1].kind, HExprKind::Const(Const::None)));
                }
            }
            other => panic!("expected eval() call for {name}, got {other:?}"),
        }
    }
}

#[test]
fn types_celltype_is_a_callable_module_member() {
    let source = "\
import types
c = types.CellType(1)
flag = isinstance(c, types.CellType)
";
    let hir = lower_ok(source);
    match &assigned(&hir, "c").kind {
        HExprKind::ModuleCall {
            module, function, ..
        } => assert_eq!((module.as_str(), function.as_str()), ("types", "CellType")),
        other => panic!("expected types.CellType call, got {other:?}"),
    }
}

#[test]
fn bootstrap_language_slice_is_implemented() {
    // The implemented slice that unblocks the first NumPy/annotationlib
    // lowering stages. This deliberately does not claim annotationlib itself
    // imports cleanly: its remaining dependencies are tracked in docs/TODO.
    let source = "\
if True:
    def conditional():
        return 1
class WithSlots:
    __slots__ = ('x',)
class Dynamic:
    value = 1
    del value
    del missing
import builtins
namespace = builtins.__dict__
number = eval('1 + 2 * 3', globals={}, locals={})
import types
cell = types.CellType(1)
flag = isinstance(cell, types.CellType)
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(hir
        .functions
        .iter()
        .any(|function| function.name == "conditional"));

    let mut has_slots = false;
    let mut has_dynamic_class = false;
    for stmt in &hir.functions[0].body {
        match &stmt.kind {
            HStmtKind::ClassDef {
                name, attributes, ..
            } => {
                if name == "WithSlots" {
                    has_slots |= attributes.iter().any(|(name, _)| name == "__slots__");
                }
                if name == "Dynamic" {
                    has_dynamic_class = true;
                }
            }
            HStmtKind::If { then_body, .. } => {
                has_dynamic_class |= then_body
                    .iter()
                    .any(|stmt| matches!(&stmt.kind, HStmtKind::SetSubscript { .. }));
            }
            _ => {}
        }
    }
    assert!(has_slots);
    // A class body with `del` lowers to the dynamic namespace sequence.
    assert!(has_dynamic_class);
}

#[test]
#[ignore = "blocked on typing/warnings/ast/NameError parity; tracked in docs/TODO"]
fn annotationlib_lowers_without_diagnostics() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.hydro/cpython/Python-3.14.9/Lib/annotationlib.py");
    let Ok(source) = fs::read_to_string(&path) else {
        // The CPython source tree is an optional fixture; the gate that owns
        // it runs the real import.
        return;
    };
    let module = parse(&source);
    let (_, _, diags) = lower_source(&source, &module);
    assert!(diags.is_empty(), "{diags:?}");
}

/// True when `stmts` or a nested block stores `key` into a namespace dict.
fn sets_namespace_key(stmts: &[HStmt], key: &str) -> bool {
    stmts.iter().any(|stmt| match &stmt.kind {
        HStmtKind::SetSubscript { index, .. } => {
            matches!(&index.kind, HExprKind::Const(Const::Str(name)) if name == key)
        }
        HStmtKind::If {
            then_body,
            else_body,
            ..
        } => sets_namespace_key(then_body, key) || sets_namespace_key(else_body, key),
        HStmtKind::While {
            body, else_body, ..
        }
        | HStmtKind::ForRange {
            body, else_body, ..
        }
        | HStmtKind::ForEach {
            body, else_body, ..
        } => sets_namespace_key(body, key) || sets_namespace_key(else_body, key),
        HStmtKind::Try {
            body,
            handlers,
            else_body,
            finally_body,
        } => {
            sets_namespace_key(body, key)
                || handlers
                    .iter()
                    .any(|handler| sets_namespace_key(&handler.body, key))
                || sets_namespace_key(else_body, key)
                || sets_namespace_key(finally_body, key)
        }
        _ => false,
    })
}

/// The `_type_alias_new` call of a lowered `type` statement.
fn alias_constructor(expr: &HExpr) -> (&str, &str, &[HExpr]) {
    match &expr.kind {
        HExprKind::ModuleCall {
            module,
            function,
            args,
            ..
        } => (module.as_str(), function.as_str(), args.as_slice()),
        other => panic!("expected a module call, got {other:?}"),
    }
}

#[test]
fn type_alias_statement_binds_a_runtime_alias() {
    let source = "type Shape = tuple[int, ...]\n\
                  def area(shape: Shape) -> Shape:\n    return shape\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let (module, function, args) = alias_constructor(assigned(&hir, "Shape"));
    assert_eq!((module, function), ("typing", "_type_alias_new"));
    assert_eq!(args.len(), 4);
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::Str(name)) if name == "Shape"));
    // The value is the runtime-evaluated annotation expression.
    assert!(matches!(&args[1].kind, HExprKind::Subscript { .. }));
    assert!(matches!(&args[2].kind, HExprKind::Tuple(params) if params.is_empty()));
    assert!(matches!(&args[3].kind, HExprKind::Const(Const::Str(module)) if module == "__main__"));

    // The alias is usable in annotations: they lower to no statements, so the
    // function body is exactly its `return`.
    let body = function_body(&hir, "area");
    assert_eq!(body.len(), 1, "{body:?}");
    assert!(matches!(&body[0].kind, HStmtKind::Return(Some(_))));
}

#[test]
fn generic_type_alias_sets_up_type_parameters_and_binds_the_alias() {
    let source = "type Vector[T] = list[T]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let body = &hir.functions[0].body;
    let HStmtKind::If { then_body, .. } = &body[0].kind else {
        panic!("a generic alias lowers through an `if True:` block: {body:?}");
    };
    // First statement: the TypeVar value in a hidden local.
    let HStmtKind::Assign { value, .. } = &then_body[0].kind else {
        panic!("expected the type-parameter setup: {then_body:?}");
    };
    let (module, function, args) = alias_constructor(value);
    assert_eq!((module, function), ("typing", "TypeVar"));
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::Str(name)) if name == "T"));

    // Last statement: the alias itself, with the parameter tuple.
    let HStmtKind::Assign { value, .. } = &then_body.last().unwrap().kind else {
        panic!("the alias binding must be last: {then_body:?}");
    };
    let (_, function, args) = alias_constructor(value);
    assert_eq!(function, "_type_alias_new");
    assert!(matches!(&args[2].kind, HExprKind::Tuple(params) if params.len() == 1));

    // The value reads the hidden type-parameter local rather than a module
    // binding of `T`.
    let HExprKind::Subscript { index, .. } = &args[1].kind else {
        panic!("expected the alias value subscript: {:?}", args[1].kind);
    };
    let HExprKind::Local(local) = &index.kind else {
        panic!("`list[T]` must read the type-parameter local: {index:?}");
    };
    assert!(
        hir.functions[0].locals[*local as usize]
            .name
            .starts_with("$typeparam"),
        "unexpected local name: {}",
        hir.functions[0].locals[*local as usize].name
    );

    // The parameter name is scoped to the alias.
    let (_, _, diags) = lower_source(
        "type Vector[T] = list[T]\nleaked = T\n",
        &parse("type Vector[T] = list[T]\nleaked = T\n"),
    );
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::NAME);
}

#[test]
fn type_alias_type_parameter_bound_and_variadic_forms_lower() {
    let source = "type Vector[T: int] = list[T]\n\
                  type Pairs[*Ts] = tuple[*Ts]\n\
                  type Handler[**P] = None\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let body = &hir.functions[0].body;
    // Bounded parameter: the TypeVar call carries `bound=`.
    let HStmtKind::If { then_body, .. } = &body[0].kind else {
        panic!("expected the assignment to install the class");
    };
    let HStmtKind::Assign { value, .. } = &then_body[0].kind else {
        panic!("expected the type-parameter setup: {then_body:?}");
    };
    let HExprKind::ModuleCall { keywords, .. } = &value.kind else {
        panic!("expected a TypeVar call: {value:?}");
    };
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "bound");

    // Variadic and parametric parameters are real runtime type parameters.
    let mut functions: Vec<&str> = Vec::new();
    for stmt in body {
        let HStmtKind::If { then_body, .. } = &stmt.kind else {
            continue;
        };
        let HStmtKind::Assign { value, .. } = &then_body[0].kind else {
            continue;
        };
        if let HExprKind::ModuleCall { function, .. } = &value.kind {
            functions.push(function.as_str());
        }
    }
    assert_eq!(functions, ["TypeVar", "TypeVarTuple", "ParamSpec"]);
}

#[test]
fn type_alias_in_a_function_binds_a_local_slot() {
    let source = "def build():\n    type Pair = tuple[int, int]\n    return Pair\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let function = hir
        .functions
        .iter()
        .find(|function| function.name == "build")
        .expect("build");
    let HStmtKind::Assign { target, value } = &function.body[0].kind else {
        panic!(
            "expected a local alias assignment: {:?}",
            function.body[0].kind
        );
    };
    assert_eq!(function.locals[*target as usize].name, "Pair");
    let (module, function_name, _) = alias_constructor(value);
    assert_eq!((module, function_name), ("typing", "_type_alias_new"));
}

#[test]
fn type_alias_in_a_class_body_binds_through_the_namespace() {
    let source = "class Registry:\n    type Entry = dict[str, int]\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        sets_namespace_key(&hir.functions[0].body, "Entry"),
        "the alias must bind in the class namespace: {:?}",
        hir.functions[0].body
    );
}

#[test]
fn type_alias_binding_is_mirrored_for_importers() {
    let source = "type Shape = str\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "helper.py", &mut infer);
    lowerer.set_mirror_all_globals(true);
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        binds_module_global(&hir.functions[0].body, "Shape"),
        "an imported unit must mirror the alias: {:?}",
        hir.functions[0].body
    );
}

/// Name of the hidden local a lowered expression reads, when it is one.
fn hidden_local_name<'a>(hir: &'a LHir, expr: &HExpr) -> Option<&'a str> {
    match &expr.kind {
        HExprKind::Local(local) => Some(&hir.functions[0].locals[*local as usize].name),
        _ => None,
    }
}

/// `typing.<function>` calls found in the value expressions of `stmts` and of
/// every nested block, depth-first, with their first argument when it is a
/// string constant (a PEP 695 parameter name).
fn type_param_calls(stmts: &[HStmt], out: &mut Vec<(String, String)>) {
    for stmt in stmts {
        match &stmt.kind {
            HStmtKind::Assign { value, .. } => {
                if let HExprKind::ModuleCall {
                    module,
                    function,
                    args,
                    ..
                } = &value.kind
                {
                    let name = match args.first().map(|arg| &arg.kind) {
                        Some(HExprKind::Const(Const::Str(name))) => name.clone(),
                        _ => String::new(),
                    };
                    out.push((format!("{module}.{function}"), name));
                }
            }
            HStmtKind::If {
                then_body,
                else_body,
                ..
            } => {
                type_param_calls(then_body, out);
                type_param_calls(else_body, out);
            }
            HStmtKind::While {
                body, else_body, ..
            }
            | HStmtKind::ForRange {
                body, else_body, ..
            }
            | HStmtKind::ForEach {
                body, else_body, ..
            } => {
                type_param_calls(body, out);
                type_param_calls(else_body, out);
            }
            HStmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => {
                type_param_calls(body, out);
                for handler in handlers {
                    type_param_calls(&handler.body, out);
                }
                type_param_calls(else_body, out);
                type_param_calls(finally_body, out);
            }
            _ => {}
        }
    }
}

#[test]
fn generic_class_publishes_type_params_and_scopes_them() {
    let source = "class Box[T]:\n    value = T\n";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let body = &hir.functions[0].body;
    assert_eq!(
        body.len(),
        1,
        "the parameter setup and the class share one statement: {body:?}"
    );
    let HStmtKind::If { then_body, .. } = &body[0].kind else {
        panic!("a generic class lowers through an `if True:` block: {body:?}");
    };
    // First statement: the TypeVar value in a hidden local.
    let HStmtKind::Assign { value, .. } = &then_body[0].kind else {
        panic!("expected the type-parameter setup: {then_body:?}");
    };
    let (module, function, args) = alias_constructor(value);
    assert_eq!((module, function), ("typing", "TypeVar"));
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::Str(name)) if name == "T"));

    // The class statement publishes the real parameter tuple.
    let HStmtKind::ClassDef {
        name, attributes, ..
    } = &then_body[1].kind
    else {
        panic!("expected the class statement: {then_body:?}");
    };
    assert_eq!(name, "Box");
    let params = attributes
        .iter()
        .find(|(attribute, _)| attribute == "__type_params__")
        .map(|(_, value)| value)
        .expect("__type_params__ attribute");
    let HExprKind::Tuple(items) = &params.kind else {
        panic!("`__type_params__` must be a tuple: {params:?}");
    };
    assert_eq!(items.len(), 1);
    let hidden = hidden_local_name(&hir, &items[0]).expect("the tuple holds the hidden local");
    assert!(
        hidden.starts_with("$typeparam"),
        "unexpected local name: {hidden}"
    );

    // The class-body expression reads the same hidden local, so the parameter
    // is in scope inside the body.
    let attribute = attributes
        .iter()
        .find(|(attribute, _)| attribute == "value")
        .map(|(_, value)| value)
        .expect("value attribute");
    assert_eq!(hidden_local_name(&hir, attribute), Some(hidden));

    // The parameter is scoped to the class statement.
    let leaked = "class Box[T]:\n    value = T\nleaked = T\n";
    let (_, _, diags) = lower_source(leaked, &parse(leaked));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::NAME);

    // It is not a module local either.
    assert!(
        !hir.functions[0]
            .locals
            .iter()
            .any(|local| local.name == "T"),
        "a type parameter must not get an ordinary local slot"
    );
}

#[test]
fn generic_class_type_parameter_forms_lower() {
    let source = "\
class Base:
    pass
class Limited[T: Base](Base[T]):
    pass
class Variadic[*Ts]:
    pass
class Handler[**P]:
    pass
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let body = &hir.functions[0].body;
    assert!(
        matches!(&body[0].kind, HStmtKind::ClassDef { name, .. } if name == "Base"),
        "an ordinary class is unchanged: {body:?}"
    );
    // The bounded parameter carries `bound=` and is in scope in the base list.
    let HStmtKind::If { then_body, .. } = &body[1].kind else {
        panic!("expected the generic class block: {body:?}");
    };
    let HStmtKind::Assign { value, .. } = &then_body[0].kind else {
        panic!("expected the bound setup: {then_body:?}");
    };
    let (_, function, args) = alias_constructor(value);
    assert_eq!(function, "TypeVar");
    let HExprKind::ModuleCall { keywords, .. } = &value.kind else {
        panic!("expected a TypeVar call: {value:?}");
    };
    assert_eq!(keywords.len(), 1);
    assert_eq!(keywords[0].0, "bound");
    // The bound names the base class, resolved in the enclosing scope.
    assert_eq!(hidden_local_name(&hir, &keywords[0].1), Some("Base"));
    assert!(matches!(&args[0].kind, HExprKind::Const(Const::Str(name)) if name == "T"));
    let HStmtKind::ClassDef {
        bases, attributes, ..
    } = &then_body[1].kind
    else {
        panic!("expected the class statement: {then_body:?}");
    };
    assert_eq!(bases.len(), 1);
    let HExprKind::Subscript { index, .. } = &bases[0].kind else {
        panic!("expected `Base[T]` as the base: {:?}", bases[0].kind);
    };
    let hidden = hidden_local_name(&hir, index).expect("the base reads the parameter local");
    assert!(
        hidden.starts_with("$typeparam"),
        "unexpected local: {hidden}"
    );
    assert!(attributes
        .iter()
        .any(|(attribute, _)| attribute == "__type_params__"));

    // Variadic and parametric parameter lists are real runtime values.
    let mut calls = Vec::new();
    type_param_calls(body, &mut calls);
    assert_eq!(
        calls,
        vec![
            ("typing.TypeVar".to_string(), "T".to_string()),
            ("typing.TypeVarTuple".to_string(), "Ts".to_string()),
            ("typing.ParamSpec".to_string(), "P".to_string()),
        ]
    );
}

#[test]
fn generic_class_with_a_decorator_applies_it_after_the_class() {
    let source = "\
def deco(cls):
    return cls
@deco
class Box[T]:
    pass
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let body = &hir.functions[0].body;
    let decorator_temp = body
        .iter()
        .position(|stmt| match &stmt.kind {
            HStmtKind::Assign { target, .. } => hir.functions[0].locals[*target as usize]
                .name
                .starts_with("$decor"),
            _ => false,
        })
        .expect("the decorator expression is evaluated first");
    let class_block = body
        .iter()
        .position(|stmt| match &stmt.kind {
            HStmtKind::If { then_body, .. } => then_body.iter().any(
                |inner| matches!(&inner.kind, HStmtKind::ClassDef { name, .. } if name == "Box"),
            ),
            _ => false,
        })
        .expect("the class statement runs in its parameter block");
    assert!(
        decorator_temp < class_block,
        "decorators evaluate before the class statement: {body:?}"
    );
    let HStmtKind::If { then_body, .. } = &body[class_block].kind else {
        unreachable!()
    };
    let HStmtKind::ClassDef { attributes, .. } = &then_body[1].kind else {
        panic!("expected the class statement: {then_body:?}");
    };
    assert!(attributes
        .iter()
        .any(|(attribute, _)| attribute == "__type_params__"));
    // The binding is the decorator's result, applied to the built class.
    let decorated = assigned(&hir, "Box");
    assert!(
        matches!(&decorated.kind, HExprKind::IndirectCall { .. }),
        "expected the decorator call: {decorated:?}"
    );
}

#[test]
fn nested_generic_classes_publish_their_own_type_params() {
    let source = "\
class Outer[T]:
    x = T
    class Inner[U]:
        pass
";
    let (hir, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");

    let body = &hir.functions[0].body;
    let mut calls = Vec::new();
    type_param_calls(body, &mut calls);
    assert_eq!(
        calls,
        vec![
            ("typing.TypeVar".to_string(), "T".to_string()),
            ("typing.TypeVar".to_string(), "U".to_string()),
        ]
    );
    // Both classes publish their parameters: the dynamic outer body stores
    // its tuple in the namespace, and the nested class carries the attribute.
    assert!(sets_namespace_key(body, "__type_params__"), "{body:?}");
    assert!(sets_namespace_key(body, "Inner"), "{body:?}");
    // The outer body expression (`x = T`) reads the outer parameter local.
    let mut read_outer = false;
    fn scan(stmts: &[HStmt], locals: &[HLocal], read_outer: &mut bool) {
        for stmt in stmts {
            match &stmt.kind {
                HStmtKind::SetSubscript { index, value, .. } => {
                    if matches!(&index.kind, HExprKind::Const(Const::Str(name)) if name == "x") {
                        if let HExprKind::Local(local) = &value.kind {
                            *read_outer |= locals[*local as usize].name.starts_with("$typeparam");
                        }
                    }
                }
                HStmtKind::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    scan(then_body, locals, read_outer);
                    scan(else_body, locals, read_outer);
                }
                HStmtKind::While {
                    body, else_body, ..
                }
                | HStmtKind::ForRange {
                    body, else_body, ..
                }
                | HStmtKind::ForEach {
                    body, else_body, ..
                } => {
                    scan(body, locals, read_outer);
                    scan(else_body, locals, read_outer);
                }
                _ => {}
            }
        }
    }
    scan(body, &hir.functions[0].locals, &mut read_outer);
    assert!(
        read_outer,
        "the dynamic class body must read the parameter: {body:?}"
    );
}

#[test]
fn generic_class_binding_is_mirrored_for_importers() {
    let source = "class Box[T]:\n    pass\n";
    let mut infer = Infer::new();
    let mut lowerer = Lowerer::new(source, "helper.py", &mut infer);
    lowerer.set_mirror_all_globals(true);
    let hir = lowerer.lower_module(&parse(source));
    let diags = std::mem::take(&mut lowerer.diags);
    drop(lowerer);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        binds_module_global(&hir.functions[0].body, "Box"),
        "the parameter block must mirror the class binding: {:?}",
        hir.functions[0].body
    );
}

/// Assert that `function` has no local slot named `name`: a `global`
/// declaration binds only the runtime global table.
fn assert_no_local_slot(function: &HFunction, name: &str) {
    assert!(
        !function.locals.iter().any(|local| local.name == name),
        "`{name}` is declared global, so it must not get a local slot: {:?}",
        function.locals
    );
}

#[test]
fn unpack_targets_declared_global_store_into_the_global_table() {
    let source = "def f(value):\n    global a, b\n    a, b = value\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert_no_local_slot(f, "a");
    let HStmtKind::Unpack { targets, .. } = &f.body[0].kind else {
        panic!("expected an unpack statement, got {:?}", f.body[0].kind);
    };
    assert_eq!(targets.len(), 2);
    let HUnpackTarget::Global { module, name } = &targets[0] else {
        panic!("expected a global target, got {:?}", targets[0]);
    };
    assert_eq!((module.as_str(), name.as_str()), ("__main__", "a"));
    let HUnpackTarget::Global { module, name } = &targets[1] else {
        panic!("expected a global target, got {:?}", targets[1]);
    };
    assert_eq!((module.as_str(), name.as_str()), ("__main__", "b"));

    // The target survives into the IR, where codegen and the interpreter
    // store through the runtime global table.
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let f = program
        .functions
        .iter()
        .find(|function| function.name == "f")
        .expect("function f");
    let targets = f
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            hydro_ir::StmtKind::Unpack { targets, .. } => Some(targets),
            _ => None,
        })
        .expect("emitted unpack");
    assert!(matches!(
        targets[0],
        hydro_ir::UnpackTarget::Global { ref module, ref name }
            if module == "__main__" && name == "a"
    ));
    assert!(matches!(
        targets[1],
        hydro_ir::UnpackTarget::Global { ref module, ref name }
            if module == "__main__" && name == "b"
    ));
}

#[test]
fn starred_and_nested_unpack_targets_declared_global() {
    let source = "def f(items):\n    global head, rest\n    head, *rest = items\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert_no_local_slot(f, "head");
    assert_no_local_slot(f, "rest");
    let targets = f
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Unpack { targets, .. } => Some(targets),
            _ => None,
        })
        .expect("unpack statement");
    let HUnpackTarget::Global { name, .. } = &targets[0] else {
        panic!("expected a global target, got {:?}", targets[0]);
    };
    assert_eq!(name, "head");
    let HUnpackTarget::Star(inner) = &targets[1] else {
        panic!("expected a starred target, got {:?}", targets[1]);
    };
    let HUnpackTarget::Global { name, .. } = &**inner else {
        panic!("expected a global starred target, got {inner:?}");
    };
    assert_eq!(name, "rest");
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());

    // A nested container keeps its own level: every element binds the global.
    let source = "def g(items):\n    global a, b, c\n    (a, b), *c = items\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    for name in ["a", "b", "c"] {
        assert_no_local_slot(g, name);
    }
    let targets = g
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            HStmtKind::Unpack { targets, .. } => Some(targets),
            _ => None,
        })
        .expect("unpack statement");
    let HUnpackTarget::Nested(inner) = &targets[0] else {
        panic!("expected a nested target, got {:?}", targets[0]);
    };
    let names: Vec<&str> = inner
        .iter()
        .map(|target| match target {
            HUnpackTarget::Global { name, .. } => name.as_str(),
            other => panic!("expected a global target, got {other:?}"),
        })
        .collect();
    assert_eq!(names, ["a", "b"]);
    let HUnpackTarget::Star(inner) = &targets[1] else {
        panic!("expected a starred target, got {:?}", targets[1]);
    };
    assert!(matches!(
        &**inner,
        HUnpackTarget::Global { name, .. } if name == "c"
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn simple_annotated_and_chained_assignment_to_declared_globals() {
    let source = "def f():\n\
                  \x20   global x, y, z\n\
                  \x20   x = 1\n\
                  \x20   y: int = 2\n\
                  \x20   x = z = 3\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    for name in ["x", "y", "z"] {
        assert_no_local_slot(f, name);
    }
    let HStmtKind::GlobalSet { name, .. } = &f.body[0].kind else {
        panic!("expected a global store, got {:?}", f.body[0].kind);
    };
    assert_eq!(name, "x");
    let HStmtKind::GlobalSet { name, .. } = &f.body[1].kind else {
        panic!("expected a global store, got {:?}", f.body[1].kind);
    };
    assert_eq!(name, "y");
    // The chained assignment stores the value once, then each target in
    // source order; both targets here are declared global.
    let HStmtKind::If { then_body, .. } = &f.body[2].kind else {
        panic!(
            "expected the chained-assignment wrapper, got {:?}",
            f.body[2].kind
        );
    };
    let names: Vec<&str> = then_body
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            HStmtKind::GlobalSet { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["x", "z"]);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn augmented_assignment_to_a_declared_global_reads_and_stores_the_global() {
    let source = "def f():\n    global counter\n    counter += 1\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert_no_local_slot(f, "counter");
    let HStmtKind::GlobalSet {
        module,
        name,
        value,
    } = &f.body[0].kind
    else {
        panic!("expected a global store, got {:?}", f.body[0].kind);
    };
    assert_eq!((module.as_str(), name.as_str()), ("__main__", "counter"));
    let HExprKind::Bin { lhs, .. } = &value.kind else {
        panic!("expected the augmented value, got {:?}", value.kind);
    };
    assert!(matches!(
        &lhs.kind,
        HExprKind::GlobalGet { module, name }
            if module == "__main__" && name == "counter"
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn for_statement_targets_declared_global_store_each_iteration() {
    // Iterator form: the loop binds a hidden local and the body stores it.
    let source = "def f(xs):\n    global item\n    for item in xs:\n        pass\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert_no_local_slot(f, "item");
    let HStmtKind::ForEach { var, body, .. } = &f.body[0].kind else {
        panic!("expected a for-each loop, got {:?}", f.body[0].kind);
    };
    assert!(
        f.locals[*var as usize].name.starts_with("$for"),
        "the loop needs a hidden iteration slot: {:?}",
        f.locals
    );
    let HStmtKind::GlobalSet { name, value, .. } = &body[0].kind else {
        panic!(
            "expected a global store in the loop body, got {:?}",
            body[0].kind
        );
    };
    assert_eq!(name, "item");
    assert!(matches!(&value.kind, HExprKind::Local(local) if local == var));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());

    // `range()` keeps its dedicated form but still stores the global.
    let source = "def g():\n    global i\n    for i in range(3):\n        pass\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    assert_no_local_slot(g, "i");
    let HStmtKind::ForRange { var, body, .. } = &g.body[0].kind else {
        panic!("expected a range loop, got {:?}", g.body[0].kind);
    };
    assert!(g.locals[*var as usize].name.starts_with("$for"));
    assert!(matches!(
        &body[0].kind,
        HStmtKind::GlobalSet { name, .. } if name == "i"
    ));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());

    // An unpacking loop target carries the global targets into the unpack.
    let source = "def h(pairs):\n    global a, b\n    for a, b in pairs:\n        pass\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let h = function(&hir, "h");
    assert_no_local_slot(h, "a");
    assert_no_local_slot(h, "b");
    let HStmtKind::ForEach { body, .. } = &h.body[0].kind else {
        panic!("expected a for-each loop, got {:?}", h.body[0].kind);
    };
    let HStmtKind::Unpack { targets, .. } = &body[0].kind else {
        panic!(
            "expected an unpack in the loop body, got {:?}",
            body[0].kind
        );
    };
    let names: Vec<&str> = targets
        .iter()
        .map(|target| match target {
            HUnpackTarget::Global { name, .. } => name.as_str(),
            other => panic!("expected a global target, got {other:?}"),
        })
        .collect();
    assert_eq!(names, ["a", "b"]);
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn async_for_target_declared_global_stores_each_awaited_item() {
    let source = "async def f(it):\n    global item\n    async for item in it:\n        pass\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert_no_local_slot(f, "item");
    assert!(
        has_global_set(&f.body),
        "the async-for desugaring must store the global: {:?}",
        f.body
    );
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn comprehension_targets_shadow_a_declared_global() {
    // A comprehension target is local to the comprehension, so `global x`
    // must not turn it into a global store or read.
    let source = "def f():\n    global x\n    return [x for x in range(3)]\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert!(
        !has_global_set(&f.body),
        "a comprehension target is not a global store: {:?}",
        f.body
    );
    let HStmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!("expected a return, got {:?}", f.body[0].kind);
    };
    let HExprKind::ListComp { element, clauses } = &value.kind else {
        panic!("expected a list comprehension, got {:?}", value.kind);
    };
    let HCompTarget::Name(target) = &clauses[0].target else {
        panic!(
            "expected a comprehension name target, got {:?}",
            clauses[0].target
        );
    };
    assert!(
        f.locals[*target as usize].name.starts_with("$comp"),
        "the target binds a synthetic comprehension local: {:?}",
        f.locals
    );
    assert!(
        matches!(&element.kind, HExprKind::Local(local) if local == target),
        "the element must read the comprehension local, got {:?}",
        element.kind
    );
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());

    // The same holds for an unpacking comprehension target whose elements are
    // declared global in the enclosing function.
    let source = "def g(pairs):\n    global k, v\n    return [k for k, v in pairs]\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let g = function(&hir, "g");
    let HStmtKind::Return(Some(value)) = &g.body[0].kind else {
        panic!("expected a return, got {:?}", g.body[0].kind);
    };
    let HExprKind::ListComp { element, clauses } = &value.kind else {
        panic!("expected a list comprehension, got {:?}", value.kind);
    };
    let HCompTarget::Unpack(targets) = &clauses[0].target else {
        panic!(
            "expected a comprehension unpack target, got {:?}",
            clauses[0].target
        );
    };
    for target in targets {
        assert!(
            matches!(target, HUnpackTarget::Name(_)),
            "comprehension targets stay locals, got {target:?}"
        );
    }
    assert!(matches!(&element.kind, HExprKind::Local(_)));
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}

#[test]
fn assignment_expression_to_a_declared_global_stores_and_yields_the_value() {
    let source = "def f():\n    global x\n    y = (x := 1)\n    return y\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert_no_local_slot(f, "x");
    let HStmtKind::Assign { value, .. } = &f.body[0].kind else {
        panic!("expected an assignment, got {:?}", f.body[0].kind);
    };
    let HExprKind::GlobalSet {
        module,
        name,
        value,
    } = &value.kind
    else {
        panic!("expected a global store expression, got {:?}", value.kind);
    };
    assert_eq!((module.as_str(), name.as_str()), ("__main__", "x"));
    assert!(matches!(&value.kind, HExprKind::Const(Const::Int(1))));

    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
    let program = crate::sema::emit(&hir, &mut infer);
    let f = program
        .functions
        .iter()
        .find(|function| function.name == "f")
        .expect("function f");
    let stored = f
        .body
        .iter()
        .find_map(|stmt| match &stmt.kind {
            hydro_ir::StmtKind::Assign { value, .. } => Some(value),
            _ => None,
        })
        .expect("emitted assignment");
    assert!(matches!(
        &stored.kind,
        hydro_ir::ExprKind::GlobalSet { module, name, .. }
            if module == "__main__" && name == "x"
    ));

    // A generator expression frame writes the same global table.
    let source = "def g():\n    global x\n    return (x := i for i in range(3))\n";
    let (_, _, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn global_unpack_target_binds_before_a_later_receiver_runs() {
    // Python stores each target in source order, so a later receiver observes
    // a value stored earlier in the same statement: `obj[g]` must see the new
    // `g`. The target list already encodes that order, and the global target
    // has no local slot in between.
    let source = "def f(obj):\n    global g\n    g, obj[g] = (1, 2)\n";
    let (hir, mut infer, diags) = lower_source(source, &parse(source));
    assert!(diags.is_empty(), "{diags:?}");
    let f = function(&hir, "f");
    assert_no_local_slot(f, "g");
    let HStmtKind::Unpack { targets, .. } = &f.body[0].kind else {
        panic!("expected an unpack statement, got {:?}", f.body[0].kind);
    };
    assert_eq!(targets.len(), 2);
    assert!(matches!(
        &targets[0],
        HUnpackTarget::Global { name, .. } if name == "g"
    ));
    let HUnpackTarget::Subscript { index, .. } = &targets[1] else {
        panic!("expected a subscript target, got {:?}", targets[1]);
    };
    assert!(
        matches!(&index.kind, HExprKind::GlobalGet { name, .. } if name == "g"),
        "the receiver reads the declared global, got {:?}",
        index.kind
    );
    assert!(crate::sema::check(&hir, &mut infer, "test.py").is_ok());
}
