use super::*;
use pyx_ir::ExcKind;

fn empty_prog() -> LHir {
    LHir {
        source_name: "test.py".to_string(),
        functions: Vec::new(),
    }
}

fn checker<'a>(prog: &'a LHir, inf: &'a mut Infer) -> Checker<'a> {
    Checker {
        prog,
        inf,
        filename: "test.py",
        diags: Vec::new(),
        ops: Vec::new(),
        checks: Vec::new(),
        spans: HashMap::new(),
    }
}

#[test]
fn mul_str_and_int_repeats_to_str() {
    assert_eq!(op_result(BinOp::Mul, Type::Str, Type::Int), Some(Type::Str));
    assert_eq!(op_result(BinOp::Mul, Type::Int, Type::Str), Some(Type::Str));
    assert_eq!(
        op_result(BinOp::Mul, Type::Str, Type::Bool),
        Some(Type::Str)
    );
    assert_eq!(
        op_result(BinOp::Mul, Type::Bool, Type::Str),
        Some(Type::Str)
    );
}

#[test]
fn mul_str_and_str_is_rejected() {
    assert_eq!(op_result(BinOp::Mul, Type::Str, Type::Str), None);
}

#[test]
fn mul_numeric_keeps_numeric_result() {
    assert_eq!(op_result(BinOp::Mul, Type::Int, Type::Int), Some(Type::Int));
    assert_eq!(
        op_result(BinOp::Mul, Type::Int, Type::Float),
        Some(Type::Float)
    );
    assert_eq!(
        op_result(BinOp::Mul, Type::Bool, Type::Bool),
        Some(Type::Int)
    );
    assert_eq!(
        op_result(BinOp::Mul, Type::Bool, Type::Int),
        Some(Type::Int)
    );
}

#[test]
fn mul_with_any_stays_any() {
    assert_eq!(op_result(BinOp::Mul, Type::Any, Type::Str), Some(Type::Any));
    assert_eq!(op_result(BinOp::Mul, Type::Str, Type::Any), Some(Type::Any));
}

#[test]
fn mul_one_unknown_against_str_binds_int() {
    let prog = empty_prog();
    let mut inf = Infer::new();
    let mut ck = checker(&prog, &mut inf);
    let var = ck.inf.fresh();
    let out = ck.inf.fresh();
    let op = Op {
        kind: OpKind::Bin(BinOp::Mul),
        a: var,
        b: Ty::Concrete(Type::Str),
        out,
        line: 1,
        col: 1,
        done: false,
    };
    ck.resolve_one_var(op, BinOp::Mul, true, Type::Str);
    assert!(ck.diags.is_empty());
    assert_eq!(ck.inf.resolve(var), Ty::Concrete(Type::Int));
    assert_eq!(ck.inf.resolve(out), Ty::Concrete(Type::Str));
}

#[test]
fn mul_one_unknown_against_int_stays_numeric() {
    let prog = empty_prog();
    let mut inf = Infer::new();
    let mut ck = checker(&prog, &mut inf);
    let var = ck.inf.fresh();
    let out = ck.inf.fresh();
    let op = Op {
        kind: OpKind::Bin(BinOp::Mul),
        a: Ty::Concrete(Type::Int),
        b: var,
        out,
        line: 1,
        col: 1,
        done: false,
    };
    ck.resolve_one_var(op, BinOp::Mul, false, Type::Int);
    assert!(ck.diags.is_empty());
    assert_eq!(ck.inf.resolve(var), Ty::Concrete(Type::Int));
    assert_eq!(ck.inf.resolve(out), Ty::Concrete(Type::Int));
}

#[test]
fn mul_str_by_str_reports_unsupported_operand() {
    let prog = empty_prog();
    let mut inf = Infer::new();
    let mut ck = checker(&prog, &mut inf);
    let out = ck.inf.fresh();
    let op = Op {
        kind: OpKind::Bin(BinOp::Mul),
        a: Ty::Concrete(Type::Str),
        b: Ty::Concrete(Type::Str),
        out,
        line: 3,
        col: 7,
        done: false,
    };
    ck.ops.push(op);
    assert!(ck.resolve_binary(0, op, BinOp::Mul));
    assert_eq!(ck.diags.len(), 1);
    assert_eq!(
        ck.diags[0].message,
        "unsupported operand type(s) for *: 'str' and 'str'"
    );
    assert_eq!((ck.diags[0].line, ck.diags[0].col), (3, 7));
}

fn check_errors(source: &str) -> Vec<String> {
    match crate::check_source(source, "test.py") {
        Ok(_) => Vec::new(),
        Err(diags) => diags.into_iter().map(|d| d.message).collect(),
    }
}

fn check_diags(source: &str) -> Vec<Diagnostic> {
    crate::check_source(source, "test.py").expect_err("expected diagnostics")
}

#[test]
fn unknown_method_suggests_the_closest_method() {
    let diags = check_diags("print('x'.uppe())\n");
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code, codes::ATTRIBUTE);
    assert_eq!(diags[0].message, "type 'str' has no method 'uppe'");
    assert_eq!(diags[0].help.as_deref(), Some("did you mean `.upper()`?"));
}

#[test]
fn unknown_method_offers_a_fallback_suggestion() {
    let diags = check_diags("print('x'.nope())\n");
    assert_eq!(diags[0].code, codes::ATTRIBUTE);
    assert!(
        matches!(
            diags[0].help.as_deref(),
            Some(help) if help.starts_with("did you mean `.upper()`?")
        ),
        "{:?}",
        diags[0].help
    );
}

#[test]
fn type_errors_carry_the_conversion_help() {
    let diags = check_diags("x = 1 + 'a'\n");
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code, codes::TYPE);
    assert_eq!(
        diags[0].help.as_deref(),
        Some("check the operand types or add an explicit conversion")
    );
}

#[test]
fn slice_and_subscript_errors_use_the_index_code() {
    for source in [
        "d = {'a': 1}\nprint(d[1:2])\n",
        "s = {1}\nprint(s[0])\n",
        "s = {1}\ns[0] = 2\n",
    ] {
        let diags = check_diags(source);
        assert_eq!(diags[0].code, codes::INDEX, "{source}");
    }
}

#[test]
fn slice_assignment_emits_set_slice_with_none_bounds() {
    let source = "xs = [1, 2, 3]\nxs[:] = [4]\nxs[1:] = xs\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let body = &program.functions[0].body;

    let pyx_ir::StmtKind::SetSlice {
        target,
        start,
        stop,
        step,
        value,
    } = &body[1].kind
    else {
        panic!("expected a slice assignment, got {:?}", body[1].kind);
    };
    assert!(matches!(&target.kind, ExprKind::Local(_)));
    assert!(matches!(&start.kind, ExprKind::Const(Const::None)));
    assert!(matches!(&stop.kind, ExprKind::Const(Const::None)));
    assert!(matches!(&step.kind, ExprKind::Const(Const::None)));
    assert!(matches!(&value.kind, ExprKind::List(_)));

    // A partial slice keeps the explicit bound and fills the rest with `None`.
    let pyx_ir::StmtKind::SetSlice {
        start, stop, step, ..
    } = &body[2].kind
    else {
        panic!("expected a slice assignment, got {:?}", body[2].kind);
    };
    assert!(matches!(&start.kind, ExprKind::Const(Const::Int(1))));
    assert!(matches!(&stop.kind, ExprKind::Const(Const::None)));
    assert!(matches!(&step.kind, ExprKind::Const(Const::None)));
}

#[test]
fn slice_assignment_requires_a_list_target() {
    for (source, expected) in [
        (
            "s = 'abc'\ns[0:1] = 'x'\n",
            "'str' object does not support item assignment",
        ),
        (
            "t = (1, 2)\nt[0:1] = [3]\n",
            "'tuple' object does not support item assignment",
        ),
        ("d = {'a': 1}\nd[0:1] = [2]\n", "unhashable type: 'slice'"),
    ] {
        let errors = check_errors(source);
        assert_eq!(errors, vec![expected.to_string()], "{source}");
    }

    // Lists and dynamically typed receivers are accepted; the right-hand side
    // is only checked dynamically.
    assert!(check_errors("xs = [1, 2]\nxs[0:1] = [3]\n").is_empty());
    assert!(check_errors("def f(xs, v):\n    xs[0:1] = v\n").is_empty());
    assert!(check_errors("xs = [1, 2]\nxs[:] = 5\n").is_empty());
}

#[test]
fn augmented_subscript_assignment_typechecks_and_emits_stores() {
    let source = "d = {'a': 1}\nd['a'] += 2\nxs = [1, 2]\nxs[1] *= 3\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let body = &program.functions[0].body;
    let mut stores = 0;
    for stmt in body {
        let pyx_ir::StmtKind::If { then_body, .. } = &stmt.kind else {
            continue;
        };
        for inner in then_body {
            if let pyx_ir::StmtKind::SetSubscript { value, .. } = &inner.kind {
                stores += 1;
                assert!(
                    matches!(&value.kind, ExprKind::Bin { .. }),
                    "{:?}",
                    value.kind
                );
            }
        }
    }
    assert_eq!(stores, 2, "{body:?}");
}

#[test]
fn call_arity_uses_the_call_code() {
    let diags = check_diags("def f(x: int) -> int:\n    return x\nprint(f())\n");
    assert_eq!(diags[0].code, codes::CALL);
    assert!(
        diags[0].message.contains("missing required argument 'x'"),
        "{}",
        diags[0].message
    );
}

#[test]
fn unknown_method_on_concrete_receiver_is_reported() {
    let errors = check_errors("print('x'.nope())\n");
    assert_eq!(errors, vec!["type 'str' has no method 'nope'"]);
}

#[test]
fn numeric_receiver_has_no_methods() {
    let errors = check_errors("x = 1\nprint(x.upper())\n");
    assert_eq!(errors, vec!["type 'int' has no method 'upper'"]);
}

#[test]
fn method_arity_mismatch_is_reported() {
    let errors = check_errors("print('x'.upper(1))\n");
    assert_eq!(
        errors,
        vec!["str.upper() takes 0 positional argument(s) (1 given)"]
    );
}

#[test]
fn method_arity_range_is_reported() {
    let errors = check_errors("print('a,b'.split(',', 1, 2))\n");
    assert_eq!(
        errors,
        vec!["str.split() takes 0 to 2 positional argument(s) (3 given)"]
    );
}

#[test]
fn method_argument_type_mismatch_is_reported() {
    let errors = check_errors("print('x'.replace(1, 'y'))\n");
    assert_eq!(
        errors,
        vec!["str.replace() argument 1 has wrong type: expected str, got int"]
    );
}

#[test]
fn any_receiver_accepts_any_method() {
    let source = "def f(x: Any):\n    return x.nope(1, 2)\n";
    assert!(check_errors(source).is_empty());
}

#[test]
fn untyped_receiver_accepts_any_method() {
    let source = "def f(x):\n    return x.append()\nprint(f([1] or 0))\n";
    assert!(check_errors(source).is_empty());
}

#[test]
fn method_results_drive_return_inference() {
    let source = "def f(s: str) -> str:\n    return s.upper()\ndef g(s: str) -> list:\n    return s.split(',')\ndef h(xs: list) -> list:\n    return xs.copy()\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let ret = |name: &str| {
        program
            .functions
            .iter()
            .find(|f| f.name == name)
            .expect("function")
            .ret_ty
    };
    assert_eq!(ret("f"), Type::Str);
    assert_eq!(ret("g"), Type::List);
    assert_eq!(ret("h"), Type::List);
}

#[test]
fn method_calls_emit_method_call_nodes() {
    let program = crate::check_source("x = 'ab'.upper()\n", "test.py").expect("valid program");
    let pyx_ir::StmtKind::Assign { value, .. } = &program.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    assert!(
        matches!(&value.kind, ExprKind::MethodCall { method, .. } if method == "upper"),
        "{:?}",
        value.kind
    );
    assert_eq!(value.ty, Type::Str);
}

fn assigned_type(program: &Program, index: usize) -> Type {
    let stmt = &program.functions[0].body[index];
    match &stmt.kind {
        pyx_ir::StmtKind::Assign { value, .. } => value.ty,
        other => panic!("expected an assignment, got {other:?}"),
    }
}

#[test]
fn module_calls_use_the_spec_return_type() {
    let source = "import math\n\
                      a = math.sqrt(9)\n\
                      b = math.floor(1.5)\n\
                      c = math.pi\n\
                      import sys\n\
                      d = sys.maxsize\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Float);
    assert_eq!(assigned_type(&program, 1), Type::Int);
    assert_eq!(assigned_type(&program, 2), Type::Float);
    assert_eq!(assigned_type(&program, 3), Type::Int);
}

#[test]
fn imported_functions_are_function_values_and_typed_calls() {
    let source = "from math import sqrt\nf = sqrt\ndef g(x):\n    return sqrt(x)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Function);
    let g = program
        .functions
        .iter()
        .find(|f| f.name == "g")
        .expect("function g");
    assert_eq!(g.ret_ty, Type::Float);
}

#[test]
fn module_call_arity_is_checked_from_the_spec() {
    let diags = check_diags("import math\nmath.sqrt()\n");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::CALL);
    assert_eq!(
        diags[0].message,
        "math.sqrt() takes 1 argument(s) (0 given)"
    );

    let diags = check_diags("from functools import reduce\nreduce(lambda a, b: a)\n");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::CALL);
    assert_eq!(
        diags[0].message,
        "functools.reduce() takes 2 to 3 argument(s) (1 given)"
    );
}

#[test]
fn module_attributes_emit_typed_nodes() {
    let source = "import math\nx = math.sqrt(9)\ny = math.pi\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let pyx_ir::StmtKind::Assign { value, .. } = &program.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    assert!(
        matches!(&value.kind, ExprKind::ModuleCall { module, function, .. }
                if module == "math" && function == "sqrt"),
        "{:?}",
        value.kind
    );
    assert_eq!(value.ty, Type::Float);
    let pyx_ir::StmtKind::Assign { value, .. } = &program.functions[0].body[1].kind else {
        panic!("expected an assignment");
    };
    assert!(
        matches!(&value.kind, ExprKind::ModuleAttr { module, name }
                if module == "math" && name == "pi"),
        "{:?}",
        value.kind
    );
    assert_eq!(value.ty, Type::Float);
}

#[test]
fn calling_a_module_constant_reports_not_callable() {
    let diags = check_diags("import sys\nsys.maxsize()\n");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::CALL);
    assert_eq!(diags[0].message, "'int' object is not callable");
}

#[test]
fn tuple_index_slice_and_len_types() {
    let source = "t = (1, 2, 3)\na = t[0]\nb = t[1:]\nc = t[::-1]\nn = len(t)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 1), Type::Any);
    assert_eq!(assigned_type(&program, 2), Type::Tuple);
    assert_eq!(assigned_type(&program, 3), Type::Tuple);
    assert_eq!(assigned_type(&program, 4), Type::Int);
}

#[test]
fn tuple_concat_and_repeat_types() {
    let source = "t = (1, 2)\na = t + (3,)\nb = t * 3\nc = 2 * t\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 1), Type::Tuple);
    assert_eq!(assigned_type(&program, 2), Type::Tuple);
    assert_eq!(assigned_type(&program, 3), Type::Tuple);
}

#[test]
fn tuple_ordering_typechecks_as_bool() {
    let program = crate::check_source("x = (1, 2) < (3, 4)\n", "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Bool);
}

#[test]
fn tuple_methods_return_int() {
    let source = "t = (1, 2, 1)\na = t.count(1)\nb = t.index(2)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 1), Type::Int);
    assert_eq!(assigned_type(&program, 2), Type::Int);
}

#[test]
fn dict_items_returns_list() {
    let source = "d = {'a': 1}\nxs = d.items()\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 1), Type::List);
}

#[test]
fn plain_tuple_keys_are_hashable() {
    assert!(check_errors("d = {(1, 2): 'x'}\nprint(d[(1, 2)])\n").is_empty());
}

#[test]
fn tuple_key_with_unhashable_elements_is_rejected() {
    for source in ["d = {(1, [2]): 3}\n", "d = {((1, [2]), 3): 4}\n"] {
        assert_eq!(check_errors(source), vec!["unhashable type: 'list'"]);
    }
    assert_eq!(
        check_errors("d = {(1, {2: 3}): 4}\n"),
        vec!["unhashable type: 'dict'"]
    );
}

#[test]
fn tuple_item_assignment_is_rejected() {
    let errors = check_errors("t = (1, 2)\nt[0] = 3\n");
    assert_eq!(
        errors,
        vec!["'tuple' object does not support item assignment"]
    );
}

#[test]
fn unpack_requires_a_sequence() {
    let errors = check_errors("x = 1\na, b = x\n");
    assert_eq!(errors, vec!["cannot unpack int"]);
    assert!(check_errors("t = (1, 2)\na, b = t\n").is_empty());
}

#[test]
fn unpack_accepts_iterable_sources() {
    for source in [
        "a, b = 'xy'\n",
        "a, b = {1, 2}\n",
        "a, b = {'x': 1, 'y': 2}\n",
        "a, b = range(2)\n",
        "a, b = iter((1, 2))\n",
    ] {
        let errors = check_errors(source);
        assert!(errors.is_empty(), "{source}: {errors:?}");
    }
}

#[test]
fn duplicate_underscore_unpack_target_is_allowed() {
    assert!(check_errors("t = (1, 2)\n_, _ = t\n").is_empty());
}

#[test]
fn unpack_targets_emit_attributes_subscripts_and_nesting() {
    let source =
        "class C:\n    pass\nobj = C()\nitems = [0]\nt = (1, (2, 3))\nobj.x, (a, items[0]) = t\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let pyx_ir::StmtKind::Unpack { targets, .. } = &program.functions[0].body[4].kind else {
        panic!("expected an unpack statement");
    };
    match &targets[0] {
        pyx_ir::UnpackTarget::Attr { name, .. } => assert_eq!(name, "x"),
        other => panic!("expected an attribute target, got {other:?}"),
    }
    let pyx_ir::UnpackTarget::Nested(inner) = &targets[1] else {
        panic!("expected a nested target, got {:?}", targets[1]);
    };
    assert!(matches!(inner[0], pyx_ir::UnpackTarget::Name(_)));
    assert!(matches!(inner[1], pyx_ir::UnpackTarget::Subscript { .. }));
}

#[test]
fn for_unpack_target_emits_unpack_statement() {
    let source = "xs = [(1, 2)]\nfor a, b in xs:\n    print(a, b)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let pyx_ir::StmtKind::ForEach { var, body, .. } = &program.functions[0].body[1].kind else {
        panic!("expected a foreach loop");
    };
    let pyx_ir::StmtKind::Unpack { targets, value } = &body[0].kind else {
        panic!("expected a prepended unpack, got {:?}", body[0].kind);
    };
    assert_eq!(targets.len(), 2);
    assert!(matches!(&value.kind, ExprKind::Local(id) if id == var));
    let hidden = program.functions[0]
        .locals
        .iter()
        .find(|local| local.name.starts_with("$unpack"))
        .expect("hidden unpack local");
    assert_eq!(hidden.ty, Type::Any);
}

#[test]
fn set_literal_has_set_type_and_emits_elements() {
    let program = crate::check_source("s = {1, 2, 2}\n", "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Set);
    let pyx_ir::StmtKind::Assign { value, .. } = &program.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    let ExprKind::Set(elems) = &value.kind else {
        panic!("expected a set literal, got {:?}", value.kind);
    };
    assert_eq!(elems.len(), 3);
    assert!(elems.iter().all(|elem| elem.ty == Type::Int));
}

#[test]
fn set_literal_rejects_unhashable_elements() {
    for (source, expected) in [
        ("s = {[1], 2}\n", "unhashable type: 'list'"),
        ("s = {{1}, 2}\n", "unhashable type: 'set'"),
        ("s = {{'a': 1}, 2}\n", "unhashable type: 'dict'"),
        ("s = {((1, [2]),), 2}\n", "unhashable type: 'list'"),
    ] {
        assert_eq!(check_errors(source), vec![expected], "{source}");
    }
    assert!(check_errors("s = {(1, 2), 3}\n").is_empty());
}

#[test]
fn set_builtin_accepts_iterables() {
    let source = "a = set()\nb = set([1, 2])\nc = set((1, 2))\nd = set({1})\ne = set('ab')\nf = set({'a': 1})\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in 0..6 {
        assert_eq!(assigned_type(&program, index), Type::Set);
    }
    assert_eq!(
        check_errors("x = set(1)\n"),
        vec!["set() argument must be iterable"]
    );
    assert_eq!(
        check_errors("x = set([1], [2])\n"),
        vec!["set() takes 0 to 1 positional argument(s) (2 given)"]
    );
}

#[test]
fn membership_results_are_bool() {
    let source = "a = 1 in [1, 2]\nb = 'a' in 'abc'\nc = 1 in {1}\nd = 'a' in {'a': 1}\ne = 1 in (1, 2)\nf = 1 not in [2]\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in 0..6 {
        assert_eq!(assigned_type(&program, index), Type::Bool);
    }
}

#[test]
fn membership_diagnostics_follow_operand_types() {
    assert_eq!(
        check_errors("print(1 in 'abc')\n"),
        vec!["'in <str>' requires string left operand, not int"]
    );
    assert_eq!(
        check_errors("print(1 in 2)\n"),
        vec!["'int' object is not iterable"]
    );
    assert_eq!(
        check_errors("print([1] in {1})\n"),
        vec!["unhashable type: 'list'"]
    );
    assert_eq!(
        check_errors("print((1, [2]) in {1})\n"),
        vec!["unhashable type: 'list'"]
    );
}

#[test]
fn chained_comparisons_type_as_bool() {
    let source = "x = 1\n\
                  a = 0 < x < 3\n\
                  b = 0 <= x <= 3\n\
                  c = 0 < x < 2 < 3\n\
                  d = x is not None\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in 1..=4 {
        assert_eq!(assigned_type(&program, index), Type::Bool);
    }
}

#[test]
fn assignment_expressions_are_visible_later_in_the_scope() {
    // The comprehension binds `y` in the enclosing (module) scope, so the
    // later read resolves and the walrus result feeds the comparison.
    let source = "if (n := 3) > 0:\n    m = n\n\
                  values = [y := x for x in [1, 2]]\n\
                  after = y\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let locals = &program.functions[0].locals;
    assert!(locals.iter().any(|local| local.name == "n"), "{locals:?}");
    assert!(locals.iter().any(|local| local.name == "y"), "{locals:?}");
    let after = &program.functions[0].body[2];
    let pyx_ir::StmtKind::Assign { value, .. } = &after.kind else {
        panic!("expected the final assignment");
    };
    assert!(
        matches!(&value.kind, pyx_ir::ExprKind::Local(_)),
        "{:?}",
        value.kind
    );
}

#[test]
fn assignment_expression_to_a_comprehension_target_is_rejected() {
    assert_eq!(
        check_errors("values = [(x := 1) for x in [1, 2]]\n"),
        vec!["assignment expression cannot rebind comprehension iteration variable 'x'"]
    );
}

#[test]
fn ord_and_chr_have_concrete_result_types() {
    let source = "a = ord('A')\nb = chr(65)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Int);
    assert_eq!(assigned_type(&program, 1), Type::Str);
}

#[test]
fn ord_and_chr_reject_mismatched_argument_types() {
    assert_eq!(
        check_errors("print(ord(1))\n"),
        vec!["ord() argument must be str, not int"]
    );
    assert_eq!(
        check_errors("print(chr('x'))\n"),
        vec!["chr() argument must be an int, not str"]
    );
    assert_eq!(
        check_errors("print(ord())\n"),
        vec!["ord() takes exactly one argument (0 given)"]
    );
}

#[test]
fn set_indexing_and_assignment_are_rejected() {
    assert_eq!(
        check_errors("s = {1}\nprint(s[0])\n"),
        vec!["'set' object is not subscriptable"]
    );
    assert_eq!(
        check_errors("s = {1}\nprint(s[0:1])\n"),
        vec!["'set' object is not subscriptable"]
    );
    assert_eq!(
        check_errors("s = {1}\ns[0] = 2\n"),
        vec!["'set' object does not support item assignment"]
    );
}

#[test]
fn set_methods_return_the_documented_types() {
    let source = "s = {1}\na = s.add(2)\nb = s.remove(1)\nc = s.discard(9)\nd = s.clear()\ne = s.copy()\nf = s.union([2, 3])\ng = s.union((2,))\nh = s.union({2})\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in 1..5 {
        assert_eq!(assigned_type(&program, index), Type::None);
    }
    for index in 5..9 {
        assert_eq!(assigned_type(&program, index), Type::Set);
    }
}

#[test]
fn set_method_arguments_are_checked() {
    assert_eq!(
        check_errors("s = {1}\ns.add([1])\n"),
        vec!["unhashable type: 'list'"]
    );
    assert_eq!(
        check_errors("s = {1}\ns.discard({1})\n"),
        vec!["unhashable type: 'set'"]
    );
    assert_eq!(
        check_errors("s = {1}\ns.union(1)\n"),
        vec!["set.union() argument 1 has wrong type: expected set, list or tuple, got int"]
    );
    assert_eq!(
        check_errors("s = {1}\ns.add()\n"),
        vec!["set.add() takes 1 positional argument(s) (0 given)"]
    );
}

#[test]
fn set_annotation_loop_and_len_types() {
    let source = "def f(s: set) -> set:\n    return s\ns = {1, 2}\nn = len(s)\ntotal = 0\nfor v in s:\n    total += 1\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let function = program
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    assert_eq!(function.params[0].ty, Type::Set);
    assert_eq!(function.ret_ty, Type::Set);
    assert_eq!(assigned_type(&program, 1), Type::Int);
    assert!(matches!(
        &program.functions[0].body[3].kind,
        pyx_ir::StmtKind::ForEach { .. }
    ));
}

#[test]
fn range_value_types_and_iteration() {
    let source = "r = range(5)\nn = len(r)\ns = str(r)\nx = list(r)\nt = sum(r)\nfv = 0\nfor v in r:\n    fv += 1\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Range);
    assert_eq!(assigned_type(&program, 1), Type::Int);
    assert_eq!(assigned_type(&program, 2), Type::Str);
    assert_eq!(assigned_type(&program, 3), Type::List);
    assert_eq!(assigned_type(&program, 4), Type::Int);
    assert!(matches!(
        &program.functions[0].body[6].kind,
        pyx_ir::StmtKind::ForEach { .. }
    ));
}

#[test]
fn for_range_stays_a_range_loop() {
    let source = "for i in range(3):\n    print(i)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert!(matches!(
        &program.functions[0].body[0].kind,
        pyx_ir::StmtKind::ForRange { .. }
    ));
    let var = match &program.functions[0].body[0].kind {
        pyx_ir::StmtKind::ForRange { var, .. } => *var,
        other => panic!("expected ForRange, got {other:?}"),
    };
    assert_eq!(program.functions[0].locals[var as usize].ty, Type::Int);
}

#[test]
fn loop_else_bodies_reach_the_ir() {
    let source = "def f(xs):\n\
                  \x20   for x in xs:\n        print(x)\n    else:\n        print('each')\n\
                  \x20   for i in range(3):\n        print(i)\n    else:\n        print('range')\n\
                  \x20   while False:\n        print('never')\n    else:\n        print('while')\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let function = program
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    let mut loops = 0;
    for stmt in &function.body {
        match &stmt.kind {
            pyx_ir::StmtKind::ForEach { else_body, .. } => {
                assert_eq!(else_body.len(), 1, "for-each else must reach the IR");
                loops += 1;
            }
            pyx_ir::StmtKind::ForRange { else_body, .. } => {
                assert_eq!(else_body.len(), 1, "for-range else must reach the IR");
                loops += 1;
            }
            pyx_ir::StmtKind::While {
                body, else_body, ..
            } => {
                assert!(body.is_empty(), "the `while False:` body is dead");
                assert_eq!(else_body.len(), 1, "while else must reach the IR");
                loops += 1;
            }
            _ => {}
        }
    }
    assert_eq!(loops, 3, "every loop must keep its else clause");
}

#[test]
fn iterable_builtin_result_types() {
    let source = "a = list()\nb = list((1, 2))\nc = list('ab')\nd = list({'x': 1})\ne = dict()\nf = dict([(1, 'a')])\ng = dict(zip([1], ['a']))\nh = enumerate(['a'])\ni = enumerate(['a'], 5)\nj = zip([1], [2])\nk = zip()\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let expected = [
        Type::List,
        Type::List,
        Type::List,
        Type::List,
        Type::Dict,
        Type::Dict,
        Type::Dict,
        Type::List,
        Type::List,
        Type::List,
        Type::List,
    ];
    for (index, ty) in expected.iter().enumerate() {
        assert_eq!(assigned_type(&program, index), *ty, "statement {index}");
    }
}

#[test]
fn sum_results_follow_the_input_type() {
    let source = "r = range(4)\na = sum(r)\nb = sum(range(4))\nc = sum([1, 2])\nd = sum((1, 2), 10)\ne = sum({1, 2})\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 1), Type::Int);
    assert_eq!(assigned_type(&program, 2), Type::Int);
    assert_eq!(assigned_type(&program, 3), Type::Any);
    assert_eq!(assigned_type(&program, 4), Type::Any);
    assert_eq!(assigned_type(&program, 5), Type::Any);
}

#[test]
fn list_dict_and_zip_reject_non_iterables() {
    assert_eq!(
        check_errors("x = list(1)\n"),
        vec!["list() argument must be iterable"]
    );
    assert_eq!(
        check_errors("x = dict(5)\n"),
        vec!["dict() argument must be iterable"]
    );
    assert_eq!(
        check_errors("x = zip(1)\n"),
        vec!["zip() argument must be iterable"]
    );
    assert_eq!(
        check_errors("x = enumerate(1)\n"),
        vec!["enumerate() argument must be iterable"]
    );
    assert_eq!(
        check_errors("x = sum('ab')\n"),
        vec!["sum() argument must be iterable"]
    );
    assert_eq!(
        check_errors("x = sum(1)\n"),
        vec!["sum() argument must be iterable"]
    );
}

#[test]
fn builtin_arity_diagnostics() {
    assert_eq!(
        check_errors("x = list([1], [2])\n"),
        vec!["list() takes 0 to 1 positional argument(s) (2 given)"]
    );
    assert_eq!(
        check_errors("x = dict(1, 2)\n"),
        vec!["dict() takes 0 to 1 positional argument(s) (2 given)"]
    );
    assert_eq!(
        check_errors("x = range()\n"),
        vec!["range() takes 1 to 3 positional arguments (0 given)"]
    );
    assert_eq!(
        check_errors("x = range(1, 2, 3, 4)\n"),
        vec!["range() takes 1 to 3 positional arguments (4 given)"]
    );
    assert_eq!(
        check_errors("x = sum()\n"),
        vec!["sum() takes 1 or 2 positional arguments (0 given)"]
    );
    assert_eq!(
        check_errors("x = sum([1], 2, 3)\n"),
        vec!["sum() takes 1 or 2 positional arguments (3 given)"]
    );
    assert_eq!(
        check_errors("x = enumerate()\n"),
        vec!["enumerate() takes 1 or 2 positional arguments (0 given)"]
    );
    assert_eq!(
        check_errors("x = enumerate([1], 2, 3)\n"),
        vec!["enumerate() takes 1 or 2 positional arguments (3 given)"]
    );
}

#[test]
fn iter_and_next_result_types_and_arity() {
    let source = "a = iter([1, 2])\nb = next(a)\nc = next(a, -1)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Any);
    assert_eq!(assigned_type(&program, 1), Type::Any);
    assert_eq!(assigned_type(&program, 2), Type::Any);

    // A statically known non-iterable is rejected by `iter`.
    assert_eq!(
        check_errors("x = iter(1)\n"),
        vec!["iter() argument must be iterable"]
    );
    assert_eq!(
        check_errors("x = iter()\n"),
        vec!["iter() takes exactly one argument (0 given)"]
    );
    assert_eq!(
        check_errors("x = iter([1], [2])\n"),
        vec!["iter() takes exactly one argument (2 given)"]
    );
    assert_eq!(
        check_errors("x = next()\n"),
        vec!["next() takes 1 or 2 positional arguments (0 given)"]
    );
    assert_eq!(
        check_errors("x = next([1], 0, 1)\n"),
        vec!["next() takes 1 or 2 positional arguments (3 given)"]
    );
    // `next` accepts any object statically; non-iterators are a runtime
    // TypeError (`next([])`).
    assert!(check_errors("x = next([])\n").is_empty());
}

#[test]
fn range_arguments_must_be_ints() {
    assert_eq!(
        check_errors("x = range('x')\n"),
        vec!["range() argument 'stop' has type str, expected int"]
    );
    assert_eq!(
        check_errors("x = range(0, 'x')\n"),
        vec!["range() argument 'stop' has type str, expected int"]
    );
    assert_eq!(
        check_errors("x = range(0, 1, 1.5)\n"),
        vec!["range() argument 'step' has type float, expected int"]
    );
    assert!(check_errors("x = range(True)\n").is_empty());
}

#[test]
fn sum_start_and_enumerate_start_are_checked() {
    assert_eq!(
        check_errors("x = sum([1, 2], 'x')\n"),
        vec!["sum() start argument must be a number, not str"]
    );
    assert_eq!(
        check_errors("x = enumerate([1], 'x')\n"),
        vec!["enumerate() start argument must be int, not str"]
    );
}

#[test]
fn range_membership_accepts_any_left() {
    let source = "a = 5 in range(10)\nb = 10 in range(10)\nc = True in range(3)\nd = 1 in range(0, 10, 2)\ne = 1.0 in range(3)\nf = 'a' in range(3)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in 0..6 {
        assert_eq!(assigned_type(&program, index), Type::Bool);
    }
}

#[test]
fn range_is_not_subscriptable() {
    assert_eq!(
        check_errors("r = range(3)\nx = r[0]\n"),
        vec!["'range' object is not subscriptable"]
    );
    assert_eq!(
        check_errors("r = range(3)\nx = r[1:]\n"),
        vec!["'range' object is not subscriptable"]
    );
}

#[test]
fn range_annotation_is_accepted() {
    let source = "def f(r: range) -> range:\n    return r\nr = range(3)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let function = program
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    assert_eq!(function.params[0].ty, Type::Range);
    assert_eq!(function.ret_ty, Type::Range);
}

#[test]
fn range_operators_outside_membership_are_rejected() {
    assert_eq!(
        check_errors("r = range(3)\nx = r + 1\n"),
        vec!["unsupported operand type(s) for +: 'range' and 'int'"]
    );
    assert_eq!(
        check_errors("r = range(3)\nx = -r\n"),
        vec!["bad operand type for unary -: 'range'"]
    );
}

#[test]
fn function_values_are_typed_as_function() {
    let source = "def add(a, b):\n    return a + b\nf = add\nxs = [add]\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Function);
    assert_eq!(assigned_type(&program, 1), Type::List);
}

#[test]
fn lambdas_are_function_values_usable_in_lists() {
    let source =
            "def add(a, b):\n    return a + b\nxs = [add, lambda x: x + 1]\nprint(xs[0](1, 2), xs[1](5))\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::List);
}

#[test]
fn indirect_calls_through_locals_subscripts_and_call_results() {
    for source in [
        "def add(a, b):\n    return a + b\nf = add\nprint(f(1, 2))\n",
        "def add(a, b):\n    return a + b\nxs = [add]\nprint(xs[0](1, 2))\n",
        "def make():\n    return lambda x: x + 1\nprint(make()(2))\n",
        "def apply(fn, x):\n    return fn(x)\nprint(apply(lambda v: v + 1, 2))\n",
    ] {
        crate::check_source(source, "test.py")
            .unwrap_or_else(|diags| panic!("{source}: {diags:?}"));
    }
}

#[test]
fn map_and_filter_yield_lists() {
    let source = "def inc(x):\n    return x + 1\na = map(inc, [1, 2])\nb = filter(None, [0, 1])\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::List);
    assert_eq!(assigned_type(&program, 1), Type::List);
}

#[test]
fn map_and_filter_reject_non_callables_and_non_iterables() {
    for (source, expected) in [
        (
            "print(list(map(5, [1])))\n",
            "map() argument 1 must be callable",
        ),
        (
            "print(list(filter(5, [1])))\n",
            "filter() argument 1 must be callable or None",
        ),
        (
            "print(list(map(lambda x: x, 5)))\n",
            "map() argument 2 must be iterable",
        ),
        (
            "print(list(filter(lambda x: x, 5)))\n",
            "filter() argument 2 must be iterable",
        ),
    ] {
        assert_eq!(check_errors(source), vec![expected.to_string()], "{source}");
    }
}

#[test]
fn calling_a_non_callable_reports_not_callable() {
    assert_eq!(
        check_errors("x = 5\nprint(x(1))\n"),
        vec!["'int' object is not callable"]
    );
    assert_eq!(
        check_errors("x = 'a'\nprint(x(1))\n"),
        vec!["'str' object is not callable"]
    );
}

#[test]
fn functions_do_not_support_arithmetic_or_subscripts() {
    assert_eq!(
        check_errors("def f():\n    pass\nx = f + 1\n"),
        vec!["unsupported operand type(s) for +: 'function' and 'int'"]
    );
    assert_eq!(
        check_errors("def f():\n    pass\nx = f[0]\n"),
        vec!["'function' object is not subscriptable"]
    );
}

#[test]
fn bytes_operators_and_indexing_are_typed() {
    let source = "a = b'ab'\n\
                      b = a[0]\n\
                      c = a[1:2]\n\
                      d = a + b'cd'\n\
                      e = a * 3\n\
                      f = 3 * a\n\
                      g = a == b'ab'\n\
                      h = b'c' in a\n\
                      i = 65 in a\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Bytes);
    assert_eq!(assigned_type(&program, 1), Type::Int);
    assert_eq!(assigned_type(&program, 2), Type::Bytes);
    assert_eq!(assigned_type(&program, 3), Type::Bytes);
    assert_eq!(assigned_type(&program, 4), Type::Bytes);
    assert_eq!(assigned_type(&program, 5), Type::Bytes);
    assert_eq!(assigned_type(&program, 6), Type::Bool);
    assert_eq!(assigned_type(&program, 7), Type::Bool);
    assert_eq!(assigned_type(&program, 8), Type::Bool);
}

#[test]
fn bytes_builtin_and_methods_have_concrete_result_types() {
    let source = "a = bytes()\n\
                      b = bytes(3)\n\
                      c = bytes([65, 66])\n\
                      d = bytes('x', 'utf-8')\n\
                      e = b'ab'.decode()\n\
                      f = 'ab'.encode()\n\
                      g = b'a,b'.split(b',')\n\
                      h = b'ab'.hex()\n\
                      i = b'a'.replace(b'a', b'b')\n\
                      j = b' a '.strip()\n\
                      k = b'AB'.lower()\n\
                      l = b'a'.count(b'a')\n\
                      m = b'a'.startswith(b'a')\n\
                      n = b'a,b'.join([b'a'])\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let types = [
        Type::Bytes,
        Type::Bytes,
        Type::Bytes,
        Type::Bytes,
        Type::Str,
        Type::Bytes,
        Type::List,
        Type::Str,
        Type::Bytes,
        Type::Bytes,
        Type::Bytes,
        Type::Int,
        Type::Bool,
        Type::Bytes,
    ];
    for (index, expected) in types.iter().enumerate() {
        assert_eq!(assigned_type(&program, index), *expected, "index {index}");
    }
}

#[test]
fn iterating_bytes_yields_ints() {
    let source = "for byte in b'ab':\n    x = byte\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let body = &program.functions[0].body;
    let pyx_ir::StmtKind::ForEach { var, body, .. } = &body[0].kind else {
        panic!("expected a for-each loop");
    };
    assert_eq!(program.functions[0].locals[*var as usize].ty, Type::Int);
    let pyx_ir::StmtKind::Assign { value, .. } = &body[0].kind else {
        panic!("expected an assignment");
    };
    assert_eq!(value.ty, Type::Int);
}

#[test]
fn bytes_are_hashable_in_dicts_and_sets() {
    assert!(check_errors("d = {b'a': 1}\ns = {b'a'}\n").is_empty());
}

#[test]
fn bytes_and_str_operands_are_rejected() {
    assert_eq!(
        check_errors("a = b'a' + 'b'\n"),
        vec!["unsupported operand type(s) for +: 'bytes' and 'str'"]
    );
    assert_eq!(
        check_errors("a = 'b' in b'a'\n"),
        vec!["'in <bytes>' requires bytes or int left operand, not str"]
    );
}

#[test]
fn bytes_index_must_be_int() {
    assert_eq!(
        check_errors("a = b'ab'['x']\n"),
        vec!["bytes index must be int, not str"]
    );
}

#[test]
fn bytes_builtin_and_method_arity_errors_are_reported() {
    assert_eq!(
        check_errors("a = bytes(1, 2, 3)\n"),
        vec!["bytes() takes 0 to 2 positional arguments (3 given)"]
    );
    assert_eq!(
        check_errors("a = 'x'.encode('utf-8', 'strict', 'extra')\n"),
        vec!["str.encode() takes 0 to 2 positional argument(s) (3 given)"]
    );
    assert_eq!(
        check_errors("a = b'a'.replace(b'a')\n"),
        vec!["bytes.replace() takes 2 to 3 positional argument(s) (1 given)"]
    );
}

#[test]
fn bytes_method_argument_types_are_checked() {
    assert_eq!(
        check_errors("a = b'a'.replace('a', b'b')\n"),
        vec!["bytes.replace() argument 1 has wrong type: expected bytes, got str"]
    );
    assert_eq!(
        check_errors("a = bytes(1.5)\n"),
        vec!["bytes() argument must be an int, str or iterable of ints, not float"]
    );
}

#[test]
fn attribute_values_are_dynamic() {
    assert!(check_errors("def f(x):\n    return x.name\n").is_empty());
    assert!(check_errors("def f(x):\n    return x.path.stem\n").is_empty());
}

#[test]
fn class_definitions_emit_the_class_statement_and_self_attribute_stores() {
    let source = "class C:\n    def __init__(self, x):\n        self.x = x\n    def get(self):\n        return self.x\nc = C(4)\nprint(c.get())\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let body = &program.functions[0].body;
    let pyx_ir::StmtKind::ClassDef {
        target,
        name,
        base,
        methods,
        ..
    } = &body[0].kind
    else {
        panic!("expected a class statement, got {:?}", body[0].kind);
    };
    assert_eq!(name, "C");
    assert_eq!(program.functions[0].locals[*target as usize].name, "C");
    assert!(base.is_none());
    assert_eq!(methods.len(), 2);
    let init = &program.functions[methods[0].function as usize];
    assert_eq!(init.name, "C.__init__");
    assert!(
        matches!(&init.body[0].kind, StmtKind::SetAttr { name, .. } if name == "x"),
        "{:?}",
        init.body[0].kind
    );
    let get = &program.functions[methods[1].function as usize];
    assert!(
        matches!(&get.body[0].kind, StmtKind::Return(Some(value)) if matches!(&value.kind, ExprKind::GetAttr { name, .. } if name == "x")),
        "{:?}",
        get.body[0].kind
    );
}

#[test]
fn class_setattr_and_class_bodies_typecheck_without_diagnostics() {
    let source = "class C:\n    pass\nc = C()\nc.x = 7\nprint(c.x)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let body = &program.functions[0].body;
    assert!(matches!(&body[0].kind, StmtKind::ClassDef { .. }));
    assert!(matches!(&body[2].kind, StmtKind::SetAttr { name, .. } if name == "x"));
}

#[test]
fn subclass_emits_its_base_expression() {
    let source = "class C:\n    pass\nclass D(C):\n    pass\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let body = &program.functions[0].body;
    let StmtKind::ClassDef {
        base: Some(base), ..
    } = &body[1].kind
    else {
        panic!("expected class D with a base, got {:?}", body[1].kind);
    };
    assert!(
        matches!(&base.kind, ExprKind::Local(_)),
        "base must reference the C local: {:?}",
        base.kind
    );
}

#[test]
fn exception_class_values_are_emitted_and_usable_as_bases() {
    let source = "\
x = ValueError
def make():
    return Exception
class E(Exception):
    pass
class F(E):
    pass
";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let body = &program.functions[0].body;
    assert!(
        matches!(
            &body[0].kind,
            StmtKind::Assign { value, .. }
                if matches!(&value.kind, ExprKind::ExceptionClass(ExcKind::ValueError))
        ),
        "{:?}",
        body[0].kind
    );
    let make = program_function(&program, "make");
    assert!(
        matches!(
            &make.body[0].kind,
            StmtKind::Return(Some(value))
                if matches!(&value.kind, ExprKind::ExceptionClass(ExcKind::Exception))
        ),
        "{:?}",
        make.body[0].kind
    );
    let StmtKind::ClassDef {
        base: Some(base), ..
    } = &body[1].kind
    else {
        panic!("expected class E with a base, got {:?}", body[1].kind);
    };
    assert!(
        matches!(&base.kind, ExprKind::ExceptionClass(ExcKind::Exception)),
        "base must be the Exception class: {:?}",
        base.kind
    );
    // `class F(E)` keeps referring to the class local created by `class E`.
    let StmtKind::ClassDef {
        base: Some(base), ..
    } = &body[2].kind
    else {
        panic!("expected class F with a base, got {:?}", body[2].kind);
    };
    assert!(
        matches!(&base.kind, ExprKind::Local(_)),
        "base must reference the E local: {:?}",
        base.kind
    );
}

fn program_function<'a>(program: &'a pyx_ir::Program, name: &str) -> &'a pyx_ir::Function {
    program
        .functions
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no function '{name}'"))
}

#[test]
fn generator_functions_are_marked_and_call_results_are_dynamic() {
    let source = "def g():\n    yield 1\n    yield 2\nxs = list(g())\nprint(xs)\n";
    let program = crate::check_source(source, "test.py").expect("valid generator");
    let g = program_function(&program, "g");
    assert!(g.is_generator);
    assert!(!g.is_async);

    let yields = g
        .body
        .iter()
        .filter(|stmt| matches!(&stmt.kind, pyx_ir::StmtKind::Yield { .. }))
        .count();
    assert_eq!(yields, 2, "{:?}", g.body);
    assert!(
        !g.body
            .iter()
            .any(|stmt| matches!(&stmt.kind, pyx_ir::StmtKind::Return(Some(_)))),
        "no materialised list return: {:?}",
        g.body
    );

    let module = program.main_function();
    let pyx_ir::StmtKind::Assign { value, .. } = &module.body[0].kind else {
        panic!("expected an assignment, got {:?}", module.body[0].kind);
    };
    let pyx_ir::ExprKind::Call { args, .. } = &value.kind else {
        panic!("expected list(...), got {:?}", value.kind);
    };
    assert_eq!(args[0].ty, Type::Any, "g() is a generator object");
}

#[test]
fn yield_expressions_are_typed_any_and_mark_generators() {
    let source = "def g():\n    x = (yield 1) + 2\n    yield x\n";
    let program = crate::check_source(source, "test.py").expect("valid generator");
    let g = program_function(&program, "g");
    assert!(g.is_generator, "{:?}", g.body);

    let StmtKind::Assign { target, value } = &g.body[0].kind else {
        panic!("expected an assignment, got {:?}", g.body[0].kind);
    };
    let ExprKind::Bin { lhs, .. } = &value.kind else {
        panic!("expected a bin op, got {:?}", value.kind);
    };
    let ExprKind::Yield { value: Some(inner) } = &lhs.kind else {
        panic!("expected a yield expression, got {:?}", lhs.kind);
    };
    assert_eq!(inner.ty, Type::Int);
    assert_eq!(lhs.ty, Type::Any, "the sent value is dynamically typed");
    assert_eq!(g.locals[*target as usize].ty, Type::Any);
}

#[test]
fn yield_from_is_typed_any_and_marks_generators() {
    let source = "def g():\n    x = yield from range(3)\n    yield x\n";
    let program = crate::check_source(source, "test.py").expect("valid generator");
    let g = program_function(&program, "g");
    assert!(g.is_generator, "{:?}", g.body);

    let StmtKind::Assign { value, .. } = &g.body[0].kind else {
        panic!("expected an assignment, got {:?}", g.body[0].kind);
    };
    assert!(
        matches!(&value.kind, ExprKind::YieldFrom { .. }),
        "{:?}",
        value.kind
    );
    assert_eq!(value.ty, Type::Any, "the delegate result is dynamic");

    let statement_source = "def g():\n    yield from [1, 2]\n";
    let statement_program =
        crate::check_source(statement_source, "test.py").expect("valid generator");
    let statement_g = program_function(&statement_program, "g");
    assert!(statement_g.is_generator);
    assert!(matches!(
        &statement_g.body[0].kind,
        StmtKind::Expr(expr) if matches!(expr.kind, ExprKind::YieldFrom { .. })
    ));
}

#[test]
fn async_functions_are_marked_and_call_results_are_dynamic() {
    let source = "import asyncio\nasync def f():\n    return 42\nx = asyncio.run(f())\nprint(x)\n";
    let program = crate::check_source(source, "test.py").expect("valid async function");
    let f = program_function(&program, "f");
    assert!(f.is_async);
    assert!(!f.is_generator);
    assert_eq!(f.ret_ty, Type::Int);
    assert!(matches!(&f.body[0].kind, pyx_ir::StmtKind::Return(Some(_))));

    let module = program.main_function();
    let assign = module
        .body
        .iter()
        .find(|stmt| matches!(&stmt.kind, pyx_ir::StmtKind::Assign { .. }))
        .expect("x = asyncio.run(...)");
    let pyx_ir::StmtKind::Assign { value, .. } = &assign.kind else {
        unreachable!("filtered above");
    };
    let pyx_ir::ExprKind::ModuleCall { function, args, .. } = &value.kind else {
        panic!("expected asyncio.run(...), got {:?}", value.kind);
    };
    assert_eq!(function.as_str(), "run");
    assert_eq!(args[0].ty, Type::Any, "f() is a coroutine object");
}

#[test]
fn await_emits_an_await_expression_typed_any() {
    let source = "import asyncio\nasync def inner():\n    return 5\nasync def outer():\n    return await inner()\nprint(asyncio.run(outer()))\n";
    let program = crate::check_source(source, "test.py").expect("valid await");
    let outer = program_function(&program, "outer");
    assert!(outer.is_async);
    let pyx_ir::StmtKind::Return(Some(value)) = &outer.body[0].kind else {
        panic!("expected a return statement, got {:?}", outer.body[0].kind);
    };
    let pyx_ir::ExprKind::Await { value: awaited } = &value.kind else {
        panic!("expected an await expression, got {:?}", value.kind);
    };
    assert!(matches!(&awaited.kind, pyx_ir::ExprKind::Call { .. }));
    assert_eq!(awaited.ty, Type::Any, "inner() is a coroutine object");
    assert_eq!(value.ty, Type::Any, "await result is dynamic");
}

#[test]
fn async_with_and_async_for_typecheck() {
    let source = "async def read():\n    lines = []\n    async with open('x.txt') as fh:\n        async for line in fh:\n            lines.append(line)\n    return lines\nread()\n";
    crate::check_source(source, "test.py").expect("valid async with/for");
}

/// True when `stmts` or any statement nested inside them matches `pred`.
fn ir_stmt_any(stmts: &[pyx_ir::Stmt], pred: &dyn Fn(&pyx_ir::StmtKind) -> bool) -> bool {
    stmts.iter().any(|stmt| {
        if pred(&stmt.kind) {
            return true;
        }
        match &stmt.kind {
            pyx_ir::StmtKind::If {
                then_body,
                else_body,
                ..
            } => ir_stmt_any(then_body, pred) || ir_stmt_any(else_body, pred),
            pyx_ir::StmtKind::While {
                body, else_body, ..
            }
            | pyx_ir::StmtKind::ForRange {
                body, else_body, ..
            }
            | pyx_ir::StmtKind::ForEach {
                body, else_body, ..
            } => ir_stmt_any(body, pred) || ir_stmt_any(else_body, pred),
            pyx_ir::StmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => {
                ir_stmt_any(body, pred)
                    || handlers
                        .iter()
                        .any(|handler| ir_stmt_any(&handler.body, pred))
                    || ir_stmt_any(else_body, pred)
                    || ir_stmt_any(finally_body, pred)
            }
            _ => false,
        }
    })
}

fn ir_catches_stop_async_iteration(kind: &pyx_ir::StmtKind) -> bool {
    matches!(
        kind,
        pyx_ir::StmtKind::Try { handlers, .. }
            if handlers
                .iter()
                .any(|handler| handler.exc == Some(ExcKind::StopAsyncIteration))
    )
}

#[test]
fn async_generators_carry_both_flags() {
    let source = "async def g():\n    yield 1\n";
    let program = crate::check_source(source, "test.py").expect("valid async generator");
    let g = program_function(&program, "g");
    assert!(g.is_async, "an async generator is a coroutine function");
    assert!(g.is_generator, "an async generator is a generator");
    assert!(matches!(&g.body[0].kind, pyx_ir::StmtKind::Yield { .. }));
}

#[test]
fn async_for_emits_await_anext_and_stop_async_iteration() {
    let source = "async def f(it):\n    async for x in it:\n        print(x)\n";
    let program = crate::check_source(source, "test.py").expect("valid async for");
    let f = program_function(&program, "f");

    // The desugaring is wrapped in an always-true block.
    let pyx_ir::StmtKind::If { then_body, .. } = &f.body[0].kind else {
        panic!("expected the async-for block, got {:?}", f.body[0].kind);
    };
    let pyx_ir::StmtKind::While {
        body, else_body, ..
    } = &then_body[1].kind
    else {
        panic!("expected the iteration loop, got {:?}", then_body[1].kind);
    };
    assert!(else_body.is_empty());

    let pyx_ir::StmtKind::Try {
        body: fetch,
        handlers,
        else_body,
        finally_body,
    } = &body[0].kind
    else {
        panic!("expected the __anext__ try, got {:?}", body[0].kind);
    };
    assert!(else_body.is_empty());
    assert!(finally_body.is_empty());
    assert_eq!(handlers.len(), 1);
    assert_eq!(handlers[0].exc, Some(ExcKind::StopAsyncIteration));
    assert_eq!(
        handlers[0].exc.map(ExcKind::tag),
        Some(18),
        "StopAsyncIteration keeps the stable tag"
    );
    assert!(matches!(&handlers[0].body[0].kind, pyx_ir::StmtKind::Break));

    let pyx_ir::StmtKind::Assign { value, .. } = &fetch[0].kind else {
        panic!("expected the awaited item, got {:?}", fetch[0].kind);
    };
    let pyx_ir::ExprKind::Await { value } = &value.kind else {
        panic!("expected await, got {:?}", value.kind);
    };
    let pyx_ir::ExprKind::MethodCall { method, .. } = &value.kind else {
        panic!("expected a method call, got {:?}", value.kind);
    };
    assert_eq!(method, "__anext__");
    assert_eq!(value.ty, Type::Any);
}

#[test]
fn async_for_else_sets_an_exhaustion_flag() {
    let source =
        "async def f(it):\n    async for x in it:\n        print(x)\n    else:\n        print(0)\n";
    let program = crate::check_source(source, "test.py").expect("valid async for/else");
    let f = program_function(&program, "f");
    let pyx_ir::StmtKind::If { then_body, .. } = &f.body[0].kind else {
        panic!("expected the async-for block");
    };
    // [$aloop = False, $aiter = ..., while, if $aloop: else]
    assert_eq!(then_body.len(), 4);
    let pyx_ir::StmtKind::If {
        then_body: else_body,
        ..
    } = &then_body[3].kind
    else {
        panic!("expected the else guard");
    };
    assert_eq!(else_body.len(), 1, "the else body runs behind the guard");
    let pyx_ir::StmtKind::While { body, .. } = &then_body[2].kind else {
        panic!("expected the loop");
    };
    let pyx_ir::StmtKind::Try { handlers, .. } = &body[0].kind else {
        panic!("expected the __anext__ try");
    };
    assert_eq!(handlers[0].exc, Some(ExcKind::StopAsyncIteration));
    assert!(matches!(
        &handlers[0].body[0].kind,
        pyx_ir::StmtKind::Assign { .. }
    ));
    assert!(matches!(&handlers[0].body[1].kind, pyx_ir::StmtKind::Break));
}

#[test]
fn async_with_awaits_aenter_and_aexit() {
    let source = "async def f(cm):\n    async with cm as value:\n        print(value)\n";
    let program = crate::check_source(source, "test.py").expect("valid async with");
    let f = program_function(&program, "f");
    let pyx_ir::StmtKind::If { then_body, .. } = &f.body[0].kind else {
        panic!("expected the async-with block, got {:?}", f.body[0].kind);
    };
    assert_eq!(then_body.len(), 4, "manager, exception slot, entry, try");

    let pyx_ir::StmtKind::Assign { value, .. } = &then_body[2].kind else {
        panic!("expected the entry binding, got {:?}", then_body[2].kind);
    };
    let pyx_ir::ExprKind::Await { value } = &value.kind else {
        panic!("expected `await __aenter__()`");
    };
    let pyx_ir::ExprKind::MethodCall { method, .. } = &value.kind else {
        panic!("expected a method call");
    };
    assert_eq!(method, "__aenter__");

    let pyx_ir::StmtKind::Try {
        handlers,
        finally_body,
        ..
    } = &then_body[3].kind
    else {
        panic!("expected the __aexit__ try, got {:?}", then_body[3].kind);
    };
    assert_eq!(handlers.len(), 1);
    assert_eq!(handlers[0].exc, Some(ExcKind::BaseException));
    assert!(handlers[0].binding.is_some());

    // The handler awaits `__aexit__(type(exc), exc, None)`.
    let pyx_ir::StmtKind::If {
        cond, then_body, ..
    } = &handlers[0].body[0].kind
    else {
        panic!("expected the suppression check");
    };
    let pyx_ir::ExprKind::Un { op, operand } = &cond.kind else {
        panic!("expected `not await __aexit__(...)`, got {:?}", cond.kind);
    };
    assert_eq!(*op, pyx_ir::UnOp::Not);
    let pyx_ir::ExprKind::Await { value } = &operand.kind else {
        panic!("expected an await");
    };
    let pyx_ir::ExprKind::MethodCall { method, args, .. } = &value.kind else {
        panic!("expected a method call");
    };
    assert_eq!(method, "__aexit__");
    assert_eq!(args.len(), 3);
    assert!(matches!(
        &args[0].kind,
        pyx_ir::ExprKind::Call {
            callee: pyx_ir::Callee::Builtin(pyx_ir::Builtin::Type),
            ..
        }
    ));
    assert_eq!(args[2].ty, Type::Any, "PyX passes None as the traceback");
    assert!(matches!(
        &then_body[0].kind,
        pyx_ir::StmtKind::Raise { exc: None, .. }
    ));

    // The finally arm calls `__aexit__(None, None, None)` on a normal exit.
    assert_eq!(finally_body.len(), 1);
    let pyx_ir::StmtKind::If { then_body, .. } = &finally_body[0].kind else {
        panic!("expected the normal-exit guard");
    };
    let pyx_ir::StmtKind::Expr(exit) = &then_body[0].kind else {
        panic!("expected the normal exit call");
    };
    let pyx_ir::ExprKind::Await { value } = &exit.kind else {
        panic!("expected an await");
    };
    let pyx_ir::ExprKind::MethodCall { method, args, .. } = &value.kind else {
        panic!("expected a method call");
    };
    assert_eq!(method, "__aexit__");
    assert_eq!(args.len(), 3);
    assert!(args.iter().all(|arg| arg.ty == Type::Any));
}

#[test]
fn async_list_comprehension_emits_an_async_helper() {
    let source = "async def f(it):\n    return [x async for x in it]\n";
    let program = crate::check_source(source, "test.py").expect("valid async comprehension");
    let f = program_function(&program, "f");
    let pyx_ir::StmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!("expected the returned comprehension");
    };
    assert_eq!(value.ty, Type::List);
    let pyx_ir::ExprKind::Await { value } = &value.kind else {
        panic!("expected the awaited helper, got {:?}", value.kind);
    };
    let pyx_ir::ExprKind::Call {
        callee: pyx_ir::Callee::Function(id),
        ..
    } = &value.kind
    else {
        panic!("expected a helper call, got {:?}", value.kind);
    };
    let helper = program.function(*id);
    assert_eq!(helper.name, "<asynccomp>");
    assert!(helper.is_async);
    assert!(!helper.is_generator);
    assert!(matches!(
        helper.body.first().map(|stmt| &stmt.kind),
        Some(pyx_ir::StmtKind::Assign { .. })
    ));
    assert!(matches!(
        helper.body.last().map(|stmt| &stmt.kind),
        Some(pyx_ir::StmtKind::Return(Some(_)))
    ));
    assert!(
        ir_stmt_any(&helper.body, &ir_catches_stop_async_iteration),
        "the helper handles the end of the async iterator"
    );
}

#[test]
fn async_generator_expression_emits_an_async_generator() {
    let source = "async def f(it):\n    return (x async for x in it)\n";
    let program = crate::check_source(source, "test.py").expect("valid async genexp");
    let f = program_function(&program, "f");
    let pyx_ir::StmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!("expected the returned generator expression");
    };
    let pyx_ir::ExprKind::Call {
        callee: pyx_ir::Callee::Function(id),
        ..
    } = &value.kind
    else {
        panic!("expected the generator function call, got {:?}", value.kind);
    };
    let helper = program.function(*id);
    assert_eq!(helper.name, "<genexpr>");
    assert!(helper.is_async);
    assert!(helper.is_generator);
    assert!(ir_stmt_any(&helper.body, &|kind| matches!(
        kind,
        pyx_ir::StmtKind::Yield { .. }
    )));
    assert!(ir_stmt_any(&helper.body, &ir_catches_stop_async_iteration));
}

#[test]
fn generator_expression_with_await_is_an_async_generator() {
    let source =
        "import asyncio\nasync def f(xs):\n    return (await asyncio.sleep(0) for x in xs)\n";
    let program = crate::check_source(source, "test.py").expect("valid async genexp");
    let f = program_function(&program, "f");
    let pyx_ir::StmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!("expected the returned generator expression");
    };
    let pyx_ir::ExprKind::Call {
        callee: pyx_ir::Callee::Function(id),
        ..
    } = &value.kind
    else {
        panic!("expected the generator function call, got {:?}", value.kind);
    };
    let helper = program.function(*id);
    assert_eq!(helper.name, "<genexpr>");
    assert!(helper.is_async, "a frame that awaits is an async generator");
    assert!(helper.is_generator);
    assert!(ir_stmt_any(&helper.body, &|kind| matches!(
        kind,
        pyx_ir::StmtKind::Yield { value: Some(value) }
            if matches!(value.kind, pyx_ir::ExprKind::Await { .. })
    )));
}

#[test]
fn stop_async_iteration_is_a_catchable_exception() {
    let source = "def f():\n    try:\n        pass\n    except StopAsyncIteration:\n        pass\n";
    let program = crate::check_source(source, "test.py").expect("catchable StopAsyncIteration");
    let f = program_function(&program, "f");
    let pyx_ir::StmtKind::Try { handlers, .. } = &f.body[0].kind else {
        panic!("expected a try statement");
    };
    assert_eq!(handlers[0].exc, Some(ExcKind::StopAsyncIteration));
    assert_eq!(ExcKind::StopAsyncIteration.tag(), 18);
    assert_eq!(ExcKind::StopAsyncIteration.name(), "StopAsyncIteration");
    assert_eq!(
        ExcKind::from_name("StopAsyncIteration"),
        Some(ExcKind::StopAsyncIteration)
    );
}

#[test]
fn async_comprehension_outside_async_function_is_a_control_error() {
    let diags = check_diags("def f():\n    return [x async for x in range(3)]\n");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::CONTROL);
    assert_eq!(
        diags[0].message,
        "asynchronous comprehension outside of an asynchronous function"
    );
}

#[test]
fn asyncio_sleep_and_gather_typecheck() {
    let source = "import asyncio\nasync def f():\n    await asyncio.sleep(0.001)\n    values = asyncio.gather([1, 2])\n    return values\nf()\n";
    let program = crate::check_source(source, "test.py").expect("valid asyncio usage");
    let f = program_function(&program, "f");
    assert_eq!(f.ret_ty, Type::List);
}

#[test]
fn asyncio_unsupported_members_are_rejected() {
    let diags = check_diags("import asyncio\nasyncio.wait_for(1, 2)\n");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::ATTRIBUTE);
    assert_eq!(
        diags[0].message,
        "module 'asyncio' has no attribute 'wait_for'"
    );
}

#[test]
fn asyncio_task_api_is_supported() {
    let source = "import asyncio\n\
                  task = asyncio.create_task(None)\n\
                  current = asyncio.current_task()\n\
                  group = asyncio.TaskGroup()\n\
                  factory = asyncio.TaskGroup\n\
                  cancelled = asyncio.CancelledError\n";
    assert!(
        check_errors(source).is_empty(),
        "{:?}",
        check_errors(source)
    );

    let imported = r#"from asyncio import TaskGroup
async def main():
    async with TaskGroup() as group:
        pass
main()
"#;
    assert!(
        check_errors(imported).is_empty(),
        "{:?}",
        check_errors(imported)
    );
}

#[test]
fn extension_nodes_type_as_any() {
    let prog = empty_prog();
    let mut inf = Infer::new();
    let mut ck = checker(&prog, &mut inf);
    let function = HFunction {
        name: "<module>".to_string(),
        params: Vec::new(),
        capture_count: 0,
        ret_ty: Ty::Concrete(Type::None),
        locals: Vec::new(),
        body: Vec::new(),
        is_async: false,
        is_generator: false,
        is_module: true,
        line: 1,
        col: 1,
    };

    let attr_ty = ck.inf.fresh();
    let attr = HExpr {
        kind: HExprKind::ExtensionAttr {
            path: "/tmp/mod_ext.abi3.so".to_string(),
            member: "tag".to_string(),
        },
        ty: attr_ty,
        line: 1,
        col: 1,
    };
    ck.check_expr(&attr, &function);
    assert!(ck.diags.is_empty(), "{:?}", ck.diags);
    assert_eq!(ck.inf.resolve(attr_ty), Ty::Concrete(Type::Any));

    let arg_ty = ck.inf.fresh();
    let arg = HExpr {
        kind: HExprKind::Const(pyx_ir::Const::Int(2)),
        ty: arg_ty,
        line: 1,
        col: 1,
    };
    let out_ty = ck.inf.fresh();
    let call = HExpr {
        kind: HExprKind::Call {
            callee: Callee::Extension {
                path: "/tmp/mod_ext.abi3.so".to_string(),
                member: "add".to_string(),
            },
            args: vec![arg],
        },
        ty: out_ty,
        line: 1,
        col: 1,
    };
    ck.check_expr(&call, &function);
    assert!(ck.diags.is_empty(), "{:?}", ck.diags);
    assert_eq!(ck.inf.resolve(out_ty), Ty::Concrete(Type::Any));

    let emitted = emit_expr(&call, &mut *ck.inf);
    match emitted.kind {
        ExprKind::Call {
            callee: Callee::Extension { path, member },
            args,
        } => {
            assert_eq!(
                (path.as_str(), member.as_str()),
                ("/tmp/mod_ext.abi3.so", "add")
            );
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected an extension call, got {other:?}"),
    }
}

#[test]
fn raise_from_emits_the_cause_expression() {
    let source = "def f(cause):\n    raise RuntimeError('outer') from cause\n";
    let program = crate::check_source(source, "test.py").expect("valid raise-from");
    let f = program_function(&program, "f");
    let StmtKind::Raise { exc, msg, cause } = &f.body[0].kind else {
        panic!("expected a raise statement, got {:?}", f.body[0].kind);
    };
    assert_eq!(*exc, Some(pyx_ir::ExcKind::RuntimeError));
    assert!(msg.is_some());
    let cause = cause.as_ref().expect("cause must be emitted");
    assert!(
        matches!(&cause.kind, ExprKind::Local(_)),
        "cause must reference the parameter: {:?}",
        cause.kind
    );
}

#[test]
fn del_emits_ir_delete_targets() {
    let source = "def f(xs, table):\n    del xs[0], table['k'], xs[1:]\n";
    let program = crate::check_source(source, "test.py").expect("valid del");
    let f = program_function(&program, "f");
    let StmtKind::Delete { targets } = &f.body[0].kind else {
        panic!("expected a delete statement, got {:?}", f.body[0].kind);
    };
    assert_eq!(targets.len(), 3);
    assert!(matches!(&targets[0], DeleteTarget::Subscript { .. }));
    assert!(matches!(&targets[1], DeleteTarget::Subscript { .. }));
    let DeleteTarget::Slice { stop, .. } = &targets[2] else {
        panic!("expected a slice target, got {:?}", targets[2]);
    };
    // An omitted bound becomes the runtime `None` sentinel.
    assert!(matches!(&stop.kind, ExprKind::Const(Const::None)));
}

#[test]
fn system_exit_and_name_error_handlers_keep_their_tags() {
    let source = "def f():\n    try:\n        pass\n    except SystemExit:\n        pass\n    except NameError:\n        pass\n";
    let program = crate::check_source(source, "test.py").expect("valid handlers");
    let f = program_function(&program, "f");
    let StmtKind::Try { handlers, .. } = &f.body[0].kind else {
        panic!("expected a try statement, got {:?}", f.body[0].kind);
    };
    assert_eq!(handlers[0].exc, Some(pyx_ir::ExcKind::SystemExit));
    assert_eq!(handlers[1].exc, Some(pyx_ir::ExcKind::NameError));
}

#[test]
fn input_and_get_ipython_calls_are_typed() {
    let source = "def f():\n    a = input('> ')\n    return get_ipython()\n";
    let program = crate::check_source(source, "test.py").expect("valid probes");
    let f = program_function(&program, "f");
    let StmtKind::Assign { value, .. } = &f.body[0].kind else {
        panic!("expected an assignment, got {:?}", f.body[0].kind);
    };
    assert_eq!(value.ty, pyx_ir::Type::Str);
    let StmtKind::Return(Some(value)) = &f.body[1].kind else {
        panic!("expected a return, got {:?}", f.body[1].kind);
    };
    assert_eq!(value.ty, pyx_ir::Type::Any);
}

#[test]
fn raise_from_none_emits_a_none_constant_cause() {
    let source = "def f():\n    raise ValueError('inner') from None\n";
    let program = crate::check_source(source, "test.py").expect("valid raise-from-none");
    let f = program_function(&program, "f");
    let StmtKind::Raise {
        exc,
        cause: Some(cause),
        ..
    } = &f.body[0].kind
    else {
        panic!(
            "expected a raise statement with a cause, got {:?}",
            f.body[0].kind
        );
    };
    assert_eq!(*exc, Some(pyx_ir::ExcKind::ValueError));
    assert!(matches!(&cause.kind, ExprKind::Const(pyx_ir::Const::None)));
}

#[test]
fn assert_emits_the_test_and_message() {
    let source = "def f(value):\n    assert value, 'value is required'\n";
    let program = crate::check_source(source, "test.py").expect("valid assert");
    let f = program_function(&program, "f");
    let StmtKind::Assert { test, msg } = &f.body[0].kind else {
        panic!("expected an assert statement, got {:?}", f.body[0].kind);
    };
    assert!(
        matches!(&test.kind, ExprKind::Local(_)),
        "the test must reference the parameter: {:?}",
        test.kind
    );
    assert!(
        matches!(
            msg.as_ref().map(|msg| &msg.kind),
            Some(ExprKind::Const(pyx_ir::Const::Str(text))) if text == "value is required"
        ),
        "the message must be emitted: {msg:?}"
    );
}

#[test]
fn assert_without_a_message_and_with_any_message_value_is_valid() {
    let bare = crate::check_source("def f(value):\n    assert value\n", "test.py")
        .expect("a bare assert is valid");
    let f = program_function(&bare, "f");
    let StmtKind::Assert { msg, .. } = &f.body[0].kind else {
        panic!("expected an assert statement, got {:?}", f.body[0].kind);
    };
    assert!(msg.is_none(), "a bare assert has no message");

    // `assert x, <non-str>` is legal Python: the value becomes the exception
    // argument, so it is not restricted to `str`.
    crate::check_source("def f(value):\n    assert value, [1, 2]\n", "test.py")
        .expect("a non-str assert message is valid");
}

#[test]
fn tuple_handler_emits_one_handler_per_type_in_source_order() {
    let source = "def f():\n    try:\n        pass\n    except (TypeError, ValueError) as e:\n        print(e)\n";
    let program = crate::check_source(source, "test.py").expect("valid tuple handler");
    let f = program_function(&program, "f");
    let StmtKind::Try { handlers, .. } = &f.body[0].kind else {
        panic!("expected a try statement, got {:?}", f.body[0].kind);
    };
    assert_eq!(handlers.len(), 2, "{handlers:?}");
    assert_eq!(handlers[0].exc, Some(pyx_ir::ExcKind::TypeError));
    assert_eq!(handlers[1].exc, Some(pyx_ir::ExcKind::ValueError));
    assert!(handlers[0].binding.is_some());
    assert_eq!(handlers[0].binding, handlers[1].binding);
    assert_eq!(handlers[0].body.len(), handlers[1].body.len());
}

#[test]
fn exception_hierarchy_handlers_survive_emission() {
    let source = "def f():\n    try:\n        pass\n    except Exception:\n        pass\n    except BaseException:\n        pass\n";
    let program = crate::check_source(source, "test.py").expect("valid hierarchy handlers");
    let f = program_function(&program, "f");
    let StmtKind::Try { handlers, .. } = &f.body[0].kind else {
        panic!("expected a try statement, got {:?}", f.body[0].kind);
    };
    let kinds: Vec<Option<pyx_ir::ExcKind>> = handlers.iter().map(|h| h.exc).collect();
    assert_eq!(
        kinds,
        vec![
            Some(pyx_ir::ExcKind::Exception),
            Some(pyx_ir::ExcKind::BaseException)
        ]
    );
}

#[test]
fn identity_comparisons_are_boolean_for_any_operand_pair() {
    assert_eq!(
        op_result(BinOp::Is, Type::Int, Type::None),
        Some(Type::Bool)
    );
    assert_eq!(
        op_result(BinOp::IsNot, Type::List, Type::Str),
        Some(Type::Bool)
    );
    assert_eq!(
        op_result(BinOp::Is, Type::Bool, Type::Bool),
        Some(Type::Bool)
    );
    // A call that never returns cannot be compared.
    assert_eq!(op_result(BinOp::Is, Type::Never, Type::Int), None);
}

#[test]
fn identity_comparison_does_not_narrow_the_operand() {
    let source = "def f(x):\n    return x is None\n";
    let program = crate::check_source(source, "test.py").expect("valid identity comparison");
    let f = program.functions.iter().find(|f| f.name == "f").expect("f");
    assert_eq!(f.ret_ty, Type::Bool);
    // `x is None` must leave `x` dynamically typed, not force it to None.
    assert_eq!(f.locals[0].ty, Type::Any);
}

#[test]
fn new_builtins_have_the_documented_result_types() {
    let source = "a = max(1, 2)\nb = min(1, 2)\nc = round(1.5)\nd = format(1, \"d\")\ne = isinstance(1, int)\n";
    let program = crate::check_source(source, "test.py").expect("valid builtin program");
    assert_eq!(assigned_type(&program, 0), Type::Any);
    assert_eq!(assigned_type(&program, 1), Type::Any);
    assert_eq!(assigned_type(&program, 2), Type::Any);
    assert_eq!(assigned_type(&program, 3), Type::Str);
    assert_eq!(assigned_type(&program, 4), Type::Bool);
}

#[test]
fn sorted_returns_a_list_and_validates_its_slots() {
    let source = "a = sorted([3, 1])\nb = sorted((1, 2), reverse=True)\nc = sorted([1], key=str)\n";
    let program = crate::check_source(source, "test.py").expect("valid sorted program");
    for index in 0..3 {
        assert_eq!(assigned_type(&program, index), Type::List);
    }

    assert_eq!(
        check_errors("a = sorted(1)\n"),
        vec!["sorted() argument must be iterable"]
    );
    assert_eq!(
        check_errors("a = sorted([1], key=1)\n"),
        vec!["sorted() key must be callable or None, not int"]
    );
}

#[test]
fn conditional_expression_joins_the_branch_types() {
    let source = "a = 1 if True else 2\nb = 1 if True else \"x\"\n";
    let program = crate::check_source(source, "test.py").expect("valid conditional expressions");
    assert_eq!(assigned_type(&program, 0), Type::Int);
    // Mixed branch types fall back to a dynamic result.
    assert_eq!(assigned_type(&program, 1), Type::Any);
}

#[test]
fn bitwise_operands_produce_int_or_dynamic_results() {
    for op in [
        BinOp::BitAnd,
        BinOp::BitOr,
        BinOp::BitXor,
        BinOp::Shl,
        BinOp::Shr,
    ] {
        assert_eq!(op_result(op, Type::Int, Type::Int), Some(Type::Int));
        assert_eq!(op_result(op, Type::Bool, Type::Int), Some(Type::Int));
        assert_eq!(op_result(op, Type::Bool, Type::Bool), Some(Type::Int));
        assert_eq!(op_result(op, Type::Any, Type::Int), Some(Type::Any));
        // Non-integer operands are only rejected at runtime.
        assert_eq!(op_result(op, Type::Str, Type::Str), Some(Type::Any));
        assert_eq!(op_result(op, Type::Float, Type::Int), Some(Type::Any));
    }
}

#[test]
fn bitwise_expressions_typecheck_as_int() {
    let source = "a = 6 & 3\nb = 1 << 4\nc = 8 >> 1\nd = True | False\n";
    let program = crate::check_source(source, "test.py").expect("valid bitwise expressions");
    for index in 0..4 {
        assert_eq!(assigned_type(&program, index), Type::Int);
    }
}

#[test]
fn repr_returns_a_str() {
    let source = "a = repr(1)\n";
    let program = crate::check_source(source, "test.py").expect("valid repr call");
    assert_eq!(assigned_type(&program, 0), Type::Str);
}

#[test]
fn invert_requires_an_int_operand() {
    assert!(check_errors("x = ~1\ny = ~True\n").is_empty());
    assert_eq!(
        check_errors("x = ~1.5\n"),
        vec!["bad operand type for unary ~: 'float'"]
    );
    assert_eq!(
        check_errors("x = ~'s'\n"),
        vec!["bad operand type for unary ~: 'str'"]
    );
}

#[test]
fn hash_builtin_checks_arity_and_returns_int() {
    assert!(check_errors("x = hash(1)\ny = hash((1, 2))\n").is_empty());
    assert_eq!(
        check_errors("x = hash()\n"),
        vec!["hash() takes exactly one argument (0 given)"]
    );
}

#[test]
fn bigint_arithmetic_and_comparison_types() {
    let source = "a = 1152921504606846976\n\
                  b = a + 1\n\
                  c = a + 1.5\n\
                  d = a < 2\n\
                  e = a & 1\n\
                  f = a * True\n\
                  g = -a\n\
                  h = 1 + 2\n";
    let program = crate::check_source(source, "test.py").expect("valid bigint expressions");
    assert_eq!(assigned_type(&program, 0), Type::BigInt);
    assert_eq!(assigned_type(&program, 1), Type::BigInt);
    assert_eq!(assigned_type(&program, 2), Type::Float);
    assert_eq!(assigned_type(&program, 3), Type::Bool);
    assert_eq!(assigned_type(&program, 4), Type::BigInt);
    assert_eq!(assigned_type(&program, 5), Type::BigInt);
    assert_eq!(assigned_type(&program, 6), Type::BigInt);
    // Tagged-only arithmetic stays tagged; the runtime promotes on overflow.
    assert_eq!(assigned_type(&program, 7), Type::Int);
}

#[test]
fn bigint_annotation_types_parameters_and_returns() {
    let source = "def f(x: BigInt, y: int) -> BigInt:\n    return x\n";
    let program = crate::check_source(source, "test.py").expect("BigInt annotation accepted");
    let f = program
        .functions
        .iter()
        .find(|f| f.name == "f")
        .expect("function f");
    assert_eq!(f.params[0].ty, Type::BigInt);
    assert_eq!(f.params[1].ty, Type::Int);
    assert_eq!(f.ret_ty, Type::BigInt);
}

#[test]
fn bigint_annotation_names_itself_in_diagnostics() {
    assert_eq!(
        check_errors("def f() -> BigInt:\n    return 1.5\n"),
        vec!["return type float does not match declared return type BigInt"]
    );
}

#[test]
fn bigint_literals_satisfy_numeric_annotations() {
    assert!(check_errors("def f(x: BigInt = 1152921504606846976):\n    return x\n").is_empty());
    assert!(check_errors("def f(x: int) -> int:\n    return 1152921504606846976\n").is_empty());
    assert!(check_errors("def f(x: float):\n    return x\nf(1152921504606846976)\n").is_empty());
}

// ---------------------------------------------------------------------------
// String-accumulator pass
// ---------------------------------------------------------------------------

/// Top-level body of the emitted `<module>` function.
fn module_body(source: &str) -> Vec<pyx_ir::Stmt> {
    let program = crate::check_source(source, "test.py").expect("valid program");
    program.functions[0].body.clone()
}

/// Body of the first top-level loop of `stmts`.
fn loop_body(stmts: &[pyx_ir::Stmt]) -> &[pyx_ir::Stmt] {
    stmts
        .iter()
        .find_map(|stmt| match &stmt.kind {
            StmtKind::While { body, .. }
            | StmtKind::ForRange { body, .. }
            | StmtKind::ForEach { body, .. } => Some(body.as_slice()),
            _ => None,
        })
        .expect("a top-level loop")
}

fn builder_appends(stmts: &[pyx_ir::Stmt]) -> usize {
    stmts
        .iter()
        .filter(|stmt| {
            matches!(
                &stmt.kind,
                StmtKind::Assign { value, .. }
                    if matches!(&value.kind, ExprKind::StrBuilderAppend { .. })
            )
        })
        .count()
}

fn is_finish(stmt: &pyx_ir::Stmt) -> bool {
    matches!(
        &stmt.kind,
        StmtKind::Assign { value, .. }
            if matches!(&value.kind, ExprKind::StrBuilderFinish { .. })
    )
}

fn is_builder_new(stmt: &pyx_ir::Stmt) -> bool {
    matches!(
        &stmt.kind,
        StmtKind::Assign { value, .. }
            if matches!(&value.kind, ExprKind::StrBuilderNew { .. })
    )
}

/// String concatenations inside `stmts`: `s += <str>` or `s = s + <str>`.
///
/// Narrowed to `str` results so loop counters such as `i += 1` are not
/// mistaken for accumulator appends; a rewritten append yields a builder
/// (typed `Any`) and is therefore not counted either.
fn string_additions(stmts: &[pyx_ir::Stmt]) -> usize {
    stmts
        .iter()
        .filter(|stmt| match &stmt.kind {
            StmtKind::AugAssign { op, value, .. } => *op == BinOp::Add && value.ty == Type::Str,
            StmtKind::Assign { value, .. } => match &value.kind {
                ExprKind::Bin { op, .. } => *op == BinOp::Add && value.ty == Type::Str,
                _ => false,
            },
            _ => false,
        })
        .count()
}

fn all_locals(source: &str) -> Vec<pyx_ir::LocalInfo> {
    crate::check_source(source, "test.py")
        .expect("valid program")
        .functions
        .into_iter()
        .flat_map(|function| function.locals)
        .collect()
}

fn has_builder_local(source: &str) -> bool {
    all_locals(source)
        .iter()
        .any(|local| local.name.contains("#builder") && local.ty == Type::Any)
}

#[test]
fn module_body_accumulator_uses_the_builder() {
    // The benchmark shape: `s += "x"` in a `while` loop, read afterwards.
    let body = module_body(
        "n = 3\ns = \"\"\ni = 0\nwhile i < n:\n    s += \"x\"\n    i += 1\nprint(len(s))\n",
    );
    let seed = body
        .iter()
        .position(|stmt| {
            matches!(
                &stmt.kind,
                StmtKind::Assign { value, .. }
                    if matches!(&value.kind, ExprKind::Const(pyx_ir::Const::Str(text)) if text.is_empty())
            )
        })
        .expect("string seed");
    let loop_index = body
        .iter()
        .position(|stmt| matches!(&stmt.kind, StmtKind::While { .. }))
        .expect("while loop");
    assert_eq!(builder_appends(loop_body(&body)), 1);
    assert_eq!(string_additions(loop_body(&body)), 0);
    // The builder is created right after the seed and finished right after
    // the loop, before the first read.
    assert!(is_builder_new(&body[seed + 1]));
    assert!(is_finish(&body[loop_index + 1]));
}

#[test]
fn function_local_accumulator_uses_the_builder() {
    let source = "def build(n):\n    s = \"\"\n    for i in range(n):\n        s = s + str(i)\n    return s\n";
    assert!(has_builder_local(source));
    let program = crate::check_source(source, "test.py").expect("valid program");
    let function = program
        .functions
        .iter()
        .find(|function| function.name == "build")
        .expect("function build");
    let loop_index = function
        .body
        .iter()
        .position(|stmt| matches!(stmt.kind, StmtKind::ForRange { .. }))
        .expect("for loop");
    assert_eq!(builder_appends(loop_body(&function.body)), 1);
    assert_eq!(string_additions(loop_body(&function.body)), 0);
    assert!(is_finish(&function.body[loop_index + 1]));
    assert_eq!(function.ret_ty, Type::Str);
}

#[test]
fn multiple_accumulators_are_rewritten_independently() {
    let source = "first = \"\"\nsecond = \"\"\nfor i in range(3):\n    first += \"a\"\n    second += \"b\"\nprint(first, second)\n";
    let builders = all_locals(source)
        .iter()
        .filter(|local| local.name.contains("#builder"))
        .count();
    assert_eq!(builders, 2, "one hidden builder per accumulator");
    let body = module_body(source);
    let first_loop = loop_body(&body);
    assert_eq!(builder_appends(first_loop), 2);
    assert_eq!(string_additions(first_loop), 0);
    // Both results are materialised before the first read.
    assert_eq!(body.iter().filter(|stmt| is_finish(stmt)).count(), 2);
}

#[test]
fn aliasing_before_the_loop_keeps_concatenation() {
    // `t = s` escapes the seed, so the loop must keep immutable concatenation.
    let source = "s = \"\"\nt = s\nfor _ in range(3):\n    s += \"x\"\nprint(len(s), len(t))\n";
    assert!(!has_builder_local(source));
    let body = module_body(source);
    assert_eq!(string_additions(loop_body(&body)), 1);
    assert_eq!(builder_appends(loop_body(&body)), 0);
}

#[test]
fn conditional_append_keeps_concatenation() {
    let source =
        "s = \"\"\nfor i in range(4):\n    if i % 2 == 0:\n        s += \"x\"\nprint(len(s))\n";
    assert!(!has_builder_local(source));
    assert_eq!(builder_appends(loop_body(&module_body(source))), 0);
}

#[test]
fn reading_the_accumulator_inside_the_loop_keeps_concatenation() {
    let source = "s = \"\"\nwhile len(s) < 4:\n    s += \"x\"\nprint(len(s))\n";
    assert!(!has_builder_local(source));
    let body = module_body(source);
    assert_eq!(string_additions(loop_body(&body)), 1);
    assert_eq!(builder_appends(loop_body(&body)), 0);
}

#[test]
fn accumulator_without_a_read_after_the_loop_keeps_concatenation() {
    let source = "s = \"\"\nfor _ in range(3):\n    s += \"x\"\n";
    assert!(!has_builder_local(source));
    assert_eq!(builder_appends(loop_body(&module_body(source))), 0);
}

#[test]
fn module_global_read_by_a_function_keeps_concatenation() {
    // The per-iteration `GlobalSet` makes the accumulator observable as a
    // module global, so the pattern must fall back.
    let source = "s = \"\"\nfor _ in range(3):\n    s += \"x\"\ndef read():\n    return s\nprint(len(read()))\n";
    assert!(!has_builder_local(source));
    assert_eq!(builder_appends(loop_body(&module_body(source))), 0);
}
