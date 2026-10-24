//! PyX code generator: lowers `pyx_ir::Program` to Cranelift IR and runs it
//! either through the JIT (`pyx run`) or as a native object file (`pyx build`).
//!
//! Runtime symbols are resolved from `pyx_runtime::abi` for the JIT and by the
//! linker for AOT. The exact ABI is documented in `docs/ARCHITECTURE.md` §3.

use std::fmt;

use pyx_ir::Program;

mod backend;
mod lower;

/// Internal codegen failure (never used for user program errors; those are
/// reported by the frontend or at runtime through the exception protocol).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodegenError {
    pub message: String,
}

impl CodegenError {
    pub fn new(message: impl Into<String>) -> CodegenError {
        CodegenError {
            message: message.into(),
        }
    }
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodegenError {}

/// Compile and execute the program in-process via Cranelift's JIT.
/// Returns the process exit code (0 or 1).
pub fn run_jit(program: &Program) -> Result<i32, CodegenError> {
    backend::jit::run(program)
}

/// Compile the program to a relocatable native object file (ELF on Linux).
/// The object exports `pyx_main` and `main`; the latter is the AOT entry point.
pub fn compile_object(program: &Program) -> Result<Vec<u8>, CodegenError> {
    backend::aot::compile(program)
}
