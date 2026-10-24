//! Cranelift backends: the in-process JIT (`pyx run`) and the AOT
//! object-file emitter (`pyx build`). Both share the lowering in `crate::lower`.

pub(crate) mod aot;
pub(crate) mod jit;
