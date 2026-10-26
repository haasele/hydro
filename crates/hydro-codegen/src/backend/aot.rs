//! AOT object-file backend (`tea build`).

use cranelift_module::default_libcall_names;
use cranelift_object::{ObjectBuilder, ObjectModule};
use hydro_ir::Program;

use crate::lower::{build_isa, lower_program};
use crate::{CodegenError, CodegenOptions};

pub(crate) fn compile(
    program: &Program,
    options: &CodegenOptions,
) -> Result<Vec<u8>, CodegenError> {
    let isa = build_isa(true)?;
    let builder = ObjectBuilder::new(isa, "hydro_module", default_libcall_names())
        .map_err(|e| CodegenError::new(format!("cannot create object builder: {e}")))?;
    let mut module = ObjectModule::new(builder);

    lower_program(&mut module, program, options)?;

    let product = module.finish();
    product
        .emit()
        .map_err(|e| CodegenError::new(format!("cannot emit object file: {e}")))
}
