//! In-process JIT backend (`pyx run`).

use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::default_libcall_names;
use pyx_ir::Program;

use crate::lower::{build_isa, lower_program};
use crate::CodegenError;

/// Register every runtime ABI symbol with the JIT so that the generated code
/// can call into `pyx_runtime` directly. The names are the `#[no_mangle]`
/// symbols documented in `docs/ARCHITECTURE.md` §3.
fn register_runtime_symbols(builder: &mut JITBuilder) {
    macro_rules! bind {
        ($($name:ident),+ $(,)?) => {
            $(
                builder.symbol(stringify!($name), pyx_runtime::abi::$name as *const u8);
            )+
        };
    }

    bind!(
        pyx_none,
        pyx_true,
        pyx_false,
        pyx_bool,
        pyx_int,
        pyx_str_new,
        pyx_str_builder_new,
        pyx_str_builder_append,
        pyx_str_builder_finish,
        pyx_bigint_from_str,
        pyx_bytes_new,
        pyx_bytes_build,
        pyx_float_new,
        pyx_binop,
        pyx_unop,
        pyx_truthy,
        pyx_print,
        pyx_len,
        pyx_to_str,
        pyx_to_repr,
        pyx_format_value,
        pyx_to_int,
        pyx_to_float,
        pyx_abs,
        pyx_all,
        pyx_ord,
        pyx_chr,
        pyx_list_new,
        pyx_list_append,
        pyx_tuple_new,
        pyx_set_new,
        pyx_set_add,
        pyx_set_from,
        pyx_range_new,
        pyx_list_from,
        pyx_tuple_from,
        pyx_dict_from,
        pyx_function_new,
        pyx_closure_new,
        pyx_cell_new,
        pyx_cell_get,
        pyx_cell_set,
        pyx_function_params,
        pyx_function_meta,
        pyx_builtin_function,
        pyx_call,
        pyx_call_star,
        pyx_deferred_call,
        pyx_arity_error,
        pyx_coroutine_new,
        pyx_generator_new,
        pyx_async_generator_new,
        pyx_await,
        pyx_yield,
        pyx_yield_from,
        pyx_map,
        pyx_filter,
        pyx_sum,
        pyx_enumerate,
        pyx_zip,
        pyx_unpack,
        pyx_unpack_seq,
        pyx_unpack_star,
        pyx_dict_new,
        pyx_dict_set,
        pyx_dict_merge,
        pyx_subscript,
        pyx_subscript_set,
        pyx_delete_subscript,
        pyx_global_delete,
        pyx_delete_attr,
        pyx_delete_slice,
        pyx_slice,
        pyx_slice_set,
        pyx_module_call,
        pyx_module_call_kw,
        pyx_math_floor_sqrt_int,
        pyx_module_get,
        pyx_module_value,
        pyx_deferred_import,
        pyx_global_get,
        pyx_global_set,
        pyx_extension_call,
        pyx_extension_get,
        pyx_getattr,
        pyx_hasattr,
        pyx_setattr,
        pyx_class_new,
        pyx_instance_new,
        pyx_bind_class_kwargs,
        pyx_exception_class,
        pyx_method,
        pyx_method_kw,
        pyx_call_kw,
        pyx_method_star,
        pyx_iter_new,
        pyx_iter_next,
        pyx_iter,
        pyx_next,
        pyx_set_file,
        pyx_set_line,
        pyx_frame_enter,
        pyx_safepoint,
        pyx_frame_leave,
        pyx_gc_frame_push,
        pyx_gc_frame_pop,
        pyx_gc_frame_used,
        pyx_gc_scope_push,
        pyx_gc_scope_pop,
        pyx_exc_pending,
        pyx_exc_tag,
        pyx_exc_value,
        pyx_exc_clear,
        pyx_exc_handler_push,
        pyx_exc_handler_pop,
        pyx_exc_handler_depth,
        pyx_exc_handler_truncate,
        pyx_raise,
        pyx_raise_msg,
        pyx_raise_value,
        pyx_raise_from,
        pyx_raise_value_from,
        pyx_exc_matches,
        pyx_uncaught,
        pyx_runtime_init,
        pyx_register_function,
        pyx_spawn_child,
    );
}

pub(crate) fn run(program: &Program) -> Result<i32, CodegenError> {
    let isa = build_isa(false)?;
    let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
    register_runtime_symbols(&mut builder);
    let mut module = JITModule::new(builder);

    let entry = lower_program(&mut module, program)?;
    module
        .finalize_definitions()
        .map_err(|e| CodegenError::new(format!("JIT finalization failed: {e}")))?;

    let pointer = module.get_finalized_function(entry);
    // SAFETY: `pointer` refers to the just-finalized `pyx_main` function whose
    // signature is `() -> i32` (`extern "C"`). The module is still alive while
    // the function runs, so the code and its relocations remain valid.
    let entry: extern "C" fn() -> i32 = unsafe { std::mem::transmute(pointer) };
    Ok(entry())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyx_ir::{Expr, ExprKind, Function, LocalInfo, Stmt, StmtKind, Type};

    /// Module body assigning `x = math.floor(math.sqrt(x))` with an `int`
    /// local: the shape that imports `pyx_math_floor_sqrt_int`.
    fn math_fast_path_program() -> Program {
        let argument = Expr::new(ExprKind::Local(0), Type::Int, 1);
        let sqrt = Expr::new(
            ExprKind::ModuleCall {
                module: "math".to_string(),
                function: "sqrt".to_string(),
                args: vec![argument],
                keywords: Vec::new(),
            },
            Type::Float,
            1,
        );
        let floor = Expr::new(
            ExprKind::ModuleCall {
                module: "math".to_string(),
                function: "floor".to_string(),
                args: vec![sqrt],
                keywords: Vec::new(),
            },
            Type::Int,
            1,
        );
        Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: vec![LocalInfo {
                    name: "x".to_string(),
                    ty: Type::Int,
                }],
                body: vec![Stmt::new(
                    StmtKind::Assign {
                        target: 0,
                        value: floor,
                    },
                    1,
                )],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        }
    }

    #[test]
    fn math_fast_path_symbol_is_registered_for_the_jit() {
        let isa = build_isa(false).expect("native ISA");
        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        register_runtime_symbols(&mut builder);
        let mut module = JITModule::new(builder);
        lower_program(&mut module, &math_fast_path_program()).expect("math fast path must lower");
        // Finalization resolves every import, so a missing JIT binding for
        // `pyx_math_floor_sqrt_int` (declared by the lowering) fails here.
        module
            .finalize_definitions()
            .expect("every runtime import must be bound for the JIT");
        assert_ne!(
            pyx_runtime::abi::pyx_math_floor_sqrt_int as *const () as usize,
            0
        );
    }

    #[test]
    fn string_accumulator_symbols_are_registered_for_the_jit() {
        use pyx_ir::{Builtin, Callee};

        let line = 1;
        let text = |value: &str| {
            Expr::new(
                ExprKind::Const(pyx_ir::Const::Str(value.to_string())),
                Type::Str,
                line,
            )
        };
        let local = |id| Expr::new(ExprKind::Local(id), Type::Any, line);
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: vec![
                    LocalInfo {
                        name: "s".to_string(),
                        ty: Type::Str,
                    },
                    LocalInfo {
                        name: "builder".to_string(),
                        ty: Type::Any,
                    },
                ],
                body: vec![
                    Stmt::new(
                        StmtKind::Assign {
                            target: 0,
                            value: text("a"),
                        },
                        line,
                    ),
                    Stmt::new(
                        StmtKind::Assign {
                            target: 1,
                            value: Expr::new(
                                ExprKind::StrBuilderNew {
                                    init: Box::new(Expr::new(ExprKind::Local(0), Type::Str, line)),
                                },
                                Type::Any,
                                line,
                            ),
                        },
                        line,
                    ),
                    Stmt::new(
                        StmtKind::Assign {
                            target: 1,
                            value: Expr::new(
                                ExprKind::StrBuilderAppend {
                                    builder: Box::new(local(1)),
                                    value: Box::new(text("b")),
                                },
                                Type::Any,
                                line,
                            ),
                        },
                        line,
                    ),
                    Stmt::new(
                        StmtKind::Assign {
                            target: 0,
                            value: Expr::new(
                                ExprKind::StrBuilderFinish {
                                    builder: Box::new(local(1)),
                                },
                                Type::Str,
                                line,
                            ),
                        },
                        line,
                    ),
                    Stmt::new(
                        StmtKind::Expr(Expr::new(
                            ExprKind::Call {
                                callee: Callee::Builtin(Builtin::Len),
                                args: vec![Expr::new(ExprKind::Local(0), Type::Str, line)],
                            },
                            Type::Int,
                            line,
                        )),
                        line,
                    ),
                ],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };

        let isa = build_isa(false).expect("native ISA");
        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        register_runtime_symbols(&mut builder);
        let mut module = JITModule::new(builder);
        lower_program(&mut module, &program).expect("string accumulator must lower");
        // Finalization resolves every import, so a missing JIT binding for one
        // of the `pyx_str_builder_*` symbols (declared by the lowering) fails
        // here.
        module
            .finalize_definitions()
            .expect("every runtime import must be bound for the JIT");
        assert_ne!(
            pyx_runtime::abi::pyx_str_builder_new as *const () as usize,
            0
        );
        assert_ne!(
            pyx_runtime::abi::pyx_str_builder_append as *const () as usize,
            0
        );
        assert_ne!(
            pyx_runtime::abi::pyx_str_builder_finish as *const () as usize,
            0
        );
    }

    #[test]
    fn class_runtime_symbols_are_available_to_the_jit() {
        assert_ne!(pyx_runtime::abi::pyx_class_new as *const () as usize, 0);
        assert_ne!(
            pyx_runtime::abi::pyx_exception_class as *const () as usize,
            0
        );
        assert_ne!(pyx_runtime::abi::pyx_setattr as *const () as usize, 0);
        assert_ne!(pyx_runtime::abi::pyx_delete_attr as *const () as usize, 0);
        assert_ne!(
            pyx_runtime::abi::pyx_delete_subscript as *const () as usize,
            0
        );
        assert_ne!(pyx_runtime::abi::pyx_delete_slice as *const () as usize, 0);
        assert_ne!(pyx_runtime::abi::pyx_global_delete as *const () as usize, 0);
    }
}
