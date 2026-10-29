use super::*;
use crate::lower::HParam;
use hydro_ir::ExcKind;

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
fn complex_arithmetic_results_are_complex() {
    for (op, a, b) in [
        (BinOp::Add, Type::Complex, Type::Int),
        (BinOp::Add, Type::Int, Type::Complex),
        (BinOp::Sub, Type::Complex, Type::Float),
        (BinOp::Mul, Type::Float, Type::Complex),
        (BinOp::Div, Type::Complex, Type::Int),
        (BinOp::Div, Type::Int, Type::Complex),
        (BinOp::Pow, Type::Complex, Type::Int),
        (BinOp::Pow, Type::Int, Type::Complex),
        (BinOp::Add, Type::Complex, Type::Complex),
        (BinOp::Add, Type::Complex, Type::Bool),
        (BinOp::Add, Type::Complex, Type::BigInt),
    ] {
        assert_eq!(op_result(op, a, b), Some(Type::Complex), "{op:?} {a} {b}");
    }
}

#[test]
fn complex_rejects_unsupported_operators() {
    for op in [
        BinOp::FloorDiv,
        BinOp::Mod,
        BinOp::Lt,
        BinOp::Le,
        BinOp::Gt,
        BinOp::Ge,
    ] {
        assert_eq!(
            op_result(op, Type::Complex, Type::Int),
            None,
            "{op:?} complex int"
        );
        assert_eq!(
            op_result(op, Type::Float, Type::Complex),
            None,
            "{op:?} float complex"
        );
    }
    assert_eq!(
        op_result(BinOp::Mul, Type::Complex, Type::Str),
        None,
        "complex has no string repetition"
    );
    assert_eq!(
        op_result(BinOp::Add, Type::Complex, Type::Str),
        None,
        "complex has no string concatenation"
    );
}

#[test]
fn complex_variables_accept_real_arithmetic() {
    let program = crate::check_source(
        "z = 1j\nw = z + 1\nv = 1 - z\nu = z * 2\nt = z / 2\np = z ** 2\n",
        "test.py",
    )
    .expect("valid program");
    let module = &program.functions[0];
    let mut checked = 0;
    for stmt in &module.body {
        let hydro_ir::StmtKind::Assign { target, value } = &stmt.kind else {
            continue;
        };
        let name = module.locals[*target as usize].name.as_str();
        if matches!(name, "z" | "w" | "v" | "u" | "t" | "p") {
            assert_eq!(value.ty, Type::Complex, "{name}");
            checked += 1;
        }
    }
    assert_eq!(checked, 6, "every complex assignment is checked");
}

#[test]
fn complex_annotations_accept_real_numbers() {
    assert!(annotation_accepts(Type::Complex, Type::Int));
    assert!(annotation_accepts(Type::Complex, Type::Float));
    assert!(annotation_accepts(Type::Complex, Type::Bool));
    assert!(annotation_accepts(Type::Complex, Type::BigInt));
    assert!(!annotation_accepts(Type::Complex, Type::Str));
    crate::check_source("z: complex = 1j\n", "test.py").expect("literal annotation");
    crate::check_source(
        "def f(z: complex):\n    return z\nf(1)\nf(1.5)\nf(True)\nf(1j)\n",
        "test.py",
    )
    .expect("real numbers satisfy a complex parameter");
    let diags = check_diags("def f(z: complex):\n    return z\nf('x')\n");
    assert_eq!(diags[0].code, codes::TYPE, "{diags:?}");
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

    let hydro_ir::StmtKind::SetSlice {
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
    let hydro_ir::StmtKind::SetSlice {
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
        let hydro_ir::StmtKind::If { then_body, .. } = &stmt.kind else {
            continue;
        };
        for inner in then_body {
            if let hydro_ir::StmtKind::SetSubscript { value, .. } = &inner.kind {
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

/// A parameter of a synthetic signature.
fn signature_param(name: &str, kind: ParamKind, default: Option<Const>) -> HParam {
    HParam {
        local: 0,
        name: name.to_string(),
        ty: Ty::Concrete(Type::Any),
        declared: false,
        default,
        default_expr: None,
        kind,
    }
}

#[test]
fn user_call_arity_allows_omitted_default_arguments() {
    // Regression: `ntypes.obj2sctype(dtype)` forwards only the supplied
    // positional arguments, and `default=None` must not be required.
    let params = [
        signature_param("rep", ParamKind::Positional, None),
        signature_param("default", ParamKind::Positional, Some(Const::None)),
    ];
    let arity = CallArity::from_params("obj2sctype", &params, 0);
    assert_eq!(arity.positional_error(1), None);
    assert_eq!(
        arity.positional_error(0),
        Some("obj2sctype() missing required argument 'rep'".to_string())
    );
    assert_eq!(
        arity.positional_error(3),
        Some("obj2sctype() takes 2 positional argument(s) but 3 were given".to_string())
    );
}

#[test]
fn user_call_arity_excludes_closure_captures() {
    let params = [
        signature_param("captured", ParamKind::Positional, None),
        signature_param("value", ParamKind::Positional, None),
    ];
    let arity = CallArity::from_params("f", &params, 1);
    assert_eq!(arity.positional_error(1), None);
    assert_eq!(
        arity.positional_error(0),
        Some("f() missing required argument 'value'".to_string())
    );
}

#[test]
fn user_call_arity_accepts_varargs_and_positional_only_names() {
    let params = [
        signature_param("a", ParamKind::PosOnly, None),
        signature_param("rest", ParamKind::VarArg, None),
    ];
    let arity = CallArity::from_params("f", &params, 0);
    assert_eq!(arity.positional_error(1), None);
    assert_eq!(arity.positional_error(4), None);
    assert_eq!(
        arity.positional_error(0),
        Some("f() missing required positional argument 'a'".to_string())
    );
}

#[test]
fn user_call_arity_checks_keyword_only_parameters() {
    let params = [
        signature_param("a", ParamKind::Positional, None),
        signature_param("b", ParamKind::KwOnly, None),
        signature_param("c", ParamKind::KwOnly, Some(Const::Int(1))),
    ];
    let arity = CallArity::from_params("f", &params, 0);
    assert_eq!(arity.keyword_error(1, &["b"]), None);
    assert_eq!(arity.keyword_error(1, &["b", "c"]), None);
    assert_eq!(
        arity.keyword_error(1, &["c"]),
        Some("f() missing required keyword-only argument 'b'".to_string())
    );
    // A required positional parameter may also arrive as a keyword.
    assert_eq!(arity.keyword_error(0, &["a", "b"]), None);
    assert_eq!(
        arity.keyword_error(0, &["b"]),
        Some("f() missing required argument 'a'".to_string())
    );
}

#[test]
fn user_call_accepts_omitted_defaults_end_to_end() {
    assert!(check_errors("def f(a, b=None):\n    return a\nf(1)\n").is_empty());
    assert!(check_errors("def f(a, *args):\n    return a\nf(1, 2, 3)\n").is_empty());
    assert!(check_errors("def f(a, *, b=2):\n    return a\nf(1)\n").is_empty());
    assert!(check_errors("def f(a, *, b=2):\n    return a\nf(1, b=3)\n").is_empty());
}

#[test]
fn user_call_rejects_missing_required_arguments_end_to_end() {
    let errors = check_errors("def f(a, b=None):\n    return a\nf()\n");
    assert_eq!(
        errors,
        vec!["f() missing required argument 'a'"],
        "{errors:?}"
    );
    let errors = check_errors("def f(a, *, b):\n    return a\nf(1)\n");
    assert_eq!(
        errors,
        vec!["f() missing required keyword-only argument 'b'"],
        "{errors:?}"
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
fn missing_method_on_a_variable_receiver_is_left_to_the_runtime() {
    // Python resolves methods from the runtime class. A local can hold a
    // subclass, or a value an `isinstance` check narrowed at runtime while the
    // frontend keeps the assignment type (`missing_values or ()` stays a
    // tuple), so the lookup is dynamic and the result is `Any`.
    let cases = [
        ("xs = [1]\na = xs.items()\n", Type::Any),
        ("xs = [1]\na = xs.decode()\n", Type::Any),
        ("t = ()\na = t.decode()\n", Type::Any),
        ("d = {}\na = d.decode()\n", Type::Any),
        ("s = 'x'\na = s.nope()\n", Type::Any),
        ("blob = b'x'\na = blob.items()\n", Type::Any),
        ("s = 'abc'\na = s[0].nope()\n", Type::Any),
    ];
    for (source, expected) in cases {
        let program = crate::check_source(source, "test.py").expect("valid program");
        assert_eq!(assigned_type(&program, 1), expected, "{source}");
    }
}

#[test]
fn missing_method_on_a_literal_receiver_is_still_reported() {
    for (source, expected) in [
        ("print([1].nope())\n", "type 'list' has no method 'nope'"),
        ("print({}.decode())\n", "type 'dict' has no method 'decode'"),
        (
            "print((1, 2).nope())\n",
            "type 'tuple' has no method 'nope'",
        ),
        (
            "print(b'x'.items())\n",
            "type 'bytes' has no method 'items'",
        ),
        ("print('x'.nope())\n", "type 'str' has no method 'nope'"),
    ] {
        assert_eq!(check_errors(source), vec![expected], "{source}");
    }
}

#[test]
fn dict_update_accepts_an_iterable_of_pairs() {
    for source in [
        "d = {}\nd.update([('a', 1)])\n",
        "d = {}\nd.update([('a', 1), ('b', 2)])\n",
        "d = {}\nd.update((('a', 1),))\n",
        "d = {}\nd.update({'a': 1})\n",
        "d = {}\nd.update([])\n",
    ] {
        assert_eq!(check_errors(source), Vec::<String>::new(), "{source}");
    }

    assert_eq!(
        check_errors("d = {}\nd.update(1)\n"),
        vec![
            "dict.update() argument 1 has wrong type: \
             expected mapping or iterable of pairs, got int"
        ]
    );
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
fn join_accepts_lists_tuples_generators_and_other_iterables() {
    let cases = [
        ("','.join(['x', 'y'])", Type::Str),
        ("','.join(('x', 'y'))", Type::Str),
        ("','.join(ch for ch in ['x', 'y'])", Type::Str),
        ("','.join({'x', 'y'})", Type::Str),
        ("','.join({'x': 1})", Type::Str),
        ("','.join('xy')", Type::Str),
        ("b','.join((b'x', b'y'))", Type::Bytes),
    ];
    for (expr, expected) in cases {
        let source = format!("a = {expr}\n");
        let program = crate::check_source(&source, "test.py").expect("valid program");
        assert_eq!(assigned_type(&program, 0), expected, "{source}");
    }
}

#[test]
fn join_rejects_non_iterables() {
    let errors = check_errors("print(','.join(1))\n");
    assert_eq!(
        errors,
        vec!["str.join() argument 1 has wrong type: expected iterable, got int"]
    );

    let errors = check_errors("print(b','.join(1))\n");
    assert_eq!(
        errors,
        vec!["bytes.join() argument 1 has wrong type: expected iterable, got int"]
    );
}

#[test]
fn list_extend_accepts_any_iterable() {
    let cases = [
        "xs.extend([2, 3])",
        "xs.extend((2, 3))",
        "xs.extend({2, 3})",
        "xs.extend({'a': 1})",
        "xs.extend('ab')",
        "xs.extend(b'ab')",
        "xs.extend(range(3))",
        "xs.extend(n for n in [1, 2])",
    ];
    for call in cases {
        let source = format!("xs = [1]\na = {call}\n");
        let program = crate::check_source(&source, "test.py").expect("valid program");
        assert_eq!(assigned_type(&program, 1), Type::None, "{source}");
    }
}

#[test]
fn list_extend_rejects_non_iterables() {
    assert_eq!(
        check_errors("xs = [1]\nxs.extend(1)\n"),
        vec!["list.extend() argument 1 has wrong type: expected iterable, got int"]
    );
    assert_eq!(
        check_errors("xs = [1]\nxs.extend(1.5)\n"),
        vec!["list.extend() argument 1 has wrong type: expected iterable, got float"]
    );
}

#[test]
fn padding_method_signatures_are_registered() {
    let source = "a = 'x'.rjust(3)\n\
                  b = 'x'.ljust(3, '-')\n\
                  c = 'x'.center(3, '-')\n\
                  d = 'x'.zfill(3)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in 0..4 {
        assert_eq!(assigned_type(&program, index), Type::Str, "index {index}");
    }

    let errors = check_errors("print('x'.rjust())\n");
    assert_eq!(
        errors,
        vec!["str.rjust() takes 1 to 2 positional argument(s) (0 given)"]
    );

    let errors = check_errors("print('x'.center(1, '-', '-'))\n");
    assert_eq!(
        errors,
        vec!["str.center() takes 1 to 2 positional argument(s) (3 given)"]
    );

    let errors = check_errors("print('x'.zfill('3'))\n");
    assert_eq!(
        errors,
        vec!["str.zfill() argument 1 has wrong type: expected int, got str"]
    );

    let errors = check_errors("print('x'.rjust(3, 0))\n");
    assert_eq!(
        errors,
        vec!["str.rjust() argument 2 has wrong type: expected str, got int"]
    );
}

#[test]
fn strip_method_signatures_are_registered() {
    // The optional argument is a character set: a `str` or `None`.
    let source = "a = 'x.y'.strip('.')\n\
                  b = 'x.y'.lstrip(None)\n\
                  c = 'x.y'.rstrip()\n\
                  d = 'x.y'.rstrip('').lstrip(None).strip('.')\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in 0..4 {
        assert_eq!(assigned_type(&program, index), Type::Str, "index {index}");
    }

    let errors = check_errors("print('x'.strip('.', '.'))\n");
    assert_eq!(
        errors,
        vec!["str.strip() takes 0 to 1 positional argument(s) (2 given)"]
    );

    let errors = check_errors("print('x'.lstrip(1))\n");
    assert_eq!(
        errors,
        vec!["str.lstrip() argument 1 has wrong type: expected str or None, got int"]
    );

    let errors = check_errors("print('x'.rstrip(b'.'))\n");
    assert_eq!(
        errors,
        vec!["str.rstrip() argument 1 has wrong type: expected str or None, got bytes"]
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
    let hydro_ir::StmtKind::Assign { value, .. } = &program.functions[0].body[0].kind else {
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
        hydro_ir::StmtKind::Assign { value, .. } => value.ty,
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
    let hydro_ir::StmtKind::Assign { value, .. } = &program.functions[0].body[0].kind else {
        panic!("expected an assignment");
    };
    assert!(
        matches!(&value.kind, ExprKind::ModuleCall { module, function, .. }
                if module == "math" && function == "sqrt"),
        "{:?}",
        value.kind
    );
    assert_eq!(value.ty, Type::Float);
    let hydro_ir::StmtKind::Assign { value, .. } = &program.functions[0].body[1].kind else {
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
fn dict_view_methods_have_dynamic_runtime_types() {
    // `keys()`/`values()`/`items()` are live views, not lists: their members
    // may be used as sets (`need <= d.keys()`, `d.keys() - need`), which the
    // static list type would reject.
    let source = "d = {'a': 1}\nks = d.keys()\nvs = d.values()\nits = d.items()\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 1), Type::Any);
    assert_eq!(assigned_type(&program, 2), Type::Any);
    assert_eq!(assigned_type(&program, 3), Type::Any);
}

#[test]
fn dict_keys_view_supports_set_operations() {
    let source =
        "d = {'a': 1}\nneed = {'a'}\nsubset = need <= d.keys()\nexcess = d.keys() - need\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 2), Type::Bool);
    assert_eq!(assigned_type(&program, 3), Type::Any);
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
    let hydro_ir::StmtKind::Unpack { targets, .. } = &program.functions[0].body[4].kind else {
        panic!("expected an unpack statement");
    };
    match &targets[0] {
        hydro_ir::UnpackTarget::Attr { name, .. } => assert_eq!(name, "x"),
        other => panic!("expected an attribute target, got {other:?}"),
    }
    let hydro_ir::UnpackTarget::Nested(inner) = &targets[1] else {
        panic!("expected a nested target, got {:?}", targets[1]);
    };
    assert!(matches!(inner[0], hydro_ir::UnpackTarget::Name(_)));
    assert!(matches!(inner[1], hydro_ir::UnpackTarget::Subscript { .. }));
}

#[test]
fn for_unpack_target_emits_unpack_statement() {
    let source = "xs = [(1, 2)]\nfor a, b in xs:\n    print(a, b)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    let hydro_ir::StmtKind::ForEach { var, body, .. } = &program.functions[0].body[1].kind else {
        panic!("expected a foreach loop");
    };
    let hydro_ir::StmtKind::Unpack { targets, value } = &body[0].kind else {
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
    let hydro_ir::StmtKind::Assign { value, .. } = &program.functions[0].body[0].kind else {
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
    let source = "a = set()\nb = set([1, 2])\nc = set((1, 2))\nd = set({1})\ne = set('ab')\nf = set({'a': 1})\ng = set(range(3))\nh = set(r for r in range(3))\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in 0..8 {
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
    let hydro_ir::StmtKind::Assign { value, .. } = &after.kind else {
        panic!("expected the final assignment");
    };
    assert!(
        matches!(&value.kind, hydro_ir::ExprKind::Local(_)),
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
    let source = "s = {1}\na = s.add(2)\nb = s.remove(1)\nc = s.discard(9)\nd = s.clear()\ne = s.copy()\nf = s.union([2, 3], (4,), {5})\ng = s.union((2,))\nh = s.union({2})\ni = s.update([2])\nj = s.pop()\nk = s.intersection([1], {2})\nl = s.difference([1])\nm = s.symmetric_difference([1])\nn = s.intersection_update([1])\no = s.difference_update([1])\np = s.symmetric_difference_update([1])\nq = s.issubset([1])\nr = s.issuperset([1])\nt = s.isdisjoint([2])\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in [1, 2, 3, 4, 9, 14, 15, 16] {
        assert_eq!(assigned_type(&program, index), Type::None);
    }
    for index in [5, 6, 7, 8, 11, 12, 13] {
        assert_eq!(assigned_type(&program, index), Type::Set);
    }
    assert_eq!(assigned_type(&program, 10), Type::Any);
    for index in [17, 18, 19] {
        assert_eq!(assigned_type(&program, index), Type::Bool);
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
        vec!["set.union() argument 1 has wrong type: expected iterable, got int"]
    );
    assert_eq!(
        check_errors("s = {1}\ns.symmetric_difference()\n"),
        vec!["set.symmetric_difference() takes 1 positional argument(s) (0 given)"]
    );
    assert_eq!(
        check_errors("s = {1}\ns.add()\n"),
        vec!["set.add() takes 1 positional argument(s) (0 given)"]
    );
}

#[test]
fn set_comparisons_return_bool() {
    let source = "a = {1} < {1, 2}\nb = {1} <= {1}\nc = {1} > {2}\nd = {1} >= {1}\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in 0..4 {
        assert_eq!(assigned_type(&program, index), Type::Bool);
    }
    assert_eq!(op_result(BinOp::Lt, Type::Set, Type::Set), Some(Type::Bool));
    assert_eq!(op_result(BinOp::Lt, Type::Set, Type::Int), None);
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
        hydro_ir::StmtKind::ForEach { .. }
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
        hydro_ir::StmtKind::ForEach { .. }
    ));
}

#[test]
fn for_range_stays_a_range_loop() {
    let source = "for i in range(3):\n    print(i)\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert!(matches!(
        &program.functions[0].body[0].kind,
        hydro_ir::StmtKind::ForRange { .. }
    ));
    let var = match &program.functions[0].body[0].kind {
        hydro_ir::StmtKind::ForRange { var, .. } => *var,
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
            hydro_ir::StmtKind::ForEach { else_body, .. } => {
                assert_eq!(else_body.len(), 1, "for-each else must reach the IR");
                loops += 1;
            }
            hydro_ir::StmtKind::ForRange { else_body, .. } => {
                assert_eq!(else_body.len(), 1, "for-range else must reach the IR");
                loops += 1;
            }
            hydro_ir::StmtKind::While {
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
fn int_and_float_accept_any_object_argument() {
    // CPython dispatches to `__int__`/`__index__`/`__float__`, so any object
    // (including a tuple subclass) may be convertible; a concrete argument
    // type is not a static error, only a possible runtime failure.
    let source = "xs = (1, 2)\na = int(xs)\nb = int([1, 2])\nc = float(xs)\nd = float('1.5')\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 1), Type::Int);
    assert_eq!(assigned_type(&program, 2), Type::Int);
    assert_eq!(assigned_type(&program, 3), Type::Float);
    assert_eq!(assigned_type(&program, 4), Type::Float);
    assert!(check_errors("def f(x):\n    return int(x)\nprint(f({'a': 1}))\n").is_empty());
}

#[test]
fn int_with_explicit_base_requires_a_string() {
    let program = crate::check_source("x = int('10', 2)\n", "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Int);
    assert!(check_errors("x = int(b'10', 2)\n").is_empty());

    // `int(number, base)` is a TypeError in CPython: the base only applies to
    // string-like values.
    assert_eq!(
        check_errors("x = int(10, 2)\n"),
        vec!["int() can't convert non-string with explicit base"]
    );
    assert_eq!(
        check_errors("x = int([1], 2)\n"),
        vec!["int() can't convert non-string with explicit base"]
    );
    // An unknown argument keeps type-checking; the runtime decides.
    assert!(check_errors("def f(x):\n    return int(x, 2)\nprint(f('10'))\n").is_empty());
}

#[test]
fn builtin_arity_diagnostics() {
    assert_eq!(
        check_errors("x = int(1, 2, 3)\n"),
        vec!["int() takes 0 to 2 argument(s) (3 given)"]
    );
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
        vec!["iter() takes 1 or 2 positional arguments (0 given)"]
    );
    assert_eq!(
        check_errors("x = iter([1], [2], [3])\n"),
        vec!["iter() takes 1 or 2 positional arguments (3 given)"]
    );
    // `iter(callable, sentinel)` is accepted; the callable check (and the
    // sentinel comparison) happen at runtime.
    assert!(check_errors("def f():\n    return b''\nx = iter(f, b'')\n").is_empty());
    assert!(check_errors("x = iter([1], [2])\n").is_empty());
    let program = crate::check_source("x = iter(input, b'')\n", "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 0), Type::Any);
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
fn range_subscript_yields_int_and_slicing_yields_range() {
    let source = "r = range(3)\na = r[0]\nb = r[-1]\nc = r[1]\nd = r[0:2]\ne = r[::2]\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    assert_eq!(assigned_type(&program, 1), Type::Int);
    assert_eq!(assigned_type(&program, 2), Type::Int);
    assert_eq!(assigned_type(&program, 3), Type::Int);
    assert_eq!(assigned_type(&program, 4), Type::Range);
    assert_eq!(assigned_type(&program, 5), Type::Range);
}

#[test]
fn range_index_must_be_int() {
    assert_eq!(
        check_errors("r = range(3)\nx = r['a']\n"),
        vec!["range index must be int, not str"]
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
fn generic_builtin_subscripts_typecheck_dynamically() {
    // The generic builtin classes take type parameters in annotations. Hydro
    // has no generic substitution, so the alias value is dynamic; the operand
    // types are unconstrained (`...` is the variadic parameter).
    let source = "a = tuple[int, ...]\n\
                  b = list[str]\n\
                  c = dict[str, int]\n\
                  d = type[int]\n\
                  e = list[tuple[int, str]]\n";
    let program = crate::check_source(source, "test.py").expect("valid program");
    for index in 0..5 {
        assert_eq!(assigned_type(&program, index), Type::Any);
    }
    // A non-generic builtin class value keeps rejecting the subscription.
    assert_eq!(
        check_errors("x = print[int]\n"),
        vec!["'function' object is not subscriptable"]
    );
    // The reported failure: a PEP 695 alias whose value subscripts a generic
    // builtin class.
    crate::check_source("type Shape = tuple[int, ...]\n", "test.py")
        .expect("generic alias value typechecks");
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
    let hydro_ir::StmtKind::ForEach { var, body, .. } = &body[0].kind else {
        panic!("expected a for-each loop");
    };
    assert_eq!(program.functions[0].locals[*var as usize].ty, Type::Int);
    let hydro_ir::StmtKind::Assign { value, .. } = &body[0].kind else {
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
        check_errors("a = bytes(1, 2, 3, 4)\n"),
        vec!["bytes() takes 0 to 3 positional arguments (4 given)"]
    );
    assert!(check_errors("a = bytes('x', 'ascii', 'replace')\n").is_empty());
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
    let hydro_ir::StmtKind::ClassDef {
        target,
        name,
        bases,
        methods,
        ..
    } = &body[0].kind
    else {
        panic!("expected a class statement, got {:?}", body[0].kind);
    };
    assert_eq!(name, "C");
    assert_eq!(program.functions[0].locals[*target as usize].name, "C");
    assert!(bases.is_empty());
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
    let StmtKind::ClassDef { bases, .. } = &body[1].kind else {
        panic!("expected class D with a base, got {:?}", body[1].kind);
    };
    let base = bases.first().expect("expected one base");
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
    let StmtKind::ClassDef { bases, .. } = &body[1].kind else {
        panic!("expected class E with a base, got {:?}", body[1].kind);
    };
    let base = bases.first().expect("expected one base");
    assert!(
        matches!(&base.kind, ExprKind::ExceptionClass(ExcKind::Exception)),
        "base must be the Exception class: {:?}",
        base.kind
    );
    // `class F(E)` keeps referring to the class local created by `class E`.
    let StmtKind::ClassDef { bases, .. } = &body[2].kind else {
        panic!("expected class F with a base, got {:?}", body[2].kind);
    };
    let base = bases.first().expect("expected one base");
    assert!(
        matches!(&base.kind, ExprKind::Local(_)),
        "base must reference the E local: {:?}",
        base.kind
    );
}

fn program_function<'a>(program: &'a hydro_ir::Program, name: &str) -> &'a hydro_ir::Function {
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
        .filter(|stmt| matches!(&stmt.kind, hydro_ir::StmtKind::Yield { .. }))
        .count();
    assert_eq!(yields, 2, "{:?}", g.body);
    assert!(
        !g.body
            .iter()
            .any(|stmt| matches!(&stmt.kind, hydro_ir::StmtKind::Return(Some(_)))),
        "no materialised list return: {:?}",
        g.body
    );

    let module = program.main_function();
    let hydro_ir::StmtKind::Assign { value, .. } = &module.body[0].kind else {
        panic!("expected an assignment, got {:?}", module.body[0].kind);
    };
    let hydro_ir::ExprKind::Call { args, .. } = &value.kind else {
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
    assert!(matches!(
        &f.body[0].kind,
        hydro_ir::StmtKind::Return(Some(_))
    ));

    let module = program.main_function();
    let assign = module
        .body
        .iter()
        .find(|stmt| matches!(&stmt.kind, hydro_ir::StmtKind::Assign { .. }))
        .expect("x = asyncio.run(...)");
    let hydro_ir::StmtKind::Assign { value, .. } = &assign.kind else {
        unreachable!("filtered above");
    };
    let hydro_ir::ExprKind::ModuleCall { function, args, .. } = &value.kind else {
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
    let hydro_ir::StmtKind::Return(Some(value)) = &outer.body[0].kind else {
        panic!("expected a return statement, got {:?}", outer.body[0].kind);
    };
    let hydro_ir::ExprKind::Await { value: awaited } = &value.kind else {
        panic!("expected an await expression, got {:?}", value.kind);
    };
    assert!(matches!(&awaited.kind, hydro_ir::ExprKind::Call { .. }));
    assert_eq!(awaited.ty, Type::Any, "inner() is a coroutine object");
    assert_eq!(value.ty, Type::Any, "await result is dynamic");
}

#[test]
fn async_with_and_async_for_typecheck() {
    let source = "async def read():\n    lines = []\n    async with open('x.txt') as fh:\n        async for line in fh:\n            lines.append(line)\n    return lines\nread()\n";
    crate::check_source(source, "test.py").expect("valid async with/for");
}

/// True when `stmts` or any statement nested inside them matches `pred`.
fn ir_stmt_any(stmts: &[hydro_ir::Stmt], pred: &dyn Fn(&hydro_ir::StmtKind) -> bool) -> bool {
    stmts.iter().any(|stmt| {
        if pred(&stmt.kind) {
            return true;
        }
        match &stmt.kind {
            hydro_ir::StmtKind::If {
                then_body,
                else_body,
                ..
            } => ir_stmt_any(then_body, pred) || ir_stmt_any(else_body, pred),
            hydro_ir::StmtKind::While {
                body, else_body, ..
            }
            | hydro_ir::StmtKind::ForRange {
                body, else_body, ..
            }
            | hydro_ir::StmtKind::ForEach {
                body, else_body, ..
            } => ir_stmt_any(body, pred) || ir_stmt_any(else_body, pred),
            hydro_ir::StmtKind::Try {
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

fn ir_catches_stop_async_iteration(kind: &hydro_ir::StmtKind) -> bool {
    matches!(
        kind,
        hydro_ir::StmtKind::Try { handlers, .. }
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
    assert!(matches!(&g.body[0].kind, hydro_ir::StmtKind::Yield { .. }));
}

#[test]
fn async_for_emits_await_anext_and_stop_async_iteration() {
    let source = "async def f(it):\n    async for x in it:\n        print(x)\n";
    let program = crate::check_source(source, "test.py").expect("valid async for");
    let f = program_function(&program, "f");

    // The desugaring is wrapped in an always-true block.
    let hydro_ir::StmtKind::If { then_body, .. } = &f.body[0].kind else {
        panic!("expected the async-for block, got {:?}", f.body[0].kind);
    };
    let hydro_ir::StmtKind::While {
        body, else_body, ..
    } = &then_body[1].kind
    else {
        panic!("expected the iteration loop, got {:?}", then_body[1].kind);
    };
    assert!(else_body.is_empty());

    let hydro_ir::StmtKind::Try {
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
    assert!(matches!(
        &handlers[0].body[0].kind,
        hydro_ir::StmtKind::Break
    ));

    let hydro_ir::StmtKind::Assign { value, .. } = &fetch[0].kind else {
        panic!("expected the awaited item, got {:?}", fetch[0].kind);
    };
    let hydro_ir::ExprKind::Await { value } = &value.kind else {
        panic!("expected await, got {:?}", value.kind);
    };
    let hydro_ir::ExprKind::MethodCall { method, .. } = &value.kind else {
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
    let hydro_ir::StmtKind::If { then_body, .. } = &f.body[0].kind else {
        panic!("expected the async-for block");
    };
    // [$aloop = False, $aiter = ..., while, if $aloop: else]
    assert_eq!(then_body.len(), 4);
    let hydro_ir::StmtKind::If {
        then_body: else_body,
        ..
    } = &then_body[3].kind
    else {
        panic!("expected the else guard");
    };
    assert_eq!(else_body.len(), 1, "the else body runs behind the guard");
    let hydro_ir::StmtKind::While { body, .. } = &then_body[2].kind else {
        panic!("expected the loop");
    };
    let hydro_ir::StmtKind::Try { handlers, .. } = &body[0].kind else {
        panic!("expected the __anext__ try");
    };
    assert_eq!(handlers[0].exc, Some(ExcKind::StopAsyncIteration));
    assert!(matches!(
        &handlers[0].body[0].kind,
        hydro_ir::StmtKind::Assign { .. }
    ));
    assert!(matches!(
        &handlers[0].body[1].kind,
        hydro_ir::StmtKind::Break
    ));
}

#[test]
fn async_with_awaits_aenter_and_aexit() {
    let source = "async def f(cm):\n    async with cm as value:\n        print(value)\n";
    let program = crate::check_source(source, "test.py").expect("valid async with");
    let f = program_function(&program, "f");
    let hydro_ir::StmtKind::If { then_body, .. } = &f.body[0].kind else {
        panic!("expected the async-with block, got {:?}", f.body[0].kind);
    };
    assert_eq!(then_body.len(), 4, "manager, exception slot, entry, try");

    let hydro_ir::StmtKind::Assign { value, .. } = &then_body[2].kind else {
        panic!("expected the entry binding, got {:?}", then_body[2].kind);
    };
    let hydro_ir::ExprKind::Await { value } = &value.kind else {
        panic!("expected `await __aenter__()`");
    };
    let hydro_ir::ExprKind::MethodCall { method, .. } = &value.kind else {
        panic!("expected a method call");
    };
    assert_eq!(method, "__aenter__");

    let hydro_ir::StmtKind::Try {
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
    let hydro_ir::StmtKind::If {
        cond, then_body, ..
    } = &handlers[0].body[0].kind
    else {
        panic!("expected the suppression check");
    };
    let hydro_ir::ExprKind::Un { op, operand } = &cond.kind else {
        panic!("expected `not await __aexit__(...)`, got {:?}", cond.kind);
    };
    assert_eq!(*op, hydro_ir::UnOp::Not);
    let hydro_ir::ExprKind::Await { value } = &operand.kind else {
        panic!("expected an await");
    };
    let hydro_ir::ExprKind::MethodCall { method, args, .. } = &value.kind else {
        panic!("expected a method call");
    };
    assert_eq!(method, "__aexit__");
    assert_eq!(args.len(), 3);
    assert!(matches!(
        &args[0].kind,
        hydro_ir::ExprKind::Call {
            callee: hydro_ir::Callee::Builtin(hydro_ir::Builtin::Type),
            ..
        }
    ));
    assert_eq!(args[2].ty, Type::Any, "Hydro passes None as the traceback");
    assert!(matches!(
        &then_body[0].kind,
        hydro_ir::StmtKind::Raise { exc: None, .. }
    ));

    // The finally arm calls `__aexit__(None, None, None)` on a normal exit.
    assert_eq!(finally_body.len(), 1);
    let hydro_ir::StmtKind::If { then_body, .. } = &finally_body[0].kind else {
        panic!("expected the normal-exit guard");
    };
    let hydro_ir::StmtKind::Expr(exit) = &then_body[0].kind else {
        panic!("expected the normal exit call");
    };
    let hydro_ir::ExprKind::Await { value } = &exit.kind else {
        panic!("expected an await");
    };
    let hydro_ir::ExprKind::MethodCall { method, args, .. } = &value.kind else {
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
    let hydro_ir::StmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!("expected the returned comprehension");
    };
    assert_eq!(value.ty, Type::List);
    let hydro_ir::ExprKind::Await { value } = &value.kind else {
        panic!("expected the awaited helper, got {:?}", value.kind);
    };
    let hydro_ir::ExprKind::Call {
        callee: hydro_ir::Callee::Function(id),
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
        Some(hydro_ir::StmtKind::Assign { .. })
    ));
    assert!(matches!(
        helper.body.last().map(|stmt| &stmt.kind),
        Some(hydro_ir::StmtKind::Return(Some(_)))
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
    let hydro_ir::StmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!("expected the returned generator expression");
    };
    let hydro_ir::ExprKind::Call {
        callee: hydro_ir::Callee::Function(id),
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
        hydro_ir::StmtKind::Yield { .. }
    )));
    assert!(ir_stmt_any(&helper.body, &ir_catches_stop_async_iteration));
}

#[test]
fn generator_expression_with_await_is_an_async_generator() {
    let source =
        "import asyncio\nasync def f(xs):\n    return (await asyncio.sleep(0) for x in xs)\n";
    let program = crate::check_source(source, "test.py").expect("valid async genexp");
    let f = program_function(&program, "f");
    let hydro_ir::StmtKind::Return(Some(value)) = &f.body[0].kind else {
        panic!("expected the returned generator expression");
    };
    let hydro_ir::ExprKind::Call {
        callee: hydro_ir::Callee::Function(id),
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
        hydro_ir::StmtKind::Yield { value: Some(value) }
            if matches!(value.kind, hydro_ir::ExprKind::Await { .. })
    )));
}

#[test]
fn stop_async_iteration_is_a_catchable_exception() {
    let source = "def f():\n    try:\n        pass\n    except StopAsyncIteration:\n        pass\n";
    let program = crate::check_source(source, "test.py").expect("catchable StopAsyncIteration");
    let f = program_function(&program, "f");
    let hydro_ir::StmtKind::Try { handlers, .. } = &f.body[0].kind else {
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
        kind: HExprKind::Const(hydro_ir::Const::Int(2)),
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
    assert_eq!(*exc, Some(hydro_ir::ExcKind::RuntimeError));
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
    assert_eq!(handlers[0].exc, Some(hydro_ir::ExcKind::SystemExit));
    assert_eq!(handlers[1].exc, Some(hydro_ir::ExcKind::NameError));
}

#[test]
fn input_and_get_ipython_calls_are_typed() {
    let source = "def f():\n    a = input('> ')\n    return get_ipython()\n";
    let program = crate::check_source(source, "test.py").expect("valid probes");
    let f = program_function(&program, "f");
    let StmtKind::Assign { value, .. } = &f.body[0].kind else {
        panic!("expected an assignment, got {:?}", f.body[0].kind);
    };
    assert_eq!(value.ty, hydro_ir::Type::Str);
    let StmtKind::Return(Some(value)) = &f.body[1].kind else {
        panic!("expected a return, got {:?}", f.body[1].kind);
    };
    assert_eq!(value.ty, hydro_ir::Type::Any);
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
    assert_eq!(*exc, Some(hydro_ir::ExcKind::ValueError));
    assert!(matches!(
        &cause.kind,
        ExprKind::Const(hydro_ir::Const::None)
    ));
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
            Some(ExprKind::Const(hydro_ir::Const::Str(text))) if text == "value is required"
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
    assert_eq!(handlers[0].exc, Some(hydro_ir::ExcKind::TypeError));
    assert_eq!(handlers[1].exc, Some(hydro_ir::ExcKind::ValueError));
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
    let kinds: Vec<Option<hydro_ir::ExcKind>> = handlers.iter().map(|h| h.exc).collect();
    assert_eq!(
        kinds,
        vec![
            Some(hydro_ir::ExcKind::Exception),
            Some(hydro_ir::ExcKind::BaseException)
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
fn module_body(source: &str) -> Vec<hydro_ir::Stmt> {
    let program = crate::check_source(source, "test.py").expect("valid program");
    program.functions[0].body.clone()
}

/// Body of the first top-level loop of `stmts`.
fn loop_body(stmts: &[hydro_ir::Stmt]) -> &[hydro_ir::Stmt] {
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

fn builder_appends(stmts: &[hydro_ir::Stmt]) -> usize {
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

fn is_finish(stmt: &hydro_ir::Stmt) -> bool {
    matches!(
        &stmt.kind,
        StmtKind::Assign { value, .. }
            if matches!(&value.kind, ExprKind::StrBuilderFinish { .. })
    )
}

fn is_builder_new(stmt: &hydro_ir::Stmt) -> bool {
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
fn string_additions(stmts: &[hydro_ir::Stmt]) -> usize {
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

fn all_locals(source: &str) -> Vec<hydro_ir::LocalInfo> {
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
                    if matches!(&value.kind, ExprKind::Const(hydro_ir::Const::Str(text)) if text.is_empty())
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

#[test]
fn optional_local_indexing_is_dynamic() {
    // `arg` starts as `None` and later receives a still-unresolved value.
    // The slot must widen instead of pinning the value to `None`, so the
    // tuple index stays valid (`dis.ArgResolver.get_argval_argrepr`).
    let source =
        "def lookup(value):\n    arg = None\n    arg = value\n    return (None, str, repr, ascii)[arg]\n";
    crate::check_source(source, "test.py").expect("optional local indexing stays dynamic");
}

#[test]
fn optional_local_floordiv_is_dynamic() {
    // `arg` is `None` on one path and an int on another (`dis._unpack_opargs`).
    let source = "def halve(value):\n    arg = None\n    arg = value\n    return arg // 2\n";
    crate::check_source(source, "test.py").expect("optional local // stays dynamic");
}

#[test]
fn optional_dict_receiver_keeps_method_calls_dynamic() {
    // An omitted `None` default must not type `sink`'s parameter. The same
    // unresolved argument is also checked as a dict in `helper`; pinning it
    // to `None` would report a bogus `.values()` error
    // (`dis._get_lineno_width`).
    let source = "def sink(x=None):\n    return x\n\n\
                  def early():\n    sink()\n\n\
                  def caller():\n    ls = make()\n    helper(ls)\n    sink(ls)\n\n\
                  def helper(ls):\n    return ls.values()\n\n\
                  def make():\n    return {}\n";
    crate::check_source(source, "test.py").expect("optional dict receiver stays dynamic");
}

#[test]
fn disagreeing_call_sites_make_a_parameter_dynamic() {
    let source = "def f(x):\n    return x // 2\n\n\
                  def a():\n    return f('s')\n\n\
                  def b():\n    return f(1)\n";
    crate::check_source(source, "test.py").expect("disagreeing call sites widen the parameter");
}

#[test]
fn builtin_class_methods_have_static_results() {
    let source = "a = int.from_bytes(b'\\x01', 'little')\n\
                  b = int.to_bytes(1, 2, 'little')\n\
                  c = int.bit_length(7)\n\
                  d = str.maketrans('a', 'b')\n\
                  e = bytes.fromhex('00')\n\
                  f = bytearray.fromhex('00')\n\
                  g = dict.fromkeys([1, 2])\n\
                  h = float.fromhex('1.0')\n\
                  i = list.not_a_real_class_method()\n";
    let program = crate::check_source(source, "test.py").expect("builtin class methods compile");
    assert_eq!(assigned_type(&program, 0), Type::Int);
    assert_eq!(assigned_type(&program, 1), Type::Bytes);
    assert_eq!(assigned_type(&program, 2), Type::Int);
    assert_eq!(assigned_type(&program, 3), Type::Dict);
    assert_eq!(assigned_type(&program, 4), Type::Bytes);
    assert_eq!(assigned_type(&program, 5), Type::Bytes);
    assert_eq!(assigned_type(&program, 6), Type::Dict);
    assert_eq!(assigned_type(&program, 7), Type::Float);
    assert_eq!(
        assigned_type(&program, 8),
        Type::Any,
        "unknown class-level methods dispatch dynamically"
    );
}

#[test]
fn type_class_methods_have_static_results() {
    // The `type` metaclass exposes C-level type operations that typing stubs
    // and metaclasses call directly (`type.__subclasscheck__(cls, other)`,
    // `type.__new__(mcls, name, bases, namespace)`).
    let source = "a = type.__subclasscheck__(int, bool)\n\
                  b = type.__instancecheck__(1, int)\n\
                  c = type.__hash__(int)\n\
                  d = type.mro(int)\n\
                  e = type.__new__(type, 'C', (), {})\n\
                  f = type.__repr__(int)\n\
                  g = type.__prepare__('C', ())\n\
                  h = type.not_a_real_class_method()\n";
    let program = crate::check_source(source, "test.py").expect("type class methods compile");
    assert_eq!(assigned_type(&program, 0), Type::Bool);
    assert_eq!(assigned_type(&program, 1), Type::Bool);
    assert_eq!(assigned_type(&program, 2), Type::Int);
    assert_eq!(assigned_type(&program, 3), Type::List);
    assert_eq!(assigned_type(&program, 4), Type::Any);
    assert_eq!(assigned_type(&program, 5), Type::Str);
    assert_eq!(assigned_type(&program, 6), Type::Dict);
    assert_eq!(
        assigned_type(&program, 7),
        Type::Any,
        "unknown type methods dispatch dynamically"
    );
}

#[test]
fn literal_expression_errors_survive_dynamic_widening() {
    assert_eq!(
        check_errors("1 + \"x\"\n"),
        vec!["unsupported operand type(s) for +: 'int' and 'str'"]
    );
    assert_eq!(
        check_errors("[1][\"x\"]\n"),
        vec!["list index must be int, not str"]
    );
}

#[test]
fn percent_formatting_accepts_a_dynamic_operand() {
    // CPython `tokenize.py:523` is `perror("error: %s" % message)`: the
    // untyped parameter is gradual, so the format string alone decides the
    // result type and the runtime parses the format.
    assert!(check_errors("def error(message):\n    print('%s' % message)\n").is_empty());
    assert!(
        check_errors("def error(message):\n    print('%s: error: %s' % (message, message))\n")
            .is_empty()
    );
    assert!(check_errors("def f(message: Any):\n    print('%s' % message)\n").is_empty());
    assert_eq!(op_result(BinOp::Mod, Type::Str, Type::Any), Some(Type::Str));
    assert_eq!(op_result(BinOp::Mod, Type::Any, Type::Str), Some(Type::Any));
}

#[test]
fn percent_formatting_result_keeps_the_format_string_type() {
    let program = crate::check_source(
        "def error(message):\n    return '%s' % message\n",
        "test.py",
    )
    .expect("valid program");
    let function = program
        .functions
        .iter()
        .find(|f| f.name == "error")
        .expect("function error");
    assert_eq!(function.ret_ty, Type::Str);
}

#[test]
fn percent_with_an_unknown_right_operand_does_not_report_unsupported() {
    let prog = empty_prog();
    let mut inf = Infer::new();
    let mut ck = checker(&prog, &mut inf);
    let value = ck.inf.fresh();
    let out = ck.inf.fresh();
    let op = Op {
        kind: OpKind::Bin(BinOp::Mod),
        a: Ty::Concrete(Type::Str),
        b: value,
        out,
        line: 4,
        col: 9,
        done: false,
    };
    ck.ops.push(op);
    assert!(ck.resolve_binary(0, op, BinOp::Mod));
    assert!(ck.diags.is_empty());
    assert_eq!(ck.inf.resolve(out), Ty::Concrete(Type::Str));
    // The argument stays gradual; nothing pins it to a concrete type.
    assert!(matches!(ck.inf.resolve(value), Ty::Var(_)));
}

#[test]
fn percent_with_an_unknown_left_operand_is_dynamic() {
    let prog = empty_prog();
    let mut inf = Infer::new();
    let mut ck = checker(&prog, &mut inf);
    let value = ck.inf.fresh();
    let out = ck.inf.fresh();
    let op = Op {
        kind: OpKind::Bin(BinOp::Mod),
        a: value,
        b: Ty::Concrete(Type::Str),
        out,
        line: 6,
        col: 3,
        done: false,
    };
    ck.ops.push(op);
    assert!(ck.resolve_binary(0, op, BinOp::Mod));
    assert!(ck.diags.is_empty());
    assert_eq!(ck.inf.resolve(out), Ty::Concrete(Type::Any));
}

#[test]
fn any_operand_is_never_an_unsupported_operand() {
    for source in [
        "def f(x: Any):\n    return x + 1\n",
        "def f(x: Any):\n    return 1 + x\n",
        "def f(x: Any):\n    return [1] - x\n",
        "def f(x: Any):\n    return x % 'a'\n",
        "def f(x: Any):\n    return x < 1\n",
        "def f(x: Any):\n    return -x\n",
        "def f(x: Any):\n    return ~x\n",
    ] {
        assert!(
            check_errors(source).is_empty(),
            "unexpected diagnostics for {source:?}"
        );
    }
}

#[test]
fn any_operands_widen_arithmetic_to_any() {
    assert_eq!(op_result(BinOp::Add, Type::Any, Type::Int), Some(Type::Any));
    assert_eq!(op_result(BinOp::Add, Type::Int, Type::Any), Some(Type::Any));
    assert_eq!(op_result(BinOp::Div, Type::Any, Type::Int), Some(Type::Any));
    assert_eq!(
        op_result(BinOp::Sub, Type::List, Type::Any),
        Some(Type::Any)
    );
    assert_eq!(op_result(BinOp::Lt, Type::Any, Type::Int), Some(Type::Bool));
    assert_eq!(op_result(BinOp::Eq, Type::Int, Type::Any), Some(Type::Bool));
}

#[test]
fn untyped_operands_do_not_report_unsupported_operators() {
    for source in [
        "def f(x):\n    return [1] - x\n",
        "def f(x):\n    return x + range(3)\n",
        "def f(x):\n    return range(3) + x\n",
        "def f(x):\n    return 'a' - x\n",
    ] {
        assert!(
            check_errors(source).is_empty(),
            "unexpected diagnostics for {source:?}"
        );
    }
}

#[test]
fn concrete_incompatible_operands_are_still_reported() {
    assert_eq!(
        check_errors("1 + 'x'\n"),
        vec!["unsupported operand type(s) for +: 'int' and 'str'"]
    );
    assert_eq!(
        check_errors("1 % 'a'\n"),
        vec!["unsupported operand type(s) for %: 'int' and 'str'"]
    );
    assert_eq!(
        check_errors("r = range(3)\nx = r - 1\n"),
        vec!["unsupported operand type(s) for -: 'range' and 'int'"]
    );
}

#[test]
fn bytes_percent_formatting_is_accepted() {
    assert_eq!(
        op_result(BinOp::Mod, Type::Bytes, Type::Int),
        Some(Type::Bytes)
    );
    assert!(check_errors("def f(v):\n    return b'%d' % v\n").is_empty());
}

#[test]
fn stub_bodies_accept_return_annotations() {
    for source in [
        "def f() -> int: ...\n",
        "def f() -> int:\n    ...\n",
        "def f() -> int:\n    pass\n",
        "def f() -> int:\n    \"\"\"Doc.\"\"\"\n    ...\n",
        "def f() -> int:\n    \"\"\"Doc.\"\"\"\n",
        "def f() -> int:\n    'Doc.'\n    pass\n",
        "def f() -> list:\n    ...\n",
        "async def f() -> int:\n    ...\n",
    ] {
        assert!(check_errors(source).is_empty(), "{source}");
    }
}

#[test]
fn protocol_method_stubs_typecheck() {
    let source = "\
class Proto:
    def size(self) -> int: ...
    def entries(self) -> int:
        \"\"\"Number of entries.\"\"\"
        ...
";
    assert!(check_errors(source).is_empty());
}

#[test]
fn raise_only_bodies_accept_return_annotations() {
    for source in [
        "def f() -> int:\n    raise NotImplementedError\n",
        "def f() -> int:\n    raise NotImplementedError()\n",
        "def f() -> int:\n    raise ValueError(\"no\")\n",
        "def f() -> int:\n    \"\"\"Doc.\"\"\"\n    raise NotImplementedError\n",
        "def f() -> int:\n    'Doc.'\n    raise ValueError('no')\n",
        "def f() -> list:\n    raise NotImplementedError\n",
        "async def f() -> int:\n    raise NotImplementedError\n",
    ] {
        assert!(check_errors(source).is_empty(), "{source}");
    }
}

#[test]
fn protocol_method_raise_bodies_typecheck() {
    // NumPy protocol methods are often spelled as a bare `raise`.
    let source = "\
class Proto:
    def __len__(self, /) -> int:
        \"\"\"Implement len.\"\"\"
        raise NotImplementedError

    def entries(self) -> int:
        raise NotImplementedError
";
    assert!(check_errors(source).is_empty());
}

#[test]
fn unannotated_raise_only_functions_used_as_values_are_accepted() {
    // The body never returns, so it cannot miss a value the call site expects.
    let source = "\
def take(x: int) -> int:
    return x

def f():
    raise ValueError('no')

take(f())
";
    assert!(check_errors(source).is_empty());
}

#[test]
fn non_stub_bodies_still_report_a_missing_return() {
    let expected = "function 'f' is declared to return int, but has no return value";
    for source in [
        // A real body with no `return`.
        "def f() -> int:\n    x = 1\n",
        "def f() -> int:\n    print(1)\n",
        // `...` mixed with real code is not a stub declaration either.
        "def f() -> int:\n    ...\n    x = 1\n",
        "def f() -> int:\n    x = 1\n    ...\n",
        // Only a lone `raise` cannot fall through: a conditional raise and a
        // raise followed by more code still fall through.
        "def f(x: bool) -> int:\n    if x:\n        raise ValueError\n",
        "def f(x: bool) -> int:\n    \"\"\"Doc.\"\"\"\n    if x:\n        raise ValueError\n",
        "def f() -> int:\n    raise ValueError\n    x = 1\n",
    ] {
        let diags = check_diags(source);
        assert_eq!(diags.len(), 1, "{source}: {diags:?}");
        assert_eq!(diags[0].code, codes::CONTROL, "{source}");
        assert_eq!(diags[0].message, expected, "{source}");
    }
}

#[test]
fn unannotated_stubs_used_as_values_still_report() {
    // Only a declared return type makes a stub body a promise; without one the
    // result type is inferred from its uses, and a `None` body cannot satisfy
    // them.
    let diags =
        check_diags("def take(x: int) -> int:\n    return x\n\ndef f():\n    ...\n\ntake(f())\n");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::CONTROL);
    assert_eq!(
        diags[0].message,
        "function 'f' has no return value but is used as a value-returning function"
    );
}

#[test]
fn stub_bodies_keep_their_declared_return_type() {
    let program = crate::check_source("def f() -> int: ...\nprint(f())\n", "test.py")
        .expect("a stub body typechecks");
    assert_eq!(program_function(&program, "f").ret_ty, Type::Int);
}

#[test]
fn docstring_only_bodies_are_declared_stubs() {
    // Protocol methods in typing stubs often document the contract and omit
    // the `...` marker (`typing_extensions.Reader.read`). The declared return
    // type still describes the implementation's result.
    let program = crate::check_source(
        "def f() -> int:\n    \"\"\"Documented stub.\"\"\"\n\nprint(f())\n",
        "test.py",
    )
    .expect("a docstring-only stub typechecks");
    assert_eq!(program_function(&program, "f").ret_ty, Type::Int);

    let source = "class Reader:\n\x20   def read(self, size: int = -1) -> bytes:\n\x20       \"\"\"Read at most *size* bytes.\"\"\"\n";
    let errors = check_errors(source);
    assert!(errors.is_empty(), "{errors:?}");

    // Without a declared return type a docstring-only body is still an
    // implementation returning `None`, so a use as a value reports.
    let diags = check_diags(
        "def take(x: int) -> int:\n    return x\n\ndef f():\n    \"\"\"Doc.\"\"\"\n\ntake(f())\n",
    );
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::CONTROL);
}

#[test]
fn mixed_bare_and_value_returns_are_allowed() {
    // A bare `return` returns `None`, and CPython allows one function to mix
    // it with value returns. The inferred result is the dynamic join, no
    // matter which form the body checks first. This is `uuid._parse_mac`'s
    // shape: a guard that returns early and a value return at the end.
    let source = "\
def leading(flag):
    if flag:
        return
    return 1

def trailing(flag):
    if flag:
        return 1
    return
";
    let program =
        crate::check_source(source, "test.py").expect("mixed bare and value returns typecheck");
    assert_eq!(program_function(&program, "leading").ret_ty, Type::Any);
    assert_eq!(program_function(&program, "trailing").ret_ty, Type::Any);

    // The bare `return` still lowers to a `None` return, so the branch that
    // takes it produces `None` at runtime.
    let leading = program_function(&program, "leading");
    let StmtKind::If { then_body, .. } = &leading.body[0].kind else {
        panic!("expected an if statement, got {:?}", leading.body[0].kind);
    };
    assert!(matches!(then_body[0].kind, StmtKind::Return(None)));
}

#[test]
fn bare_return_inside_a_loop_mixes_with_value_returns() {
    // uuid._parse_mac returns `int(...)` at the end and `None` from the
    // guards; the guards may sit inside a loop or a `try` block.
    let source = "\
def first_positive(values):
    for value in values:
        if value <= 0:
            return
        return value
    return 0

def parse_mac(value):
    if value is None:
        return None
    for part in value:
        if part == ':':
            return
    try:
        return int(value, 16)
    except ValueError:
        return
";
    let program = crate::check_source(source, "test.py")
        .expect("loops and guarded returns mix with value returns");
    assert_eq!(
        program_function(&program, "first_positive").ret_ty,
        Type::Any
    );
    assert_eq!(program_function(&program, "parse_mac").ret_ty, Type::Any);
}

#[test]
fn a_declared_return_type_tolerates_a_bare_return() {
    // The annotation stays the contract for value returns; a bare return on
    // one path is legal Python and must not be reported as inconsistent.
    let source = "def f(flag: bool) -> int:\n    if flag:\n        return\n    return 1\n";
    let program =
        crate::check_source(source, "test.py").expect("a bare return is legal with an annotation");
    assert_eq!(program_function(&program, "f").ret_ty, Type::Int);
}

#[test]
fn bare_returns_only_infer_none() {
    let program = crate::check_source(
        "def stop(flag):\n    if flag:\n        return\n    return\n",
        "test.py",
    )
    .expect("only bare returns typecheck");
    assert_eq!(program_function(&program, "stop").ret_ty, Type::None);
}

#[test]
fn a_declared_return_type_still_rejects_bare_return_only_bodies() {
    // `return` alone never produces the declared value, so the missing-return
    // diagnostic stays.
    let diags =
        check_diags("def f(flag: bool) -> int:\n    if flag:\n        return\n    return\n");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::CONTROL);
    assert_eq!(
        diags[0].message,
        "function 'f' is declared to return int, but has no return value"
    );
}

#[test]
fn generator_bare_returns_keep_yield_semantics() {
    // A generator's `return` value is its result; a bare one is unchanged.
    let source = "\
def gen(flag):
    yield 1
    if flag:
        return
    return 2
";
    let program =
        crate::check_source(source, "test.py").expect("generators allow both return forms");
    let g = program_function(&program, "gen");
    assert!(g.is_generator, "{:?}", g.body);
}

#[test]
fn value_return_type_mismatches_are_still_reported() {
    // Relaxing the mixed-return check must not hide an incompatible value
    // return against a declared annotation.
    let diags = check_diags("def f() -> int:\n    return 'x'\n");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::TYPE);
    assert_eq!(
        diags[0].message,
        "return type str does not match declared return type int"
    );
}

#[test]
fn unannotated_parameter_sequence_body_stays_dynamic() {
    // The caller passes a marker (`bool`) where the helper uses `parameters`
    // as a sequence. The parameter is unannotated, so the call site must not
    // turn subscripting, `len` and comparisons in the body into errors.
    // `caller` is checked first, which pins the parameter before the helper
    // body is visited.
    let source = r#"
def caller():
    return _check_generic(True)

def _check_generic(parameters):
    alen = len(parameters)
    if parameters[0] < alen or parameters < 'x':
        return parameters[0]
    return parameters
"#;
    crate::check_source(source, "test.py")
        .expect("an unannotated parameter stays dynamic in the body");
}

#[test]
fn unannotated_parameter_comparison_stays_dynamic() {
    // The call pins `parameters` to `bool`; comparing it with a string is
    // invalid for that concrete pair but valid Python for an unannotated
    // parameter, whose runtime value the call site does not restrict.
    let source = r#"
def caller():
    return compare(True)

def compare(parameters):
    return parameters < 'x'
"#;
    crate::check_source(source, "test.py")
        .expect("a comparison through a gradual parameter stays dynamic");
}

#[test]
fn local_copied_from_a_gradual_parameter_stays_dynamic() {
    // A local that receives an unannotated parameter value inherits the
    // gradual contract; a call site narrowing the parameter must not turn the
    // local's comparison into a static error either.
    let source = r#"
def caller():
    return compare(True)

def compare(parameters):
    value = parameters
    return value < 'x'
"#;
    crate::check_source(source, "test.py")
        .expect("a local copied from a gradual parameter stays dynamic");
}

#[test]
fn unannotated_parameter_sequence_calls_keep_their_shape() {
    // A consistent call site still allows normal sequence operations; the
    // parameter is gradual, not rejected.
    let source = r#"
def first(xs):
    n = len(xs)
    return xs[0], n, xs == xs

first((1, 2))
"#;
    crate::check_source(source, "test.py")
        .expect("a sequence-shaped unannotated parameter typechecks");
}

#[test]
fn annotated_parameter_mismatch_is_still_reported() {
    let errors = check_errors(
        "def f(parameters: int):\n    return parameters\n\ndef caller():\n    return f('s')\n",
    );
    assert_eq!(
        errors,
        vec!["argument 1 of f has type str, expected int"],
        "{errors:?}"
    );

    // An unannotated argument conflict must not stop the loop before a
    // declared parameter is checked.
    let errors = check_errors(
        "def f(a, b: int):\n    return a, b\n\ndef caller():\n    return f('x', 'y')\n",
    );
    assert_eq!(
        errors,
        vec!["argument 2 of f has type str, expected int"],
        "{errors:?}"
    );
}

#[test]
fn unannotated_parameter_conflict_widens_to_any() {
    let program = crate::check_source(
        r#"
def f(parameters):
    return parameters

def a():
    return f(1)

def b():
    return f('s')
"#,
        "test.py",
    )
    .expect("disagreeing call sites widen an unannotated parameter");
    let f = program_function(&program, "f");
    assert!(!f.params[0].declared, "the source has no annotation");
    assert_eq!(f.params[0].ty, Type::Any);
}

#[test]
fn parameter_annotation_provenance_reaches_the_ir() {
    let program = crate::check_source(
        "def f(unannotated, annotated: int):\n    return unannotated, annotated\n",
        "test.py",
    )
    .expect("valid program");
    let f = program_function(&program, "f");
    assert!(!f.params[0].declared);
    assert!(f.params[1].declared);
    assert_eq!(f.params[1].ty, Type::Int);
}

#[test]
fn narrowed_parameter_provenance_reaches_its_reads() {
    // A call site binds an unannotated parameter to the argument type, so
    // `resolve` no longer shows the slot as gradual. Every body use reads the
    // slot through its own variable, which must keep the provenance.
    let prog = empty_prog();
    let mut inf = Infer::new();
    let ck = checker(&prog, &mut inf);
    let parameter = ck.inf.fresh();
    ck.inf.mark_gradual(parameter);
    assert!(ck.inf.unify(parameter, Ty::Concrete(Type::Bool)).is_ok());
    let read = ck.inf.fresh();
    assert!(ck.inf.unify(parameter, read).is_ok());
    assert_eq!(ck.inf.resolve(read), Ty::Concrete(Type::Bool));
    assert!(
        ck.inf.is_gradual(read),
        "a use of a narrowed unannotated parameter stays gradual"
    );

    // A slot that already holds an unrelated concrete type does not become
    // gradual just because it is unified with a gradual value.
    let typed = ck.inf.fresh();
    assert!(ck.inf.unify(typed, Ty::Concrete(Type::Int)).is_ok());
    let gradual = ck.inf.fresh();
    ck.inf.mark_gradual(gradual);
    assert!(ck.inf.unify(gradual, Ty::Concrete(Type::Bool)).is_ok());
    assert!(ck.inf.unify(typed, gradual).is_ok());
    assert!(!ck.inf.is_gradual(typed));
}

#[test]
fn gradual_boolop_operands_stay_dynamic() {
    // `configparser` builds `'get' + (key or None)` where `key` is an
    // unannotated parameter. The `or` result is `str | None` at runtime, not a
    // constant `None`; unifying the gradual operand with `None` used to pin it
    // and report a bogus `str + None`.
    let source = "def f(key):\n    return 'get' + (key or None)\n";
    crate::check_source(source, "test.py").expect("dynamic `or` result must stay open");

    // A genuine constant `str + None` is still a static error.
    let source = "x = 'get' + None\n";
    let diags = crate::check_source(source, "test.py").expect_err("str + None is invalid");
    assert!(diags[0].message.contains("'str' and 'None'"), "{diags:?}");
}

#[test]
fn range_index_returns_int() {
    // `calendar`/`_strptime`/`imaplib`/`mailbox` call `range.index(value)`;
    // the runtime implements it, so the static range method table must too.
    let source = "r = range(3)\ni = r.index(2)\n";
    let program = crate::check_source(source, "test.py").expect("range.index typechecks");
    assert_eq!(assigned_type(&program, 1), Type::Int);

    // The argument is still required.
    let source = "r = range(3)\ni = r.index()\n";
    let diags = crate::check_source(source, "test.py").expect_err("index() needs a value");
    assert!(
        diags[0].message.contains("index"),
        "missing argument is diagnosed: {diags:?}"
    );
}
