//! In-process JIT backend (`tea infuse`).

use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::default_libcall_names;
use hydro_ir::Program;

use crate::lower::{build_isa, lower_program};
use crate::{CodegenError, CodegenOptions};

/// Register every runtime ABI symbol with the JIT so that the generated code
/// can call into `hydro_runtime` directly. The names are the `#[no_mangle]`
/// symbols documented in `docs/ARCHITECTURE.md` §3.
fn register_runtime_symbols(builder: &mut JITBuilder) {
    macro_rules! bind {
        ($($name:ident),+ $(,)?) => {
            $(
                builder.symbol(stringify!($name), hydro_runtime::abi::$name as *const u8);
            )+
        };
    }

    bind!(
        hydro_none,
        hydro_true,
        hydro_false,
        hydro_bool,
        hydro_int,
        hydro_str_new,
        hydro_str_builder_new,
        hydro_str_builder_append,
        hydro_str_builder_finish,
        hydro_bigint_from_str,
        hydro_bytes_new,
        hydro_bytes_build,
        hydro_float_new,
        hydro_complex_new,
        hydro_binop,
        hydro_unop,
        hydro_truthy,
        hydro_print,
        hydro_len,
        hydro_to_str,
        hydro_to_repr,
        hydro_format_value,
        hydro_to_int,
        hydro_to_float,
        hydro_abs,
        hydro_all,
        hydro_ord,
        hydro_chr,
        hydro_list_new,
        hydro_list_append,
        hydro_tuple_new,
        hydro_set_new,
        hydro_set_add,
        hydro_set_from,
        hydro_range_new,
        hydro_list_from,
        hydro_tuple_from,
        hydro_dict_from,
        hydro_function_new,
        hydro_closure_new,
        hydro_cell_new,
        hydro_cell_get,
        hydro_cell_set,
        hydro_function_params,
        hydro_function_meta,
        hydro_builtin_function,
        hydro_call,
        hydro_call_star,
        hydro_deferred_call,
        hydro_arity_error,
        hydro_coroutine_new,
        hydro_generator_new,
        hydro_async_generator_new,
        hydro_await,
        hydro_yield_checked,
        hydro_yield_from,
        hydro_map,
        hydro_map_many,
        hydro_filter,
        hydro_sum,
        hydro_enumerate,
        hydro_zip,
        hydro_unpack,
        hydro_unpack_seq,
        hydro_unpack_star,
        hydro_dict_new,
        hydro_dict_set,
        hydro_dict_merge,
        hydro_subscript,
        hydro_subscript_set,
        hydro_delete_subscript,
        hydro_global_delete,
        hydro_delete_attr,
        hydro_delete_slice,
        hydro_slice,
        hydro_slice_set,
        hydro_module_call,
        hydro_module_call_kw,
        hydro_math_floor_sqrt_int,
        hydro_module_get,
        hydro_module_value,
        hydro_extension_module_value,
        hydro_deferred_import,
        hydro_global_get,
        hydro_global_set,
        hydro_extension_call,
        hydro_extension_get,
        hydro_getattr,
        hydro_hasattr,
        hydro_setattr,
        hydro_class_new,
        hydro_class_new_multi,
        hydro_instance_new,
        hydro_bind_class_kwargs,
        hydro_exception_class,
        hydro_method,
        hydro_method_kw,
        hydro_call_kw,
        hydro_call_star_kw,
        hydro_method_star,
        hydro_method_star_kw,
        hydro_iter_new,
        hydro_iter_next,
        hydro_iter,
        hydro_next,
        hydro_set_file,
        hydro_set_line,
        hydro_frame_enter,
        hydro_frame_bind,
        hydro_safepoint,
        hydro_frame_leave,
        hydro_gc_frame_push,
        hydro_gc_frame_pop,
        hydro_gc_scope_push,
        hydro_gc_scope_pop,
        hydro_exc_pending,
        hydro_exc_tag,
        hydro_exc_value,
        hydro_exc_clear,
        hydro_exc_handler_push,
        hydro_exc_handler_pop,
        hydro_exc_handler_depth,
        hydro_exc_handler_truncate,
        hydro_raise,
        hydro_raise_msg,
        hydro_raise_value,
        hydro_raise_from,
        hydro_raise_value_from,
        hydro_exc_matches,
        hydro_uncaught,
        hydro_runtime_init,
        hydro_register_function,
        hydro_register_module_initializer,
        hydro_initialize_module,
        hydro_spawn_child,
        hydro_profile_reset,
        hydro_profile_hit,
        hydro_profile_dump,
    );
}

pub(crate) fn run(program: &Program, options: &CodegenOptions) -> Result<i32, CodegenError> {
    let isa = build_isa(false)?;
    let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
    register_runtime_symbols(&mut builder);
    let mut module = JITModule::new(builder);

    let entry = lower_program(&mut module, program, options)?;
    module
        .finalize_definitions()
        .map_err(|e| CodegenError::new(format!("JIT finalization failed: {e}")))?;

    let pointer = module.get_finalized_function(entry);
    // SAFETY: `pointer` refers to the just-finalized `hydro_main` function whose
    // signature is `() -> i32` (`extern "C"`). The module is still alive while
    // the function runs, so the code and its relocations remain valid.
    let entry: extern "C" fn() -> i32 = unsafe { std::mem::transmute(pointer) };
    Ok(entry())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydro_ir::{Expr, ExprKind, Function, LocalInfo, Stmt, StmtKind, Type};

    /// Module body assigning `x = math.floor(math.sqrt(x))` with an `int`
    /// local: the shape that imports `hydro_math_floor_sqrt_int`.
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
        lower_program(
            &mut module,
            &math_fast_path_program(),
            &CodegenOptions::default(),
        )
        .expect("math fast path must lower");
        // Finalization resolves every import, so a missing JIT binding for
        // `hydro_math_floor_sqrt_int` (declared by the lowering) fails here.
        module
            .finalize_definitions()
            .expect("every runtime import must be bound for the JIT");
        assert_ne!(
            hydro_runtime::abi::hydro_math_floor_sqrt_int as *const () as usize,
            0
        );
    }

    #[test]
    fn string_accumulator_symbols_are_registered_for_the_jit() {
        use hydro_ir::{Builtin, Callee};

        let line = 1;
        let text = |value: &str| {
            Expr::new(
                ExprKind::Const(hydro_ir::Const::Str(value.to_string())),
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
        lower_program(&mut module, &program, &CodegenOptions::default())
            .expect("string accumulator must lower");
        // Finalization resolves every import, so a missing JIT binding for one
        // of the `hydro_str_builder_*` symbols (declared by the lowering) fails
        // here.
        module
            .finalize_definitions()
            .expect("every runtime import must be bound for the JIT");
        assert_ne!(
            hydro_runtime::abi::hydro_str_builder_new as *const () as usize,
            0
        );
        assert_ne!(
            hydro_runtime::abi::hydro_str_builder_append as *const () as usize,
            0
        );
        assert_ne!(
            hydro_runtime::abi::hydro_str_builder_finish as *const () as usize,
            0
        );
    }

    #[test]
    fn class_runtime_symbols_are_available_to_the_jit() {
        assert_ne!(hydro_runtime::abi::hydro_class_new as *const () as usize, 0);
        assert_ne!(
            hydro_runtime::abi::hydro_exception_class as *const () as usize,
            0
        );
        assert_ne!(hydro_runtime::abi::hydro_setattr as *const () as usize, 0);
        assert_ne!(
            hydro_runtime::abi::hydro_delete_attr as *const () as usize,
            0
        );
        assert_ne!(
            hydro_runtime::abi::hydro_delete_subscript as *const () as usize,
            0
        );
        assert_ne!(
            hydro_runtime::abi::hydro_delete_slice as *const () as usize,
            0
        );
        assert_ne!(
            hydro_runtime::abi::hydro_global_delete as *const () as usize,
            0
        );
    }
}
