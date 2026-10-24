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
        pyx_bytes_new,
        pyx_bytes_build,
        pyx_float_new,
        pyx_binop,
        pyx_unop,
        pyx_truthy,
        pyx_print,
        pyx_len,
        pyx_to_str,
        pyx_format_value,
        pyx_to_int,
        pyx_to_float,
        pyx_abs,
        pyx_list_new,
        pyx_list_append,
        pyx_tuple_new,
        pyx_set_new,
        pyx_set_add,
        pyx_set_from,
        pyx_range_new,
        pyx_list_from,
        pyx_dict_from,
        pyx_function_new,
        pyx_closure_new,
        pyx_builtin_function,
        pyx_call,
        pyx_coroutine_new,
        pyx_generator_new,
        pyx_await,
        pyx_yield,
        pyx_map,
        pyx_filter,
        pyx_sum,
        pyx_enumerate,
        pyx_zip,
        pyx_unpack,
        pyx_dict_new,
        pyx_dict_set,
        pyx_subscript,
        pyx_subscript_set,
        pyx_slice,
        pyx_module_call,
        pyx_module_get,
        pyx_extension_call,
        pyx_extension_get,
        pyx_getattr,
        pyx_setattr,
        pyx_class_new,
        pyx_method,
        pyx_method_kw,
        pyx_iter_new,
        pyx_iter_next,
        pyx_iter,
        pyx_next,
        pyx_set_file,
        pyx_set_line,
        pyx_frame_enter,
        pyx_safepoint,
        pyx_frame_leave,
        pyx_exc_pending,
        pyx_exc_tag,
        pyx_exc_value,
        pyx_exc_clear,
        pyx_raise,
        pyx_raise_msg,
        pyx_raise_value,
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
    #[test]
    fn class_runtime_symbols_are_available_to_the_jit() {
        assert_ne!(pyx_runtime::abi::pyx_class_new as *const () as usize, 0);
        assert_ne!(pyx_runtime::abi::pyx_setattr as *const () as usize, 0);
    }
}
