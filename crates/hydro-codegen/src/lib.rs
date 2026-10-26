//! Hydro code generator: lowers `hydro_ir::Program` to Cranelift IR and runs it
//! either through the JIT (`tea infuse`) or as a native object file (`tea build`).
//!
//! Runtime symbols are resolved from `hydro_runtime::abi` for the JIT and by the
//! linker for AOT. The exact ABI is documented in `docs/ARCHITECTURE.md` §3.

use std::collections::HashMap;
use std::fmt;

use hydro_ir::Program;

mod backend;
mod lower;

/// Options controlling code generation.
///
/// The default (see [`CodegenOptions::default`]) preserves the historical
/// output exactly: no profiling calls are emitted and function bodies keep
/// their source order.
#[derive(Clone, Debug, Default)]
pub struct CodegenOptions {
    /// Emit a `hydro_profile_hit` call at the entry of every generated IR
    /// function (including the module body). Wrappers are not instrumented.
    pub collect_profile: bool,
    /// Function-name -> observed call count. Used to emit hot functions first.
    ///
    /// When non-empty, real function bodies are lowered in descending observed
    /// count order (ties keep the original function-id order). Symbol ids,
    /// wrappers and semantics are unchanged. Missing names count as zero.
    pub hot_functions: HashMap<String, u64>,
}

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
    run_jit_with_options(program, &CodegenOptions::default())
}

/// Compile and execute the program in-process with explicit codegen options.
///
/// Returns the process exit code (0 or 1).
pub fn run_jit_with_options(
    program: &Program,
    options: &CodegenOptions,
) -> Result<i32, CodegenError> {
    backend::jit::run(program, options)
}

/// Compile the program to a relocatable native object file (ELF on Linux).
/// The object exports `hydro_main` and `main`; the latter is the AOT entry point.
pub fn compile_object(program: &Program) -> Result<Vec<u8>, CodegenError> {
    compile_object_with_options(program, &CodegenOptions::default())
}

/// Compile the program to a relocatable native object file with explicit
/// codegen options.
pub fn compile_object_with_options(
    program: &Program,
    options: &CodegenOptions,
) -> Result<Vec<u8>, CodegenError> {
    backend::aot::compile(program, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydro_ir::{
        Callee, Const, Expr, ExprKind, Function, FunctionId, LocalInfo, Stmt, StmtKind, Type,
    };

    /// `<module>` calling `hot()` then `cold()`; each callee assigns a local
    /// so its body is non-trivial and emitted as a real function.
    fn sample_program() -> Program {
        fn callee(name: &str, value: i64) -> Function {
            Function {
                name: name.to_string(),
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
                        value: Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1),
                    },
                    1,
                )],
                is_async: false,
                is_generator: false,
            }
        }

        fn call(id: FunctionId) -> Stmt {
            Stmt::new(
                StmtKind::Expr(Expr::new(
                    ExprKind::Call {
                        callee: Callee::Function(id),
                        args: Vec::new(),
                    },
                    Type::None,
                    1,
                )),
                1,
            )
        }

        Program {
            source_name: "test.py".to_string(),
            functions: vec![
                Function {
                    name: "<module>".to_string(),
                    params: Vec::new(),
                    capture_count: 0,
                    ret_ty: Type::None,
                    locals: Vec::new(),
                    body: vec![call(1), call(2)],
                    is_async: false,
                    is_generator: false,
                },
                callee("hot", 1),
                callee("cold", 2),
            ],
            main: 0,
            initializers: Vec::new(),
        }
    }

    fn profiled_options() -> CodegenOptions {
        let mut hot_functions = HashMap::new();
        hot_functions.insert("hot".to_string(), 100);
        hot_functions.insert("cold".to_string(), 3);
        hot_functions.insert("<module>".to_string(), 1);
        CodegenOptions {
            collect_profile: true,
            hot_functions,
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn default_options_emit_no_profile_import() {
        let object = compile_object(&sample_program()).expect("default object");
        assert!(!object.is_empty());
        assert!(!contains(&object, b"hydro_profile_hit"));
    }

    #[test]
    fn profiled_object_instruments_functions_and_stays_valid() {
        let program = sample_program();
        let default = compile_object(&program).expect("default object");
        let profiled =
            compile_object_with_options(&program, &profiled_options()).expect("profiled object");
        assert!(!default.is_empty());
        assert!(!profiled.is_empty());
        assert!(contains(&profiled, b"hydro_profile_hit"));
        assert_ne!(default, profiled);
    }

    #[test]
    fn hot_function_map_runs_with_unchanged_semantics() {
        let program = sample_program();
        let default = run_jit(&program).expect("default run");
        let profiled = run_jit_with_options(&program, &profiled_options()).expect("profiled run");
        assert_eq!(default, 0);
        assert_eq!(profiled, default);
    }
}
