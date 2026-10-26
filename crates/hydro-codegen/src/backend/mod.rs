//! Cranelift backends: the in-process JIT (`tea infuse`) and the AOT
//! object-file emitter (`tea build`). Both share the lowering in `crate::lower`.

pub(crate) mod aot;
pub(crate) mod jit;
