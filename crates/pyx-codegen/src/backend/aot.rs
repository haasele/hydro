//! AOT object-file backend (`pyx build`).

use cranelift_module::default_libcall_names;
use cranelift_object::{ObjectBuilder, ObjectModule};
use pyx_ir::Program;

use crate::lower::{build_isa, lower_program};
use crate::CodegenError;

pub(crate) fn compile(program: &Program) -> Result<Vec<u8>, CodegenError> {
    let isa = build_isa(true)?;
    let builder = ObjectBuilder::new(isa, "pyx_module", default_libcall_names())
        .map_err(|e| CodegenError::new(format!("cannot create object builder: {e}")))?;
    let mut module = ObjectModule::new(builder);

    lower_program(&mut module, program)?;

    let product = module.finish();
    product
        .emit()
        .map_err(|e| CodegenError::new(format!("cannot emit object file: {e}")))
}
