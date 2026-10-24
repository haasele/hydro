use crate::*;

#[test]
fn op_codes_are_stable() {
    assert_eq!(BinOp::Add.code(), 0);
    assert_eq!(BinOp::Ge.code(), 12);
    assert_eq!(UnOp::Not.code(), 2);
    assert_eq!(ExcKind::ZeroDivisionError.tag(), 2);
    assert_eq!(ExcKind::from_name("ValueError"), Some(ExcKind::ValueError));
    assert_eq!(ExcKind::from_name("IndexError"), Some(ExcKind::IndexError));
    assert_eq!(ExcKind::StopIteration.tag(), 8);
    assert_eq!(ExcKind::AttributeError.tag(), 9);
    assert_eq!(ExcKind::from_name("Nope"), None);
}

#[test]
fn const_types() {
    assert_eq!(Const::Int(1).ty(), Type::Int);
    assert_eq!(Const::None.ty(), Type::None);
    assert!(Type::Bool.is_numeric());
    assert!(!Type::Str.is_numeric());
    assert_eq!(Type::List.name(), "list");
    assert_eq!(Type::Dict.name(), "dict");
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
        (1, 1, Type::List)
    );
    assert!(stdlib::module("asyncio").unwrap().constants.is_empty());
}
