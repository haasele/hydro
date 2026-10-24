use super::*;

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
    assert!(diags[0].message.contains("takes 1 positional argument(s)"));
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
        "math.sqrt() takes 1 positional argument(s) (0 given)"
    );

    let diags = check_diags("from functools import reduce\nreduce(lambda a, b: a)\n");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::CALL);
    assert_eq!(
        diags[0].message,
        "functools.reduce() takes 2 to 3 positional argument(s) (1 given)"
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
fn tuple_ordering_is_rejected() {
    let errors = check_errors("print((1, 2) < (3, 4))\n");
    assert_eq!(
        errors,
        vec!["unsupported operand type(s) for <: 'tuple' and 'tuple'"]
    );
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
    assert_eq!(
        check_errors("print(1 in [2] in [3])\n"),
        vec!["chained comparisons are not supported in Phase 1"]
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
        check_errors("a = 'x'.encode('utf-8', 'strict')\n"),
        vec!["str.encode() takes 0 to 1 positional argument(s) (2 given)"]
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

#[test]
fn asyncio_sleep_and_gather_typecheck() {
    let source = "import asyncio\nasync def f():\n    await asyncio.sleep(0.001)\n    values = asyncio.gather([1, 2])\n    return values\nf()\n";
    let program = crate::check_source(source, "test.py").expect("valid asyncio usage");
    let f = program_function(&program, "f");
    assert_eq!(f.ret_ty, Type::List);
}

#[test]
fn asyncio_unsupported_members_are_rejected() {
    let diags = check_diags("import asyncio\nasyncio.create_task(1)\n");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::ATTRIBUTE);
    assert_eq!(
        diags[0].message,
        "module 'asyncio' has no attribute 'create_task'"
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
