use crate::*;

#[test]
fn op_codes_are_stable() {
    assert_eq!(BinOp::Add.code(), 0);
    assert_eq!(BinOp::Ge.code(), 12);
    assert_eq!(BinOp::In.code(), 13);
    assert_eq!(BinOp::NotIn.code(), 14);
    assert_eq!(BinOp::Is.code(), 15);
    assert_eq!(BinOp::IsNot.code(), 16);
    assert_eq!(BinOp::BitAnd.code(), 17);
    assert_eq!(BinOp::BitOr.code(), 18);
    assert_eq!(BinOp::BitXor.code(), 19);
    assert_eq!(BinOp::Shl.code(), 20);
    assert_eq!(BinOp::Shr.code(), 21);
    assert_eq!(BinOp::MatMul.code(), 22);
    assert_eq!(BinOp::MatMul.symbol(), "@");
    assert_eq!(BinOp::MatMulInPlace.code(), 23);
    assert_eq!(BinOp::MatMulInPlace.symbol(), "@=");
    assert_eq!(UnOp::Not.code(), 2);
    assert_eq!(UnOp::Invert.code(), 3);
    assert_eq!(UnOp::Invert.symbol(), "~");
    assert_eq!(Builtin::from_name("repr"), Some(Builtin::Repr));
    assert_eq!(Builtin::Repr.name(), "repr");
    assert_eq!(Builtin::from_name("hash"), Some(Builtin::Hash));
    assert_eq!(Builtin::Hash.name(), "hash");
    assert_eq!(Builtin::from_name("ord"), Some(Builtin::Ord));
    assert_eq!(Builtin::Ord.name(), "ord");
    assert_eq!(Builtin::from_name("chr"), Some(Builtin::Chr));
    assert_eq!(Builtin::Chr.name(), "chr");
    assert_eq!(Builtin::from_name("divmod"), Some(Builtin::Divmod));
    assert_eq!(Builtin::Divmod.name(), "divmod");
    assert_eq!(Builtin::from_name("callable"), Some(Builtin::Callable));
    assert_eq!(Builtin::Callable.name(), "callable");
    assert_eq!(Builtin::from_name("nope"), None);
    assert_eq!(ExcKind::ZeroDivisionError.tag(), 2);
    assert_eq!(ExcKind::from_name("ValueError"), Some(ExcKind::ValueError));
    assert_eq!(ExcKind::from_name("IndexError"), Some(ExcKind::IndexError));
    assert_eq!(ExcKind::StopIteration.tag(), 8);
    assert_eq!(ExcKind::AttributeError.tag(), 9);
    assert_eq!(ExcKind::AssertionError.tag(), 14);
    assert_eq!(ExcKind::GeneratorExit.tag(), 11);
    assert_eq!(
        ExcKind::from_class_name("AssertionError"),
        Some(ExcKind::AssertionError)
    );
    assert_eq!(
        ExcKind::from_class_name("GeneratorExit"),
        Some(ExcKind::GeneratorExit)
    );
    assert_eq!(
        ExcKind::from_class_name("ValueError"),
        Some(ExcKind::ValueError)
    );
    // `raise`/`except` keep their historical name table.
    assert_eq!(
        ExcKind::from_name("AssertionError"),
        Some(ExcKind::AssertionError)
    );
    assert_eq!(ExcKind::from_name("GeneratorExit"), None);
    assert_eq!(ExcKind::AssertionError.name(), "AssertionError");
    assert_eq!(ExcKind::GeneratorExit.name(), "GeneratorExit");
    assert_eq!(ExcKind::from_name("Nope"), None);
}

#[test]
fn const_types() {
    assert_eq!(Const::Int(1).ty(), Type::Int);
    assert_eq!(
        Const::BigInt("1152921504606846976".to_string()).ty(),
        Type::BigInt
    );
    assert_eq!(Const::None.ty(), Type::None);
    assert_eq!(Const::NotImplemented.ty(), Type::Any);
    // Documented limitation: `...` has no distinct sentinel and is `None`.
    assert_eq!(Const::Ellipsis.ty(), Type::None);
    assert_eq!(UnOp::Invert.symbol(), "~");
    assert!(Type::Bool.is_numeric());
    assert!(Type::BigInt.is_numeric());
    assert_eq!(Type::BigInt.name(), "BigInt");
    assert_eq!(Type::BigInt.to_string(), "BigInt");
    assert_ne!(Type::BigInt, Type::Int);
    assert!(!Type::Str.is_numeric());
    assert_eq!(Type::List.name(), "list");
    assert_eq!(Type::Dict.name(), "dict");
}

#[test]
fn console_builtins_and_exception_tags() {
    assert_eq!(Builtin::from_name("input"), Some(Builtin::Input));
    assert_eq!(Builtin::Input.name(), "input");
    assert_eq!(Builtin::from_name("get_ipython"), Some(Builtin::GetIpython));
    assert_eq!(Builtin::GetIpython.name(), "get_ipython");

    // New tags extend the fixed encoding without renumbering existing ones.
    assert_eq!(ExcKind::NameError.tag(), 16);
    assert_eq!(ExcKind::SystemExit.tag(), 17);
    assert_eq!(ExcKind::NameError.name(), "NameError");
    assert_eq!(ExcKind::SystemExit.name(), "SystemExit");
    assert_eq!(ExcKind::from_name("NameError"), Some(ExcKind::NameError));
    assert_eq!(ExcKind::from_name("SystemExit"), Some(ExcKind::SystemExit));
    assert_eq!(
        ExcKind::from_class_name("NameError"),
        Some(ExcKind::NameError)
    );
    assert_eq!(
        ExcKind::from_class_name("SystemExit"),
        Some(ExcKind::SystemExit)
    );
}

#[test]
fn random_and_sys_specs_expose_the_style_surface() {
    let getrandbits = stdlib::function("random", "getrandbits").expect("getrandbits");
    assert_eq!(
        (getrandbits.min_args, getrandbits.max_args, getrandbits.ret),
        (1, 1, Type::Int)
    );
    for name in ["stdout", "stderr"] {
        let stream = stdlib::constant("sys", name).expect("sys stream");
        assert_eq!(stream.ty, Type::Any);
    }
}

#[test]
fn asyncio_module_spec_is_registered() {
    let module = stdlib::module("asyncio").expect("asyncio module");
    assert_eq!(module.name, "asyncio");
    let run = stdlib::function("asyncio", "run").expect("run");
    assert_eq!((run.min_args, run.max_args, run.ret), (1, 1, Type::Any));
    let sleep = stdlib::function("asyncio", "sleep").expect("sleep");
    assert_eq!(
        (sleep.min_args, sleep.max_args, sleep.ret),
        (1, 1, Type::None)
    );
    let gather = stdlib::function("asyncio", "gather").expect("gather");
    assert_eq!(
        (gather.min_args, gather.max_args, gather.ret),
        (0, usize::MAX, Type::List)
    );
    let create_task = stdlib::function("asyncio", "create_task").expect("create_task");
    assert_eq!(
        (create_task.min_args, create_task.max_args, create_task.ret),
        (1, 1, Type::Any)
    );
    let current_task = stdlib::function("asyncio", "current_task").expect("current_task");
    assert_eq!(
        (
            current_task.min_args,
            current_task.max_args,
            current_task.ret
        ),
        (0, 0, Type::Any)
    );
    let constants = stdlib::module("asyncio").unwrap().constants;
    assert!(constants
        .iter()
        .any(|constant| constant.name == "CancelledError"));
    let task_group = stdlib::function("asyncio", "TaskGroup").expect("TaskGroup");
    assert_eq!(
        (task_group.min_args, task_group.max_args, task_group.ret),
        (0, 0, Type::Any)
    );
}
